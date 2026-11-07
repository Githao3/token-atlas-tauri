//! codex 采集器：`~/.codex/sessions/**/*.jsonl`，JSONL 增量扫描。
//!
//! 口径出处：原版 `electron/adapters/codex.ts` + AGENTS.md 坑记录。要点：
//! - **`total_token_usage` 是累计值**，必须取相邻两条的差值；
//!   用 `last_token_usage` 会因每轮重复出现而翻倍；
//! - 每个文件维护累计值累加器 `prev`，跨扫描持久化在游标 parser_state；
//! - 差分 `max(0, cur - prev)` 防计数器回退，dTotal <= 0 跳过；
//! - `payload.info` 可为 null（实测存在），必须判空；
//! - 新版 codex 有 `cache_write_input_tokens`（0.166+），原版硬编码 0
//!   的口径升级为按差分映射；
//! - 模型/会话/项目来自 `session_meta` 与 `turn_context` 行，增量续读时
//!   由 parser_state 恢复；
//! - fork/重写防重：request_id = `{session_id}:{ordinal}`（同文件重扫幂等），
//!   另将事件语义哈希记入 dedup_ledger，回放的重复事件不再计数。

use super::jsonl::{load_cursors, sync_jsonl, upsert_cursor, FileCursor, JsonlSync};
use super::{upsert_entries, upsert_sessions, AdapterScanResult, SessionTouch};
use crate::models::{normalize_model, normalize_project, RawParts, UsageEntry};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

pub const ADAPTER: &str = "codex";

fn source_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
}

