# PROGRESS.md — 进度真相源

> **维护规则**：每个会话开工前先读本文件和 `AGENTS.md`；每完成一个能跑通的小改动，**先更新本文件再 commit**。决策变化、新踩的坑、卡住的地方都要随手记，不要攒。

## 当前阶段

**Tauri 重构启动**（2026-11-08）。范围锁定在"原版功能平移"，所有功能扩展进 Backlog，本轮不做。

## 里程碑

### M0 — 脚手架
- [x] Tauri 2 + Vite + React + TS 骨架，`tauri dev` 跑通出窗口
- [x] `.gitignore`：`target/` `node_modules/` `dist/` **`sources/`**
- [x] `rust-toolchain.toml` 钉 Rust 版本

### M1 — 数据层（Rust 侧，重构的核心）
- [x] 账本 schema：明细表（含 `input_token_semantics` 枚举列）+ 日汇总表 + 文件游标表 + 去重台账（`model_pricing` 不进 DB，改为 Rust 模块 + JSON 覆盖文件，见决策日志 2026-11-08 计价条目）
- [ ] 4 个采集器：zcode / claude / codex / opencode（口径对照 `sources/` 原版的 `electron/adapters/`）
- [ ] 增量扫描器：游标（mtime + 字节偏移 + 尾部指纹），seek 只读新增
- [ ] 计价模块：内置规则 + 用户可编辑（对齐原版 `~/.token-atlas/pricing.json` 的行为）
- [ ] 汇总聚合：输出形状对齐原版 `Dashboard` 类型（`src/shared/types.ts`），让 UI 层零改动迁移
- [ ] 对拍验证：与原版 `npm run scan:test` 在同一批源数据上出数一致

### M2 — 壳与 IPC
- [ ] 三页导航布局 + 深色/浅色主题 + 中英 i18n（移植 `src/App.tsx` / `lib/i18n.tsx` / `styles/`）
- [ ] Tauri command：scan / openPath / editPricing（替代原 `window.tk` IPC）

### M3 — Overview 迁移
- [ ] 概览卡、热力图、每日堆叠柱状、项目排行、Top5+Others、成本与缓存、数据源分布

### M4 — Trends 迁移
- [ ] 趋势线（每日 + 7 日移动平均）、模型趋势（Top5 + Others，Tokens/调用次数切换）

### M5 — 3D Lab 迁移
- [ ] three.js Token Landscape 全套：分位台地、生长动画、月份轴、全屏（移植 `ThreeDLab.tsx`，658 行，逐段搬）

### M6 — 打包分发
- [ ] `tauri build` 出 NSIS/MSI，实测安装启动
- [ ] 实测内存/磁盘占用，与 Electron 版（383MB / 353MB）对比记录

## 决策日志

