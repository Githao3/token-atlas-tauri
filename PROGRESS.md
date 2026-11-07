# PROGRESS.md — 进度真相源

> **维护规则**：每个会话开工前先读本文件和 `AGENTS.md`；每完成一个能跑通的小改动，**先更新本文件再 commit**。决策变化、新踩的坑、卡住的地方都要随手记，不要攒。

## 当前阶段

**Tauri 重构启动**（2026-11-08）。范围锁定在"原版功能平移"，所有功能扩展进 Backlog，本轮不做。

## 里程碑

### M0 — 脚手架
- [ ] Tauri 2 + Vite + React + TS 骨架，`tauri dev` 跑通出窗口
- [x] `.gitignore`：`target/` `node_modules/` `dist/` **`sources/`**
- [ ] `rust-toolchain.toml` 钉 Rust 版本

### M1 — 数据层（Rust 侧，重构的核心）
- [ ] 账本 schema：明细表（含 `input_token_semantics` 枚举列）+ 日汇总表 + 文件游标表 + 去重台账 + `model_pricing` 表
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

## 已完成

- 2026-11-08 完成 Tk-Us 现状盘点（架构、踩坑史、待办）与 cc-switch 用量模块调研（账本/增量/rollup/语义枚举/去重五项核心设计）。
- 2026-11-08 实测 Electron 版资源占用：内存稳定 ~383MB、空闲 CPU ~0%、最小化归零、解压后 353MB —— 重构动机成立。
- 2026-11-08 建立本文档与 `AGENTS.md`。
- 2026-11-08 初始化 git 仓库（main 分支），创建远端 GitHub 仓库 `Githao3/token-atlas-tauri`（public，与原版 `token-atlas` 一致；原版仓库名已被 Electron 版占用）。

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
