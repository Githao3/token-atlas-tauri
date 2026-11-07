-- 0001: 初始账本 schema
--
-- 设计蓝本：sources/cc-switch（MIT），按本项目需求裁剪。
-- 核心决策见 AGENTS.md「架构约束」与 PROGRESS.md 决策日志。

-- ----------------------------------------------------------------
-- 明细表：每行 = 一次模型请求的用量。UI 永远查账本，不直接扫源。
--
-- 口径（AGENTS.md「Token 口径」）：
--   input_tokens        存源的原始值（不做扣减），语义见 input_token_semantics
--   fresh_input_tokens  归一后的纯新增输入（不含缓存），已吸收对账 drift
--   不变量：fresh_input_tokens + cache_read_tokens + cache_write_tokens
--           + output_tokens == total_tokens
--   total_tokens        权威总数（优先用源自己上报的 total，缺省用四桶和）
--   成本不入库：计价随 pricing.json 可变，查询时按 model × tokens 现算
--
-- 主键 (adapter, request_id)：request_id 在单采集器内唯一
--   zcode   = logical_request_id
--   claude  = "session:" + message.id
--   codex   = 文件内事件语义 id（累计值差分后仍有 fork/重写风险，台账兜底）
--   opencode= message id
-- INSERT/UPSERT 语义：同 request_id 重扫时整行覆盖（幂等）。
-- ----------------------------------------------------------------
CREATE TABLE IF NOT EXISTS usage_entries (
    request_id            TEXT NOT NULL,
    adapter               TEXT NOT NULL,
    model                 TEXT NOT NULL,
    ts                    INTEGER NOT NULL,              -- Unix 毫秒（UTC）
    input_tokens          INTEGER NOT NULL DEFAULT 0,    -- 源原始值
    fresh_input_tokens    INTEGER NOT NULL DEFAULT 0,    -- 归一后，不变量成立
    cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens         INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens      INTEGER NOT NULL DEFAULT 0,    -- 多数 provider 已含在 output 内
    total_tokens          INTEGER NOT NULL DEFAULT 0,
    input_token_semantics INTEGER NOT NULL DEFAULT 0,    -- 0=legacy 1=total(含缓存) 2=fresh(不含)
    session_id            TEXT,
    project               TEXT,                          -- 归一后的工作目录
    source_file           TEXT,                          -- 调试用：出处（sqlite 路径 / jsonl 路径）
    PRIMARY KEY (adapter, request_id)
);

CREATE INDEX IF NOT EXISTS idx_entries_ts           ON usage_entries(ts);
CREATE INDEX IF NOT EXISTS idx_entries_adapter_ts   ON usage_entries(adapter, ts);
CREATE INDEX IF NOT EXISTS idx_entries_model        ON usage_entries(model);
CREATE INDEX IF NOT EXISTS idx_entries_session      ON usage_entries(session_id) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_entries_project      ON usage_entries(project)    WHERE project IS NOT NULL;

-- ----------------------------------------------------------------
-- 日汇总表：过期明细聚合后的归档（按本地日分桶）。
-- 四维主键保证 Dashboard 所有聚合（热力图/趋势/按模型/按项目/按适配器）
-- 在明细被清理后仍可完整回答。rollup 行的 tokens 一律已是 fresh 语义
-- （fresh_input_tokens 列），无需再归一。
-- ----------------------------------------------------------------
CREATE TABLE IF NOT EXISTS usage_daily (
    day                  TEXT NOT NULL,     -- 本地日期 'YYYY-MM-DD'
    adapter              TEXT NOT NULL,
    model                TEXT NOT NULL,
    project              TEXT NOT NULL DEFAULT '',
    request_count        INTEGER NOT NULL DEFAULT 0,
    fresh_input_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens    INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens        INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens     INTEGER NOT NULL DEFAULT 0,
    total_tokens         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, adapter, model, project)
);

CREATE INDEX IF NOT EXISTS idx_daily_day     ON usage_daily(day);
CREATE INDEX IF NOT EXISTS idx_daily_adapter ON usage_daily(adapter, day);

-- ----------------------------------------------------------------
-- 会话台账：session 级元数据（不能进日汇总的高基数维度）。
-- 会话数 / 项目↔会话映射靠它；明细被 rollup 清理后仍可回答"该范围
-- 内有多少活跃会话"。first/last_ts 取该会话明细的 min/max ts。
-- ----------------------------------------------------------------
CREATE TABLE IF NOT EXISTS sessions (
    adapter    TEXT NOT NULL,
    session_id TEXT NOT NULL,
    project    TEXT,
    first_ts   INTEGER NOT NULL,
    last_ts    INTEGER NOT NULL,
    PRIMARY KEY (adapter, session_id)
);

-- ----------------------------------------------------------------
-- 文件游标表：增量解析的续读位置（AGENTS.md 架构约束 #2）。
--   mtime_ms         上次同步时的文件修改时间（毫秒）
--   byte_offset      已消费到的字节偏移（只推进到最后一个完整行末尾；
--                    半行不入账、不推进，等下轮 append 后补读）
--   tail_fingerprint 游标边界前 4096 字节的指纹（SHA-256 加域标签后取
--                    前 4 字节），识别"同尺寸重写"这种 mtime/size 检测
--                    不到的外部改动；NULL = 未建立指纹
--   parser_state     解析器上下文（如 codex 的累计值累加器），与游标
--                    原子提交，保证"数据 + 游标"同进退
-- SQLite 源（zcode/opencode 主路径）不走文件游标：整表重读 +
-- request_id 幂等 upsert 已足够快。
-- ----------------------------------------------------------------
CREATE TABLE IF NOT EXISTS file_cursors (
    source_key       TEXT PRIMARY KEY,   -- 源文件绝对路径
    adapter          TEXT NOT NULL,
    mtime_ms         INTEGER NOT NULL,
    byte_offset      INTEGER NOT NULL DEFAULT 0,
    tail_fingerprint INTEGER,
    last_synced_at   INTEGER NOT NULL,
    parser_state     TEXT
);

-- ----------------------------------------------------------------
-- 去重台账（AGENTS.md 架构约束 #5）：持久化的"已入库收据"。
-- 明细被 rollup 清掉后台账仍在，Codex fork/重写回放旧区间时不会
-- 重复计数。semantic_id 是内容指纹；无稳定业务 ID 的源
-- request_id == semantic_id。
-- ----------------------------------------------------------------
CREATE TABLE IF NOT EXISTS dedup_ledger (
    adapter     TEXT NOT NULL,
    request_id  TEXT NOT NULL,
    semantic_id TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (adapter, request_id)
);

CREATE INDEX IF NOT EXISTS idx_dedup_semantic ON dedup_ledger(adapter, semantic_id);

-- ----------------------------------------------------------------
-- 元信息：schema 版本之外的小型键值（如上次 rollup 时间）。
-- ----------------------------------------------------------------
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
