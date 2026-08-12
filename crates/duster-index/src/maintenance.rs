//! 对**别人家**的 SQLite 库做维护性写操作。
//!
//! 与 [`crate::sqlite_probe`] 成对：那边只读估算"能回收多少"，
//! 这边真的去回收。与 [`crate::db`] 无关——那管的是 duster 自己的索引库。
//!
//! 唯一的写操作是 VACUUM，它是 l0（无损）的定义本身：原地重写，
//! 空闲页还给文件系统，数据一行不少。本机验收目标：
//! `~/.codex/logs_2.sqlite` 784 MB → 约 10 MB，11353 行日志一条不少。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

use duster_fs::lockprobe::{LockStatus, ensure_free};

/// FTS5 影子表的后缀。它们是虚表的内部存储（段 blob、docsize、config），
/// 行数由分词与合并策略决定，不是"用户数据的条数"，进了校验值只会制造假警报。
const SHADOW_SUFFIXES: [&str; 5] = ["_data", "_idx", "_docsize", "_config", "_content"];

/// 一次 VACUUM 的结果。
#[derive(Debug, Clone, Copy)]
pub struct VacuumResult {
    /// 执行前文件字节数。
    pub before: u64,
    /// 执行后文件字节数。
    pub after: u64,
    /// 实际释放（`before - after`，不会为负）。
    pub freed: u64,
    /// 执行前后的总行数校验值（各表 row count 之和）。相等才算无损。
    pub rows_before: i64,
    pub rows_after: i64,
}

/// 对 `path` 执行 VACUUM，返回前后体积与行数校验。
///
/// 硬要求：
/// - **执行前先过锁探测**（`duster_fs::lockprobe::ensure_free`），拿不到写锁即报错；
/// - 执行前后各统计一次全表行数，**不相等则报错**——l0 承诺的是"数据一条不少"，
///   这是它唯一的机器守卫；
/// - VACUUM 需要临时空间（约等于库大小），空间不足时 SQLite 会报错，原样上抛；
/// - 不改 `journal_mode`、不 checkpoint 别人的 WAL、不留任何 pragma 副作用。
pub fn vacuum(path: &Path) -> Result<VacuumResult> {
    // 判据与执行一致：VACUUM 本来就要写锁，探测拿不到就没必要开工。
    ensure_free(path)?;

    let before = file_len(path)?;
    let rows_before = total_row_count(path)?;

    {
        // 不带 CREATE：路径不存在就该报错，而不是凭空造一个空库出来 VACUUM。
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| {
            format!(
                "failed to open sqlite database for writing: {}",
                path.display()
            )
        })?;
        // 只发这一条语句：不设 journal_mode、不 checkpoint 别人的 WAL、
        // 不留 pragma 副作用——这是别人家的库，改了它的模式就是越权。
        conn.execute_batch("VACUUM")
            .with_context(|| format!("VACUUM failed on {}", path.display()))?;
    }

    // 重新 stat / 重新计数：都必须是执行**之后**的实测值，不能沿用预估。
    let after = file_len(path)?;
    let rows_after = total_row_count(path)?;
    if rows_before != rows_after {
        bail!(
            "VACUUM changed the row count of {}: {rows_before} rows before, {rows_after} after. \
             l0 promises not a single row lost; treat this database as suspect and restore a backup",
            path.display()
        );
    }

    Ok(VacuumResult {
        before,
        after,
        freed: before.saturating_sub(after),
        rows_before,
        rows_after,
    })
}

/// 文件字节数。VACUUM 前后各取一次。
fn file_len(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len())
}

/// 只读打开别人家的库：绝不建库、绝不迁移、绝不写 WAL。
fn open_ro(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "failed to open sqlite database read-only: {}",
            path.display()
        )
    })?;
    conn.pragma_update(None, "query_only", true).ok();
    Ok(conn)
}

/// `name` 是否是某个已存在表的影子表。
///
/// 用"后缀剥掉之后的主名在本库里确实是一张表"来判定，而不是光看后缀：
/// 一张用户自己建的 `metrics_data` 不该因为名字撞了模式就被排除在校验之外。
fn is_shadow(name: &str, tables: &HashSet<&str>) -> bool {
    SHADOW_SUFFIXES.iter().any(|suffix| {
        name.strip_suffix(suffix)
            .is_some_and(|base| !base.is_empty() && tables.contains(base))
    })
}

