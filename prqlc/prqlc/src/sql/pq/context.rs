//! Context for the PQ anchoring stage: tracks table, column, and relation-instance
//! declarations, and generates fresh IDs and names for tables and columns as RQ is
//! lowered towards SQL.
use std::collections::{HashMap, HashSet};
use std::iter::zip;

use enum_as_inner::EnumAsInner;
use serde::Serialize;

use super::anchor::CidCollector;
use super::ast::{SqlRelation, SqlTransform};
use crate::ir::pl::Ident;
use crate::ir::rq::{
    fold_table, CId, Compute, Expr, ExprKind, Relation, RelationColumn, RelationKind,
    RelationalQuery, RqFold, TId, TableDecl, TableRef, Transform,
};
use crate::sql::pq::positional_mapping::PositionalMapper;
use crate::utils::{IdGenerator, NameGenerator};
use crate::{ir::pl::TableExternRef::LocalTable, Result};

/// The AnchorContext struct stores information about tables and columns, and
/// is used to generate new IDs and names.
#[derive(Default, Debug)]
pub struct AnchorContext {
    pub column_decls: HashMap<CId, ColumnDecl>,
    pub column_names: HashMap<CId, String>,

    pub table_decls: HashMap<TId, SqlTableDecl>,

    pub relation_instances: HashMap<RIId, RelationInstance>,

    pub positional_mapping: PositionalMapper,

    pub col_name: NameGenerator,
    pub table_name: NameGenerator,

    pub cid: IdGenerator<CId>,
    pub tid: IdGenerator<TId>,
    pub riid: IdGenerator<RIId>,
}

#[derive(Debug, Clone)]
pub struct SqlTableDecl {
    #[allow(dead_code)]
    pub id: TId,

    /// Name of the table. Sometimes pull-in from RQ name hints (or database table names).
    /// Generated in postprocessing.
    pub name: Option<Ident>,

    /// When set, any references to this decl will be redirected to the set TId.
    pub redirect_to: Option<TId>,

    /// Relation that still needs to be defined (usually as CTE) so it can be referenced by name.
    /// None means that it has already been defined, or was not needed to be defined in the
    /// first place.
    pub relation: RelationStatus,
}

#[derive(Debug, Clone)]
pub enum RelationStatus {
    /// Table or a common table expression. It can be referenced by name.
    Defined,

    /// Relation expression which is yet to be defined.
    NotYetDefined(RelationAdapter),
}

#[derive(Debug)]
pub struct RelationInstance {
    pub table_ref: TableRef,

    /// When a pipeline is split, [CId]s from first pipeline are assigned a new
    /// [CId] in the second pipeline.
    pub cid_redirects: HashMap<CId, CId>,

    /// All of cids pulled in when using a wildcard
    pub original_cids: Vec<CId>,
}

impl RelationStatus {
    /// Analogous to [Option::take]
    pub fn take_to_define(&mut self) -> RelationStatus {
        std::mem::replace(self, RelationStatus::Defined)
    }
}

/// A relation which may have already been preprocessed.
#[derive(Debug, Clone)]
pub enum RelationAdapter {
    Rq(Relation),
    Preprocessed(Vec<SqlTransform>, Vec<RelationColumn>),
    Pq(SqlRelation),
}

impl From<SqlRelation> for RelationAdapter {
    fn from(rel: SqlRelation) -> Self {
        RelationAdapter::Pq(rel)
    }
}

impl From<Relation> for RelationAdapter {
    fn from(rel: Relation) -> Self {
        RelationAdapter::Rq(rel)
    }
}

/// Table instance id
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct RIId(usize);

impl From<usize> for RIId {
    fn from(id: usize) -> Self {
        RIId(id)
    }
}

/// Column declaration.
#[derive(Debug, PartialEq, Clone, strum::AsRefStr, EnumAsInner)]
pub enum ColumnDecl {
    RelationColumn(RIId, CId, RelationColumn),
    Compute(Box<Compute>),
}

impl AnchorContext {
    /// Returns a new AnchorContext object based on a Query object. This method
    /// generates new IDs and names for tables and columns as needed.
    pub fn of(query: RelationalQuery) -> (Self, Relation) {
        let (cid, tid, query) = IdGenerator::load(query);

        let context = AnchorContext {
            cid,
            tid,
            riid: IdGenerator::new(),
            col_name: NameGenerator::new("_expr_"),
            table_name: NameGenerator::new("table_"),
            ..Default::default()
        };
        QueryLoader::load(context, query)
    }

    pub fn register_compute(&mut self, compute: Compute) {
        let id = compute.id;
        let decl = ColumnDecl::Compute(Box::new(compute));
        self.column_decls.insert(id, decl);
    }

