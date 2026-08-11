//! 连接管理:WAL、busy_timeout、单实例写锁。
//!
//! 单实例锁方案:在旁路 SQLite 文件 `<db>.lock` 上持有 `BEGIN EXCLUSIVE`,
//! 事务不放即为进程级写锁。取舍:
//! - 不能锁 `index.db` 自身——EXCLUSIVE 会把持有者自己的主连接也挡在外面;
//! - 不用裸 lock 文件(O_EXCL)——进程崩溃会残留死锁文件,需要脆弱的清理逻辑;
//! - SQLite 事务锁在进程崩溃/被 kill 时由内核自动释放,零残留。
//!   第二个 duster 进程 `BEGIN EXCLUSIVE` 立即失败(busy_timeout=0),
//!   CLI 据此映射为退出码 5(锁冲突)。代价:多一个 `<db>.lock` 旁路小文件。

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

use crate::schema;

/// 打开(必要时创建)索引库的可写句柄。
///
/// 持有期间独占写权:第二个 [`Index::open`] 会以 [`LockBusy`](Error) 失败。
/// 只读消费方用 [`Index::open_readonly`],不受写锁影响。
pub struct Index {
    conn: Connection,
    /// 旁路锁库连接,持有 `BEGIN EXCLUSIVE` 直到 drop。字段本身即 RAII。
    _lock_conn: Connection,
}

/// 锁冲突的可识别错误(CLI 据此映射退出码 5)。
#[derive(Debug, thiserror::Error)]
#[error("index database is locked by another duster instance: {0}")]
pub struct LockBusy(pub String);

impl Index {
    /// 建父目录 -> 打开连接 -> WAL/busy_timeout/synchronous -> 抢单实例锁 -> 迁移 schema。
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create index directory: {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open index database: {}", path.display()))?;
        Self::tune(&conn)?;

        // 单实例写锁:旁路 <db>.lock 库,busy_timeout=0,冲突立即失败而非等待。
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock_conn = Connection::open(Path::new(&lock_path))?;
        lock_conn.busy_timeout(std::time::Duration::ZERO)?;
        lock_conn
            .execute_batch("BEGIN EXCLUSIVE")
            .map_err(|e| LockBusy(e.to_string()))
            .with_context(|| format!("index database: {}", path.display()))?;

        schema::migrate(&conn)?;
        Ok(Self {
            conn,
            _lock_conn: lock_conn,
        })
    }

    /// 只读打开:不建目录、不迁移、不抢锁。库不存在或版本超前一律报错。
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| {
            format!(
                "failed to open index database read-only: {}",
                path.display()
            )
        })?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update(None, "query_only", true)?;
        // 只读句柄不需要写锁;_lock_conn 用一个内存库占位,零成本。
        let placeholder = Connection::open_in_memory()?;
        Ok(Self {
            conn,
            _lock_conn: placeholder,
        })
    }

    fn tune(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(())
    }

    /// 借出底层连接。上层(upsert / 检索)在此之上工作。
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_migrates_and_locks() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("nested/index.db");
        let idx = Index::open(&db).unwrap();
        // schema 已迁移
        let n: i64 = idx
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='resource'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        // 第二个实例被锁拒绝
        let err = Index::open(&db).err().unwrap();
        assert!(err.downcast_ref::<LockBusy>().is_some(), "err = {err:#}");
        // 释放后可重开
        drop(idx);
        Index::open(&db).unwrap();
    }

    #[test]
    fn readonly_coexists_with_writer_and_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let writer = Index::open(&db).unwrap();
        let ro = Index::open_readonly(&db).unwrap();
        let err = ro
            .conn()
            .execute("INSERT INTO agent(agent_id) VALUES('x')", []);
        assert!(err.is_err(), "只读句柄不得可写");
        drop(writer);
    }
}
