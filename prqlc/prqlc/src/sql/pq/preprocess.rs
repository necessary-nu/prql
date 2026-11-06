use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};

use itertools::Itertools;

use super::anchor::{infer_complexity, CidCollector, Complexity};
use super::ast::*;

use crate::ir::generic::{ColumnSort, SortDirection, WindowFrame, WindowKind};
use crate::ir::pl::Literal;
use crate::ir::rq::{
    self, maybe_binop, new_binop, CId, Compute, Expr, ExprKind, RqFold, Transform, Window,
};
use crate::sql::Context;
use crate::{debug, Error, Result};
use prqlc_parser::generic::{InterpolateItem, Range};

/// Converts RQ AST into SqlRQ AST and applies a few preprocessing operations.
///
/// Note that some SQL translation mechanisms depend on behavior of some of these
/// functions (i.e. reorder).
pub(in crate::sql) fn preprocess(
    pipeline: Vec<Transform>,
    ctx: &mut Context,
) -> Result<Vec<SqlTransform>> {
    Ok(pipeline)
        .and_then(normalize)
        .and_then(|p| wrap(p, ctx))
        .and_then(|p| prune_inputs(p, ctx))
        .and_then(|p| distinct(p, ctx))
        .and_then(|p| union(p, ctx))
        .map(reorder)
        .inspect(|p| {
            debug::log_entry(|| debug::DebugEntryKind::ReprPqEarly(p.clone()));
        })
}

// This function was disabled because it changes semantics of the pipeline in some cases.
// /// Pushes all [Transform::Select]s to the back of the pipeline.
// pub(in crate::sql) fn push_down_selects(pipeline: Vec<Transform>) -> Vec<Transform> {
//     let mut select = None;
//     let mut res = Vec::with_capacity(pipeline.len());
//     for t in pipeline {
//         if let Transform::Select(_) = t {
//             select = Some(t);
//         } else {
//             res.push(t);
//         }
//     }
//     if let Some(select) = select {
//         res.push(select);
//     }
//     res
// }

/// Removes unused relation inputs
pub(in crate::sql) fn prune_inputs(
    mut pipeline: Vec<SqlTransform>,
    ctx: &mut Context,
) -> Result<Vec<SqlTransform>> {
    use SqlTransform::Super;

    let mut used_cids = HashSet::new();

    let mut res = Vec::new();
    while let Some(mut transform) = pipeline.pop() {
        // collect cids (special case for Join & From)
        match transform {
            SqlTransform::Join { ref filter, .. } => {
                used_cids.extend(CidCollector::collect(filter.clone()));
            }
            SqlTransform::From(_) => {}
            Super(t) => {
                let (t, cids) = CidCollector::collect_t(t);
                used_cids.extend(cids);
                transform = Super(t);
            }
            _ => unreachable!(),
        }

        // prune unused inputs
        if let SqlTransform::From(with) | SqlTransform::Join { with, .. } = &mut transform {
            let relation = ctx.anchor.relation_instances.get_mut(with).unwrap();
            (relation.table_ref.columns).retain(|(_, cid)| used_cids.contains(cid));
        }

        res.push(transform);
    }

    res.reverse();
    Ok(res)
}

pub(in crate::sql) fn wrap(pipe: Vec<Transform>, ctx: &mut Context) -> Result<Vec<SqlTransform>> {
    // We map From and Join into SqlTransforms, because we need to change their RIIds.
    // Others we just wrap into SqlTransform::Super.

    pipe.into_iter()
        .map(|x| {
            Ok(match x {
                Transform::From(table_ref) => {
                    let riid = ctx
                        .anchor
                        .create_relation_instance(table_ref, HashMap::new());
                    SqlTransform::From(riid)
                }
                Transform::Join { with, side, filter } => {
                    let with = ctx.anchor.create_relation_instance(with, HashMap::new());
                    SqlTransform::Join { with, side, filter }
                }
                x => SqlTransform::Super(x),
            })
        })
        .try_collect()
}

fn vecs_contain_same_elements<T: Eq + std::hash::Hash>(a: &[T], b: &[T]) -> bool {
    let a: HashSet<&T, RandomState> = a.iter().collect();
    let b: HashSet<&T, RandomState> = b.iter().collect();
    a == b
}

