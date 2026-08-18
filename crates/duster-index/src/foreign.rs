//! 安全只读打开**别人家**正在用的 SQLite 库。
//!
//! 与 [`crate::sqlite_probe`]（只看空闲页）和 [`crate::maintenance`]（VACUUM）
//! 的区别：这里要真的把数据读出来——opencode 的会话在 `opencode.db` 里，
//! omp 的在 `history.db` 里，Codex 的记忆在 `memories_1.sqlite` 里。
//!
//! # 难点是活跃 WAL
//!
//! 这些库在 agent 运行时是热的：`-wal` 里有还没 checkpoint 的帧，`-shm` 被
//! 别的进程 mmap 着。一个天真的只读连接会做两件坏事：
//!
//! 1. **触发 WAL 恢复**——只读连接读到一个需要恢复的 WAL 时，SQLite 会尝试
//!    写 `-shm`。库所在目录不可写就直接失败，可写则我们悄悄改了别人的状态。
//! 2. **挡住对方的 checkpoint**——长事务持有读锁期间，写方无法把 WAL 截断，
//!    对方的 `-wal` 会一直涨。duster 只是来看一眼的，不该让别人的库变胖。
//!
//! # 策略
//!
//! - `SQLITE_OPEN_READ_ONLY`，绝不带 `CREATE`；
//! - `PRAGMA query_only = 1` 兜底，任何写语句直接报错而不是悄悄成功；
//! - **不开长事务**：每次调用自己 `prepare` → 取完 → 立刻收尾，
//!   读锁的存活时间以毫秒计，对方的 checkpoint 不会被卡住；
//! - `busy_timeout` 给一个短值（默认 [`BUSY_TIMEOUT_MS`]）：等一下是合理的，
//!   等很久说明对方正忙，这时候放弃比排队更礼貌；
//! - 打不开、正在恢复、被加密——**都不是错误**，是"这一项读不到"。
//!   返回 `Ok(None)` 让调用方降级成只统计体积，绝不让一次扫描因为
//!   别人家的库而整体失败。
//!
//! # 写侧（uninstall residue）
//!
//! [`delete_row`] 是同一套策略的写侧：短 `busy_timeout`、不做重试循环，
//! 标识符走白名单、值走参数绑定。删的是**别人家**库里的行，所以动刀前
//! 仍要先问一遍「这张表/这一列还在吗」——上游换 schema 是常态，把一条
//! 会失败的 DELETE 交给 SQLite 去猜，不如先问出来报成可读的错误。

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

/// 等锁上限。别人正在写就让开，排队没有意义。
pub const BUSY_TIMEOUT_MS: u64 = 500;

/// SQLite 文件头魔数。
const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// 只读打开一个外部 SQLite 库。
///
/// 返回 `Ok(None)` 表示"读不到，但这不是错误"：不是 SQLite 文件、
/// 被独占、损坏、加密、需要 WAL 恢复而目录不可写。
/// 调用方据此降级为 stats-only，并把原因记进 warnings。
///
/// # 调用方的义务：拿了就走
///
/// 本函数返回的句柄**不持有任何事务**，但只要调用方 `prepare` 出一条语句并
/// 让它活着（比如把 `Rows` 存起来慢慢消费），SQLite 就会一直持有读锁。
/// 读锁存活期间，拥有这个库的 agent 进程无法 checkpoint，它的 `-wal`
/// 会一直涨、永远截不掉——我们只是来看一眼，却让别人的库无限变胖。
///
/// 所以：**prepare → 一次性取完 → 立刻 drop**。不要跨 I/O、不要跨用户交互、
/// 不要把 `Statement` 或 `Rows` 塞进长命结构体。duster 在这里是客人。
pub fn open(path: &Path) -> Result<Option<Connection>> {
    // 先看魔数：省掉给每个日志文件都开一次连接的开销，也顺带挡掉不存在的路径。
    if !is_sqlite(path) {
        return Ok(None);
    }

    // 只读，绝不带 CREATE——别人家的目录里不该因为我们多出一个空库。
    let conn = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        // 被独占、损坏、加密——都不是错误，只是这一项读不到。
        Err(_) => return Ok(None),
    };

    // 等一下是合理的，等很久说明对方正忙；不做重试循环，让开就是了。
    if conn
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .is_err()
    {
        return Ok(None);
    }
    // 只读 flag 已经拦住写了，query_only 是兜底：万一将来有人换了 flag，
    // 一条走错的写语句应该当场报错，而不是悄悄改了别人的库。
    if conn.pragma_update(None, "query_only", true).is_err() {
        return Ok(None);
    }

    // 打得开不等于用得了：`PRAGMA schema_version` 要真的开一次读事务读 page 1，
    // 于是"WAL 需要恢复但目录不可写"“文件头是 SQLite 但内容是垃圾”这两类问题
    // 都在这里暴露。留到调用方第一次 SELECT 才炸，只是把错误挪到更不方便的地方。
    if conn
        .query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
        .is_err()
    {
        return Ok(None);
    }

    Ok(Some(conn))
}