/// 文件内跨扫描状态：累计值累加器 + 会话上下文。
/// 序列化进 file_cursors.parser_state，与游标原子提交。
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct CodexState {
    #[serde(default)]
    prev: Cumul,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Cumul {
    input: i64,
    cached: i64,
    cache_write: i64,
    output: i64,
    reasoning: i64,
    total: i64,
}

/// token_count 行的 payload.info.total_token_usage。
#[derive(Debug, serde::Deserialize)]
struct TokenCountPayload {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    info: Option<TokenInfo>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TokenInfo {
    #[serde(default)]
    total_token_usage: Option<TokenUsage>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cached_input_tokens: i64,
    #[serde(rename = "cache_write_input_tokens", default)]
    cache_write_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(rename = "reasoning_output_tokens", default)]
    reasoning_output_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
}

/// session_meta / turn_context 行（只取需要的字段）。
#[derive(Debug, serde::Deserialize)]
struct MetaPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct CodexLine {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    ordinal: Option<i64>,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    payload: serde_json::Value,
}

pub fn scan(conn: &mut Connection) -> AdapterScanResult {
    let mut result = AdapterScanResult { adapter: ADAPTER, ..Default::default() };
    match scan_inner(conn) {
        Ok(records) => {
            result.available = true;
            result.records = records;
        }
        Err(e) => result.error = Some(e.to_string()),
    }
    result
}

fn scan_inner(conn: &mut Connection) -> Result<u64, Box<dyn std::error::Error>> {
    let root = source_root().ok_or("无法解析用户主目录")?;
    let files = super::jsonl::collect_jsonl_files(&root);
    if files.is_empty() {
        return Ok(0);
    }
    let cursors: HashMap<String, FileCursor> = load_cursors(conn, ADAPTER)?;

    let mut entries: Vec<UsageEntry> = Vec::new();
    let mut semantic_ids: Vec<String> = Vec::new(); // 与 entries 平行
    let mut touches: Vec<SessionTouch> = Vec::new();
    let mut cursor_writes: Vec<(String, JsonlSync, CodexState)> = Vec::new();

    for file in &files {
        let source_key = file.display().to_string();
        let cursor = cursors.get(&source_key);
        // 状态先归零；首行时若是续读（!fresh）再从 parser_state 恢复。
        // 不能在闭包里按 fresh 每行清零——fresh 是整轮常量，不是首行标志。
        let mut state = CodexState::default();
        let mut file_entries: Vec<UsageEntry> = Vec::new();
        let mut file_sems: Vec<String> = Vec::new();
        let mut state_ready = false;
        let sync = sync_jsonl(file, cursor, |line, fresh| {
            if !state_ready {
                state_ready = true;
                if !fresh {
                    if let Some(saved) = cursor.and_then(|c| c.parser_state.as_deref()) {
                        if let Ok(s) = serde_json::from_str::<CodexState>(saved) {
                            state = s;
                        }
                    }
                }
            }
            // 行级预过滤（原版同款）
            if !(line.contains("token_count") || line.contains("session_meta") || line.contains("turn_context")) {
                return;
            }
            let Ok(l) = serde_json::from_str::<CodexLine>(line) else { return };
            let ptype = l.r#type.as_deref().unwrap_or("");
            match ptype {
                "session_meta" | "turn_context" => {
                    if let Ok(meta) = serde_json::from_value::<MetaPayload>(l.payload.clone()) {
                        if meta.session_id.is_some() {
                            state.session_id = meta.session_id;
                        }
                        if meta.cwd.is_some() {
                            state.cwd = meta.cwd;
                        }
                        if meta.model.is_some() {
                            state.model = meta.model.map(|m| normalize_model(&m));
                        }
                    }
                }
                "event_msg" => {
                    let Ok(p) = serde_json::from_value::<TokenCountPayload>(l.payload.clone()) else {
                        return;
                    };
                    if p.kind != "token_count" {
                        return;
                    }
                    let Some(info) = p.info else { return };
                    let Some(u) = info.total_token_usage else { return };
                    // 差分（原版同款）：max(0, cur - prev) 防回退
                    let d_in = (u.input_tokens - state.prev.input).max(0);
                    let d_cached = (u.cached_input_tokens - state.prev.cached).max(0);
                    let d_cache_write = (u.cache_write_input_tokens - state.prev.cache_write).max(0);
                    let d_out = (u.output_tokens - state.prev.output).max(0);
                    let d_reason = (u.reasoning_output_tokens - state.prev.reasoning).max(0);
                    let d_total = (u.total_tokens - state.prev.total).max(0);
                    state.prev = Cumul {
                        input: u.input_tokens,
                        cached: u.cached_input_tokens,
                        cache_write: u.cache_write_input_tokens,
                        output: u.output_tokens,
                        reasoning: u.reasoning_output_tokens,
                        total: u.total_tokens,
                    };
                    if d_total <= 0 {
                        return;
                    }
                    let Some(ts) = l.timestamp.as_deref().and_then(parse_ts_ms) else { return };
                    let raw = RawParts {
                        input: d_in,
                        cache_read: d_cached,
                        cache_write: d_cache_write,
                        output: d_out,
                        reasoning: d_reason,
                        reported_total: Some(d_total),
                        input_excludes_cache: false,
                    };
                    let n = crate::models::normalize_parts(&raw);
                    if n.total <= 0 {
                        return;
                    }
                    let session = state.session_id.clone().unwrap_or_default();
                    let ordinal = l.ordinal.unwrap_or(-1);
                    let request_id = format!("{session}:{ordinal}");
                    let semantic_id = semantic_hash(&l.timestamp.unwrap_or_default(), &u);
                    let model = state.model.clone().unwrap_or_else(|| "unknown".to_string());
                    let project = state.cwd.as_deref().and_then(normalize_project);

                    touches.push(SessionTouch {
                        adapter: ADAPTER,
                        session_id: session.clone(),
                        project: project.clone(),
                        ts,
                    });
                    file_sems.push(semantic_id);

                    file_entries.push(UsageEntry {
                        adapter: ADAPTER.to_string(),
                        request_id,
                        model,
                        ts,
                        input_tokens: raw.input,
                        fresh_input_tokens: n.fresh_input,
                        cache_read_tokens: n.cache_read,
                        cache_write_tokens: n.cache_write,
                        output_tokens: n.output,
                        reasoning_tokens: raw.reasoning,
                        total_tokens: n.total,
                        input_token_semantics: n.semantics as u8,
                        session_id: if session.is_empty() { None } else { Some(session) },
                        project,
                        source_file: Some(source_key.clone()),
                    });
                }
                _ => {}
            }
        })?;
        if sync.skipped {
            continue;
        }
        entries.extend(file_entries);
        semantic_ids.extend(file_sems);
        cursor_writes.push((source_key, sync, state));
    }

    // 数据 + 台账 + 游标同一事务原子提交；台账当"收据"：
    // 语义指纹已存在的事件（fork 回放/重放）直接不进明细
    let tx = conn.transaction()?;
    let mut kept: Vec<UsageEntry> = Vec::new();
    for (e, sem) in entries.into_iter().zip(semantic_ids) {
        if semantic_seen(&tx, &sem)? {
            continue;
        }
        insert_receipt(&tx, &e.request_id, &sem);
        kept.push(e);
    }
    let n = upsert_entries(&tx, &kept)? as u64;
    upsert_sessions(&tx, &touches)?;
    for (key, sync, state) in &cursor_writes {
        let ps = serde_json::to_string(state).ok();
        upsert_cursor(&tx, ADAPTER, key, sync, ps.as_deref())?;
    }
    tx.commit()?;
    Ok(n)
}

/// 语义指纹是否已经入过账。
fn semantic_seen(tx: &Connection, semantic_id: &str) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM dedup_ledger WHERE adapter=?1 AND semantic_id=?2)",
        rusqlite::params![ADAPTER, semantic_id],
        |r| r.get(0),
    )
}