/// Creates [SqlTransform::Distinct] from [Transform::Take]
pub(in crate::sql) fn distinct(
    pipeline: Vec<SqlTransform>,
    ctx: &mut Context,
) -> Result<Vec<SqlTransform>> {
    use SqlTransform::Super;
    use Transform::*;

    let mut res = Vec::new();
    for transform in pipeline.clone() {
        match transform {
            Super(Take(rq::Take { ref partition, .. })) if partition.is_empty() => {
                res.push(transform);
            }

            Super(Take(rq::Take {
                range,
                partition,
                sort,
            })) => {
                let range_int = range
                    .clone()
                    .try_map(as_int)
                    .map_err(|_| Error::new_simple("Invalid take arguments"))?;

                let take_only_first =
                    range_int.start.unwrap_or(1) == 1 && matches!(range_int.end, Some(1));

                // Check whether the columns within the partition are the same
                // as the columns in the table; otherwise we can't use DISTINCT.
                let columns_in_frame = ctx.anchor.determine_select_columns(&pipeline.clone());
                let matching_columns = vecs_contain_same_elements(&columns_in_frame, &partition);

                // A literal key partitions nothing. Written into `DISTINCT ON`,
                // PostgreSQL and DuckDB read it as an output-column position or
                // reject it, and MySQL rejects it in the `PARTITION BY` of the
                // row-number fallback (see `AnchorContext::is_literal`). Only the
                // DISTINCT test above compares the partition against the frame,
                // and a literal can be part of the frame, so it sees the full key.
                let partition = partition
                    .into_iter()
                    .filter(|cid| !ctx.anchor.is_literal(*cid))
                    .collect_vec();

                if take_only_first && sort.is_empty() && matching_columns {
                    // DISTINCT

                    res.push(SqlTransform::Distinct);
                } else if partition.is_empty() {
                    // Every key was a literal, so there is one group: take from the
                    // whole relation, spelled the way `sort | take` lowers. The
                    // group's sort decides which rows; with none, the empty sort
                    // still drops any earlier ordering, which a group does not
                    // inherit, as the `DISTINCT ON` branch below does.
                    res.push(Super(Sort(sort.clone())));
                    res.push(Super(Take(rq::Take {
                        range,
                        partition,
                        sort,
                    })));
                } else if ctx.dialect.supports_distinct_on() && range_int.end == Some(1) {
                    // DISTINCT ON (only if we want to select only one row per group)

                    let sort = if sort.is_empty() {
                        vec![]
                    } else {
                        [into_column_sort(&partition), sort].concat()
                    };

                    res.push(SqlTransform::Sort(sort));
                    res.push(SqlTransform::DistinctOn(partition));
                } else {
                    // convert `take range` into:
                    //   derive _rn = s"ROW NUMBER"
                    //   filter (_rn | in range)
                    res.extend(create_filter_by_row_number(range, sort, partition, ctx));
                }
            }
            _ => {
                res.push(transform);
            }
        }
    }
    Ok(res)
}

fn into_column_sort(partition: &[CId]) -> Vec<ColumnSort<CId>> {
    partition
        .iter()
        .map(|cid| ColumnSort {
            direction: SortDirection::Asc,
            column: *cid,
        })
        .collect_vec()
}

fn create_filter_by_row_number(
    range: Range<Expr>,
    sort: Vec<ColumnSort<CId>>,
    partition: Vec<CId>,
    ctx: &mut Context,
) -> Vec<SqlTransform> {
    // declare new column
    let expr = Expr {
        kind: ExprKind::SString(vec![InterpolateItem::String("ROW_NUMBER()".to_string())]),
        span: None,
    };

    let is_unsorted = sort.is_empty();
    let window = Window {
        frame: if is_unsorted {
            WindowFrame {
                kind: WindowKind::Rows,
                range: Range::unbounded(),
            }
        } else {
            WindowFrame {
                kind: WindowKind::Range,
                range: Range {
                    start: None,
                    end: Some(int_expr(0)),
                },
            }
        },
        partition,
        sort,
    };

    let compute = Compute {
        id: ctx.anchor.cid.gen(),
        expr,
        window: Some(window),
        is_aggregation: false,
    };

    ctx.anchor.register_compute(compute.clone());

    let col_ref = Expr {
        kind: ExprKind::ColumnRef(compute.id),
        span: None,
    };

    // add the two transforms
    let range_int = range.try_map(as_int).unwrap();

    let compute = SqlTransform::Super(Transform::Compute(compute));
    let filter = SqlTransform::Super(Transform::Filter(match (range_int.start, range_int.end) {
        (Some(s), Some(e)) if s == e => new_binop(col_ref, "std.eq", int_expr(s)),
        (start, end) => {
            let start = start.map(|start| new_binop(col_ref.clone(), "std.gte", int_expr(start)));
            let end = end.map(|end| new_binop(col_ref, "std.lte", int_expr(end)));

            maybe_binop(start, "std.and", end).unwrap_or(Expr {
                kind: ExprKind::Literal(Literal::Boolean(true)),
                span: None,
            })
        }
    }));

    vec![compute, filter]
}

fn as_int(expr: Expr) -> Result<i64, ()> {
    let lit = expr.kind.as_literal().ok_or(())?;
    lit.as_integer().cloned().ok_or(())
}

fn int_expr(i: i64) -> Expr {
    Expr {
        span: None,
        kind: ExprKind::Literal(Literal::Integer(i)),
    }
}

