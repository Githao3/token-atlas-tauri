//! 核心数据模型：用量明细、input 语义枚举、四桶归一。
//!
//! 口径出处：AGENTS.md「Token 口径」与原版 `electron/scan/normalize.ts`
//! 的 `splitTokens()`。本模块是纯逻辑，单测即口径文档。

use serde::Serialize;

/// `input_tokens` 语义（AGENTS.md 架构约束 #4，对齐 cc-switch 的枚举三分法）。
///
/// 新 provider 默认按 Fresh（不含缓存）处理：配错的表现是缓存命中率
/// 异常低（显眼的错），反方向则是静默多扣（哑的错）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[repr(u8)]
pub enum InputSemantics {
    /// 历史行，语义未知（按 Fresh 解释）
    Legacy = 0,
    /// input_tokens **已包含** cache read/write（OpenAI Responses / Gemini
    /// 风格；zcode 实测 computed_total = input + output，亦属此类）
    Total = 1,
    /// input_tokens 为纯新增输入，与缓存**互斥不相交**（Anthropic 风格）
    Fresh = 2,
}

impl InputSemantics {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => InputSemantics::Total,
            2 => InputSemantics::Fresh,
            _ => InputSemantics::Legacy,
        }
    }

    /// 该语义下 input 是否含缓存。
    pub fn is_cache_inclusive(self) -> bool {
        matches!(self, InputSemantics::Total)
    }
}

/// 采集器吐出的原始四桶 + 权威总数，交给 [`normalize_parts`] 归一。
#[derive(Debug, Clone, Copy, Default)]
pub struct RawParts {
    /// 源的 input_tokens（语义未知，由 hints 判定）
    pub input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    /// 多数 provider 已含在 output 内，仅随行透传
    pub reasoning: i64,
    /// 源自己的权威总数（provider_total_tokens / total_tokens）；
    /// None 则四桶和即总数
    pub reported_total: Option<i64>,
    /// 采集器显式声明 input 不含缓存（claude 专用，压过自动判定）
    pub input_excludes_cache: bool,
}

/// 归一化结果：四桶互不重叠，且和恒等于 total。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizedParts {
    pub fresh_input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    pub total: i64,
    /// 判定出的语义（写库用）
    pub semantics: InputSemantics,
}

fn clamp(v: i64) -> i64 {
    if v < 0 {
        0
    } else {
        v
    }
}

/// 判定 input 是否含缓存——原版 `splitTokens()` 的判定链：
/// 显式声明 > 无缓存平凡情形 > 权威总数对账 > 启发式兜底。
fn detect_inclusive(raw: &RawParts) -> bool {
    if raw.input_excludes_cache {
        return false;
    }
    // 没有缓存参与时两种解释数值等价，记 Fresh（语义更诚实）
    if raw.cache_read + raw.cache_write == 0 {
        return false;
    }
    if let Some(total) = raw.reported_total {
        // 对上 input+output：缓存必在 input 里
        if total == raw.input + raw.output {
            return true;
        }
        // 对上 input+output+cache：input 是纯新增
        if total == raw.input + raw.output + raw.cache_read + raw.cache_write {
            return false;
        }
    }
    // 兜底启发式：input 至少装得下缓存才可能含缓存
    raw.cache_read + raw.cache_write > 0 && raw.input >= raw.cache_read + raw.cache_write
}

/// 四桶归一：对账 drift 全部吸收回 freshInput，保证不变量严格成立。
///
/// 注意 zcode 这类源：`computed_total = input + output`（input 含缓存），
/// 归一后 fresh = input - cacheRead - cacheWrite。
pub fn normalize_parts(raw: &RawParts) -> NormalizedParts {
    let cache_read = clamp(raw.cache_read);
    let cache_write = clamp(raw.cache_write);
    let output = clamp(raw.output);

    let inclusive = detect_inclusive(raw);
    let semantics = if inclusive {
        InputSemantics::Total
    } else {
        InputSemantics::Fresh
    };

    let mut fresh = if inclusive {
        raw.input - cache_read - cache_write
    } else {
        raw.input
    };

    // 权威总数：优先源上报值，缺省四桶和
    let sum = fresh + cache_read + cache_write + output;
    let reported = raw.reported_total.filter(|t| *t > 0);
    let mut total = reported.unwrap_or(sum);

    // 对账吸收：残差并回 freshInput（原版 normalize.ts 同款）
    fresh += total - sum;

    // 极端情况 fresh 被钳到 0 时，总数以四桶和为准（不变量优先于上报值）
    if fresh < 0 {
        fresh = 0;
        total = fresh + cache_read + cache_write + output;
    }

    NormalizedParts {
        fresh_input: fresh,
        cache_read,
        cache_write,
        output,
        total,
        semantics,
    }
}

/// 一条已归一化的用量明细（对应 `usage_entries` 表的一行）。
#[derive(Debug, Clone, Serialize)]
pub struct UsageEntry {
    pub adapter: String,
    pub request_id: String,
    pub model: String,
    /// Unix 毫秒（UTC）
    pub ts: i64,
    /// 源原始 input（不做扣减，重归一能力靠它 + semantics）
    pub input_tokens: i64,
    /// 归一后的纯新增输入（不变量成立）
    pub fresh_input_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
    pub input_token_semantics: u8,
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub source_file: Option<String>,
}