/// 全表行数之和。VACUUM 前后各跑一次，作为"无损"的校验值。
///
/// 跳过 `sqlite_*` 内部表与 FTS 影子表（`*_data` / `*_idx` / `*_docsize` /
/// `*_config` / `*_content`）——它们装的是分词后的段 blob，行数由合并策略
/// 决定，跟"用户数据有几条"没关系，进了校验值只会制造假警报。虚表本身照常
/// 计数；拒绝全表扫描的（contentless FTS5）跳过，反正前后两次都跳。
pub fn total_row_count(path: &Path) -> Result<i64> {
    let conn = open_ro(path)?;

    let tables: Vec<(String, String)> = {
        let mut st = conn
            .prepare(
                "SELECT name, IFNULL(sql, '') FROM sqlite_master
                 WHERE type = 'table' ORDER BY name",
            )
            .context("failed to enumerate tables")?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .context("failed to enumerate tables")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to enumerate tables")?
    };
    let names: HashSet<&str> = tables.iter().map(|(n, _)| n.as_str()).collect();

    let mut total: i64 = 0;
    for (name, sql) in &tables {
        if name.starts_with("sqlite_") || is_shadow(name, &names) {
            continue;
        }
        let is_virtual = sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("CREATE VIRTUAL TABLE");
        // 标识符按 SQLite 规则加双引号转义：别人家的表名什么样都可能。
        let count_sql = format!("SELECT COUNT(*) FROM \"{}\"", name.replace('"', "\"\""));
        match conn.query_row(&count_sql, [], |r| r.get::<_, i64>(0)) {
            Ok(n) => total += n,
            // contentless FTS5 之类的虚表会拒绝全表扫描。跳过是**确定性**的
            // （前后两次都跳），校验值仍然可比；报错反而会让 VACUUM 白白失败。
            Err(_) if is_virtual => {}
            Err(e) => {
                return Err(e).with_context(|| format!("failed to count rows of table `{name}`"));
            }
        }
    }
    Ok(total)
}

/// `PRAGMA integrity_check` 是否通过。VACUUM 前的体检，也是 M2 doctor 的一项。
pub fn integrity_ok(path: &Path) -> Result<bool> {
    let conn = open_ro(path)?;
    let mut st = conn
        .prepare("PRAGMA integrity_check")
        .context("failed to prepare integrity_check")?;
    let rows: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))
        .context("failed to run integrity_check")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read integrity_check result")?;
    // 通过时 SQLite 恰好回一行 `ok`；任何多余的行都是问题描述。
    Ok(rows.len() == 1 && rows[0].eq_ignore_ascii_case("ok"))
}

/// 孤儿 WAL/SHM 判定：`<db>-wal` / `<db>-shm` 存在，但主库能拿到独占锁
/// （说明没有活跃连接），且 WAL 已无未 checkpoint 的帧。
///
/// 返回可安全删除的旁路文件路径与它们的字节数合计。
/// **拿不准一律返回空**——一个有内容的 WAL 被删掉就是数据丢失。
pub fn orphan_sidecars(path: &Path) -> Result<(Vec<PathBuf>, u64)> {
    /// 拿不准时的返回值。写成常量是为了让下面每一处"收手"都长得一样显眼。
    const UNSURE: (Vec<PathBuf>, u64) = (Vec::new(), 0);

    let wal = sidecar(path, "-wal");
    let shm = sidecar(path, "-shm");
    if !wal.exists() && !shm.exists() {
        return Ok(UNSURE);
    }

    // ① 先看 WAL 有没有帧，而且是**纯 stat**——这一步必须在任何打开动作之前：
    //    只读连接碰上热 WAL 会去做恢复，那就等于我们自己动了别人的库。
    //    WAL 头是 32 字节，短于它不可能承载一个完整帧；≥32 一律当作"有内容"。
    if let Ok(md) = std::fs::metadata(&wal)
        && md.len() >= 32
    {
        return Ok(UNSURE);
    }

    // ② 主库得真的是个能打开的库。打不开就说明这两个旁路文件的来历不明，不碰。
    let Ok(conn) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Ok(UNSURE);
    };
    let readable = conn
        .query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
        .is_ok();
    drop(conn);
    if !readable {
        return Ok(UNSURE);
    }

    // ③ 锁探测。这里**只认 Free**：`Unknown` 意味着这台机器根本没检查成功，
    //    而 `-shm` 可能正被某个活连接 mmap 着，删它就是在拆人家脚下的地板。
    if !matches!(ensure_free(path), Ok(LockStatus::Free)) {
        return Ok(UNSURE);
    }

    // 尺寸在最后取：上面的探测会以读写方式开一次库，可能顺手重建过旁路文件。
    let mut paths = Vec::new();
    let mut bytes = 0u64;
    for p in [wal, shm] {
        if let Ok(md) = std::fs::metadata(&p) {
            bytes += md.len();
            paths.push(p);
        }
    }
    Ok((paths, bytes))
}