/// 按文件头魔数判断是不是 SQLite 库。读不到视为"不是"。
pub fn is_sqlite(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 16];
    f.read_exact(&mut head).is_ok() && &head == MAGIC
}

/// 库里有没有这张表。表不存在时调用方应降级，而不是让 SQL 报错。
///
/// 上游 agent 换 schema 是常态（opencode 的 `migration` 表里已经有 38 条），
/// 所以每个原生适配器在查之前都要先问一句。
///
/// 视图同样算"有"：调用方关心的是"能不能 SELECT 它"，而上游把一张表换成
/// 同名视图是一种常见的兼容做法，对读方无差别。
pub fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    // 表名走绑定参数：名字来自适配器常量，但没有理由让它有机会变成 SQL。
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
            [table],
            |r| r.get(0),
        )
        .with_context(|| format!("failed to probe table {table:?} in foreign database"))?;
    Ok(n > 0)
}

/// 取某张表的列名集合。列被上游改名/删掉时同样要能降级。
///
/// 按 `cid` 顺序返回，也就是建表时的声明顺序。表不存在时返回空 `Vec`
/// （`PRAGMA table_info` 对未知表不报错，只是没有行）。
pub fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    // `PRAGMA table_info` 的参数是标识符，不能绑定；那就先验后拼，
    // 拒绝任何不是 [A-Za-z0-9_]+ 的名字，而不是把它原样插进 SQL。
    if !is_plain_identifier(table) {
        bail!("refusing unsafe table name for PRAGMA table_info: {table:?}");
    }
    // 名字已经证明只含 [A-Za-z0-9_]，双引号只是让 `order` 这类关键字也能过。
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .with_context(|| format!("failed to prepare table_info for {table:?}"))?;
    // 列序：cid, name, type, notnull, dflt_value, pk。
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .with_context(|| format!("failed to read columns of {table:?}"))?;
    let mut out = Vec::new();
    for name in rows {
        out.push(name.with_context(|| format!("failed to read a column name of {table:?}"))?);
    }
    Ok(out)
}

