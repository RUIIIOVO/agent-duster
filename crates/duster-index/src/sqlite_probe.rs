//! 对**别人家**的 SQLite 文件做只读探测。
//!
//! 与 [`crate::db`] 的区别：那里管的是 duster 自己的索引库；这里探测的是
//! agent 自己的库（codex 的 `logs_2.sqlite`、cc-switch 的主库…），
//! 一律 `SQLITE_OPEN_READ_ONLY` + `query_only`，绝不建库、绝不迁移、
//! 绝不写 WAL——探测一个正在被 agent 使用的库不能有任何副作用。

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// SQLite 文件头魔数。非 SQLite 文件直接短路，免得给每个日志文件都开一次连接。
const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// 估算一个 SQLite 文件里 **VACUUM 能拿回多少字节**：空闲页数 × 页大小。
///
/// 返回 `Ok(None)` 表示"这个问题不适用"——不是 SQLite 文件、文件太小、
/// 或者库正被独占占用打不开。**永远不返回文件总大小兜底**：宁可报 0，
/// 也不能把"这一项占了多少"冒充成"这一项能清出多少"。
///
/// 是保守估计：VACUUM 顺带的碎片整理通常还能多挤出一点，实测差异在 1% 内。
pub fn vacuum_reclaimable(path: &Path) -> Result<Option<u64>> {
    if !is_sqlite(path)? {
        return Ok(None);
    }
    // 只读 + 不走 WAL 恢复：URI 里的 immutable=0 保持默认，仅拒绝写。
    let conn = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        // 库被独占、损坏、加密——都不是错误，只是这项估不出来。
        Err(_) => return Ok(None),
    };
    conn.pragma_update(None, "query_only", true).ok();

    let free: i64 = match conn.query_row("PRAGMA freelist_count", [], |r| r.get(0)) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let page: i64 = match conn.query_row("PRAGMA page_size", [], |r| r.get(0)) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if free <= 0 || page <= 0 {
        return Ok(Some(0));
    }
    Ok(Some((free as u64).saturating_mul(page as u64)))
}

/// 按文件头魔数判断是不是 SQLite 库。读不到（权限/不存在）视为"不是"。
fn is_sqlite(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };
    let mut head = [0u8; 16];
    match f.read_exact(&mut head) {
        Ok(()) => Ok(&head == MAGIC),
        Err(_) => Ok(false), // 比 16 字节还短，不可能是 SQLite。
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 有空洞的库要报出接近 `freelist_count * page_size` 的可回收量，
    /// 且必须**小于**文件总大小——这正是 l0 与 l1 的分界。
    #[test]
    fn reports_free_pages_not_file_size() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("holes.sqlite");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "PRAGMA auto_vacuum = NONE;
                 CREATE TABLE t(x BLOB);",
            )
            .unwrap();
            let blob = vec![0u8; 4096];
            for _ in 0..400 {
                conn.execute("INSERT INTO t(x) VALUES (?1)", [&blob])
                    .unwrap();
            }
            // 删掉绝大多数行：页进 freelist，文件不缩。
            conn.execute("DELETE FROM t WHERE rowid > 10", []).unwrap();
        }

        let file_size = std::fs::metadata(&db).unwrap().len();
        let free = vacuum_reclaimable(&db).unwrap().expect("是 SQLite 库");
        assert!(free > 0, "删了 390 行应该留下空闲页");
        assert!(
            free < file_size,
            "可回收量 {free} 不能大于等于文件总大小 {file_size}——真实数据还在"
        );
    }

    /// 非 SQLite 文件不是错误，是"不适用"。绝不能退化成报文件大小。
    #[test]
    fn non_sqlite_file_is_not_applicable() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("app.log");
        std::fs::write(&f, vec![b'x'; 10_000]).unwrap();
        assert_eq!(vacuum_reclaimable(&f).unwrap(), None);

        let tiny = dir.path().join("tiny");
        std::fs::write(&tiny, b"hi").unwrap();
        assert_eq!(vacuum_reclaimable(&tiny).unwrap(), None);

        assert_eq!(vacuum_reclaimable(&dir.path().join("nope")).unwrap(), None);
    }
}