    /// Creates a new table instance and registers it in the AnchorContext's
    /// table_instances HashMap. Also generates new IDs and names for columns
    /// as needed.
    pub fn create_relation_instance(
        &mut self,
        table_ref: TableRef,
        cid_redirects: HashMap<CId, CId>,
    ) -> RIId {
        let riid = self.riid.gen();

        for (col, cid) in &table_ref.columns {
            let def = ColumnDecl::RelationColumn(riid, *cid, col.clone());
            self.column_decls.insert(*cid, def);
        }

        let original_cids = table_ref.columns.iter().map(|(_, c)| *c).collect();
        let relation_instance = RelationInstance {
            table_ref,
            cid_redirects,
            original_cids,
        };

        self.relation_instances.insert(riid, relation_instance);
        riid
    }

    /// Returns the name of a column if it has been given a name already, or generates
    /// a new name for it and registers it in the AnchorContext's column_names HashMap.
    pub(crate) fn ensure_column_name(&mut self, cid: CId) -> Option<&String> {
        // don't name wildcards & named RelationColumns
        let decl = &self.column_decls[&cid];
        if let ColumnDecl::RelationColumn(_, _, col) = decl {
            match col {
                RelationColumn::Single(Some(name)) => {
                    let entry = self.column_names.entry(cid);
                    return Some(entry.or_insert_with(|| name.clone()));
                }
                RelationColumn::Wildcard => return None,
                _ => {}
            }
        }

        let entry = self.column_names.entry(cid);
        Some(entry.or_insert_with(|| self.col_name.gen()))
    }

    pub(super) fn load_names(
        &mut self,
        pipeline: &[SqlTransform],
        output_cols: Vec<RelationColumn>,
    ) {
        let output_cids = self.determine_select_columns(pipeline);

        assert_eq!(output_cids.len(), output_cols.len());

        for (cid, col) in zip(output_cids.iter(), output_cols) {
            if let RelationColumn::Single(Some(name)) = col {
                self.column_names.insert(*cid, name);
            }
        }
    }

    pub(super) fn determine_select_columns(&self, pipeline: &[SqlTransform]) -> Vec<CId> {
        use SqlTransform::Super;

        if let Some((last, remaining)) = pipeline.split_last() {
            match last {
                SqlTransform::From(table) => {
                    let rel = self.relation_instances.get(table).unwrap();
                    rel.table_ref.columns.iter().map(|(_, cid)| *cid).collect()
                }
                SqlTransform::Join { with, .. } => {
                    let mut cols = self.determine_select_columns(remaining);

                    let with = self.relation_instances.get(with).unwrap();
                    let with = &with.table_ref.columns;
                    cols.extend(with.iter().map(|(_, cid)| *cid));
                    cols
                }
                Super(Transform::Select(cols)) => cols.clone(),
                Super(Transform::Aggregate { partition, compute }) => {
                    [partition.clone(), compute.clone()].concat()
                }
                _ => self.determine_select_columns(remaining),
            }
        } else {
            Vec::new()
        }
    }

    /// True when the column is a literal: a constant written out in the SQL as a
    /// bare token such as `5`, `-5`, `'k'`, `true` or `NULL`.
    ///
    /// Such a column never distinguishes one row from another, so as a key it
    /// partitions nothing. Written into a key it is harmful. PostgreSQL, SQLite,
    /// DuckDB and MySQL all read an integer literal in `GROUP BY` or `ORDER BY`
    /// as the position of an output column, and PostgreSQL and DuckDB do the
    /// same in `DISTINCT ON`. PostgreSQL rejects any other literal in those
    /// positions.
    ///
    /// Follows renames (`derive {y = x}`) and negation, which PostgreSQL, SQLite
    /// and DuckDB fold into the literal (`-5`).
    pub(crate) fn is_literal(&self, cid: CId) -> bool {
        let Some(ColumnDecl::Compute(compute)) = self.column_decls.get(&cid) else {
            return false;
        };
        if compute.window.is_some() || compute.is_aggregation {
            return false;
        }
        self.is_literal_expr(&compute.expr)
    }