/// 只认 `[A-Za-z0-9_]+`。够用：我们要读的都是上游自己建的普通表名。
fn is_plain_identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// 统计外部库里 `column = equals` 的行数（只读）。uninstall residue 的
/// dry-run 预演用：预览要说出「会删什么」，而「读都读不了」不等于
/// 「本来就没了」——所以读不到返回 `Ok(None)`，调用方据此记成 Failed
/// 而不是 NotFound。
///
/// 表/列名走 [`is_plain_identifier`] 白名单，值走参数绑定：名字不是数据，
/// 但没有理由让它有机会变成 SQL。
pub fn count_rows(db: &Path, table: &str, column: &str, equals: &str) -> Result<Option<u64>> {
    for name in [table, column] {
        if !is_plain_identifier(name) {
            bail!("refusing unsafe identifier in foreign database query: {name:?}");
        }
    }
    let Some(conn) = open(db)? else {
        return Ok(None);
    };
    if !has_table(&conn, table)? {
        bail!("foreign database has no `{table}` table: {}", db.display());
    }
    if !columns(&conn, table)?.iter().any(|c| c == column) {
        bail!(
            "foreign database table `{table}` has no `{column}` column: {}",
            db.display()
        );
    }
    let n: i64 = conn
        .query_row(
            &format!("SELECT count(*) FROM \"{table}\" WHERE \"{column}\" = ?1"),
            [equals],
            |r| r.get(0),
        )
        .with_context(|| {
            format!("failed to count rows of {table}.{column} in {}", db.display())
        })?;
    Ok(Some(n as u64))
}

