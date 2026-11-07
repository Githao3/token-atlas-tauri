//! 账本数据库：打开、PRAGMA、迁移。
//!
//! 自建账本库（AGENTS.md 架构约束 #1）：UI 永远查这里，不直接扫源。
//! 默认路径 `~/.token-atlas/ledger.db`，与原版 pricing.json 同目录。

pub mod migrations {
    pub const INIT: &str = include_str!("db/migrations/0001_init.sql");
}

use rusqlite::Connection;
use std::path::PathBuf;

/// 账本默认路径：`~/.token-atlas/ledger.db`。
pub fn ledger_path() -> PathBuf {
    let home = dirs::home_dir().expect("无法解析用户主目录");
    home.join(".token-atlas").join("ledger.db")
}

/// 打开（并按需迁移）账本库。WAL 模式，busy_timeout 5s。
pub fn open(path: &PathBuf) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// 打开默认路径的账本库。
pub fn open_default() -> rusqlite::Result<Connection> {
    open(&ledger_path())
}

/// 内存库（测试用）：跑完整迁移。
pub fn open_in_memory() -> rusqlite::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

/// 按 `PRAGMA user_version` 顺序应用迁移；每条迁移一个事务。
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let mut version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    const MIGRATIONS: &[(u32, &str)] = &[(1, migrations::INIT)];
    for (target, sql) in MIGRATIONS {
        if *target > version {
            conn.execute_batch(&format!(
                "BEGIN;\n{sql}\nPRAGMA user_version = {target};\nCOMMIT;"
            ))?;
            version = *target;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_creates_all_tables() {
        let conn = open_in_memory().unwrap();
        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            tables,
            vec![
                "dedup_ledger",
                "file_cursors",
                "meta",
                "sessions",
                "usage_daily",
                "usage_entries"
            ]
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = open_in_memory().unwrap();
        migrate(&conn).unwrap(); // 再跑一遍不应报错、不应重复建表
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn entry_roundtrip() {
        let conn = open_in_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO usage_entries (request_id, adapter, model, ts, input_tokens,
                fresh_input_tokens, cache_read_tokens, cache_write_tokens, output_tokens,
                reasoning_tokens, total_tokens, input_token_semantics, session_id, project)
             VALUES ('req-1', 'zcode', 'glm-5.3-flash', 1794073095726, 133656,
                1304, 132352, 0, 1217, 0, 134873, 1, 'sess-1', 'D:\\proj')",
        )
        .unwrap();
        let (fresh, semantics): (i64, u8) = conn
            .query_row(
                "SELECT fresh_input_tokens, input_token_semantics FROM usage_entries WHERE adapter='zcode' AND request_id='req-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(fresh, 1304);
        assert_eq!(semantics, 1);
    }
}
