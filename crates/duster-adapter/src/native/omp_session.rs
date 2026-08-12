//! omp 会话解析（`~/.omp/agent/history.db`，SQLite，**活跃 WAL**）。
//!
//! 本机实测：`history.db` 344 KB 主库 + 1.2 MB 未 checkpoint 的 WAL——
//! omp 就是当前正在跑的这个 agent，它的库几乎永远是热的。
//! 这是 duster 第一次读一个**明确正在被写**的库，所以只读策略不是防御性
//! 编程，是必须条件：读它的时候不能触发 WAL 恢复、不能拿长读锁挡住对方的
//! checkpoint（挡住了它的 WAL 就会一直涨，我们等于在给用户制造问题）。
//!
//! # 实测词汇表（2026-08-11，`sqlite3 ~/.omp/agent/history.db .schema` + 抽样）
//!
//! **omp 和 opencode 是两个不相干的产品，表名一个都不重合。**
//! 整个库只有一张业务表：
//!
//! ```text
//! history(
//!   id         INTEGER PRIMARY KEY AUTOINCREMENT,
//!   prompt     TEXT NOT NULL,
//!   created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),  -- Unix 秒
//!   cwd        TEXT,
//!   session_id TEXT
//! )
//! ```
//!
//! 外加 `history_fts`（fts5 外部内容表，`content='history'`）、它的四张
//! `history_fts_*` 影子表、`sqlite_sequence`，以及触发器 `history_ai`。
//! 那些都是 omp 自己的检索设施，与 duster 无关，一律不读。
//!
//! 实测规模：567 行、97 个不同 `session_id`、`session_id` 无 NULL；
//! `created_at` 最大值 `1786432016` = 2026-08-11 07:06:56 UTC，
//! 确认单位是**秒**（opencode 那边是毫秒，两边不要抄错）。
//!
//! # 白名单取舍
//!
//! - 这张表**只存用户输入的 prompt**，没有模型回复、没有工具结果，
//!   所以正常轮次一律 [`Role::User`]。
//! - 斜杠命令是 CLI 控制动作而不是对话：实测 `/model`(53) / `/new`(40) /
//!   `/usage`(28) / `/resume`(24) / `/compact`(12) / `/login`(2) /
//!   `/skill:<名字>`。它们记 [`Role::Tool`]（进索引、不进 FTS）。
//! - 但**不能**简单地按「以 `/` 开头」判断：实测大量真实提问就是以绝对路径
//!   开头的（`/Users/laibu/Documents/…m4a 帮我解析`）。判据是
//!   [`is_bare_command`]：整条 prompt 就是**单独一个**
//!   `/[A-Za-z][A-Za-z0-9:_-]*` token，不含空白也不含第二个 `/`。
//!   带参数的调用（`/skill:plan-doc 这个 skills 属于…`）后面跟着真实内容，
//!   算用户轮次。
//! - 纯空白 prompt 不产生轮次。
//!
//! 字节区间约定与 [`super::opencode_session`] 完全相同：
//! **`byte_len == 0` 表示 `byte_off` 是源库里的行 id，不是文件偏移。**
//! 两个 SQLite 适配器共用一个哨兵，回读方只需要认一种。
//! 这里的行 id 就是 `history.id`，[`read_turn`] 拿它回读，返回值与
//! [`parse_all`] 放进 `TurnRecord.text` 的**逐字节相同**。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use duster_model::{Role, SessionMeta, TurnRecord};

// 只读打开策略与表/列探测由 opencode 适配器持有,两个 SQLite 适配器共用同一份。
// 它**不是**从 duster-index 引进来的:duster-index 在本 crate 上面,适配层
// 不能依赖索引层,所以 `duster_index::foreign` 的那套规则在 crate 内重写了一遍
// (见 `opencode_session::open_readonly` 的文档)。crate 内共用一份而不是再抄
// 第三遍——重复只该跨 crate 边界发生一次。
use super::opencode_session::{columns, has_table, open_readonly};

/// 库里的一个会话。
#[derive(Debug, Clone)]
pub struct OmpSession {
    /// 会话标识（库内主键）。实测是 `history.session_id`（UUIDv7 文本）。
    pub id: String,
    /// 工作目录。omp 的会话目录名是路径编码（`~/.omp/agent/sessions/<编码>/`），
    /// 与 Claude 一样不可逆，只能从库里的字段还原——这里取该会话第一条
    /// 非空的 `history.cwd`。
    pub cwd: Option<String>,
    pub meta: SessionMeta,
    pub turns: Vec<TurnRecord>,
}

