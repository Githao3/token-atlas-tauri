//! zcode 采集器：只读直连 live DB（`~/.zcode/cli/db/db.sqlite`），
//! `model_usage` 全表读出归一入账。SQLite 源不做文件增量——整表重读 +
//! request_id 幂等 upsert 已足够快，且天然抗 zcode 的历史裁剪。
//!
//! 口径出处：原版 `electron/adapters/zcode.ts`（SQLite 主路径）；
//! input 含缓存为本机实测确认（见 AGENTS.md「数据层」坑记录），
//! 归一由 [`crate::models::normalize_parts`] 按权威总数自动判定。

use super::{upsert_entries, upsert_sessions, AdapterScanResult, SessionTouch};
use crate::models::{normalize_model, normalize_project, RawParts, UsageEntry};
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;

/// zcode 的 live DB 路径。
pub fn source_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".zcode").join("cli").join("db").join("db.sqlite"))
}

const SOURCE_SQL: &str = "
SELECT
    COALESCE(m.logical_request_id, m.span_id, m.assistant_message_id,
             m.session_id || '-' || m.started_at || '-' || m.model_id
             || '-' || m.input_tokens || '-' || m.output_tokens) AS request_id,
    m.session_id,
    m.model_id,
    m.started_at,
    m.input_tokens,
    m.output_tokens,
    m.reasoning_tokens,
    m.cache_creation_input_tokens,
    m.cache_read_input_tokens,
    COALESCE(NULLIF(m.computed_total_tokens, 0), NULLIF(m.provider_total_tokens, 0)) AS reported_total,
    s.directory AS project_dir,
    s.path      AS project_path
FROM model_usage m
LEFT JOIN session s ON s.id = m.session_id
WHERE m.started_at IS NOT NULL";

/// 扫描 zcode live DB 并入账。失败不 panic，报告里带 error。
pub fn scan(conn: &mut Connection) -> AdapterScanResult {
    let mut result = AdapterScanResult { adapter: "zcode", ..Default::default() };
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
        return Err(format!("zcode 数据库不存在: {}", path.display()).into());
    }
    // 只读直连 live DB（WAL 模式可与运行中的 Agent 共存，AGENTS.md 坑记录）
    let src = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    let mut stmt = src.prepare(SOURCE_SQL)?;
    let mut rows = stmt.query([])?;

    let mut entries: Vec<UsageEntry> = Vec::new();
    let mut touches: Vec<SessionTouch> = Vec::new();
    while let Some(row) = rows.next()? {
        let ts: i64 = row.get("started_at")?;
        let input: i64 = row.get("input_tokens")?;
        let cache_read: i64 = row.get("cache_read_input_tokens")?;
        let cache_write: i64 = row.get("cache_creation_input_tokens")?;
        let output: i64 = row.get("output_tokens")?;
        let reasoning: i64 = row.get("reasoning_tokens")?;
        let reported_total: Option<i64> = row.get("reported_total")?;

        let raw = RawParts {
            input,
            cache_read,
            cache_write,
            output,
            reasoning,
            reported_total,
            input_excludes_cache: false,
        };

        // 四桶和（归一后）<= 0 的行丢弃（原版口径）
        let model = normalize_model(&row.get::<_, Option<String>>("model_id")?.unwrap_or_default());
        let n = crate::models::normalize_parts(&raw);
        if n.total <= 0 {
            continue;
        }

        let request_id: String = row.get("request_id")?;
        let session_id: Option<String> = row.get("session_id")?;
        let project_dir: Option<String> = row.get("project_dir")?;
        let project_path: Option<String> = row.get("project_path")?;
        let project = normalize_project(&project_dir.or(project_path).unwrap_or_default());

        touches.push(SessionTouch {
            adapter: "zcode",
            session_id: session_id.clone().unwrap_or_default(),
            project: project.clone(),
            ts,
        });

        entries.push(UsageEntry {
            adapter: "zcode".to_string(),
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
            session_id,
            project,
            source_file: Some(path.display().to_string()),
        });
    }
    drop(rows);
    drop(stmt);
    drop(src); // 先关源库再写账本

    // 数据 + 会话台账同一事务原子提交
    let tx = conn.transaction()?;
    let n = upsert_entries(&tx, &entries)? as u64;
    upsert_sessions(&tx, &touches)?;
    tx.commit()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// 实机验证：读本机 live DB，校验四桶不变量，打印摘要供对拍。
    /// 运行：cargo test -- --ignored --nocapture live_zcode_scan
    #[test]
    #[ignore = "实机测试：需要本机 ~/.zcode/cli/db/db.sqlite"]
    fn live_zcode_scan() {
        let mut conn = db::open_in_memory().unwrap();
        let res = scan(&mut conn);
        println!("scan result: {res:?}");
        assert!(res.available, "live DB 应可读");
        assert!(res.records > 0, "应至少入库 1 行");

        // 不变量全表校验：fresh + cacheR + cacheW + output == total
        let bad: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_entries
                 WHERE fresh_input_tokens + cache_read_tokens + cache_write_tokens + output_tokens != total_tokens",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad, 0, "四桶不变量必须全表成立");

        // 摘要（供与源库 SQL 直查对拍）
        let (n, total, fresh, cr, cw, out): (i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), SUM(total_tokens), SUM(fresh_input_tokens),
                        SUM(cache_read_tokens), SUM(cache_write_tokens), SUM(output_tokens)
                 FROM usage_entries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        println!("ledger: rows={n} total={total} fresh={fresh} cacheRead={cr} cacheWrite={cw} output={out}");

        let (sessions, projects): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM sessions),
                        (SELECT COUNT(DISTINCT project) FROM usage_entries WHERE project IS NOT NULL)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        println!("sessions={sessions} distinct_projects={projects}");
    }
}
