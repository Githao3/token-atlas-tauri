//! opencode 采集器：`~/.local/share/opencode/opencode.db`（SQLite）。
//!
//! 口径出处：原版 `electron/adapters/opencode.ts`。要点：
//! - `message.data` 是 JSON blob，按消息行取才有模型/项目/时间戳；
//! - 只认 `role === 'assistant'` 且有 `tokens` 的行；
//! - 四桶来自 `tokens.{input, output, reasoning, cache.read, cache.write}`，
//!   reported_total = `tokens.total`，语义由权威总数自动判定；
//! - request_id = message id（opencode 是四家里唯一 cache.write 有明细的
//!   JSON/SQLite 源之外——zcode SQLite 也有）；
//! - 只读直连，失败整体报错（与 zcode 同策略；快照复制兜底待 M6 需要时加）。

use super::{upsert_entries, upsert_sessions, AdapterScanResult, SessionTouch};
use crate::models::{normalize_model, normalize_project, RawParts, UsageEntry};
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;

pub const ADAPTER: &str = "opencode";

fn source_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".local")
            .join("share")
            .join("opencode")
            .join("opencode.db")
    })
}

const SOURCE_SQL: &str = "
SELECT m.id, m.session_id, m.time_created, m.data,
       s.directory AS s_dir, s.path AS s_path
FROM message m
LEFT JOIN session s ON s.id = m.session_id";

#[derive(Debug, serde::Deserialize)]
struct OpenCodeData {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    tokens: Option<OpenCodeTokens>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(rename = "modelID", default)]
    model_id_camel: Option<String>,
    #[serde(default)]
    path: Option<OpenCodePath>,
    #[serde(default)]
    time: Option<OpenCodeTime>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenCodeTokens {
    #[serde(default)]
    input: i64,
    #[serde(default)]
    output: i64,
    #[serde(default)]
    reasoning: i64,
    #[serde(default)]
    total: i64,
    #[serde(default)]
    cache: Option<OpenCodeCache>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenCodeCache {
    #[serde(default)]
    read: i64,
    #[serde(default)]
    write: i64,
}

#[derive(Debug, serde::Deserialize)]
struct OpenCodePath {
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenCodeTime {
    #[serde(default)]
    created: Option<i64>,
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
    let path = source_db_path().ok_or("无法解析用户主目录")?;
    if !path.exists() {
        return Err(format!("opencode 数据库不存在: {}", path.display()).into());
    }
    let src = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = src.prepare(SOURCE_SQL)?;
    let mut rows = stmt.query([])?;

    let mut entries: Vec<UsageEntry> = Vec::new();
    let mut touches: Vec<SessionTouch> = Vec::new();
    while let Some(row) = rows.next()? {
        let Ok(data) = row.get::<_, String>("data") else { continue };
        let Ok(d) = serde_json::from_str::<OpenCodeData>(&data) else { continue };
        if d.role.as_deref() != Some("assistant") {
            continue;
        }
        let Some(tokens) = d.tokens else { continue };
        let (cache_read, cache_write) = tokens.cache.map_or((0, 0), |c| (c.read, c.write));
        let raw = RawParts {
            input: tokens.input,
            cache_read,
            cache_write,
            output: tokens.output,
            reasoning: tokens.reasoning,
            reported_total: (tokens.total > 0).then_some(tokens.total),
            input_excludes_cache: false,
        };
        let n = crate::models::normalize_parts(&raw);
        if n.total <= 0 {
            continue;
        }
        // 时间：data.time.created 优先，回退行字段 time_created（原版同款）
        let ts = d.time.and_then(|t| t.created).or_else(|| row.get("time_created").ok());
        let Some(ts) = ts.filter(|t| *t > 0) else { continue };

        let id: String = row.get("id")?;
        let session_id: Option<String> = row.get("session_id")?;
        let s_dir: Option<String> = row.get("s_dir")?;
        let s_path: Option<String> = row.get("s_path")?;
        let model = normalize_model(
            &d.model_id_camel.or(d.model_id).unwrap_or_default(),
        );
        let project = d
            .path
            .and_then(|p| p.cwd)
            .or(s_dir)
            .or(s_path)
            .as_deref()
            .and_then(normalize_project);

        if let Some(sid) = &session_id {
            touches.push(SessionTouch {
                adapter: ADAPTER,
                session_id: sid.clone(),
                project: project.clone(),
                ts,
            });
        }
        entries.push(UsageEntry {
            adapter: ADAPTER.to_string(),
            request_id: id,
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
            session_id,
            project,
            source_file: Some(path.display().to_string()),
        });
    }
    drop(rows);
    drop(stmt);
    drop(src);

    let tx = conn.transaction()?;
    let n = upsert_entries(&tx, &entries)? as u64;
    upsert_sessions(&tx, &touches)?;
    tx.commit()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实机验证：扫描本机 opencode.db。
    /// 运行：cargo test -- --ignored --nocapture live_opencode_scan
    #[test]
    #[ignore = "实机测试：需要本机 ~/.local/share/opencode/opencode.db"]
    fn live_opencode_scan() {
        let mut conn = crate::db::open_in_memory().unwrap();
        let res = scan(&mut conn);
        println!("scan result: {res:?}");
        assert!(res.available, "opencode.db 应可读");
        assert!(res.records > 0, "应至少入库 1 行");

        let bad: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_entries WHERE adapter='opencode'
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
                 FROM usage_entries WHERE adapter='opencode'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .unwrap();
        println!("ledger: rows={n} total={total} fresh={fresh} cacheRead={cr} cacheWrite={cw} output={out} reasoning={reason}");
    }
}