/// 枚举 `history.db` 里的全部会话。
///
/// 返回 `(会话列表, 警告列表)`；**读不到不是错误**，同 opencode 适配器。
///
/// 硬要求与 opencode 适配器一致，另加一条**因为这个库是热的**：
/// - 自己用 `rusqlite` 只读打开（适配层不能依赖 duster-index），
///   `SQLITE_OPEN_READ_ONLY` + `PRAGMA query_only = 1`，短 `busy_timeout`；
/// - **一次取完就断**：把行读进内存再收连接，绝不在遍历中途做别的事。
///   长读锁会卡住 omp 自己的 checkpoint。
/// - 表/列先探测后查询；缺表返回空 + warning，不报错。
/// - schema 已实测（见模块文档），表名 `history`，**与 opencode 无任何关系**。
pub fn parse_all(db: &Path) -> Result<(Vec<OmpSession>, Vec<String>)> {
    let mut warnings = Vec::new();
    let Some(conn) = open_readonly(db) else {
        warnings.push(format!(
            "omp session database is not readable, skipped: {}",
            db.display()
        ));
        return Ok((Vec::new(), warnings));
    };

    let out = collect_all(&conn, &mut warnings);
    // 一次取完就断:collect_all 里的 Statement 已经 drop,这里连连接一起放掉,
    // 让 omp 自己的 checkpoint 立刻能推进。
    drop(conn);

    match out {
        Ok(sessions) => Ok((sessions, warnings)),
        Err(e) => {
            warnings.push(format!(
                "omp session database could not be parsed, skipped: {} ({e:#})",
                db.display()
            ));
            Ok((Vec::new(), warnings))
        }
    }
}

/// 一条 SELECT 全量读进内存,再在 Rust 里按 `session_id` 分桶。
fn collect_all(conn: &rusqlite::Connection, warnings: &mut Vec<String>) -> Result<Vec<OmpSession>> {
    if !has_table(conn, "history")? {
        warnings.push(
            "omp session database has no `history` table (upstream schema changed?), skipped"
                .to_string(),
        );
        return Ok(Vec::new());
    }
    let cols = columns(conn, "history")?;
    let has = |c: &str| cols.iter().any(|x| x == c);
    for c in ["id", "prompt", "session_id"] {
        if !has(c) {
            warnings.push(format!(
                "omp session database is missing `history.{c}` \
                 (upstream schema changed?), skipped"
            ));
            return Ok(Vec::new());
        }
    }
    // created_at / cwd 是"有就用":缺了只是少了时间戳和工作目录,轮次照出。
    let ts_sel = if has("created_at") {
        "\"created_at\""
    } else {
        "NULL"
    };
    let cwd_sel = if has("cwd") { "\"cwd\"" } else { "NULL" };

    let sql = format!(
        "SELECT \"id\", \"session_id\", \"prompt\", {ts_sel}, {cwd_sel} \
         FROM \"history\" ORDER BY \"id\""
    );
    let mut stmt = conn.prepare(&sql).context("prepare history listing")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .context("query history listing")?;

    let mut sessions: BTreeMap<String, OmpSession> = BTreeMap::new();
    let mut orphans = 0usize;
    for row in rows {
        let (id, sid, prompt, ts_s, cwd) = row.context("read a history row")?;
        // `session_id` 可空(实测本机 0 条)。归不到会话的 prompt 没法进
        // 「一会话一行」的索引模型,只能丢——但要让用户看见丢了多少。
        let Some(sid) = sid.filter(|s| !s.is_empty()) else {
            orphans += 1;
            continue;
        };
        if prompt.trim().is_empty() {
            continue;
        }
        let s = sessions.entry(sid.clone()).or_insert_with(|| OmpSession {
            id: sid,
            cwd: None,
            meta: SessionMeta {
                cwd: None,
                title: None,
                turn_count: 0,
            },
            turns: Vec::new(),
        });
        if s.cwd.is_none() {
            s.cwd = cwd.filter(|c| !c.trim().is_empty());
        }
        s.turns.push(TurnRecord {
            seq: s.turns.len() as u32,
            role: if is_bare_command(&prompt) {
                Role::Tool
            } else {
                Role::User
            },
            // created_at 是 Unix **秒**,索引层统一用毫秒。
            ts_ms: ts_s.map(|t| t.saturating_mul(1000)),
            // 哨兵:byte_len == 0 ⇒ byte_off 是源库行 id,不是文件偏移。
            byte_off: id as u64,
            byte_len: 0,
            text: prompt,
        });
    }
    if orphans > 0 {
        warnings.push(format!(
            "omp session database has {orphans} history row(s) with no session id, skipped"
        ));
    }

    let mut out: Vec<OmpSession> = sessions.into_values().collect();
    for s in &mut out {
        s.meta = SessionMeta {
            cwd: s.cwd.clone(),
            title: None, // omp 的 history 表没有标题列。
            turn_count: s.turns.len() as u32,
        };
    }
    Ok(out)
}

