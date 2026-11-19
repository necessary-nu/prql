//! Internal machinery for collecting and rendering debug logs.
#![doc(hidden)]

use jiff::Timestamp;
use serde::Serialize;
use std::marker::PhantomData;
use std::sync::RwLock;
use strum_macros::AsRefStr;

use crate::ir::{decl, pl, rq};
use crate::sql::pq_ast;
use prqlc_parser::lexer::lr;
use prqlc_parser::parser::pr;

/// Stores debug info about current compilation.
/// Is reset by [log_start] and [log_finish].
static CURRENT_LOG: RwLock<Option<DebugLog>> = RwLock::new(None);

pub fn log_start() {
    let mut lock = CURRENT_LOG.write().unwrap();
    assert!(lock.is_none());

    *lock = Some(DebugLog {
        started_at: rfc3339(Timestamp::now()),
        version: crate::compiler_version().to_string(),
        entries: Vec::new(),

        suppress_count: 0,
    });
}

/// A timestamp in RFC 3339 at UTC, written `+00:00`, with as many groups of
/// three fractional digits as its nanoseconds need.
fn rfc3339(timestamp: Timestamp) -> String {
    let fraction = match timestamp.subsec_nanosecond() {
        0 => "",
        nanos if nanos % 1_000_000 == 0 => "%.3f",
        nanos if nanos % 1_000 == 0 => "%.6f",
        _ => "%.9f",
    };
    timestamp
        .strftime(&format!("%Y-%m-%dT%H:%M:%S{fraction}%:z"))
        .to_string()
}

pub fn log_finish() -> Option<DebugLog> {
    let mut lock = CURRENT_LOG.write().unwrap();
    lock.take()
}

/// Will discard any new entries until the lock is dropped.
pub fn log_suppress() -> Option<LogSuppressLock> {
    LogSuppressLock::new()
}

pub fn log_stage(stage: Stage) {
    log_entry(|| DebugEntryKind::NewStage(stage));
}

pub fn log_entry(entry: impl FnOnce() -> DebugEntryKind) {
    let mut lock: std::sync::RwLockWriteGuard<'_, Option<DebugLog>> = CURRENT_LOG.write().unwrap();
    if let Some(log) = lock.as_mut() {
        if log.suppress_count > 0 {
            return;
        }

        log.entries.push(DebugEntry { kind: entry() });
    }
}

pub fn log_is_enabled() -> bool {
    let lock: std::sync::RwLockReadGuard<'_, Option<DebugLog>> = CURRENT_LOG.read().unwrap();
    if let Some(log) = lock.as_ref() {
        log.suppress_count == 0
    } else {
        false
    }
}

#[derive(Serialize)]
pub struct DebugLog {
    pub(super) started_at: String,
    pub(super) version: String,
    pub(super) entries: Vec<DebugEntry>,

    #[serde(skip)]
    suppress_count: usize,
}

#[derive(Serialize)]
pub(super) struct DebugEntry {
    pub(crate) kind: DebugEntryKind,
}

#[derive(Serialize, AsRefStr)]
pub enum DebugEntryKind {
    ReprPrql(crate::SourceTree),
    ReprLr(lr::Tokens),
    ReprPr(pr::ModuleDef),
    ReprPl(pl::ModuleDef),
    ReprDecl(decl::RootModule),
    ReprRq(rq::RelationalQuery),
    ReprPqEarly(Vec<pq_ast::SqlTransform>),
    ReprPq(pq_ast::SqlQuery),
    ReprSqlParser(Box<sqlparser::ast::Query>),
    ReprSql(String),

    Message(Message),
    NewStage(Stage),
}

#[derive(Clone, Serialize)]
pub struct Message {
    // TODO: this is inefficient, replace with Cow<'static, str>
    pub level: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub module_path: Option<String>,

    pub text: String,
}

#[derive(Clone, Copy, Serialize, AsRefStr)]
pub enum Stage {
    Parsing,
    Semantic(StageSemantic),
    Sql(StageSql),
}

impl Stage {
    pub(super) fn full_name(&self) -> String {
        let stage = self.as_ref().to_lowercase();
        let substage = self
            .sub_stage()
            .map(|s| "-".to_string() + &s.to_lowercase())
            .unwrap_or_default();
        format!("{stage}{substage}")
    }

    pub(super) fn sub_stage(&self) -> Option<&'_ str> {
        match self {
            Stage::Parsing => None,
            Stage::Semantic(s) => Some(s.as_ref()),
            Stage::Sql(s) => Some(s.as_ref()),
        }
    }
}

#[derive(Clone, Copy, Serialize, AsRefStr)]
pub enum StageSemantic {
    AstExpand,
    Resolver,
    Lowering,
}

#[derive(Clone, Copy, Serialize, AsRefStr)]
pub enum StageSql {
    Anchor,
    Postprocess,
    Main,
}

pub struct LogSuppressLock(PhantomData<usize>);

impl LogSuppressLock {
    fn new() -> Option<Self> {
        let mut lock = CURRENT_LOG.write().unwrap();
        if let Some(log) = lock.as_mut() {
            log.suppress_count += 1;

            Some(LogSuppressLock(PhantomData))
        } else {
            None
        }
    }
}

impl Drop for LogSuppressLock {
    fn drop(&mut self) {
        let mut lock = CURRENT_LOG.write().unwrap();
        if let Some(log) = lock.as_mut() {
            log.suppress_count -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::rfc3339;

    #[test]
    fn started_at_is_rfc3339() {
        let at = |nanos| rfc3339(Timestamp::new(1_700_000_000, nanos).unwrap());
        assert_eq!(at(0), "2023-11-14T22:13:20+00:00");
        assert_eq!(at(120_000_000), "2023-11-14T22:13:20.120+00:00");
        assert_eq!(at(123_456_000), "2023-11-14T22:13:20.123456+00:00");
        assert_eq!(at(123_456_789), "2023-11-14T22:13:20.123456789+00:00");
        assert_eq!(at(100), "2023-11-14T22:13:20.000000100+00:00");
    }
}
