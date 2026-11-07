//! 每日 rollup（AGENTS.md 架构约束 #3）：过期明细聚合进 `usage_daily`
//! 后清理，查询时两表合并。
//!
//! 机制出处：cc-switch `usage_rollup.rs`。关键不变量：
//! - 聚合 cutoff 对齐**本地午夜**，保证最年轻的 rollup 行永远是完整一天；
//! - 聚合与已有 rollup 行 LEFT JOIN 合并（老行 + 新聚合），可重复执行；
//! - 聚合、清理与 meta 记录同一事务，崩溃要么全发生要么全不发生；
//! - detail 与 rollup 按天互斥（同一天不会两边都有），查询侧合并时
//!   部分覆盖的天只信 detail（见 dashboard.rs 的日期边界收窄）。

use chrono::{Datelike, Duration, Local, TimeZone};
use rusqlite::{params, Connection};
use serde::Serialize;

/// 明细保留天数（默认；meta 表 `retain_days` 可覆盖）。
/// 取 365 而不是 cc-switch 的 30：Dashboard 有 90d 档，30 天剪枝会让
/// 远端边界天只剩 rollup 粒度。剪枝从未开启过 detail 就等于全量。
pub const DEFAULT_RETAIN_DAYS: u32 = 365;

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollupReport {
    /// 本次聚合进 rollup 的明细行数
    pub pruned_rows: u64,
    /// 聚合覆盖的本地天数
    pub days: u64,
    /// cutoff（Unix 毫秒）
    pub cutoff_ms: i64,
}

fn retain_days(conn: &Connection) -> u32 {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'retain_days'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(DEFAULT_RETAIN_DAYS)
}

/// cutoff：`now - retain_days` 所在本地日的**下一天** 00:00（本地）。
/// DST 歧义取 earliest，空洞回退 +1 小时。
pub fn compute_local_midnight_cutoff(retain_days: u32) -> Result<i64, Box<dyn std::error::Error>> {
    let target_day = (Local::now() - Duration::days(retain_days as i64)).date_naive();
    let next_day = target_day.succ_opt().ok_or("日期越界")?;
    match Local.with_ymd_and_hms(next_day.year(), next_day.month(), next_day.day(), 0, 0, 0) {
        chrono::LocalResult::Single(dt) => Ok(dt.timestamp_millis()),
        chrono::LocalResult::Ambiguous(dt, _) => Ok(dt.timestamp_millis()),
        chrono::LocalResult::None => {
            // DST 空洞（本地 00:00 不存在）：回退 +1 小时
            let dt = Local
                .with_ymd_and_hms(next_day.year(), next_day.month(), next_day.day(), 1, 0, 0)
                .earliest()
                .ok_or("DST 回退失败")?;
            Ok(dt.timestamp_millis())
        }
    }
}

