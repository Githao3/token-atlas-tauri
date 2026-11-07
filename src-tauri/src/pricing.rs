//! 计价模块：内置规则 + 用户覆盖文件（`~/.token-atlas/pricing.json`）。
//!
//! 忠实移植原版 `electron/pricing.ts`：规则为大小写不敏感子串匹配、
//! 自上而下首个命中生效；成本是估算（未命中走 fallback 并标记），
//! 永远不假装是账单。
//!
//! 成本不入库（决策日志 2026-11-08）：价目可被用户随时修改并立即
//! 作用于全部历史，查询时按 model × tokens 现算。

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// 每百万 token 的美元单价。
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Rate {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Default for Rate {
    fn default() -> Self {
        Rate { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 }
    }
}

/// 内置规则表（自上而下，子串匹配）。出处：原版 pricing.ts DEFAULT_RULES。
const DEFAULT_RULES: &[(&str, Rate)] = &[
    ("-free", Rate { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 }),
    ("claude-opus", Rate { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
    ("claude-sonnet", Rate { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }),
    ("claude-haiku", Rate { input: 0.8, output: 4.0, cache_read: 0.08, cache_write: 1.0 }),
    ("claude", Rate { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }),
    ("gpt-5", Rate { input: 1.25, output: 10.0, cache_read: 0.125, cache_write: 1.56 }),
    ("gpt-", Rate { input: 1.25, output: 10.0, cache_read: 0.125, cache_write: 1.56 }),
    ("gemini", Rate { input: 1.25, output: 10.0, cache_read: 0.31, cache_write: 1.6 }),
    ("grok", Rate { input: 3.0, output: 15.0, cache_read: 0.75, cache_write: 3.75 }),
    ("deepseek-v4-pro", Rate { input: 0.55, output: 2.19, cache_read: 0.07, cache_write: 0.7 }),
    ("deepseek", Rate { input: 0.28, output: 0.42, cache_read: 0.028, cache_write: 0.35 }),
    ("glm", Rate { input: 0.6, output: 2.2, cache_read: 0.11, cache_write: 0.75 }),
    ("kimi", Rate { input: 0.6, output: 2.5, cache_read: 0.15, cache_write: 0.75 }),
    ("mimo", Rate { input: 0.3, output: 1.2, cache_read: 0.05, cache_write: 0.4 }),
    ("qwen", Rate { input: 0.4, output: 1.2, cache_read: 0.08, cache_write: 0.5 }),
    ("doubao", Rate { input: 0.15, output: 0.6, cache_read: 0.03, cache_write: 0.2 }),
    ("minimax", Rate { input: 0.3, output: 1.2, cache_read: 0.06, cache_write: 0.4 }),
];

/// 未命中任何规则时的兜底费率（原版 FALLBACK）。
const FALLBACK: Rate = Rate { input: 1.0, output: 3.0, cache_read: 0.1, cache_write: 1.25 };

/// 用户覆盖文件（与原版同格式）。
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct OverrideFile {
    /// 额外规则，排在内置规则之前（除非 replace_rules）
    rules: Option<Vec<PricingRule>>,
    /// true 则完全忽略内置表
    replace_rules: Option<bool>,
    /// 精确模型 id 补丁（优先级最高）
    models: Option<HashMap<String, PartialRate>>,
    /// 未命中模型的兜底费率补丁
    fallback: Option<PartialRate>,
}

/// JSON 字段是 "match"（Rust 关键字，用 rename 处理）。
#[derive(Debug, Deserialize)]
struct PricingRule {
    #[serde(rename = "match")]
    match_: String,
    rate: PartialRate,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PartialRate {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

impl PartialRate {
    /// 叠加到基准费率上（Some 字段覆盖）。
    fn apply_to(self, base: Rate) -> Rate {
        Rate {
            input: self.input.unwrap_or(base.input),
            output: self.output.unwrap_or(base.output),
            cache_read: self.cache_read.unwrap_or(base.cache_read),
            cache_write: self.cache_write.unwrap_or(base.cache_write),
        }
    }
}

/// 计价器：加载覆盖文件（坏文件静默退回内置默认），提供 rate_for。
#[derive(Debug, Clone)]
pub struct Pricing {
    models: HashMap<String, Rate>,
    rules: Vec<(String, Rate)>,
    fallback: Rate,
    /// 覆盖文件路径（UI 展示"价目来自哪"）
    pub path: PathBuf,
    /// 覆盖文件是否真的生效（逐字复制默认表不算，原版 sameAsDefaults 口径）
    pub using_overrides: bool,
}

impl Pricing {
    /// 覆盖文件路径：`~/.token-atlas/pricing.json`（与原版一致）。
    pub fn override_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".token-atlas")
            .join("pricing.json")
    }

    /// 从默认路径加载；文件缺失/损坏一律退回内置默认，永不抛错。
    pub fn load() -> Pricing {
        Pricing::load_from(&Pricing::override_path())
    }

    pub fn load_from(path: &PathBuf) -> Pricing {
        let mut pricing = Pricing::builtin(path.clone());
        let Ok(text) = std::fs::read_to_string(path) else {
            return pricing;
        };
        let Ok(ov) = serde_json::from_str::<OverrideFile>(&text) else {
            return pricing; // 坏文件：静默退回（原版同款）
        };

        let mut rules: Vec<(String, Rate)> = Vec::new();
        // 用户规则段先原样收好（sameAsDefaults 比较用它，而非合并后的列表）
        let user_rules: Vec<(String, Rate)> = ov
            .rules
            .map(|rs| {
                rs.into_iter()
                    .map(|r| (r.match_.to_lowercase(), r.rate.apply_to(FALLBACK)))
                    .collect()
            })
            .unwrap_or_default();
        rules.extend(user_rules.iter().cloned());
        // 用户规则前插；replaceRules 时不叠加内置表
        let replace = ov.replace_rules.unwrap_or(false);
        if !replace {
            for (m, rate) in DEFAULT_RULES {
                rules.push((m.to_lowercase(), *rate));
            }
        }

        let models = ov
            .models
            .map(|m| {
                m.into_iter()
                    .map(|(k, v)| (k.to_lowercase(), v.apply_to(FALLBACK)))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        let fallback = ov.fallback.map(|f| f.apply_to(FALLBACK)).unwrap_or(FALLBACK);

        // 逐字复制默认表不算真覆盖（原版 sameAsDefaults 口径）
        let using_overrides = {
            let file_rules_same = !replace
                && user_rules.len() == DEFAULT_RULES.len()
                && user_rules
                    .iter()
                    .zip(DEFAULT_RULES.iter())
                    .all(|((um, ur), (dm, dr))| um == *dm && ur == dr);
            !(file_rules_same && models.is_empty() && fallback == FALLBACK)
        };

        pricing.models = models;
        pricing.rules = rules;
        pricing.fallback = fallback;
        pricing.using_overrides = using_overrides;
        pricing
    }

    fn builtin(path: PathBuf) -> Pricing {
        Pricing {
            models: HashMap::new(),
            rules: DEFAULT_RULES.iter().map(|(m, r)| (m.to_lowercase(), *r)).collect(),
            fallback: FALLBACK,
            path,
            using_overrides: false,
        }
    }

    /// 查价：精确 models 补丁 > 规则子串自上而下 > fallback。
    /// 返回 (费率, 是否命中规则)；未命中时 UI 会提示"估算不可当账单"。
    pub fn rate_for(&self, model: &str) -> (Rate, bool) {
        let key = model.to_lowercase();
        if let Some(r) = self.models.get(&key) {
            return (*r, true);
        }
        for (m, rate) in &self.rules {
            if key.contains(m.as_str()) {
                return (*rate, true);
            }
        }
        (self.fallback, false)
    }

    /// 四桶算钱（USD）：tokens / 1e6 × 单价。
    pub fn cost_of(rate: &Rate, fresh_input: i64, cache_read: i64, cache_write: i64, output: i64) -> f64 {
        const M: f64 = 1_000_000.0;
        fresh_input as f64 / M * rate.input
            + cache_read as f64 / M * rate.cache_read
            + cache_write as f64 / M * rate.cache_write
            + output as f64 / M * rate.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_rule_order() {
        let p = Pricing::builtin(PathBuf::from("x"));
        // -free 最优先
        let (r, m) = p.rate_for("glm-5.2-free");
        assert!(m && r.input == 0.0);
        // gpt-5 比 gpt- 优先（虽然费率相同）
        let (r5, m5) = p.rate_for("gpt-5.2");
        assert!(m5 && r5.input == 1.25);
        // claude-opus 比 claude 优先
        let (ro, mo) = p.rate_for("claude-opus-5");
        assert!(mo && ro.input == 15.0);
        // 大小写不敏感
        let (_, mg) = p.rate_for("GLM-5.3-Flash");
        assert!(mg);
        // 未命中
        let (rf, mf) = p.rate_for("some-unknown-model");
        assert!(!mf && rf == FALLBACK);
    }

    #[test]
    fn exact_model_patch_beats_rules() {
        let dir = std::env::temp_dir().join(format!("ta-pricing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pricing.json");
        std::fs::write(
            &path,
            r#"{"models": {"glm-5.2": {"input": 9.9, "output": 9.9}}}"#,
        )
        .unwrap();
        let p = Pricing::load_from(&path);
        let (r, m) = p.rate_for("glm-5.2");
        assert!(m && r.input == 9.9 && r.output == 9.9);
        // 未指定的字段继承 FALLBACK 基准
        assert_eq!(r.cache_read, FALLBACK.cache_read);
        // 其他模型不受影响
        let (r2, m2) = p.rate_for("glm-5.3");
        assert!(m2 && r2.input == 0.6);
        assert!(p.using_overrides);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_rules_prepended_and_replace() {
        let dir = std::env::temp_dir().join(format!("ta-pricing2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pricing.json");
        // 用户规则把 glm 定成 7.7：应压过内置 glm
        std::fs::write(&path, r#"{"rules": [{"match": "glm", "rate": {"input": 7.7}}]}"#).unwrap();
        let p = Pricing::load_from(&path);
        let (r, _) = p.rate_for("glm-5.2");
        assert_eq!(r.input, 7.7);
        assert!(p.using_overrides);

        // replaceRules: 只剩用户规则，claude 未命中 → fallback
        std::fs::write(&path, r#"{"rules": [{"match": "glm", "rate": {"input": 7.7}}], "replaceRules": true}"#).unwrap();
        let p2 = Pricing::load_from(&path);
        let (r2, m2) = p2.rate_for("claude-sonnet-5");
        assert!(!m2 && r2 == FALLBACK);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_file_falls_back_to_builtin() {
        let dir = std::env::temp_dir().join(format!("ta-pricing3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pricing.json");
        std::fs::write(&path, "{{{ not json").unwrap();
        let p = Pricing::load_from(&path);
        assert!(!p.using_overrides);
        let (r, m) = p.rate_for("glm-5.2");
        assert!(m && r.input == 0.6);
        // 缺失文件同理
        let p2 = Pricing::load_from(&dir.join("missing.json"));
        assert!(!p2.using_overrides);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verbatim_default_table_is_not_override() {
        let dir = std::env::temp_dir().join(format!("ta-pricing4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pricing.json");
        // 逐字复制内置表（顺序也一致）：using_overrides 应为 false
        let rules: Vec<String> = DEFAULT_RULES
            .iter()
            .map(|(m, r)| {
                format!(
                    r#"{{"match": "{}", "rate": {{"input": {}, "output": {}, "cacheRead": {}, "cacheWrite": {}}}}}"#,
                    m, r.input, r.output, r.cache_read, r.cache_write
                )
            })
            .collect();
        std::fs::write(&path, format!(r#"{{"rules": [{}]}}"#, rules.join(","))).unwrap();
        let p = Pricing::load_from(&path);
        assert!(!p.using_overrides);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cost_math() {
        let rate = Rate { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 };
        let c = Pricing::cost_of(&rate, 1_000_000, 2_000_000, 0, 100_000);
        assert!((c - (3.0 + 0.6 + 0.0 + 1.5)).abs() < 1e-9);
    }
}
