//! JSONL 增量扫描器（AGENTS.md 架构约束 #2）。
//!
//! 每个源文件记游标（mtime + 字节偏移 + 尾部指纹），下次扫描：
//! 1. mtime 门：文件没变直接跳过；
//! 2. seek 到字节偏移只读新增；
//! 3. 尾部指纹识别"同尺寸重写/截断"这种 mtime 检测不到的外部改动，
//!    检出后从头重扫——重放旧区间的重复入库由 request_id 幂等
//!    upsert 和去重台账挡住；
//! 4. 游标只推进到最后一个完整行（`\n` 结尾）之后，半行不入账，
//!    等 append 抬高 mtime 后补读。
//!
//! 机制出处：cc-switch `session_usage.rs`（游标判定/半行处理），
//! 按本项目需求裁剪。

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// 指纹覆盖游标边界前的字节数。
const TAIL_FINGERPRINT_BYTES: u64 = 4096;
/// 指纹域标签——一经发布即冻结（台账/游标比代码活得久，改了会全量重扫）。
const FINGERPRINT_TAG: &[u8] = b"token-atlas-tail-v1";

/// 游标表的一行（读侧视图）。
#[derive(Debug, Clone)]
pub struct FileCursor {
    pub mtime_ms: i64,
    pub byte_offset: u64,
    pub tail_fingerprint: Option<i64>,
    pub parser_state: Option<String>,
}

/// 一次 sync 的结果与应写回的新游标。
#[derive(Debug)]
pub struct JsonlSync {
    pub mtime_ms: i64,
    /// 已消费完整行的字节偏移（半行不计入）
    pub byte_offset: u64,
    pub tail_fingerprint: i64,
    /// true = 检测到截断/重写，本次从 0 重扫
    pub reset: bool,
    /// true = mtime 未变，什么都没读
    pub skipped: bool,
    pub lines_parsed: u64,
}

/// 一次性预取某采集器的全部游标（避免逐文件查库）。
pub fn load_cursors(conn: &Connection, adapter: &str) -> rusqlite::Result<HashMap<String, FileCursor>> {
    let mut stmt = conn.prepare(
        "SELECT source_key, mtime_ms, byte_offset, tail_fingerprint, parser_state
         FROM file_cursors WHERE adapter = ?1",
    )?;
    let rows = stmt.query_map([adapter], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (key, mtime_ms, byte_offset, fp, parser_state) = row?;
        map.insert(
            key,
            FileCursor {
                mtime_ms,
                byte_offset: byte_offset.max(0) as u64,
                tail_fingerprint: fp,
                parser_state,
            },
        );
    }
    Ok(map)
}

/// 写回游标（调用方负责事务）。
pub fn upsert_cursor(
    conn: &Connection,
    adapter: &str,
    source_key: &str,
    sync: &JsonlSync,
    parser_state: Option<&str>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO file_cursors (source_key, adapter, mtime_ms, byte_offset, tail_fingerprint, last_synced_at, parser_state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(source_key) DO UPDATE SET
            adapter = excluded.adapter,
            mtime_ms = excluded.mtime_ms,
            byte_offset = excluded.byte_offset,
            tail_fingerprint = excluded.tail_fingerprint,
            last_synced_at = excluded.last_synced_at,
            parser_state = COALESCE(excluded.parser_state, file_cursors.parser_state)",
        rusqlite::params![
            source_key,
            adapter,
            sync.mtime_ms,
            sync.byte_offset as i64,
            sync.tail_fingerprint,
            now_ms(),
            parser_state,
        ],
    )
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 计算文件 [max(0, offset-4096) .. offset] 区间加域标签的指纹。
/// offset 超出文件大小（并发截断）返回 None。
fn fingerprint_at(path: &Path, offset: u64) -> std::io::Result<Option<i64>> {
    let size = fs::metadata(path)?.len();
    if offset > size {
        return Ok(None);
    }
    let start = offset.saturating_sub(TAIL_FINGERPRINT_BYTES);
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (offset - start) as usize];
    file.read_exact(&mut buf)?;
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_TAG);
    hasher.update(&buf);
    let digest = hasher.finalize();
    Ok(Some(i64::from_be_bytes(
        digest[..8].try_into().expect("sha256 digest >= 8 bytes"),
    )))
}