/// 执行一次 rollup + 剪枝。`retain_days_override = Some(0)` 表示全部剪掉（测试用）。
pub fn rollup_and_prune(
    conn: &mut Connection,
    retain_days_override: Option<u32>,
) -> Result<RollupReport, Box<dyn std::error::Error>> {
    let days = retain_days_override.unwrap_or_else(|| retain_days(conn));
    let cutoff_ms = compute_local_midnight_cutoff(days)?;

    let tx = conn.transaction()?;

    // 聚合过期明细，按六元组与已有 rollup 行合并（old + new）
    tx.execute(
        "INSERT OR REPLACE INTO usage_daily (
            day, adapter, model, project, request_count,
            fresh_input_tokens, cache_read_tokens, cache_write_tokens,
            output_tokens, reasoning_tokens, total_tokens
        )
        SELECT agg.day, agg.adapter, agg.model, agg.project,
            COALESCE(old.request_count, 0) + agg.req,
            COALESCE(old.fresh_input_tokens, 0) + agg.f,
            COALESCE(old.cache_read_tokens, 0) + agg.cr,
            COALESCE(old.cache_write_tokens, 0) + agg.cw,
            COALESCE(old.output_tokens, 0) + agg.o,
            COALESCE(old.reasoning_tokens, 0) + agg.rg,
            COALESCE(old.total_tokens, 0) + agg.t
        FROM (
            SELECT date(ts / 1000, 'unixepoch', 'localtime') AS day,
                   adapter, model, COALESCE(project, '') AS project,
                   COUNT(*) AS req,
                   SUM(fresh_input_tokens) AS f,
                   SUM(cache_read_tokens) AS cr,
                   SUM(cache_write_tokens) AS cw,
                   SUM(output_tokens) AS o,
                   SUM(reasoning_tokens) AS rg,
                   SUM(total_tokens) AS t
            FROM usage_entries
            WHERE ts < ?1
            GROUP BY day, adapter, model, project
        ) agg
        LEFT JOIN usage_daily old
            ON old.day = agg.day AND old.adapter = agg.adapter
           AND old.model = agg.model AND old.project = agg.project",
        params![cutoff_ms],
    )?;

    let pruned = tx.execute("DELETE FROM usage_entries WHERE ts < ?1", params![cutoff_ms])? as u64;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('last_rollup_ms', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![cutoff_ms.to_string()],
    )?;
    tx.commit()?;

    let report_days: u64 = {
        let c = conn;
        c.query_row(
            "SELECT COUNT(DISTINCT day) FROM usage_daily",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64
    };

    Ok(RollupReport { pruned_rows: pruned, days: report_days, cutoff_ms })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn insert_entry(conn: &Connection, request_id: &str, ts: i64, total: i64, fresh: i64) {
        conn.execute(
            "INSERT INTO usage_entries (request_id, adapter, model, ts, input_tokens,
                fresh_input_tokens, cache_read_tokens, cache_write_tokens, output_tokens,
                reasoning_tokens, total_tokens, input_token_semantics)
             VALUES (?1, 'zcode', 'glm-test', ?2, ?3, ?3, 0, 0, 0, 0, ?3, 2)",
            params![request_id, ts, total],
        )
        .unwrap();
        let _ = fresh;
    }

    #[test]
    fn rollup_merges_and_prunes() {
        let mut conn = db::open_in_memory().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        // 未过期（今天）：留在明细
        insert_entry(&conn, "keep-1", now, 100, 100);
        // 过期（35 天前）：进 rollup；同一天两条应合并
        let old = now - 35 * 24 * 3600 * 1000;
        insert_entry(&conn, "old-1", old, 10, 10);
        insert_entry(&conn, "old-2", old + 1000, 20, 20);

        let report = rollup_and_prune(&mut conn, Some(30)).unwrap();
        assert_eq!(report.pruned_rows, 2);

        // 明细只剩今天那条
        let detail: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(detail, 1);

        // rollup 一行，量是两条之和
        let (req, total): (i64, i64) = conn
            .query_row(
                "SELECT request_count, total_tokens FROM usage_daily",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((req, total), (2, 30));

        // 再来一条过期数据 → 合并进已有 rollup 行（可重复执行）
        insert_entry(&conn, "old-3", old + 2000, 5, 5);
        rollup_and_prune(&mut conn, Some(30)).unwrap();
        let (req, total): (i64, i64) = conn
            .query_row(
                "SELECT request_count, total_tokens FROM usage_daily",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((req, total), (3, 35));

        // 剪完再跑一遍：无变化（幂等）
        let r2 = rollup_and_prune(&mut conn, Some(30)).unwrap();
        assert_eq!(r2.pruned_rows, 0);
        let (_, total): (i64, i64) = conn
            .query_row(
                "SELECT request_count, total_tokens FROM usage_daily",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(total, 35);
    }

    #[test]
    fn cutoff_is_next_local_midnight() {
        // retain_days = 30：cutoff 必须落在"30 天前那天的下一天"的 00:00 本地
        let cutoff = compute_local_midnight_cutoff(30).unwrap();
        let dt: chrono::DateTime<chrono::Local> = chrono::Local.timestamp_millis_opt(cutoff).unwrap();
        assert_eq!(dt.time(), chrono::NaiveTime::MIN);
        let expected_day = (chrono::Local::now() - chrono::Duration::days(30)).date_naive().succ_opt().unwrap();
        assert_eq!(dt.date_naive(), expected_day);
    }
}