/// Creates [SqlTransform::Union], [SqlTransform::Intersect] and
/// [SqlTransform::Except] from [Transform::Append], [Transform::Intersect] and
/// [Transform::Except]
pub(in crate::sql) fn union(
    pipeline: Vec<SqlTransform>,
    ctx: &mut Context,
) -> Result<Vec<SqlTransform>> {
    use SqlTransform::*;

    let mut res = Vec::with_capacity(pipeline.len());
    let mut pipeline = pipeline.into_iter().peekable();
    while let Some(t) = pipeline.next() {
        let (bottom, op) = match t {
            Super(Transform::Append(bottom)) => (bottom, SetOp::Union),
            Super(Transform::Intersect(bottom)) => (bottom, SetOp::Intersect),
            Super(Transform::Except(bottom)) => (bottom, SetOp::Except),
            t => {
                res.push(t);
                continue;
            }
        };
        let bottom = ctx.anchor.create_relation_instance(bottom, HashMap::new());

        // `UNION ALL` and `INTERSECT ALL` followed by `DISTINCT` are their
        // `DISTINCT` forms. So are `INTERSECT ALL` and `EXCEPT ALL` of a
        // distinct top: each row of it then occurs once, which is what the
        // `DISTINCT` forms make of it, whatever the bottom holds. `EXCEPT ALL`
        // followed by `DISTINCT` is not: it keeps a row the bottom holds fewer
        // times than the top.
        let distinct_top = op != SetOp::Union && matches!(res.last(), Some(Distinct));
        let distinct_after = op != SetOp::Except && matches!(pipeline.peek(), Some(Distinct));
        if distinct_top {
            res.pop();
        }
        if distinct_after {
            pipeline.next();
        }
        let distinct = distinct_top || distinct_after;

        if !distinct {
            let supported = match op {
                SetOp::Union => true,
                SetOp::Intersect => ctx.dialect.intersect_all(),
                SetOp::Except => ctx.dialect.except_all(),
            };
            if !supported {
                return Err(Error::new_simple(format!(
                    "The dialect {:?} does not support {}",
                    ctx.dialect,
                    op.sql_all()
                )));
            }
        }

        res.push(match op {
            SetOp::Union => Union { bottom, distinct },
            SetOp::Intersect => Intersect { bottom, distinct },
            SetOp::Except => Except { bottom, distinct },
        });
    }
    Ok(res)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetOp {
    Union,
    Intersect,
    Except,
}

impl SetOp {
    fn sql_all(self) -> &'static str {
        match self {
            SetOp::Union => "UNION ALL",
            SetOp::Intersect => "INTERSECT ALL",
            SetOp::Except => "EXCEPT ALL",
        }
    }
}

/// Pull Compute transforms in front of other transforms if possible.
/// Position of Compute is important for two reasons:
/// - when splitting pipelines, they provide information in which pipeline the
///   column is computed and subsequently, with which table name should be used
///   for name materialization.
/// - the transform order in SQL requires Computes to be before Filter. This
///   can be circumvented by materializing the column earlier in the pipeline,
///   which is done in this function.
pub(in crate::sql) fn reorder(mut pipeline: Vec<SqlTransform>) -> Vec<SqlTransform> {
    use SqlTransform::Super;
    use Transform::*;

    // iter over Computes
    for i in 1..pipeline.len() {
        if !matches!(&pipeline[i], Super(Compute(_))) {
            continue;
        }

        // iter all preceding transforms
        for j in 0..(i - 1) {
            let compute_i = i - j;
            let prev_i = compute_i - 1;

            let compute = pipeline[compute_i]
                .as_super()
                .unwrap()
                .as_compute()
                .unwrap();
            let prev = &pipeline[prev_i];

            let should_swap = match prev {
                // don't reorder with From or Join or another Compute
                SqlTransform::From(_) | SqlTransform::Join { .. } | Super(Compute(_)) => false,

                // reorder always
                Super(Sort(_)) => true,

                // reorder if col decl is plain
                Super(Take(_)) if infer_complexity(compute) == Complexity::Plain => true,

                // don't reorder by default
                _ => false,
            };

            if should_swap {
                pipeline.swap(compute_i, prev_i);
            } else {
                break;
            }
        }
    }
    pipeline
}

/// Normalize query:
/// - Swap null checks such that null is always on the right side.
///   This is needed to simplify code for Except and for compiling to IS NULL.
pub(in crate::sql) fn normalize(pipeline: Vec<Transform>) -> Result<Vec<Transform>> {
    Normalizer {}.fold_transforms(pipeline)
}

struct Normalizer {}

impl RqFold for Normalizer {
    fn fold_expr(&mut self, expr: Expr) -> Result<Expr> {
        let expr = Expr {
            kind: rq::fold_expr_kind(self, expr.kind)?,
            ..expr
        };

        if let ExprKind::Operator { name, args } = &expr.kind {
            if name == "std.eq" && args.len() == 2 {
                let (left, right) = (&args[0], &args[1]);
                let span = expr.span;
                let new_args = if let ExprKind::Literal(Literal::Null) = &left.kind {
                    vec![right.clone(), left.clone()]
                } else {
                    vec![left.clone(), right.clone()]
                };
                let new_kind = ExprKind::Operator {
                    name: name.clone(),
                    args: new_args,
                };
                return Ok(Expr {
                    kind: new_kind,
                    span,
                });
            }
        }

        Ok(expr)
    }
}