- **2026-11-08 重构方向确定**：Electron → Tauri 2。动机：实测原版内存 ~383MB（4 进程）、磁盘 353MB，大头是捆绑的 Chromium 运行时；空闲 CPU 正常（~0%，启动后有约 1 分钟渲染高峰）。
- **2026-11-08 范围锁定**：本轮只做原版功能平移 + 数据层升级；更多 Agent、3D 下钻、额度查询等全部进 Backlog。
- **2026-11-08 数据层设计以 cc-switch 为蓝本**：自建账本、SyncCursor 增量、每日 rollup、`input_token_semantics` 枚举、去重台账。参考实现可直接读 `sources/cc-switch`（MIT）。
- **2026-11-08 可视化沿用原版**：ECharts + three.js，几何与图表逻辑从 `sources/` 原版移植。
- **2026-11-08 参考仓库隔离**：两个参考仓库放 `sources/`，不入库、不改它们的内容。
- **2026-11-08 身份与图标沿用原版**：identifier `com.tokenatlas.app`、productName "Token Atlas"，图标从原版 ICO 重生成，保持产品观感延续。
- **2026-11-08 CSP 暂为模板默认（null）**：M2 移植前端时收紧为 `script-src 'self'`（原版口径，echarts-gl 的坑就是它暴露的），届时必须同步验证 dev 模式下 Vite HMR 不被 CSP 拦。
- **2026-11-08 成本不入库、查询时现算**：原版每次扫描都按当前价目表重算全部成本（用户改 pricing.json 立即作用于历史），账本存成本会冻结旧价。因此明细表不设 cost 列，聚合时按 model × tokens × 规则现算；M1 清单里的 `model_pricing` 表随之取消——价目 = Rust 模块内置规则 + `~/.token-atlas/pricing.json` 覆盖（与原版同格式同路径）。
- **2026-11-08 明细表双轨存 input**：既存源的原始 `input_tokens` + 语义枚举（语义判错时可重归一，数据不毁），也存归一后的 `fresh_input_tokens`（四桶不变量直接成立，查询不用跑 CASE）。归一逻辑 = 原版 `splitTokens` 移植（显式声明 > 无缓存平凡 > 权威总数对账 > 启发式，drift 全部吸收回 fresh）。
- **2026-11-08 会话台账独立成表**：session 是高基数维度进不了日汇总，单独建 `sessions` 表（first_ts/last_ts/project），明细被 rollup 清理后"范围内活跃会话数"仍可回答。日汇总主键带 project 维度（day×adapter×model×project），保证热力图/趋势/模型/项目/适配器五个聚合在剪枝后都完整。

## 已完成

