# AGENTS.md — Token Atlas（Tauri 重构版）

每个会话开工前先读本文和 `PROGRESS.md`。改完任何东西，**先更新 `PROGRESS.md` 再 commit**——这是硬性要求，不是建议。

## 这个项目是干什么的

**本地 AI Agent Token 用量图鉴。** 扫描电脑上各 AI 编程 Agent 留下的会话记录（zcode / Claude Code / Codex / opencode），把散落在不同工具里的 token 消耗汇总成一个桌面仪表盘：概览卡、热力图、趋势线、按项目/模型聚合，以及一个 three.js 等轴测的年度 Token Landscape（3D Lab）。

**数据全程留在本地，不上传任何内容。** 这是产品的底线约束，任何新功能都不许违反。

本仓库是原 Electron 版（`sources/` 里的 Tk-Us）的 **Tauri 重构版**。重构动机：实测原版内存 ~383MB、磁盘 353MB，几乎全是 Electron 捆绑的 Chromium；Tauri 走系统 WebView2，预估内存 ~150MB、磁盘 ~15MB。

## 参考仓库（sources/，不入版本控制）

| 仓库 | 用途 |
| --- | --- |
| `sources/token-atlas` | 原 Electron 版：可视化与数据口径的出处；其 `AGENT.md` 有完整的历史踩坑记录 |
| `sources/cc-switch` | 数据层设计的蓝本（MIT，可参考可搬运）：自建账本、增量游标、rollup、语义枚举的成熟 Rust 实现 |

读代码顺序建议：先看 `sources/cc-switch/src-tauri/src/services/session_usage.rs`（头部注释有数据流）、`database/dao/usage_rollup.rs`、`services/sql_helpers.rs`。

## 技术栈（已定，不要换）

- 壳：**Tauri 2**（Rust 1.85+，Windows 上走 WebView2）
- 前端：React + TypeScript + Vite
- 图表：ECharts + three.js（沿用原版可视化，几何逻辑直接移植 `src/lib/logo.ts` / `ThreeDLab.tsx`）
- 数据：**rusqlite**，自建账本库；不引入 Node sidecar

## 架构约束（重构的核心决策，别走回头路）

原版是"每次现扫现算"，重构版必须按下面的顺序做：

1. **自建账本**：扫描结果逐条落地进自己的 SQLite 明细表，UI 永远查账本，不直接扫源文件。理由：zcode 等工具会裁剪历史（实测 `model_usage` 表 7057 行被清到 6854 行），不落账本就丢数据。
2. **增量解析**：每个会话文件记游标（mtime + 字节偏移 + 尾部指纹），下次扫描 seek 到偏移只读新增。尾部指纹用于识别"同尺寸重写"这种 mtime/size 检测不到的外部改动。
3. **每日 rollup**：过期明细聚合进日汇总表后清理，查询时两表合并。聚合 cutoff 必须对齐本地午夜（保证最年轻的 rollup 行是完整一天）。
4. **input 语义显式化**：每个 Agent 对 `input_tokens` 是否含缓存的口径不一致，每行存 `input_token_semantics` 枚举，聚合 SQL 统一走 CASE 表达式归一。含缓存的 provider **显式列出**，新 provider 默认按"不含缓存"——配错会表现为缓存命中率异常低（响的错），好过静默多扣（哑的错）。
5. **去重台账**：Codex 的 fork/重写会重复上报，用 `request_id + semantic_id` 台账防重复计数，且台账在明细被 rollup 清掉后仍在。

## Token 口径（沿用原版，已验证）

四桶互不重叠且和恒等于权威总数：`freshInput + cacheRead + cacheWrite + output === total`。判定：拿来源自己的权威总数（`provider_total_tokens` / `total_tokens`）对账——对上 `input + output` 说明 input 含缓存，对上 `input + output + cache` 说明不含，对不上按 `input >= cache` 兜底，残差并回 freshInput。

## 踩过的坑（新坑随时补进来）

### 数据层
- **Codex JSONL 是累计值**：`total_token_usage` 记的是累计，必须取相邻两条的差值；用 `last_token_usage` 会因每轮重复出现而翻倍。
- **模型名分裂**：`zai-org/GLM-5.2` 和 `glm-5.2` 要归一成同一个——取 `/` 末段 + 小写。
- **zcode live DB 并发**：优先只读直连 live DB（WAL 模式可与运行中的 Agent 共存），失败才回退快照复制；不要一上来就 copyfile（EBUSY）。
- **各家 input 口径不一致**：见上面"架构约束"第 4 条。Anthropic 的 input 不含缓存；OpenAI Responses / Gemini 的 `promptTokenCount` 含。

### 前端
- **echarts-gl 不可用**：CSP `script-src 'self'` 下它无条件 `new Function()` 构造 EffectCompositor，直接崩且无法通过 option 关闭。3D 一律 three.js（着色器走 WebGL 编译），不给 CSP 开 `unsafe-eval`。
- **ECharts 的 animation 做不出"线条画进来"**：它是两个状态间的数值补间，新建 series 没有起点，直接画终态。进场效果用 CSS `clip-path` 擦除。验证手法：把时长临时调到 6 秒截一帧，有中间态就是在跑。
- **zrender 不认 CSS `color-mix()`**：不报错但面积填充静默不渲染。从 CSS 变量拿的颜色先转成 `rgba()` 再进 option。
- **React StrictMode 双跑 effect**：开发模式下 effect 跑两遍，"只执行一次"的 ref 守卫第一遍就被烧掉，真动画直接跳终态。守卫要么不加，要么在 cleanup 里回滚。涉及 effect 生命周期的改动必须 `dev` 模式复验，只用生产构建验证会漏。
- **切语言时 ECharts 旧标签残留**：i18n 的 `t` 身份随语言变，图表 option 的 `useMemo` 依赖数组必须含 `t`；包装组件的重建 `key` 要带上 `lang`。

### 构建与环境（本机）
- **git push 需要代理**：本机代理混合端口 `127.0.0.1:7897`（Clash/Mihomo），不一定常开。单次推送：
  ```bash
  git -c http.proxy=http://127.0.0.1:7897 -c https.proxy=http://127.0.0.1:7897 push
  ```
  `gh`（api.github.com）有时能通但 `git push`（github.com:443）不通，两者分开。
- 默认 shell 是 PowerShell 7。
- `sources/` 里的两个外部仓库**绝不进版本控制**，`.gitignore` 必须排除；`node_modules/`、`target/`、`dist/` 同理。
- Tauri 构建需要的 Rust 工具链以 `rust-toolchain.toml` 钉住版本。

## 流程约束

- **每个能跑通的小改动 = 先更 `PROGRESS.md`，再 commit**。保证每个 commit 都是可回退的还原点。
- `PROGRESS.md` 是唯一进度真相源：完成了什么、卡在哪、推翻了什么决策，随手写进去。
- 提交信息 conventional 风格（`feat:` / `fix:` / `refactor:` / `docs:` / `build:` / `test:` / `chore:`）。
- 确定的小改动直接进 main；可能整体推翻的试验才开 `try/*` 分支，验证完合并或删除。
- 远端 GitHub 仓库建好后：`git remote add origin <url>`，推送走上面的代理命令。