/// 对单个 JSONL 文件做一次增量同步。
///
/// `on_line(行内容, fresh_run)` 只收到完整行；`fresh_run` 为 true 表示
/// 本次从头重扫（截断/重写/首扫），解析器应重置其文件内状态。
/// 解析失败由调用方决定跳过或报错，不影响游标推进（坏行也计入已消费）。
pub fn sync_jsonl<F>(path: &Path, cursor: Option<&FileCursor>, mut on_line: F) -> std::io::Result<JsonlSync>
where
    F: FnMut(&str, bool),
{
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 源文件被删：返回 skipped，游标由调用方决定保留（等它回来）或清理
            return Ok(JsonlSync {
                mtime_ms: cursor.map(|c| c.mtime_ms).unwrap_or(0),
                byte_offset: cursor.map(|c| c.byte_offset).unwrap_or(0),
                tail_fingerprint: 0,
                reset: false,
                skipped: true,
                lines_parsed: 0,
            });
        }
        Err(e) => return Err(e),
    };
    let size = meta.len();
    let mtime = mtime_ms(&meta);

    // ---- 起点决策 ----
    let mut start = 0u64;
    let mut reset = true;
    if let Some(c) = cursor {
        if mtime <= c.mtime_ms && c.byte_offset <= size {
            // mtime 未变且无截断：跳过
            return Ok(JsonlSync {
                mtime_ms: mtime,
                byte_offset: c.byte_offset,
                tail_fingerprint: c.tail_fingerprint.unwrap_or(0),
                reset: false,
                skipped: true,
                lines_parsed: 0,
            });
        }
        let truncated = c.byte_offset > size;
        let rewritten = if truncated {
            false
        } else if let Some(stored_fp) = c.tail_fingerprint {
            // 有指纹才校验；NULL（旧游标）按纯追加处理
            fingerprint_at(path, c.byte_offset)?.map(|fp| fp != stored_fp) == Some(true)
        } else {
            false
        };
        if !truncated && !rewritten {
            start = c.byte_offset;
            reset = false;
        }
    }

    // ---- 增量读取：只消费完整行 ----
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut consumed = start;
    let mut lines = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if buf.last() == Some(&b'\n') {
            consumed += n as u64;
            // 去掉行尾 \n 与可能的 \r
            while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
                buf.pop();
            }
            lines += 1;
            on_line(std::str::from_utf8(&buf).unwrap_or(""), reset);
        }
        // 半行：不入账、不推进
    }

    let fp = fingerprint_at(path, consumed)?.unwrap_or_else(|| {
        // 边界无法读取（极端并发）：给一个稳定占位，下轮 mtime 门放行后重新建立
        let mut hasher = Sha256::new();
        hasher.update(FINGERPRINT_TAG);
        i64::from_be_bytes(hasher.finalize()[..8].try_into().expect("sha256 digest >= 8 bytes"))
    });

    Ok(JsonlSync {
        mtime_ms: mtime,
        byte_offset: consumed,
        tail_fingerprint: fp,
        reset,
        skipped: false,
        lines_parsed: lines,
    })
}