- 2026-11-08 完成 Tk-Us 现状盘点（架构、踩坑史、待办）与 cc-switch 用量模块调研（账本/增量/rollup/语义枚举/去重五项核心设计）。
- 2026-11-08 实测 Electron 版资源占用：内存稳定 ~383MB、空闲 CPU ~0%、最小化归零、解压后 353MB —— 重构动机成立。
- 2026-11-08 建立本文档与 `AGENTS.md`。
- 2026-11-08 初始化 git 仓库（main 分支），创建远端 GitHub 仓库 `Githao3/token-atlas-tauri`（public，与原版 `token-atlas` 一致；原版仓库名已被 Electron 版占用）。
- 2026-11-08 **M0 完成**：create-tauri-app（react-ts 模板）生成骨架并入位。版本：Tauri CLI 2.12.4 / Vite 8.3.6（rolldown 内核，传递依赖少是正常的）/ React 19.3.1 / TS 6.0.3 / Rust 1.94.1。沿用原版身份 `com.tokenatlas.app` + productName "Token Atlas"，窗口 1280×800（min 960×600）。图标：从原版 `resources/icon.ico` 抽出 256px PNG，`tauri icon` 重生成全套（源图留档 `src-tauri/icons/icon-source.png`，Android/iOS 产物已删）。`tauri dev` 首次 cargo 全量编译 2m08s，窗口截图验收通过（WebView2 正常渲染 Vite 页）。
- 2026-11-08 **M1 调研完成**：cc-switch 数据层（四表结构/游标/rollup/语义枚举/台账）与原版四采集器口径全部摸清，要点：① 原版四桶不变量靠 splitTokens 对账吸收（drift 塞回 freshInput）；② claude 显式 input 不含缓存，codex 按文件差分累计值（`Math.max(0,…)` 防回退），zcode/opencode 靠权威总数自动判定；③ zcode/opencode 主路径是 SQLite 只读直连（Windows 下 copyfile 会 EBUSY），JSONL 仅回退；④ 原版无跨文件 fork 去重（cc-switch 台账补这个位）。
- 2026-11-08 **本机 zcode 数据实测**：`~/.zcode/cli/db/db.sqlite`（WAL，321MB）`model_usage` 表 2143 行。**input 含缓存实锤**：`computed_total = input + output` 在 2126/2126 行全部成立 → zcode 语义 = Total(1)，fresh = input − cacheRead − cacheWrite。schema 较原版写作时已演化：新增 `logical_request_id`（全表唯一，天然 request 主键）、`status`（2125 completed / 16 cancelled / 3 error）、`raw_usage_json`（权威原始用量）；新增 `turn_usage` 表但是按 turn 的粗汇总（无模型拆分），不用它。历史裁剪仍在发生（此前 7057 行 → 现 2143 行），账本必要性再次确认。原版口径不过滤 status（cancelled 也烧了 token），沿用。
- 2026-11-08 **M1.1 完成**：账本 schema 落地（`usage_entries` 明细 / `usage_daily` 日汇总 / `sessions` 会话台账 / `file_cursors` 游标 / `dedup_ledger` 去重台账 / `meta`），`db.rs` 打开+WAL+迁移（user_version 驱动），`models.rs` 四桶归一 + 语义枚举 + 模型名/项目归一。12 个单测全过（含 zcode 实测形态、claude 互斥、OpenAI total、drift 吸收、钳零不变量等口径用例）。库路径定 `~/.token-atlas/ledger.db`。
- 2026-11-08 **M1.2 完成**：zcode 采集器落地（`collect/zcode.rs` 只读直连 live DB → `model_usage` 全表 → 四桶归一 → 幂等 upsert，明细 + 会话台账同事务提交；request_id 取 `COALESCE(logical_request_id, span_id, assistant_message_id, 合成键)`）。**实机对拍通过**：账本 2148 行 vs node 源库快照 2147 行，差额恰好是两次查询之间本对话新产生的一条请求（938 fresh + 166272 cacheRead + 82 output = 167292 total，四桶精确吻合）；会话数 24 = 源库 25 个去重会话减 1 个全零用量会话（被原版口径过滤）。live 测试 `cargo test -- --ignored --nocapture live_zcode_scan`。
- 2026-11-08 **M1.3 完成**：JSONL 增量扫描器（`collect/jsonl.rs`：mtime 门 → 字节偏移 seek → 尾部指纹识别同尺寸重写/截断 → 半行不推进，6 个单测覆盖首扫/续读/跳过/重写/截断/半行补全）+ claude/codex/opencode 三采集器，`scan_all()` 汇总入口。**四采集器全部实机对拍精确一致**（claude 134 行 / codex 258 行 / opencode 7058 行，与 node 严格镜像脚本七项指标全等）。要点：① claude 全局按 message.id 去重——原版会把流式重复行（usage 完全相同，本机 22 个 id）双计，这是有意升级，对拍时 claude 数字比原版略低；② codex 新版（0.166+）`total_token_usage` 已有 `cache_write_input_tokens`，按差分映射（原版硬编码 0）；累计值累加器跨扫描持久化在游标 parser_state；③ fork/重写防重：`{session_id}:{ordinal}` 作 request_id + 事件语义哈希入 dedup_ledger，回放事件不重复计数；④ opencode 语义混合分布（141 行 Total / 6917 行 Fresh）实证了逐行存语义枚举的必要性。
- 2026-11-08 **踩坑：`fresh` 标志不是"首行"标志**。codex 采集器初版在闭包里写 `if fresh { state = default }`，而 `fresh` 是整轮常量（本轮是否重扫），导致首轮扫描**每行都清零累计值累加器**，每条事件按全量累计值差分，总量多算 ~37 倍（26.9M → 997M）。对拍脚本当场揪出。修复：状态默认归零，仅首行且为续读时从 parser_state 恢复。教训：**增量扫描器的跨行状态必须只在首行初始化一次，回调参数语义要写清楚**。

## 障碍 / 进行中

- （暂无）

## Backlog（本轮明确不做，做完 M6 再回来挑）

- 更多 Agent 采集器：cc-switch 已覆盖 12 个工具（gemini / grokbuild / kimi / mcode / pi …），按需加
- 小时级活跃热力图
- 3D 柱子点击下钻当天拆解；3D 地形 tokens/成本双模式
- 订阅额度与余额查询（官方订阅 5 小时/周/月额度、账户余额——cc-switch 的另一条产品线）
- models.dev 计价自动导入
- 代码签名（SmartScreen 拦截问题）、macOS / Linux 实测
- 云同步（WebDAV / S3）