impl UsageEntry {
    /// 从原始四桶构造明细行。`parts` 归一 + 原始 input 一并落库。
    pub fn from_raw(
        adapter: &str,
        request_id: String,
        model: String,
        ts: i64,
        raw: &RawParts,
        session_id: Option<String>,
        project: Option<String>,
        source_file: Option<String>,
    ) -> Self {
        let n = normalize_parts(raw);
        UsageEntry {
            adapter: adapter.to_string(),
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
            source_file,
        }
    }
}

/// 模型名归一（原版 `normalizeModel`）：取 `/` 末段 + 小写。
/// `zai-org/GLM-5.2` → `glm-5.2`；空值 → `unknown`。
pub fn normalize_model(model: &str) -> String {
    let last = model.rsplit('/').next().unwrap_or("").trim();
    if last.is_empty() {
        "unknown".to_string()
    } else {
        last.to_lowercase()
    }
}

/// 项目路径归一（原版 `normalizeProject`）：trim + 去尾部分隔符，空串 → None。
pub fn normalize_project(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zcode_input_includes_cache() {
        // 本机实测形态：computed_total = input + output，cacheRead ⊆ input
        let raw = RawParts {
            input: 133_656,
            cache_read: 132_352,
            cache_write: 0,
            output: 1_217,
            reported_total: Some(134_873),
            ..Default::default()
        };
        let n = normalize_parts(&raw);
        assert_eq!(n.semantics, InputSemantics::Total);
        assert_eq!(n.fresh_input, 133_656 - 132_352);
        assert_eq!(n.fresh_input + n.cache_read + n.cache_write + n.output, n.total);
        assert_eq!(n.total, 134_873);
    }

    #[test]
    fn claude_input_excludes_cache() {
        // claude：input 与缓存互斥，total 缺省 = 四桶和
        let raw = RawParts {
            input: 1_000,
            cache_read: 50_000,
            cache_write: 2_000,
            output: 800,
            input_excludes_cache: true,
            ..Default::default()
        };
        let n = normalize_parts(&raw);
        assert_eq!(n.semantics, InputSemantics::Fresh);
        assert_eq!(n.fresh_input, 1_000);
        assert_eq!(n.total, 1_000 + 50_000 + 2_000 + 800);
    }

    #[test]
    fn openai_style_total_semantics() {
        // OpenAI Responses 风格：promptTokenCount 含缓存，总数同口径上报
        let raw = RawParts {
            input: 3_000,
            cache_read: 2_500,
            output: 500,
            reported_total: Some(3_500),
            ..Default::default()
        };
        let n = normalize_parts(&raw);
        assert_eq!(n.semantics, InputSemantics::Total);
        assert_eq!(n.fresh_input, 500);
        assert_eq!(n.total, 3_500);
    }

    #[test]
    fn drift_absorbed_into_fresh() {
        // 权威总数比四桶和多 15：残差并回 fresh
        let raw = RawParts {
            input: 100,
            output: 50,
            reported_total: Some(165),
            ..Default::default()
        };
        let n = normalize_parts(&raw);
        assert_eq!(n.fresh_input, 115);
        assert_eq!(n.total, 165);
        assert_eq!(n.fresh_input + n.output, n.total);
    }

    #[test]
    fn heuristic_fallback_small_input_is_fresh() {
        // input 装不下缓存且无权威总数可对账：判 Fresh，不减缓存
        let raw = RawParts {
            input: 10,
            cache_read: 500,
            output: 20,
            ..Default::default()
        };
        let n = normalize_parts(&raw);
        assert_eq!(n.semantics, InputSemantics::Fresh);
        assert_eq!(n.fresh_input, 10);
        assert_eq!(n.total, 530);
    }

    #[test]
    fn inclusive_negative_fresh_clamped_and_invariant_kept() {
        // 权威总数判成含缓存但 input < 缓存（异常源）：fresh 钳 0，total 回退四桶和
        let raw = RawParts {
            input: 10,
            cache_read: 500,
            output: 20,
            reported_total: Some(30), // = input + output → 判为含缓存
            ..Default::default()
        };
        let n = normalize_parts(&raw);
        assert_eq!(n.semantics, InputSemantics::Total);
        assert_eq!(n.fresh_input, 0);
        assert_eq!(n.fresh_input + n.cache_read + n.cache_write + n.output, n.total);
        assert_eq!(n.total, 520);
    }

    #[test]
    fn zero_cache_rows_are_fresh() {
        let raw = RawParts {
            input: 100,
            output: 20,
            reported_total: Some(120),
            ..Default::default()
        };
        let n = normalize_parts(&raw);
        assert_eq!(n.semantics, InputSemantics::Fresh);
        assert_eq!(n.fresh_input, 100);
    }

    #[test]
    fn model_name_normalization() {
        assert_eq!(normalize_model("zai-org/GLM-5.2"), "glm-5.2");
        assert_eq!(normalize_model("@cf/moonshotai/kimi-k2.7-code"), "kimi-k2.7-code");
        assert_eq!(normalize_model("GLM-5.3-Flash"), "glm-5.3-flash");
        assert_eq!(normalize_model(""), "unknown");
        assert_eq!(normalize_model("/"), "unknown");
    }

    #[test]
    fn project_normalization() {
        assert_eq!(normalize_project("D:\\AI-projects\\TokenAtlas\\"), Some("D:\\AI-projects\\TokenAtlas".to_string()));
        assert_eq!(normalize_project("  /home/u/proj/  "), Some("/home/u/proj".to_string()));
        assert_eq!(normalize_project(""), None);
        assert_eq!(normalize_project("   "), None);
    }
}
