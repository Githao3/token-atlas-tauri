//! 采集器：把各 Agent 的源数据归一成 [`UsageEntry`] 写进账本。
//!
//! 公共约定（口径出处：原版 `electron/aggregate.ts` + 各 adapter）：
//! - 不过滤 status：cancelled/error 的请求同样烧了 token（原版口径）
//! - total <= 0 的行丢弃
//! - request_id 在采集器内唯一，upsert 幂等（重扫整行覆盖）
//! - sessions 台账同步 upsert（first_ts/last_ts 取 min/max）

pub mod claude;
pub mod codex;
pub mod jsonl;
pub mod opencode;
pub mod zcode;

use crate::models::UsageEntry;
use rusqlite::{params, Connection};

/// 全量扫描：跑所有采集器，逐个入账。单个采集器失败不影响其他。
pub fn scan_all(conn: &mut Connection) -> Vec<AdapterScanResult> {
    vec![
        zcode::scan(conn),
        claude::scan(conn),
        codex::scan(conn),
        opencode::scan(conn),
    ]
}

/// 单采集器一次扫描的结果报告。
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct AdapterScanResult {
    pub adapter: &'static str,
    /// 源数据是否存在/可读
    pub available: bool,
    /// 本次 upsert 的明细行数
    pub records: u64,
    pub error: Option<String>,
}

/// 幂等 upsert 明细行（调用方负责事务）。
pub fn upsert_entries(conn: &Connection, entries: &[UsageEntry]) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "INSERT INTO usage_entries (
            request_id, adapter, model, ts, input_tokens, fresh_input_tokens,
            cache_read_tokens, cache_write_tokens, output_tokens, reasoning_tokens,
            total_tokens, input_token_semantics, session_id, project, source_file
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
        ON CONFLICT(adapter, request_id) DO UPDATE SET
            model = excluded.model,
            ts = excluded.ts,
            input_tokens = excluded.input_tokens,
            fresh_input_tokens = excluded.fresh_input_tokens,
            cache_read_tokens = excluded.cache_read_tokens,
            cache_write_tokens = excluded.cache_write_tokens,
            output_tokens = excluded.output_tokens,
            reasoning_tokens = excluded.reasoning_tokens,
            total_tokens = excluded.total_tokens,
            input_token_semantics = excluded.input_token_semantics,
            session_id = excluded.session_id,
            project = excluded.project,
            source_file = excluded.source_file",
    )?;
    let mut n = 0;
    for e in entries {
        n += stmt.execute(params![
            e.request_id,
            e.adapter,
            e.model,
            e.ts,
            e.input_tokens,
            e.fresh_input_tokens,
            e.cache_read_tokens,
            e.cache_write_tokens,
            e.output_tokens,
            e.reasoning_tokens,
            e.total_tokens,
            e.input_token_semantics,
            e.session_id,
            e.project,
            e.source_file,
        ])?;
    }
    Ok(n)
}

/// 会话台账的一行增量：(adapter, session_id, project, 该条明细 ts)。
/// 同一会话多条明细在 Rust 侧先折叠成 min/max，再 upsert。
#[derive(Debug, Clone)]
pub struct SessionTouch {
    pub adapter: &'static str,
    pub session_id: String,
    pub project: Option<String>,
    pub ts: i64,
}

/// 幂等 upsert 会话台账（调用方负责事务）。
pub fn upsert_sessions(conn: &Connection, touches: &[SessionTouch]) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "INSERT INTO sessions (adapter, session_id, project, first_ts, last_ts)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(adapter, session_id) DO UPDATE SET
            project = COALESCE(excluded.project, sessions.project),
            first_ts = MIN(sessions.first_ts, excluded.first_ts),
            last_ts = MAX(sessions.last_ts, excluded.last_ts)",
    )?;
    let mut n = 0;
    for t in touches {
        n += stmt.execute(params![t.adapter, t.session_id, t.project, t.ts, t.ts])?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn entries_upsert_is_idempotent() {
        let conn = db::open_in_memory().unwrap();
        let e = UsageEntry::from_raw(
            "zcode",
            "req-1".into(),
            "glm-5.3-flash".into(),
            1794073095726,
            &crate::models::RawParts {
                input: 1000,
                output: 100,
                ..Default::default()
            },
            None,
            None,
            None,
        );
        let tx = conn.unchecked_transaction().unwrap();
        assert_eq!(upsert_entries(&tx, &[e.clone()]).unwrap(), 1);
        assert_eq!(upsert_entries(&tx, &[e]).unwrap(), 1); // 二次 upsert 仍 1 行
        tx.commit().unwrap();
        let (count, total): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), SUM(total_tokens) FROM usage_entries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(total, 1100);
    }

    #[test]
    fn sessions_fold_min_max() {
        let conn = db::open_in_memory().unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        upsert_sessions(
            &tx,
            &[
                SessionTouch { adapter: "zcode", session_id: "s1".into(), project: Some("p".into()), ts: 100 },
                SessionTouch { adapter: "zcode", session_id: "s1".into(), project: None, ts: 300 },
                SessionTouch { adapter: "zcode", session_id: "s1".into(), project: None, ts: 200 },
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        let (first, last, project): (i64, i64, Option<String>) = conn
            .query_row(
                "SELECT first_ts, last_ts, project FROM sessions WHERE session_id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((first, last), (100, 300));
        assert_eq!(project.as_deref(), Some("p"));
    }
}