/// `<db>-wal` / `<db>-shm`：SQLite 的旁路文件名是**字节级追加**，
/// 不是换扩展名——`set_extension` 会把 `logs_2.sqlite` 变成 `logs_2-wal`。
fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个有大量空闲页的库：插满再删掉绝大部分，页进 freelist，文件不缩。
    fn bloated_db(path: &Path, keep: i64) -> i64 {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA auto_vacuum = NONE;
             CREATE TABLE t(x BLOB);",
        )
        .unwrap();
        let blob = vec![7u8; 4096];
        for _ in 0..400 {
            conn.execute("INSERT INTO t(x) VALUES (?1)", [&blob])
                .unwrap();
        }
        conn.execute("DELETE FROM t WHERE rowid > ?1", [keep])
            .unwrap();
        keep
    }

    /// l0 的全部承诺：文件缩了，行数一条不少。
    #[test]
    fn vacuum_缩小文件且行数不变() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("holes.sqlite");
        let kept = bloated_db(&db, 10);

        let r = vacuum(&db).unwrap();
        assert!(
            r.after < r.before,
            "VACUUM 后应变小：{} → {}",
            r.before,
            r.after
        );
        assert_eq!(r.freed, r.before - r.after);
        assert_eq!(r.rows_before, kept, "执行前就该只剩 {kept} 行");
        assert_eq!(r.rows_before, r.rows_after, "一条不少");
        assert_eq!(std::fs::metadata(&db).unwrap().len(), r.after);
        assert!(integrity_ok(&db).unwrap());
    }

    /// 被独占的库拒绝 VACUUM，没有 `--force`。
    #[test]
    fn vacuum_拒绝被独占的库() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("busy.sqlite");
        bloated_db(&db, 10);

        // 同进程内的第二条连接同样会被 SQLite 挡住（它自己维护进程内锁表）。
        let holder = Connection::open(&db).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let err = vacuum(&db).expect_err("库正被独占，必须拒绝");
        let msg = err.to_string();
        assert!(msg.contains("is locked by"), "错误文案应是锁冲突：{msg}");

        holder.execute_batch("ROLLBACK").unwrap();
    }

    /// 校验值只数真·用户行，FTS5 的影子表不算。
    #[test]
    fn total_row_count_忽略_fts_影子表() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("fts.sqlite");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE note(body TEXT);
                 CREATE VIRTUAL TABLE ft USING fts5(body);",
            )
            .unwrap();
            for i in 0..3 {
                conn.execute("INSERT INTO note(body) VALUES (?1)", [&format!("n{i}")])
                    .unwrap();
            }
            for i in 0..2 {
                conn.execute("INSERT INTO ft(body) VALUES (?1)", [&format!("f{i}")])
                    .unwrap();
            }

            // 影子表确实存在且有内容——否则这个测试是空转。
            let shadow: i64 = conn
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM ft_data)
                          + (SELECT COUNT(*) FROM ft_idx)
                          + (SELECT COUNT(*) FROM ft_docsize)
                          + (SELECT COUNT(*) FROM ft_config)
                          + (SELECT COUNT(*) FROM ft_content)",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(shadow > 0, "影子表应当有内容，否则这个断言什么也没证明");
        }

        // note 3 + ft 2 = 5，影子表一行不算。
        assert_eq!(total_row_count(&db).unwrap(), 5);
    }

    /// 名字撞了影子表后缀、但主名并不是一张表的用户表，必须照常计数。
    #[test]
    fn total_row_count_不误伤同后缀的用户表() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("user.sqlite");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE metrics_data(x);").unwrap();
            conn.execute("INSERT INTO metrics_data VALUES (1)", [])
                .unwrap();
        }
        assert_eq!(total_row_count(&db).unwrap(), 1);
    }

    /// 有帧的 WAL 一律不返回；空 WAL 且库空闲才认。
    #[test]
    fn orphan_sidecars_有帧的_wal_不碰() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("w.sqlite");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE t(x);").unwrap();
        }

        // 伪造一个"有内容"的 WAL：超过 32 字节的头即视为可能有帧。
        let wal = sidecar(&db, "-wal");
        std::fs::write(&wal, vec![0u8; 4096]).unwrap();
        assert_eq!(orphan_sidecars(&db).unwrap().0.len(), 0);

        // 清空后才是可回收的孤儿。
        std::fs::write(&wal, b"").unwrap();
        let (paths, bytes) = orphan_sidecars(&db).unwrap();
        assert_eq!(paths, vec![wal.clone()], "只有空 WAL 该被认领");
        assert_eq!(bytes, 0);

        // 根本不是库的路径，旁路文件再像也不动。
        let fake = dir.path().join("not-a.sqlite");
        std::fs::write(&fake, b"hello").unwrap();
        std::fs::write(sidecar(&fake, "-wal"), b"").unwrap();
        assert_eq!(orphan_sidecars(&fake).unwrap().0.len(), 0);
    }
}