/// 从外部库里按 `column = equals` 精确等值删行（uninstall residue 用）。
///
/// 写侧与读侧同一套锁策略：短 `busy_timeout`、不做重试循环——等很久说明
/// 对方正忙，让开比排队礼貌，等不到就 Err（调用方记成 Failed，别的条目
/// 照删）。
///
/// # 为什么先验形状再动刀
///
/// 这是**别人家**的库，上游换 schema 是常态。表缺了、列改名了，先问出来
/// 再报错，而不是把一条注定失败的 DELETE 交给 SQLite 去猜。标识符走白名单
/// 校验、双引号引用，值走参数绑定——**绝不拼字符串**。
///
/// 返回受影响行数：0 表示「本来就没有这一行」，调用方按 NotFound 处理。
pub fn delete_row(db: &Path, table: &str, column: &str, equals: &str) -> Result<u64> {
    for name in [table, column] {
        if !is_plain_identifier(name) {
            bail!("refusing unsafe identifier in foreign database delete: {name:?}");
        }
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open foreign database: {}", db.display()))?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;

    if !has_table(&conn, table)? {
        bail!("foreign database has no `{table}` table: {}", db.display());
    }
    if !columns(&conn, table)?.iter().any(|c| c == column) {
        bail!(
            "foreign database table `{table}` has no `{column}` column: {}",
            db.display()
        );
    }
    // 单条 DELETE 自带原子性，不需要事务。名字已过白名单，双引号只是让
    // `order` 这类关键字也能当标识符用。
    let n = conn
        .execute(
            &format!("DELETE FROM \"{table}\" WHERE \"{column}\" = ?1"),
            [equals],
        )
        .with_context(|| {
            format!("failed to delete from {table}.{column} in {}", db.display())
        })?;
    Ok(n as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个普通库，返回路径。
    fn make_db(dir: &Path, name: &str) -> std::path::PathBuf {
        let db = dir.join(name);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY, title TEXT, created_at INTEGER);
             INSERT INTO session VALUES ('s1', 'hello', 1);",
        )
        .unwrap();
        db
    }

    /// 正常库：打得开、表存在性判断正确、列名按声明顺序返回。
    #[test]
    fn opens_and_introspects_a_normal_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_db(dir.path(), "normal.db");

        let conn = open(&db).unwrap().expect("普通库应该能打开");
        assert!(has_table(&conn, "session").unwrap(), "session 表就在那儿");
        assert!(
            !has_table(&conn, "no_such_table").unwrap(),
            "不存在的表必须返回 false 而不是报错"
        );
        assert_eq!(
            columns(&conn, "session").unwrap(),
            vec!["id", "title", "created_at"],
            "列名要按建表时的声明顺序"
        );
        assert!(
            columns(&conn, "no_such_table").unwrap().is_empty(),
            "不存在的表没有列，但不是错误"
        );

        // 真的能把数据读出来，而不只是能打开。
        let title: String = conn
            .query_row("SELECT title FROM session WHERE id = 's1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, "hello");
    }

    /// 读不到的三种情况都是 `Ok(None)`：一次扫描不该因为别人家的文件整体失败。
    #[test]
    fn unreadable_inputs_are_none_not_err() {
        let dir = tempfile::tempdir().unwrap();

        let text = dir.path().join("app.log");
        std::fs::write(&text, b"just some log lines\nnot a database at all\n").unwrap();
        assert!(open(&text).unwrap().is_none(), "纯文本不是库");

        let tiny = dir.path().join("tiny");
        std::fs::write(&tiny, b"hi").unwrap();
        assert!(open(&tiny).unwrap().is_none(), "2 字节不可能是库");

        assert!(
            open(&dir.path().join("nope.db")).unwrap().is_none(),
            "路径不存在也只是读不到"
        );
    }

    /// 文件头骗过了魔数检查，内容是垃圾：必须在 open 里就暴露成 None，
    /// 而不是把错误留给调用方的第一条 SELECT。
    #[test]
    fn corrupt_file_with_valid_magic_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake.db");
        let mut bytes = MAGIC.to_vec();
        bytes.extend(std::iter::repeat_n(0xFFu8, 4096));
        std::fs::write(&fake, &bytes).unwrap();

        assert!(open(&fake).unwrap().is_none(), "坏库是读不到，不是错误");
    }

    /// 热库：WAL 里有没 checkpoint 的帧、写方连接还开着。
    /// 我们要能读到 WAL 里的数据，读完主库文件长度和 user_version 一个字节没变。
    #[test]
    fn reads_uncheckpointed_wal_without_leaving_a_trace() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("hot.db");

        // 写方保持连接不关：-wal / -shm 都活着，帧留在 WAL 里。
        let writer = Connection::open(&db).unwrap();
        writer
            .pragma_update(None, "journal_mode", "WAL")
            .expect("切 WAL");
        // 关掉自动 checkpoint，否则写够 1000 页就被搬进主库了。
        writer
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        writer.pragma_update(None, "user_version", 7i64).unwrap();
        writer
            .execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY, blob TEXT);")
            .unwrap();
        let payload = "x".repeat(2048);
        for i in 0..300 {
            writer
                .execute(
                    "INSERT INTO session VALUES (?1, ?2)",
                    (i.to_string(), &payload),
                )
                .unwrap();
        }

        let wal = dir.path().join("hot.db-wal");
        assert!(
            std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) > 0,
            "前置条件：WAL 里必须真的有没 checkpoint 的帧"
        );
        let len_before = std::fs::metadata(&db).unwrap().len();

        {
            let conn = open(&db).unwrap().expect("热库也要能读");
            assert!(has_table(&conn, "session").unwrap());
            let n: i64 = conn
                .query_row("SELECT count(*) FROM session", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 300, "WAL 里的行必须看得见");
            let uv: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(uv, 7);
        }

        // 没 checkpoint、没恢复、没改头：主库文件长度和 user_version 原样。
        assert_eq!(
            std::fs::metadata(&db).unwrap().len(),
            len_before,
            "读一遍不该让主库文件长胖——那说明我们替别人 checkpoint 了"
        );
        let uv_after: i64 = writer
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uv_after, 7, "user_version 不该被动过");
    }

    /// 拿到的句柄必须拒绝写：只读 flag + query_only 双保险。
    #[test]
    fn returned_connection_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_db(dir.path(), "ro.db");

        let conn = open(&db).unwrap().unwrap();
        assert!(
            conn.execute("INSERT INTO session VALUES ('s2', 'nope', 2)", [])
                .is_err(),
            "写语句必须当场报错"
        );
        assert!(
            conn.execute_batch("DROP TABLE session").is_err(),
            "DDL 同样要被挡住"
        );

        // 确认那条 INSERT 真的没落地。
        let n: i64 = conn
            .query_row("SELECT count(*) FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// 带引号/分号的表名一律拒绝，绝不拼进 `PRAGMA table_info`。
    #[test]
    fn columns_rejects_unsafe_table_names() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_db(dir.path(), "inject.db");
        let conn = open(&db).unwrap().unwrap();

        for bad in [
            "session\"",
            "session\"); DROP TABLE session;--",
            "session; DROP TABLE session",
            "ses sion",
            "",
        ] {
            assert!(
                columns(&conn, bad).is_err(),
                "表名 {bad:?} 必须被拒绝而不是拼进 SQL"
            );
        }

        // 被拒之后库还在：证明没有任何一条被执行。
        assert!(has_table(&conn, "session").unwrap());
        // sqlite_master 走绑定参数，脏名字只是查不到，不会报错。
        assert!(!has_table(&conn, "session\"; DROP TABLE session;--").unwrap());
    }

    /// residue 的按行删除：命中行没了、别的行还在；值走参数绑定，
    /// 表/列走白名单，脏名字连库都不打开就拒绝。
    #[test]
    fn delete_row_只删命中行_其余原样() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_db(dir.path(), "providers.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE providers(id TEXT PRIMARY KEY, enabled INTEGER);
                 INSERT INTO providers VALUES ('claude-official', 1);
                 INSERT INTO providers VALUES ('codex-official', 0);
                 INSERT INTO providers VALUES ('keeper', 1);",
            )
            .unwrap();
        }

        // 计数：命中 1 行，别的行不受影响。
        assert_eq!(
            count_rows(&db, "providers", "id", "claude-official").unwrap(),
            Some(1)
        );
        assert_eq!(
            count_rows(&db, "providers", "id", "no-such-row").unwrap(),
            Some(0)
        );

        let n = delete_row(&db, "providers", "id", "claude-official").unwrap();
        assert_eq!(n, 1, "恰好删掉一行");
        let conn = Connection::open(&db).unwrap();
        let left: Vec<String> = conn
            .prepare("SELECT id FROM providers ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(left, ["codex-official", "keeper"], "别的行必须原样还在");

        // 再删一次：0 行受影响，不是错误。
        assert_eq!(delete_row(&db, "providers", "id", "claude-official").unwrap(), 0);

        // 表不存在 → Err（不是 panic）；列不存在 → Err。
        assert!(delete_row(&db, "no_such_table", "id", "x").is_err());
        assert!(delete_row(&db, "providers", "no_such_column", "x").is_err());
        assert!(count_rows(&db, "no_such_table", "id", "x").is_err());

        // 值带引号也只能是值：永远匹配不到，而不是拼成 SQL。
        assert_eq!(
            delete_row(&db, "providers", "id", "x'; DROP TABLE providers;--").unwrap(),
            0
        );
        let n: i64 = conn
            .query_row("SELECT count(*) FROM providers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "注入值一个字都没进 SQL");
    }

    /// 只读计数与写删除共享同一道标识符闸门：脏表名/列名在拼 SQL 之前
    /// 就被拒绝，库一个字节都不动。
    #[test]
    fn count_rows_与_delete_row_拒绝脏标识符() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_db(dir.path(), "inject2.db");

        for bad in [
            "session\"",
            "session\"); DROP TABLE session;--",
            "session; DROP TABLE session",
            "ses sion",
            "",
        ] {
            assert!(count_rows(&db, bad, "id", "x").is_err(), "{bad:?}");
            assert!(delete_row(&db, bad, "id", "x").is_err(), "{bad:?}");
        }
        assert!(count_rows(&db, "session", "id\"", "x").is_err());
        assert!(delete_row(&db, "session", "id\"", "x").is_err());

        // 库毫发无损。
        let conn = open(&db).unwrap().unwrap();
        assert!(has_table(&conn, "session").unwrap());
        let n: i64 = conn
            .query_row("SELECT count(*) FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