/// 按 `history.id` 回读一条轮次的正文。
///
/// 与 [`parse_all`] 返回的 `TurnRecord.text` 逐字节相同：两边都原样取
/// `prompt` 列，不做任何裁剪或改写。
///
/// 与 [`parse_all`] 不同，这里读不到就是错误：调用方指名要这一条。
pub fn read_turn(db: &Path, rowid: i64) -> Result<String> {
    let Some(conn) = open_readonly(db) else {
        bail!("omp session database is not readable: {}", db.display());
    };
    if !has_table(&conn, "history")? {
        bail!(
            "omp session database has no `history` table: {}",
            db.display()
        );
    }
    let text: String = conn
        .query_row(
            "SELECT \"prompt\" FROM \"history\" WHERE \"id\" = ?1",
            [rowid],
            |r| r.get(0),
        )
        .with_context(|| format!("no omp history row {rowid} in {}", db.display()))?;
    drop(conn);
    Ok(text)
}

/// 整条 prompt 就是一个光杆斜杠命令吗？
///
/// 只认「`/` + 字母开头 + 只含 `[A-Za-z0-9:_-]`」且**没有第二个 token**。
/// 这样 `/model`、`/skill:plan-doc` 是控制动作，而
/// `/Users/laibu/…/x.md 帮我改一下` 和 `/skill:plan-doc 帮我分析…`
/// 是真实用户输入——实测两类都大量存在，判据必须能分开它们。
fn is_bare_command(prompt: &str) -> bool {
    let s = prompt.trim();
    let Some(rest) = s.strip_prefix('/') else {
        return false;
    };
    let mut chars = rest.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))
}