/// 记一条收据；台账写失败时静默放弃收据（宁可下轮重放靠幂等挡，
/// 也不因为收据写失败丢掉本轮数据）。
fn insert_receipt(tx: &Connection, request_id: &str, semantic_id: &str) {
    let _ = tx.execute(
        "INSERT OR IGNORE INTO dedup_ledger (adapter, request_id, semantic_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![ADAPTER, request_id, semantic_id, super::jsonl::now_ms()],
    );
}

/// 事件语义哈希：累计值 + 时间戳（fork 回放的同一事件数值相同）。
fn semantic_hash(timestamp: &str, u: &TokenUsage) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"token-atlas-codex-semantic-v1");
    hasher.update(timestamp.as_bytes());
    hasher.update(b"|");
    for v in [u.input_tokens, u.cached_input_tokens, u.cache_write_input_tokens, u.output_tokens, u.reasoning_output_tokens, u.total_tokens] {
        hasher.update(v.to_le_bytes());
        hasher.update(b"|");
    }
    hex(&hasher.finalize())
}

fn hex(digest: &[u8]) -> String {
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn parse_ts_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_max_zero_guards_regression() {
        // 计数器回退：cur < prev → 差值钳 0
        let prev = Cumul { input: 1000, cached: 900, total: 1058, ..Default::default() };
        let cur = TokenUsage {
            input_tokens: 900,
            cached_input_tokens: 800,
            output_tokens: 10,
            total_tokens: 910,
            ..Default::default()
        };
        let d = (cur.input_tokens - prev.input).max(0);
        let dt = (cur.total_tokens - prev.total).max(0);
        assert_eq!((d, dt), (0, 0));
    }

    /// 实机验证：扫描本机 ~/.codex/sessions。
    /// 运行：cargo test -- --ignored --nocapture live_codex_scan
    #[test]
    #[ignore = "实机测试：需要本机 ~/.codex/sessions"]
    fn live_codex_scan() {
        let mut conn = crate::db::open_in_memory().unwrap();
        let res = scan(&mut conn);
        println!("scan result: {res:?}");
        assert!(res.available, "codex 目录应可读");
        assert!(res.records > 0, "应至少入库 1 行");

        let bad: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_entries WHERE adapter='codex'
                 AND fresh_input_tokens + cache_read_tokens + cache_write_tokens + output_tokens != total_tokens",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad, 0, "四桶不变量必须全表成立");

        let (n, total, fresh, cr, cw, out, reason): (i64, i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), SUM(total_tokens), SUM(fresh_input_tokens), SUM(cache_read_tokens),
                        SUM(cache_write_tokens), SUM(output_tokens), SUM(reasoning_tokens)
                 FROM usage_entries WHERE adapter='codex'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .unwrap();
        println!("ledger: rows={n} total={total} fresh={fresh} cacheRead={cr} cacheWrite={cw} output={out} reasoning={reason}");

        let (entries, ledger): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM usage_entries WHERE adapter='codex'),
                        (SELECT COUNT(*) FROM dedup_ledger WHERE adapter='codex')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        println!("entries={entries} dedup_ledger={ledger}");
    }
}
