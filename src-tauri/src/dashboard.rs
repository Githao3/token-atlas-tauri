//! Dashboard 汇总聚合：把账本（明细 ⊕ 日汇总）聚合成原版 `Dashboard`
//! 形状（`sources/token-atlas/src/shared/types.ts`），UI 层零改动迁移。
//!
//! 合并规则（rollup.rs 的不变量保证 detail 与 rollup 按天互斥）：
//! - 范围完全覆盖的天：rollup 行可用；
//! - 部分覆盖的边界天只信 detail（rollup 行代表整天）——剪枝开启后
//!   若边界天已超出保留期，会损失当天的部分颗粒，属已知取舍
//!   （保留期默认 365 天，见 rollup.rs）；
//! - `start_ms = None` 表示 all：明细不加下界，rollup 行全量并入。

use crate::pricing::Pricing;
use chrono::{Duration, Local, NaiveDate, TimeZone};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

/// 热力图固定 53 周窗口（原版口径）。
pub const HEATMAP_DAYS: usize = 371;

/// 适配器展示元数据（对齐原版各 adapter 的 label/rootPath）。
pub const ADAPTER_META: &[(&str, &str, &str)] = &[
    ("zcode", "zcode", "~/.zcode/cli"),
    ("claude", "Claude Code", "~/.claude/projects"),
    ("codex", "Codex", "~/.codex/sessions"),
    ("opencode", "OpenCode", "~/.local/share/opencode"),
];