/// 造一个最小的 omp 形状的库，给**跨 crate**的测试当夹具。
///
/// 与 [`super::opencode_session::fixture_db`] 同签名，方便调用方按 mapper
/// 切换。`turns` 的 role 一栏**被忽略**：omp 的 `history` 表里没有角色列，
/// 每一行都是用户 prompt（角色由 [`is_bare_command`] 推出来）。
/// 返回各轮次的 `history.id`，也就是 `TurnRecord.byte_off` 与
/// [`read_turn`] 的入参。
#[doc(hidden)]
pub fn fixture_db(path: &Path, session_id: &str, turns: &[(&str, &str)]) -> Result<Vec<i64>> {
    let conn = rusqlite::Connection::open(path)
        .with_context(|| format!("failed to create fixture db: {}", path.display()))?;
    conn.execute_batch(
        "CREATE TABLE history (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             prompt TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             cwd TEXT,
             session_id TEXT
         );",
    )
    .context("failed to create fixture tables")?;

    let mut ids = Vec::with_capacity(turns.len());
    for (i, (_role, text)) in turns.iter().enumerate() {
        conn.execute(
            "INSERT INTO history(prompt, created_at, cwd, session_id) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                text,
                1_785_755_000i64 + i as i64,
                "/tmp/fixture",
                session_id
            ],
        )
        .context("failed to insert fixture history row")?;
        ids.push(conn.last_insert_rowid());
    }
    conn.close()
        .map_err(|(_, e)| e)
        .context("close fixture db")?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    /// 造一个接近真库的样本：两个会话、斜杠命令、路径开头的真实提问。
    fn build(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE history (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 prompt TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
                 cwd TEXT,
                 session_id TEXT
             );
             CREATE VIRTUAL TABLE history_fts USING fts5(
                 prompt, content='history', content_rowid='id');",
        )
        .unwrap();
        let ins = |prompt: &str, ts: i64, cwd: Option<&str>, sid: Option<&str>| {
            conn.execute(
                "INSERT INTO history(prompt, created_at, cwd, session_id) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![prompt, ts, cwd, sid],
            )
            .unwrap();
        };
        ins("检查一下更新脚本", 1_785_810_460, None, Some("ses-1"));
        ins("/model", 1_785_810_466, Some("/tmp/one"), Some("ses-1"));
        ins(
            "/Users/laibu/x.md 帮我改一下",
            1_785_810_508,
            Some("/tmp/one"),
            Some("ses-1"),
        );
        ins("   ", 1_785_810_509, Some("/tmp/one"), Some("ses-1")); // 空白,不出轮次
        ins(
            "另一个会话的第一句",
            1_785_811_000,
            Some("/tmp/two"),
            Some("ses-2"),
        );
        ins("没有会话归属", 1_785_811_001, None, None); // orphan
        conn.close().unwrap();
    }

    #[test]
    fn 两个会话_顺序_角色归一_哨兵为零() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("history.db");
        build(&db);

        let (sessions, warnings) = parse_all(&db).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(warnings.len(), 1, "无 session_id 的那行要报出来");
        assert!(warnings[0].contains("1 history row(s)"), "{warnings:?}");

        let a = &sessions[0];
        assert_eq!(a.id, "ses-1");
        assert_eq!(a.meta.turn_count, 3, "纯空白那行不算轮次");
        assert_eq!(a.cwd.as_deref(), Some("/tmp/one"), "取首个非空 cwd");
        assert_eq!(a.meta.title, None, "history 表没有标题列");

        assert_eq!(a.turns[0].role, Role::User);
        assert_eq!(a.turns[0].text, "检查一下更新脚本");
        assert_eq!(a.turns[0].seq, 0);
        // created_at 是秒,索引层要毫秒。
        assert_eq!(a.turns[0].ts_ms, Some(1_785_810_460_000));

        assert_eq!(a.turns[1].role, Role::Tool, "光杆斜杠命令是控制动作");
        assert_eq!(a.turns[2].role, Role::User, "以路径开头的是真实提问");

        for t in sessions.iter().flat_map(|s| &s.turns) {
            assert_eq!(t.byte_len, 0);
            assert!(t.byte_off > 0);
        }

        assert_eq!(sessions[1].id, "ses-2");
        assert_eq!(sessions[1].turns.len(), 1);
    }

    #[test]
    fn read_turn_与解析结果逐字节相同() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("history.db");
        build(&db);

        let (sessions, _) = parse_all(&db).unwrap();
        let mut checked = 0;
        for s in &sessions {
            for t in &s.turns {
                assert_eq!(read_turn(&db, t.byte_off as i64).unwrap(), t.text);
                checked += 1;
            }
        }
        assert_eq!(checked, 4);
        assert!(read_turn(&db, 9999).is_err(), "不存在的行是错误");
    }

    #[test]
    fn 缺表与非库文件_降级成警告而不是错误() {
        let dir = tempfile::tempdir().unwrap();

        let empty = dir.path().join("empty.db");
        Connection::open(&empty)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated(x)")
            .unwrap();
        let (s, w) = parse_all(&empty).unwrap();
        assert!(s.is_empty());
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("`history` table"), "{w:?}");

        let fake = dir.path().join("text.db");
        std::fs::write(&fake, b"plain text pretending to be a database").unwrap();
        let (s, w) = parse_all(&fake).unwrap();
        assert!(s.is_empty());
        assert!(w[0].contains("not readable"), "{w:?}");

        let (s, w) = parse_all(&dir.path().join("nope.db")).unwrap();
        assert!(s.is_empty());
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn 列被上游改名_降级成警告() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("renamed.db");
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE history(id INTEGER PRIMARY KEY, text TEXT, chat TEXT)")
            .unwrap();
        let (s, w) = parse_all(&db).unwrap();
        assert!(s.is_empty());
        assert!(w[0].contains("history.prompt"), "{w:?}");
    }

    /// omp 的库天生是热的:写方连着、WAL 没 checkpoint,读方照样解析。
    #[test]
    fn 活跃wal下可读且不改动主库() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("hot.db");
        build(&db);

        let writer = Connection::open(&db).unwrap();
        let mode: String = writer
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        writer
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        writer
            .execute(
                "INSERT INTO history(prompt, created_at, cwd, session_id) \
                 VALUES ('热库里的一句', 1785900000, '/tmp/two', 'ses-2')",
                [],
            )
            .unwrap();

        let before = std::fs::metadata(&db).unwrap().len();
        assert!(db.with_file_name("hot.db-wal").exists(), "WAL 应该已经落地");

        let (sessions, warnings) = parse_all(&db).unwrap();
        assert_eq!(warnings.len(), 1, "只有 orphan 那一条警告: {warnings:?}");
        let b = sessions.iter().find(|s| s.id == "ses-2").unwrap();
        assert_eq!(b.turns.len(), 2, "未 checkpoint 的帧也要读到");
        assert_eq!(b.turns[1].text, "热库里的一句");

        assert_eq!(std::fs::metadata(&db).unwrap().len(), before);
        drop(writer);
    }

    #[test]
    fn 斜杠命令判据() {
        for s in ["/model", "/new", "/usage", "/skill:plan-doc", " /compact "] {
            assert!(is_bare_command(s), "{s} 应判为控制命令");
        }
        for s in [
            "/Users/laibu/x.md 帮我改",
            "/skill:plan-doc 帮我分析",
            "/",
            "/1abc",
            "普通提问",
            "",
        ] {
            assert!(!is_bare_command(s), "{s:?} 不该判为控制命令");
        }
    }

    #[test]
    fn fixture_db_可被自己解析() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("fx.db");
        let ids = fixture_db(&db, "ses_fx", &[("user", "问"), ("user", "再问")]).unwrap();
        assert_eq!(ids, vec![1, 2]);

        let (sessions, w) = parse_all(&db).unwrap();
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].turns.len(), 2);
        assert_eq!(sessions[0].turns[0].byte_off, 1);
        assert_eq!(read_turn(&db, ids[1]).unwrap(), "再问");
    }
}
