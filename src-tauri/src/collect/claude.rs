//! claude 采集器：`~/.claude/projects/**/*.jsonl`，JSONL 增量扫描。
//!
//! 口径出处：原版 `electron/adapters/claude.ts`。要点：
//! - 只认 `type === 'assistant'` 且有 `message.usage` 的行；
//! - **input 与缓存互斥不相交**（Anthropic 风格），显式 `input_excludes_cache`；
//! - request_id = `message.id`。
//!
//! 与原版的**有意差异**（升级点）：同一 `message.id` 的流式重复行
//! （usage 完全相同）在原版会重复计数，这里按 cc-switch 口径全局
//! 去重——request_id 主键天然合并。对拍时 claude 数字会因此略低，
//! 属于修正而非偏差。

use super::jsonl::{collect_jsonl_files, load_cursors, sync_jsonl, upsert_cursor, FileCursor};
use super::{upsert_entries, upsert_sessions, AdapterScanResult, SessionTouch};
use crate::models::{normalize_model, normalize_project, RawParts, UsageEntry};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;

pub const ADAPTER: &str = "claude";

fn source_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// claude 行里我们关心的字段（宽松解析，缺字段跳过）。
#[derive(serde::Deserialize)]
struct ClaudeRow {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    sessionid: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    message: Option<ClaudeMessage>,
}

#[derive(serde::Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<ClaudeUsage>,
}

#[derive(serde::Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(rename = "cache_creation_input_tokens", default)]
    cache_creation_input_tokens: i64,
    #[serde(rename = "cache_read_input_tokens", default)]
    cache_read_input_tokens: i64,
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
    let files = collect_jsonl_files(&root);
    if files.is_empty() {
        return Ok(0);
    }
    let cursors: HashMap<String, FileCursor> = load_cursors(conn, ADAPTER)?;

    let mut entries: Vec<UsageEntry> = Vec::new();
    let mut touches: Vec<SessionTouch> = Vec::new();
    let mut cursor_writes: Vec<(String, super::jsonl::JsonlSync)> = Vec::new();

    for file in &files {
        let source_key = file.display().to_string();
        let cursor = cursors.get(&source_key);
        let mut file_entries: HashMap<String, UsageEntry> = HashMap::new();
        let sync = sync_jsonl(file, cursor, |line, _fresh| {
            // 行级预过滤（原版同款）：不含 "usage" 的行直接跳过，省一次 JSON parse
            if !line.contains("\"usage\"") {
                return;
            }
            let Ok(row) = serde_json::from_str::<ClaudeRow>(line) else {
                return;
            };
            if row.r#type.as_deref() != Some("assistant") {
                return;
            }
            let Some(msg) = row.message else { return };
            let Some(usage) = msg.usage else { return };
            let Some(id) = msg.id else { return };
            let Some(ts) = row.timestamp.as_deref().and_then(parse_ts_ms) else {
                return;
            };
            let raw = RawParts {
                input: usage.input_tokens,
                cache_read: usage.cache_read_input_tokens,
                cache_write: usage.cache_creation_input_tokens,
                output: usage.output_tokens,
                reasoning: 0,
                reported_total: None, // claude 不上报总数，四桶和即权威
                input_excludes_cache: true,
            };
            let n = crate::models::normalize_parts(&raw);
            if n.total <= 0 {
                return;
            }
            let session_id = row.session_id.or(row.sessionid);
            let project = row.cwd.as_deref().and_then(normalize_project);
            let entry = UsageEntry {
                adapter: ADAPTER.to_string(),
                request_id: id.clone(),
                model: normalize_model(msg.model.as_deref().unwrap_or("")),
                ts,
                input_tokens: raw.input,
                fresh_input_tokens: n.fresh_input,
                cache_read_tokens: n.cache_read,
                cache_write_tokens: n.cache_write,
                output_tokens: n.output,
                reasoning_tokens: 0,
                total_tokens: n.total,
                input_token_semantics: n.semantics as u8,
                session_id: session_id.clone(),
                project: project.clone(),
                source_file: Some(source_key.clone()),
            };
            // 同批次内同 id 保留最后一条（流式重复行 usage 相同，谁最后都一样）
            file_entries.insert(id, entry);
        })?;
        if sync.skipped {
            continue;
        }

        for e in file_entries.into_values() {
            if let Some(sid) = &e.session_id {
                touches.push(SessionTouch {
                    adapter: ADAPTER,
                    session_id: sid.clone(),
                    project: e.project.clone(),
                    ts: e.ts,
                });
            }
            entries.push(e);
        }
        cursor_writes.push((source_key, sync));
    }

    // 数据 + 游标同一事务原子提交
    let tx = conn.transaction()?;
    let n = upsert_entries(&tx, &entries)? as u64;
    upsert_sessions(&tx, &touches)?;
    for (key, sync) in &cursor_writes {
        upsert_cursor(&tx, ADAPTER, key, sync, None)?;
    }
    tx.commit()?;
    Ok(n)
}

/// ISO 8601 → Unix 毫秒。失败返回 None（丢行，原版同样依赖有效时间戳）。
fn parse_ts_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_ts_ms("2026-09-05T06:22:06.525Z"), Some(1788589326525));
        assert_eq!(parse_ts_ms("not-a-date"), None);
    }

    /// 实机验证：扫描本机 ~/.claude/projects。
    /// 运行：cargo test -- --ignored --nocapture live_claude_scan
    #[test]
    #[ignore = "实机测试：需要本机 ~/.claude/projects"]
    fn live_claude_scan() {
        let mut conn = crate::db::open_in_memory().unwrap();
        let res = scan(&mut conn);
        println!("scan result: {res:?}");
        assert!(res.available, "claude 目录应可读");
        assert!(res.records > 0, "应至少入库 1 行");

        let bad: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_entries WHERE adapter='claude'
                 AND fresh_input_tokens + cache_read_tokens + cache_write_tokens + output_tokens != total_tokens",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad, 0, "四桶不变量必须全表成立");

        let (n, total, fresh, cr, cw, out): (i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), SUM(total_tokens), SUM(fresh_input_tokens),
                        SUM(cache_read_tokens), SUM(cache_write_tokens), SUM(output_tokens)
                 FROM usage_entries WHERE adapter='claude'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        println!("ledger: rows={n} total={total} fresh={fresh} cacheRead={cr} cacheWrite={cw} output={out}");
    }
}