// ---- 输出类型（serde camelCase 对齐原版 TS 接口）----

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardStats {
    pub total_tokens: i64,
    pub total_cost: f64,
    pub sessions: u64,
    pub messages: u64,
    pub active_days: u64,
    pub current_streak: u64,
    pub cache_hit_rate: f64,
    pub favorite_model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSlice {
    pub model: String,
    pub total: i64,
    pub fresh_input: i64,
    pub cache_read: i64,
    pub output: i64,
    pub cost: f64,
    pub share: f64,
    pub pricing_fallback: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterSlice {
    pub adapter: String,
    pub label: String,
    pub total: i64,
    pub cost: f64,
    pub sessions: u64,
    pub messages: u64,
    pub available: bool,
    pub root_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSlice {
    pub path: String,
    pub label: String,
    pub total: i64,
    pub cost: f64,
    pub sessions: u64,
    pub messages: u64,
    pub share: f64,
    pub adapters: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    pub read_tokens: i64,
    pub write_tokens: i64,
    pub fresh_input_tokens: i64,
    pub hit_rate: f64,
    pub cost_without_cache: f64,
    pub cost_with_cache: f64,
    pub saved: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostBreakdown {
    pub fresh_input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayModelPoint {
    pub day: String,
    pub model: String,
    pub total: i64,
    pub turns: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayTrendPoint {
    pub day: String,
    pub total: i64,
    pub cost: f64,
    pub turns: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeatCell {
    pub day: String,
    pub total: i64,
    pub turns: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingInfo {
    pub path: String,
    pub using_overrides: bool,
    pub unmatched_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dashboard {
    pub generated_at: String,
    pub stats: DashboardStats,
    pub models: Vec<ModelSlice>,
    pub adapters: Vec<AdapterSlice>,
    pub projects: Vec<ProjectSlice>,
    pub cache: CacheStats,
    pub cost: CostBreakdown,
    pub per_day: Vec<DayModelPoint>,
    pub trend: Vec<DayTrendPoint>,
    pub heatmap: Vec<HeatCell>,
    pub heat_start: String,
    pub heat_end: String,
    pub pricing: PricingInfo,
}

// ---- 内部聚合结构 ----

#[derive(Debug, Default, Clone)]
struct ModelAgg {
    fresh: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    total: i64,
    turns: u64,
}

#[derive(Debug, Default)]
struct ProjectAgg {
    total: i64,
    cost: f64,
    turns: u64,
    sessions: HashSet<String>,
    adapters: HashSet<String>,
}

#[derive(Debug, Default)]
struct AdapterAgg {
    total: i64,
    cost: f64,
    turns: u64,
}

#[derive(Debug, Default)]
struct DayAgg {
    total: i64,
    cost: f64,
    turns: u64,
}

/// (day, model) 粒度的分桶，日级成本按它精确计算。
#[derive(Debug, Default, Clone)]
struct BucketAgg {
    fresh: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    total: i64,
    turns: u64,
}

impl BucketAgg {
    fn cost(&self, pricing: &Pricing, model: &str) -> f64 {
        let (rate, _) = pricing.rate_for(model);
        Pricing::cost_of(&rate, self.fresh, self.cache_read, self.cache_write, self.output)
    }
}

/// 本地日期键（YYYY-MM-DD）。
fn local_day(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .earliest()
        .unwrap_or_else(|| Local.timestamp_millis_opt(0).unwrap())
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

/// rollup 行的取数决策：
/// - `None`：all 范围，rollup 全量并入；
/// - `Some(None)`：日期区间为空（收窄后 start > end），rollup 一行都不取；
/// - `Some(Some((s, e)))`：按闭区间取。
fn rollup_day_bounds(
    start_ms: Option<i64>,
    end_ms: i64,
) -> Option<Option<(String, String)>> {
    if start_ms.is_none() {
        return None; // all：rollup 全量并入，无需收窄
    }
    let s = start_ms.unwrap();
    let end_dt = Local.timestamp_millis_opt(end_ms).earliest()?;
    // end 恰在本地最后 1 毫秒（23:59:59.999）才算整天覆盖；否则收窄到前一天
    let last_ms = chrono::NaiveTime::from_hms_nano_opt(23, 59, 59, 999_000_000).unwrap();
    let end_day = if end_dt.time() >= last_ms {
        end_dt.date_naive()
    } else {
        end_dt.date_naive().pred_opt()?
    };

    let start_dt = Local.timestamp_millis_opt(s).earliest()?;
    let start_day = if start_dt.time() == chrono::NaiveTime::MIN {
        start_dt.date_naive() // 恰是本地午夜：整天
    } else {
        start_dt.date_naive().succ_opt()? // 次日起
    };

    if start_day > end_day {
        return Some(None); // 空区间
    }
    Some(Some((
        start_day.format("%Y-%m-%d").to_string(),
        end_day.format("%Y-%m-%d").to_string(),
    )))
}

/// 构建 Dashboard。`start_ms = None` 表示 all；`end_ms` 通常是当前时刻。
pub fn build_dashboard(
    conn: &Connection,
    start_ms: Option<i64>,
    end_ms: i64,
    pricing: &Pricing,
) -> Result<Dashboard, Box<dyn std::error::Error>> {
    let mut models: HashMap<String, ModelAgg> = HashMap::new();
    let mut projects: HashMap<String, ProjectAgg> = HashMap::new();
    let mut adapters: HashMap<String, AdapterAgg> = HashMap::new();
    let mut per_day_model: BTreeMap<(String, String), (i64, u64)> = BTreeMap::new();
    let mut messages: u64 = 0;

    // ---- detail 侧：按 (day, model, adapter, project, session) 拉取并在
    //      merge 时折叠；session 维度只为项目/适配器的会话计数 ----
    {
        let mut stmt = conn.prepare(
            "SELECT date(ts / 1000, 'unixepoch', 'localtime') AS day,
                    model, adapter, COALESCE(project, '') AS project, session_id,
                    SUM(fresh_input_tokens), SUM(cache_read_tokens), SUM(cache_write_tokens),
                    SUM(output_tokens), SUM(total_tokens), COUNT(*)
             FROM usage_entries
             WHERE (?1 IS NULL OR ts >= ?1) AND ts <= ?2
             GROUP BY day, model, adapter, project, session_id",
        )?;
        let rows = stmt.query_map(rusqlite::params![start_ms, end_ms], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, i64>(10)?,
            ))
        })?;
        for row in rows {
            let (day, model, adapter, project, session_id, f, cr, cw, o, t, cnt) = row?;
            let bucket = BucketAgg {
                fresh: f,
                cache_read: cr,
                cache_write: cw,
                output: o,
                total: t,
                turns: cnt as u64,
            };
            merge_group(
                &mut models, &mut projects, &mut adapters, &mut per_day_model,
                &day, &model, &adapter, &project, session_id.as_deref(), &bucket, pricing,
            );
            messages += cnt as u64;
        }
    }

    // detail 侧的独立会话数（messages 已在分组循环里累计，这里只要会话数）
    let detail_sessions: HashMap<String, u64> = {
        let mut stmt = conn.prepare(
            "SELECT adapter, COUNT(DISTINCT session_id) FROM usage_entries
             WHERE session_id IS NOT NULL AND (?1 IS NULL OR ts >= ?1) AND ts <= ?2
             GROUP BY adapter",
        )?;
        let rows = stmt.query_map(rusqlite::params![start_ms, end_ms], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>()?
    };
    let detail_total_sessions: u64 = conn.query_row(
        "SELECT COUNT(DISTINCT session_id) FROM usage_entries
         WHERE session_id IS NOT NULL AND (?1 IS NULL OR ts >= ?1) AND ts <= ?2",
        rusqlite::params![start_ms, end_ms],
        |r| r.get::<_, i64>(0).map(|v| v as u64),
    )?;
    // messages 无需独立查询：detail 分组循环与 rollup 循环都已各自累计

    // ---- rollup 侧：all 时全量并入；有界时按收窄后的日期区间并入；
    //      收窄后为空的区间一行都不取 ----
    let rollup_bounds = rollup_day_bounds(start_ms, end_ms);
    if !matches!(rollup_bounds, Some(None)) {
        let (where_clause, params): (String, Vec<Box<dyn rusqlite::ToSql>>) =
            match rollup_bounds {
                Some(Some((s, e))) => (
                    " WHERE day >= ?1 AND day <= ?2".to_string(),
                    vec![Box::new(s), Box::new(e)],
                ),
                _ => (String::new(), vec![]), // None = all
            };
        let sql = format!(
            "SELECT day, model, adapter, project, request_count,
                    fresh_input_tokens, cache_read_tokens, cache_write_tokens,
                    output_tokens, total_tokens
             FROM usage_daily{where_clause}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, i64>(9)?,
            ))
        })?;
        for row in rows {
            let (day, model, adapter, project, req, f, cr, cw, o, t) = row?;
            let bucket = BucketAgg {
                fresh: f,
                cache_read: cr,
                cache_write: cw,
                output: o,
                total: t,
                turns: req as u64,
            };
            merge_group(
                &mut models, &mut projects, &mut adapters, &mut per_day_model,
                &day, &model, &adapter, &project, None, &bucket, pricing,
            );
            messages += req as u64;
        }
    }

    // ---- 全历史 (day, model) 分桶：热力图 / 趋势 / streak 用 ----
    let mut all_days: BTreeMap<String, DayAgg> = BTreeMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT date(ts / 1000, 'unixepoch', 'localtime') AS day, model,
                    SUM(fresh_input_tokens), SUM(cache_read_tokens), SUM(cache_write_tokens),
                    SUM(output_tokens), SUM(total_tokens), COUNT(*)
             FROM usage_entries GROUP BY day, model",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        for row in rows {
            let (day, model, f, cr, cw, o, t, cnt) = row?;
            let bucket = BucketAgg { fresh: f, cache_read: cr, cache_write: cw, output: o, total: t, turns: cnt as u64 };
            let e = all_days.entry(day).or_default();
            e.total += bucket.total;
            e.turns += bucket.turns;
            e.cost += bucket.cost(pricing, &model);
        }
        let mut stmt = conn.prepare(
            "SELECT day, model, SUM(fresh_input_tokens), SUM(cache_read_tokens), SUM(cache_write_tokens),
                    SUM(output_tokens), SUM(total_tokens), SUM(request_count)
             FROM usage_daily GROUP BY day, model",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        for row in rows {
            let (day, model, f, cr, cw, o, t, cnt) = row?;
            let bucket = BucketAgg { fresh: f, cache_read: cr, cache_write: cw, output: o, total: t, turns: cnt as u64 };
            let e = all_days.entry(day).or_default();
            e.total += bucket.total;
            e.turns += bucket.turns;
            e.cost += bucket.cost(pricing, &model);
        }
    }

    // ---- 模型切片与总成本 ----
    let total_tokens: i64 = models.values().map(|m| m.total).sum();
    let mut model_slices: Vec<ModelSlice> = models
        .iter()
        .map(|(model, agg)| {
            let (rate, matched) = pricing.rate_for(model);
            let cost = Pricing::cost_of(&rate, agg.fresh, agg.cache_read, agg.cache_write, agg.output);
            ModelSlice {
                model: model.clone(),
                total: agg.total,
                fresh_input: agg.fresh,
                cache_read: agg.cache_read,
                output: agg.output,
                cost,
                share: if total_tokens > 0 { agg.total as f64 / total_tokens as f64 } else { 0.0 },
                pricing_fallback: !matched,
            }
        })
        .collect();
    model_slices.sort_by(|a, b| b.total.cmp(&a.total).then(a.model.cmp(&b.model)));

    // ---- 适配器切片 ----
    let mut adapter_slices: Vec<AdapterSlice> = ADAPTER_META
        .iter()
        .map(|(id, label, root)| {
            let agg = adapters.get(*id);
            AdapterSlice {
                adapter: id.to_string(),
                label: label.to_string(),
                total: agg.map(|a| a.total).unwrap_or(0),
                cost: agg.map(|a| a.cost).unwrap_or(0.0),
                sessions: detail_sessions.get(*id).copied().unwrap_or(0),
                messages: agg.map(|a| a.turns).unwrap_or(0),
                // M1.5 口径：账本里有行即视为可用；M2 接真实扫描报告
                available: agg.map(|a| a.turns > 0).unwrap_or(false),
                root_path: root.to_string(),
            }
        })
        .collect();
    adapter_slices.sort_by(|a, b| b.total.cmp(&a.total));

    // ---- 项目切片 ----
    let mut project_slices: Vec<ProjectSlice> = projects
        .into_iter()
        .map(|(path, agg)| {
            let label = path
                .rsplit(['/', '\\'])
                .find(|s| !s.is_empty())
                .unwrap_or(&path)
                .to_string();
            let mut adapters_list: Vec<String> = agg.adapters.into_iter().collect();
            adapters_list.sort();
            ProjectSlice {
                label,
                sessions: agg.sessions.len() as u64,
                adapters: adapters_list,
                path,
                total: agg.total,
                cost: agg.cost,
                messages: agg.turns,
                share: if total_tokens > 0 { agg.total as f64 / total_tokens as f64 } else { 0.0 },
            }
        })
        .collect();
    project_slices.sort_by(|a, b| b.total.cmp(&a.total));

    // ---- 缓存效率与成本拆解（costWithCache = 实际；costWithoutCache = 缓存全按 input 价的反事实）----
    let read_tokens: i64 = models.values().map(|m| m.cache_read).sum();
    let write_tokens: i64 = models.values().map(|m| m.cache_write).sum();
    let fresh_tokens: i64 = models.values().map(|m| m.fresh).sum();
    let mut cost_with = 0.0;
    let mut cost_without = 0.0;
    let mut cb = CostBreakdown {
        fresh_input: 0.0,
        cache_read: 0.0,
        cache_write: 0.0,
        output: 0.0,
        total: 0.0,
    };
    for (model, agg) in &models {
        let (rate, _) = pricing.rate_for(model);
        cost_with += Pricing::cost_of(&rate, agg.fresh, agg.cache_read, agg.cache_write, agg.output);
        cost_without += (agg.fresh + agg.cache_read + agg.cache_write) as f64 / 1e6 * rate.input
            + agg.output as f64 / 1e6 * rate.output;
        cb.fresh_input += agg.fresh as f64 / 1e6 * rate.input;
        cb.cache_read += agg.cache_read as f64 / 1e6 * rate.cache_read;
        cb.cache_write += agg.cache_write as f64 / 1e6 * rate.cache_write;
        cb.output += agg.output as f64 / 1e6 * rate.output;
    }
    cb.total = cb.fresh_input + cb.cache_read + cb.cache_write + cb.output;
    let cache = CacheStats {
        read_tokens,
        write_tokens,
        fresh_input_tokens: fresh_tokens,
        hit_rate: if read_tokens + fresh_tokens > 0 {
            read_tokens as f64 / (read_tokens + fresh_tokens) as f64
        } else {
            0.0
        },
        cost_without_cache: cost_without,
        cost_with_cache: cost_with,
        saved: (cost_without - cost_with).max(0.0),
    };

    // ---- 热力图：固定 371 天窗口，止于 end 日 ----
    let heat_end_day = local_day(end_ms);
    let heat_end_date = NaiveDate::parse_from_str(&heat_end_day, "%Y-%m-%d")?;
    let heat_start_date = heat_end_date - Duration::days((HEATMAP_DAYS - 1) as i64);
    let heat_start = heat_start_date.format("%Y-%m-%d").to_string();
    let heatmap: Vec<HeatCell> = (0..HEATMAP_DAYS)
        .map(|i| {
            let d = (heat_start_date + Duration::days(i as i64)).format("%Y-%m-%d").to_string();
            let agg = all_days.get(&d);
            HeatCell {
                total: agg.map(|a| a.total).unwrap_or(0),
                turns: agg.map(|a| a.turns).unwrap_or(0),
                day: d,
            }
        })
        .collect();

    // ---- 趋势：range 全跨度补零（all 从最早活跃日开始）----
    let range_start_day = match start_ms {
        None => all_days.keys().next().cloned(),
        Some(s) => Some(local_day(s)),
    };
    let mut trend: Vec<DayTrendPoint> = Vec::new();
    if let Some(s) = &range_start_day {
        let mut cur = NaiveDate::parse_from_str(s, "%Y-%m-%d")?;
        while cur <= heat_end_date {
            let d = cur.format("%Y-%m-%d").to_string();
            let agg = all_days.get(&d);
            trend.push(DayTrendPoint {
                total: agg.map(|a| a.total).unwrap_or(0),
                cost: agg.map(|a| a.cost).unwrap_or(0.0),
                turns: agg.map(|a| a.turns).unwrap_or(0),
                day: d,
            });
            cur += Duration::days(1);
        }
    }

    // ---- 活跃天与连续天 ----
    let active_start = range_start_day.clone().unwrap_or_else(|| "0000-00-00".into());
    let active_days = all_days
        .range(active_start..=heat_end_day.clone())
        .filter(|(_, a)| a.total > 0)
        .count() as u64;
    let mut current_streak = 0u64;
    {
        let mut cur = heat_end_date;
        // 今天还没用则从昨天起算（连续到最近的活跃日为止）
        let mut allow_grace = true;
        loop {
            let key = cur.format("%Y-%m-%d").to_string();
            match all_days.get(&key) {
                Some(agg) if agg.total > 0 => {
                    current_streak += 1;
                    cur -= Duration::days(1);
                    allow_grace = false;
                }
                _ if allow_grace => {
                    allow_grace = false;
                    cur -= Duration::days(1);
                }
                _ => break,
            }
            if current_streak > 100_000 {
                break; // 防御性上限，正常远达不到
            }
        }
    }

    // ---- 会话数：detail 有就用 detail；剪枝期（rollup-only）退回 sessions 台账重叠估算 ----
    let sessions = if detail_total_sessions > 0 || start_ms.is_none() {
        detail_total_sessions
    } else {
        sessions_table_overlap(conn, start_ms, end_ms)?
    };

    let unmatched: Vec<String> = model_slices
        .iter()
        .filter(|m| m.pricing_fallback)
        .map(|m| m.model.clone())
        .collect();

    Ok(Dashboard {
        generated_at: chrono::Utc::now().to_rfc3339(),
        stats: DashboardStats {
            total_tokens,
            total_cost: cb.total,
            sessions,
            messages,
            active_days,
            current_streak,
            cache_hit_rate: cache.hit_rate,
            favorite_model: model_slices.first().map(|m| m.model.clone()),
        },
        models: model_slices,
        adapters: adapter_slices,
        projects: project_slices,
        cache,
        cost: cb,
        per_day: per_day_model
            .into_iter()
            .map(|((day, model), (total, turns))| DayModelPoint { day, model, total, turns })
            .collect(),
        trend,
        heatmap,
        heat_start,
        heat_end: heat_end_day,
        pricing: PricingInfo {
            path: pricing.path.display().to_string(),
            using_overrides: pricing.using_overrides,
            unmatched_models: unmatched,
        },
    })
}

/// 把一个 (day, model, adapter, project, session) 组折叠进各聚合维度。
#[allow(clippy::too_many_arguments)]
fn merge_group(
    models: &mut HashMap<String, ModelAgg>,
    projects: &mut HashMap<String, ProjectAgg>,
    adapters: &mut HashMap<String, AdapterAgg>,
    per_day_model: &mut BTreeMap<(String, String), (i64, u64)>,
    day: &str,
    model: &str,
    adapter: &str,
    project: &str,
    session_id: Option<&str>,
    bucket: &BucketAgg,
    pricing: &Pricing,
) {
    let m = models.entry(model.to_string()).or_default();
    m.fresh += bucket.fresh;
    m.cache_read += bucket.cache_read;
    m.cache_write += bucket.cache_write;
    m.output += bucket.output;
    m.total += bucket.total;
    m.turns += bucket.turns;

    let cost = bucket.cost(pricing, model);

    if !project.is_empty() {
        let p = projects.entry(project.to_string()).or_default();
        p.total += bucket.total;
        p.cost += cost;
        p.turns += bucket.turns;
        p.adapters.insert(adapter.to_string());
        if let Some(s) = session_id {
            p.sessions.insert(s.to_string());
        }
    }

    let a = adapters.entry(adapter.to_string()).or_default();
    a.total += bucket.total;
    a.cost += cost;
    a.turns += bucket.turns;

    let e = per_day_model
        .entry((day.to_string(), model.to_string()))
        .or_insert((0, 0));
    e.0 += bucket.total;
    e.1 += bucket.turns;
}

/// 剪枝期的会话数估算：sessions 台账里与范围有重叠的会话数。
fn sessions_table_overlap(
    conn: &Connection,
    start_ms: Option<i64>,
    end_ms: i64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sessions
         WHERE (?1 IS NULL OR last_ts >= ?1) AND first_ts <= ?2",
        rusqlite::params![start_ms, end_ms],
        |r| r.get(0),
    )?;
    Ok(n as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::path::PathBuf;

    fn seed(conn: &Connection) -> (i64, i64, String) {
        // day1 = 26 小时前（昨天或前天，跨本地午夜），day2 = 现在
        let now = chrono::Utc::now().timestamp_millis();
        let day1_ts = now - 26 * 3600 * 1000;
        let ins = |req: &str, adapter: &str, model: &str, ts: i64, f: i64, cr: i64, cw: i64, o: i64, t: i64, proj: &str, sess: &str| {
            conn.execute(
                "INSERT INTO usage_entries (request_id, adapter, model, ts, input_tokens,
                    fresh_input_tokens, cache_read_tokens, cache_write_tokens, output_tokens,
                    reasoning_tokens, total_tokens, input_token_semantics, session_id, project)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, 1, ?11, ?12)",
                rusqlite::params![req, adapter, model, ts, f + cr + cw, f, cr, cw, o, t, sess, proj],
            )
            .unwrap();
        };
        // zcode glm：input 含缓存（semantics=1），fresh = 1000
        ins("r1", "zcode", "glm-5.2", day1_ts, 1000, 9000, 0, 500, 10500, "D:\\A", "s1");
        ins("r2", "zcode", "glm-5.2", now, 2000, 0, 0, 1000, 3000, "D:\\B", "s2");
        // claude：input 不含缓存（semantics=2）
        ins("r3", "claude", "claude-sonnet-5", now, 100, 0, 0, 50, 150, "D:\\A", "s3");
        (day1_ts, now, local_day(day1_ts))
    }

    #[test]
    fn stats_models_projects_and_cost() {
        let conn = db::open_in_memory().unwrap();
        let (day1_ts, now, day1) = seed(&conn);
        let pricing = Pricing::load_from(&PathBuf::from("nonexistent.json"));
        let d = build_dashboard(&conn, Some(now - 7 * 24 * 3600 * 1000), now, &pricing).unwrap();

        // 总量
        assert_eq!(d.stats.total_tokens, 13650);
        assert_eq!(d.stats.messages, 3);
        assert_eq!(d.stats.sessions, 3);

        // 模型切片（total 降序）与成本（glm: 3000*0.6/1e6 + 9000*0.11/1e6 + 1500*2.2/1e6）
        assert_eq!(d.models[0].model, "glm-5.2");
        assert_eq!(d.models[0].total, 13500);
        let glm_cost = 3000.0 * 0.6 / 1e6 + 9000.0 * 0.11 / 1e6 + 1500.0 * 2.2 / 1e6;
        assert!((d.models[0].cost - glm_cost).abs() < 1e-12);
        assert!((d.stats.total_cost - (glm_cost + 100.0 * 3.0 / 1e6 + 50.0 * 15.0 / 1e6)).abs() < 1e-12);
        assert!(!d.models[0].pricing_fallback);
        assert!((d.stats.cache_hit_rate - 9000.0 / (9000.0 + 3100.0)).abs() < 1e-12);

        // 适配器与项目
        assert_eq!(d.adapters.iter().find(|a| a.adapter == "zcode").unwrap().total, 13500);
        assert_eq!(d.adapters.iter().find(|a| a.adapter == "claude").unwrap().sessions, 1);
        let pa = d.projects.iter().find(|p| p.path == "D:\\A").unwrap();
        assert_eq!(pa.total, 10650);
        assert_eq!(pa.sessions, 2);
        assert_eq!(pa.adapters, vec!["claude", "zcode"]);

        // 趋势补零：range 从 now-7d 的本地日开始，零填到 end；day1 的点在中间
        assert_eq!(d.trend[0].day, local_day(now - 7 * 24 * 3600 * 1000));
        assert_eq!(d.trend[0].total, 0);
        let t_day1 = d.trend.iter().find(|p| p.day == day1).unwrap();
        assert_eq!(t_day1.total, 10500);
        assert!(d.trend.iter().all(|p| p.day <= d.heat_end));

        // 热力图固定 371 格
        assert_eq!(d.heatmap.len(), HEATMAP_DAYS);
        assert_eq!(d.heatmap.last().unwrap().day, d.heat_end);
        assert_eq!(d.heatmap.last().unwrap().total, 3150);

        // perDay 按 (day, model) 聚合
        assert_eq!(d.per_day.len(), 3);

        let _ = day1_ts;
    }

    #[test]
    fn rollup_preserves_all_totals() {
        let mut conn = db::open_in_memory().unwrap();
        let (_, now, _) = seed(&conn);
        let pricing = Pricing::load_from(&PathBuf::from("nonexistent.json"));

        let before = build_dashboard(&conn, None, now, &pricing).unwrap();
        // 全部剪进 rollup（retain=0 → cutoff 明天 00:00 → 覆盖今天整天）
        crate::rollup::rollup_and_prune(&mut conn, Some(0)).unwrap();
        let detail_left: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(detail_left, 0, "剪枝后明细应为空");
        let after = build_dashboard(&conn, None, now, &pricing).unwrap();

        // all 范围下 rollup 全量并入，总量/模型/项目/日维度全部守恒
        assert_eq!(before.stats.total_tokens, after.stats.total_tokens);
        assert_eq!(before.stats.messages, after.stats.messages);
        assert!((before.stats.total_cost - after.stats.total_cost).abs() < 1e-12);
        assert_eq!(before.models.len(), after.models.len());
        assert_eq!(before.projects.len(), after.projects.len());
        assert_eq!(before.per_day.len(), after.per_day.len());
        assert_eq!(before.stats.active_days, after.stats.active_days);
        // 热力图逐格守恒
        assert_eq!(before.heatmap.len(), after.heatmap.len());
        for (b, a) in before.heatmap.iter().zip(after.heatmap.iter()) {
            assert_eq!(b.total, a.total, "heatmap cell {} 守恒", b.day);
        }
    }

    #[test]
    fn full_day_rollup_bounds_are_usable() {
        use chrono::Datelike;
        let mut conn = db::open_in_memory().unwrap();
        let (day1_ts, _now, _) = seed(&conn);
        let pricing = Pricing::load_from(&PathBuf::from("nonexistent.json"));
        crate::rollup::rollup_and_prune(&mut conn, Some(0)).unwrap();

        // 构造 day1 本地日的整日范围（DST 空洞回退 +1h）
        let midnight = |d: chrono::NaiveDate| {
            d.and_hms_opt(0, 0, 0)
                .unwrap()
                .and_local_timezone(Local)
                .earliest()
                .unwrap()
                .timestamp_millis()
        };
        let day1_date = Local
            .timestamp_millis_opt(day1_ts)
            .earliest()
            .unwrap()
            .date_naive();
        let start = midnight(day1_date);
        let end = midnight(day1_date.succ_opt().unwrap()) - 1; // day1 的最后一毫秒

        // 整天范围：rollup 行完整可用，只有 r1（day1 当天）落进来
        let d = build_dashboard(&conn, Some(start), end, &pricing).unwrap();
        assert_eq!(d.stats.total_tokens, 10500);
        assert_eq!(d.stats.messages, 1);
        assert_eq!(d.models.len(), 1);
        assert_eq!(d.models[0].model, "glm-5.2");

        // 反例：起点挪到 day1 中午（部分覆盖）→ rollup 行收窄掉 → 无数据可取
        // （剪枝后的部分天只能靠 detail，这是文档化的取舍）
        let noon = start + 12 * 3600 * 1000;
        let d2 = build_dashboard(&conn, Some(noon), end, &pricing).unwrap();
        assert_eq!(d2.stats.total_tokens, 0);
        assert_eq!(d2.stats.messages, 0);
    }
}