/// 递归收集目录下全部 .jsonl 文件（深度上限 12，对齐原版 walkFiles）。
pub fn collect_jsonl_files(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .max_depth(12)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("token-atlas-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
    }

    #[test]
    fn first_scan_reads_complete_lines_only() {
        let dir = tmpdir("first");
        let f = dir.join("a.jsonl");
        write(&f, "{\"n\":1}\n{\"n\":2}\n{\"n\":3"); // 最后一行不完整
        let mut lines = Vec::new();
        let sync = sync_jsonl(&f, None, |l, fresh| {
            assert!(fresh);
            lines.push(l.to_string());
        })
        .unwrap();
        assert_eq!(lines.len(), 2);
        // 游标停在第二个 \n 之后
        assert_eq!(sync.byte_offset, "{\"n\":1}\n{\"n\":2}\n".len() as u64);
        assert_eq!(sync.lines_parsed, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_reads_only_append() {
        let dir = tmpdir("resume");
        let f = dir.join("a.jsonl");
        write(&f, "l1\nl2\n");
        let mut lines = Vec::new();
        let s1 = sync_jsonl(&f, None, |l, _| lines.push(l.to_string())).unwrap();
        assert_eq!(lines.len(), 2);

        // 用第一轮游标续读：文件 append 两行
        write(&f, "l1\nl2\nl3\nl4\n");
        let cur = FileCursor {
            mtime_ms: s1.mtime_ms,
            byte_offset: s1.byte_offset,
            tail_fingerprint: Some(s1.tail_fingerprint),
            parser_state: None,
        };
        // mtime 可能同毫秒内不变，强制抬一下判定用的 mtime：写内容变 mtime 大概率变；
        // 若真没变（同毫秒），把游标 mtime 减 1 保证放行
        let cur = if cur.mtime_ms >= mtime_ms(&fs::metadata(&f).unwrap()) {
            FileCursor { mtime_ms: cur.mtime_ms - 1, ..cur }
        } else {
            cur
        };
        let mut lines2 = Vec::new();
        let s2 = sync_jsonl(&f, Some(&cur), |l, fresh| {
            assert!(!fresh);
            lines2.push(l.to_string());
        })
        .unwrap();
        assert!(!s2.skipped && !s2.reset);
        assert_eq!(lines2, vec!["l3", "l4"]);
        assert_eq!(s2.byte_offset, "l1\nl2\nl3\nl4\n".len() as u64);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mtime_unchanged_skips() {
        let dir = tmpdir("skip");
        let f = dir.join("a.jsonl");
        write(&f, "l1\n");
        let s1 = sync_jsonl(&f, None, |_, _| {}).unwrap();
        let cur = FileCursor {
            mtime_ms: i64::MAX, // 永远比文件新 → 跳过
            byte_offset: s1.byte_offset,
            tail_fingerprint: Some(s1.tail_fingerprint),
            parser_state: None,
        };
        let mut called = 0;
        let s2 = sync_jsonl(&f, Some(&cur), |_, _| called += 1).unwrap();
        assert!(s2.skipped);
        assert_eq!(called, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_size_rewrite_detected_by_fingerprint() {
        let dir = tmpdir("rewrite");
        let f = dir.join("a.jsonl");
        write(&f, "aaaa\nbbbb\n"); // 10 bytes
        let s1 = sync_jsonl(&f, None, |_, _| {}).unwrap();
        let cur = FileCursor {
            mtime_ms: s1.mtime_ms - 1, // 放行 mtime 门
            byte_offset: s1.byte_offset,
            tail_fingerprint: Some(s1.tail_fingerprint),
            parser_state: None,
        };
        // 同尺寸重写
        write(&f, "xxxx\nyyyy\n");
        let mut lines = Vec::new();
        let s2 = sync_jsonl(&f, Some(&cur), |l, fresh| {
            assert!(fresh, "重写必须触发 fresh 重扫");
            lines.push(l.to_string());
        })
        .unwrap();
        assert!(s2.reset);
        assert_eq!(lines, vec!["xxxx", "yyyy"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncation_detected() {
        let dir = tmpdir("trunc");
        let f = dir.join("a.jsonl");
        write(&f, "l1\nl2\nl3\n");
        let s1 = sync_jsonl(&f, None, |_, _| {}).unwrap();
        let cur = FileCursor {
            mtime_ms: s1.mtime_ms - 1,
            byte_offset: s1.byte_offset,
            tail_fingerprint: Some(s1.tail_fingerprint),
            parser_state: None,
        };
        write(&f, "l1\n"); // 截断
        let mut lines = Vec::new();
        let s2 = sync_jsonl(&f, Some(&cur), |l, fresh| {
            assert!(fresh);
            lines.push(l.to_string());
        })
        .unwrap();
        assert!(s2.reset);
        assert_eq!(lines, vec!["l1"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn incomplete_tail_not_committed_then_completed() {
        let dir = tmpdir("tail");
        let f = dir.join("a.jsonl");
        write(&f, "l1\nl2");
        let mut lines = Vec::new();
        let s1 = sync_jsonl(&f, None, |l, _| lines.push(l.to_string())).unwrap();
        assert_eq!(lines, vec!["l1"]);
        assert_eq!(s1.byte_offset, 3); // "l1\n"

        // 补完整行（append）
        write(&f, "l1\nl2\nl3\n");
        let cur = FileCursor {
            mtime_ms: s1.mtime_ms - 1,
            byte_offset: s1.byte_offset,
            tail_fingerprint: Some(s1.tail_fingerprint),
            parser_state: None,
        };
        // 指纹区间在 offset=3 处是 "l1\n" 前缀，append 后该区间内容未变 → 校验通过
        let mut lines2 = Vec::new();
        let _s2 = sync_jsonl(&f, Some(&cur), |l, fresh| {
            assert!(!fresh);
            lines2.push(l.to_string());
        })
        .unwrap();
        assert_eq!(lines2, vec!["l2", "l3"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