    fn is_literal_expr(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Literal(_) => true,
            ExprKind::ColumnRef(cid) => self.is_literal(*cid),
            ExprKind::Operator { name, args } if name == "std.neg" => {
                matches!(args.as_slice(), [arg] if self.is_literal_expr(arg))
            }
            _ => false,
        }
    }

    /// What a SELECT grouped by whole rows reads, outside an aggregation, of the
    /// relations it groups that way.
    ///
    /// `group_by` holds the grouping keys, and a wildcard among them groups its
    /// relation by the whole row (`GROUP BY cake.*`). PostgreSQL does not carry
    /// the functional dependency from that whole-row value to the row's columns:
    /// beside such a key, a column of the relation may be read outside an
    /// aggregate function only if it is written as a key of its own. That holds
    /// in the select list, in `HAVING`, in `ORDER BY` and in `DISTINCT ON`
    /// alike, and inside an expression as much as bare.
    ///
    /// `cids` are the columns the SELECT reads after grouping, and `exprs` are
    /// expressions it reads them through (the `HAVING` conditions). An
    /// aggregation is not looked into, because a column read inside one needs no
    /// grouping. Any other computed column that is not itself a key is looked
    /// through to the columns it reads.
    ///
    /// Returns the named keys of those relations that are read, which must be
    /// written out beside the whole-row key, or else the first read no key
    /// covers: a column that is not a key, or the relation's star, which stands
    /// for all of its columns.
    pub(crate) fn whole_row_reads(
        &self,
        group_by: &[CId],
        cids: &[CId],
        exprs: &[Expr],
    ) -> std::result::Result<Vec<CId>, CId> {
        let whole_rows: HashSet<RIId> = group_by
            .iter()
            .filter_map(|cid| match self.column_decls.get(cid) {
                Some(ColumnDecl::RelationColumn(riid, _, RelationColumn::Wildcard)) => Some(*riid),
                _ => None,
            })
            .collect();

        let mut pending: Vec<CId> = cids.iter().rev().copied().collect();
        for expr in exprs.iter().rev() {
            pending.extend(CidCollector::collect(expr.clone()));
        }
        let mut named_keys = Vec::new();
        let mut seen = HashSet::new();
        while let Some(cid) = pending.pop() {
            if !seen.insert(cid) {
                continue;
            }
            match self.column_decls.get(&cid) {
                Some(ColumnDecl::RelationColumn(riid, _, col)) if whole_rows.contains(riid) => {
                    let is_star = matches!(col, RelationColumn::Wildcard);
                    if is_star || !group_by.contains(&cid) {
                        return Err(cid);
                    }
                    named_keys.push(cid);
                }
                Some(ColumnDecl::Compute(compute)) => {
                    if compute.is_aggregation || group_by.contains(&cid) {
                        continue;
                    }
                    pending.extend(CidCollector::collect(compute.expr.clone()));
                    if let Some(window) = &compute.window {
                        pending.extend(window.partition.iter().copied());
                        pending.extend(window.sort.iter().map(|sort| sort.column));
                    }
                }
                _ => {}
            }
        }
        Ok(named_keys)
    }

    pub(crate) fn contains_wildcard(&self, cids: &[CId]) -> bool {
        for cid in cids {
            let decl = &self.column_decls[cid];
            if let ColumnDecl::RelationColumn(_, _, RelationColumn::Wildcard) = decl {
                return true;
            }
        }
        false
    }

    pub fn lookup_table_decl(&self, tid: &TId) -> Option<&SqlTableDecl> {
        let mut tid = tid;
        loop {
            let res = self.table_decls.get(tid)?;
            match &res.redirect_to {
                Some(redirect) => tid = redirect,
                None => return Some(res),
            }
        }
    }
}

/// Loads info about [Query] into [AnchorContext]
struct QueryLoader {
    context: AnchorContext,
}

impl QueryLoader {
    fn load(context: AnchorContext, query: RelationalQuery) -> (AnchorContext, Relation) {
        let mut loader = QueryLoader { context };

        for t in query.tables {
            loader.load_table(t).unwrap();
        }
        let relation = loader.fold_relation(query.relation).unwrap();
        (loader.context, relation)
    }

    fn load_table(&mut self, table: TableDecl) -> Result<()> {
        let decl = fold_table(self, table)?;
        let mut name = decl.name.clone().map(Ident::from_name);

        // assume name of the LocalTable that the relation is referencing
        if let RelationKind::ExternRef(LocalTable(table)) = &decl.relation.kind {
            name = Some(table.clone());
        }

        let sql_decl = SqlTableDecl {
            id: decl.id,
            name,
            relation: if matches!(decl.relation.kind, RelationKind::ExternRef(_)) {
                // this relation can be materialized by just using table name as a reference
                // ... i.e. it's already defined.
                RelationStatus::Defined
            } else {
                // this relation should be defined when needed
                RelationStatus::NotYetDefined(decl.relation.into())
            },
            redirect_to: None,
        };

        self.context.table_decls.insert(decl.id, sql_decl);
        Ok(())
    }
}

impl RqFold for QueryLoader {
    fn fold_compute(&mut self, compute: Compute) -> Result<Compute> {
        self.context.register_compute(compute.clone());
        Ok(compute)
    }

    fn fold_table_ref(&mut self, table_ref: TableRef) -> Result<TableRef> {
        Ok(table_ref)
    }
}
