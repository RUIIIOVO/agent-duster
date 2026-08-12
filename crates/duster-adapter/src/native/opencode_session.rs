//! opencode 会话解析（`~/.local/share/opencode/opencode.db`，SQLite）。
//!
//! 实测（2026-08-11，opencode 1.18.13，本机真库）：库里 27 个 `session`、65 条
//! `message`、178 条 `part`，`migration` 表已有 38 条——**上游 schema 在动**，
//! 所以每一步都要先问「这张表/这一列还在吗」再查。
//!
//! # 实测词汇表（`sqlite3 opencode.db .schema` + 逐列抽样）
//!
//! 只列本适配器读的三张表，库里另有 20+ 张与会话无关（`project` /
//! `workspace` / `account` / `credential` / `event` / `todo` / `migration` …）。
//!
//! - `session(id TEXT PK, project_id, workspace_id, parent_id, slug, directory
//!   TEXT NOT NULL, path, title TEXT NOT NULL, version, agent, model,
//!   time_created, time_updated, …)`
//!   —— `directory` 就是会话发生的工作目录（本机样本 `/private/tmp/p2p`、
//!   `/Users/laibu/Documents/Code/lb-ai-model`），**不需要**绕 `project_id`
//!   去 join `project.worktree`；`title` 是人可读标题（`候选产品评审打分`，
//!   没标题时上游填 `New session - <ISO 时间>`）。
//! - `message(id TEXT PK, session_id, time_created, time_updated, data TEXT)`
//!   —— 正文不在列里，`data` 是一整块 JSON。实测 `data.role` 只有两个值：
//!   `user`(27) / `assistant`(38)。`time_created` 是 **Unix 毫秒**
//!   （样本 `1786030494762`）。
//! - `part(id TEXT PK, message_id, session_id, time_created, time_updated,
//!   data TEXT)` —— 一条 message 拆成多段，`data.type` 实测六种：
//!   `text`(49) / `reasoning`(35) / `tool`(20) / `patch`(5) /
//!   `step-start`(36) / `step-finish`(33)。
//!   `text`/`reasoning` 带 `data.text`；`tool` 带 `data.tool`（工具名）、
//!   `data.callID`、`data.state{status,input,output,metadata,title,time,error}`；
//!   `patch` 只有 `hash` + `files`；`step-*` 只有 `snapshot`/`tokens`/`cost`。
//! - id 是有序的：`part.id` / `message.id` 的字典序与 `time_created` 顺序一致
//!   （上游用的是单调递增 id），上游自己的索引也是
//!   `(session_id, time_created, id)` 与 `(message_id, id)`，本适配器照抄这个排序。
//!
//! # 白名单取舍
//!
//! - **一条 message 一个轮次**，正文由它名下的 `part` 拼成，
//!   `byte_off` 取 message 的 rowid（见下节），所以粒度不能再细。
//! - 对话正文只认 `type == "text"` 的 part，按 `part.id` 升序、去首尾空白后
//!   用 `\n` 连接；`role` 由 `message.data.role` 白名单归一到
//!   `Role::{User, Assistant}`，认不出的 role 归 `Role::Tool`。
//! - 一条 message 没有任何 `text` part、却有 `reasoning`/`tool` 内容时，
//!   仍然出一个 `Role::Tool` 轮次（正文是这些辅助内容的紧凑渲染）：
//!   模型的思考与工具调用不是对话，但也不该从索引里凭空消失——
//!   `Role::Tool` 正是「进索引、不进 FTS」这一档。
//! - `patch` / `step-start` / `step-finish` 整类丢弃：它们只有快照哈希、
//!   token 计数和文件名，没有一个字是可检索正文。
//! - 拼完仍为空的 message（纯 step-* 消息）不产生轮次。
//!
//! # 与 jsonl 适配器的根本区别：没有字节区间
//!
//! Claude / Codex 的轮次是文件里的一段字节，`byte_off`/`byte_len` 指过去就能
//! 回读原文。SQLite 里没有这种东西：正文在列里。
//!
//! 约定（**全 workspace 共用，改这里等于改契约**）：
//! **`byte_len == 0` 表示 `byte_off` 不是文件偏移，而是源库里的行 id。**
//! 真实轮次的正文长度永远大于 0，所以这个哨兵不会和文件型会话撞车。
//! 回读方（`duster open` / `session show`）见到 `byte_len == 0` 就改走
//! 「按行 id 回源库查」这条路，而不是 seek。
//!
//! 这么做而不是给 schema 加一列 locator：会话回读只有两种源（文件、库），
//! 一个哨兵讲得清；加一列则每个写入方都要想一次填什么，而其中 99% 是文件。
//!
//! 本适配器里 `byte_off` = `message` 表的 rowid，[`read_turn`] 拿它回读，
//! 且回读出来的字符串与 [`parse_all`] 放进 `TurnRecord.text` 的**逐字节相同**。

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use duster_model::{Role, SessionMeta, TurnRecord};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

/// 等锁上限。别人正在写就让开，排队没有意义。
pub(crate) const BUSY_TIMEOUT_MS: u64 = 500;

/// SQLite 文件头魔数。
const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// 库里的一个会话。opencode 一个库装所有会话，所以解析的粒度是
/// 「一个 session 行」而不是「一个文件」。
#[derive(Debug, Clone)]
pub struct OpencodeSession {
    /// `session.id`。
    pub id: String,
    /// 会话标题（`session.title`，可缺）。
    pub title: Option<String>,
    /// 工作目录：实测直接来自 `session.directory`（NOT NULL 列），
    /// 不必绕 `project_id` join `project`/`workspace`。
    pub cwd: Option<String>,
    pub meta: SessionMeta,
    pub turns: Vec<TurnRecord>,
}

/// 枚举库里的全部会话并解析各自的轮次。
///
/// 返回 `(会话列表, 警告列表)`。**读不到不是错误**：打不开、不是 SQLite、
/// 表被上游改名——一律返回空列表 + 一条 warning，让一次 `duster scan`
/// 不会因为某一家 agent 的库而整体失败。
///
/// 硬要求：
/// - 走 `duster_index::foreign::open`？**不行**——那是 duster-index，
///   本 crate 在它下面。这里自己用 `rusqlite` 只读打开（[`open_readonly`]），
///   规则照抄：`SQLITE_OPEN_READ_ONLY` + `query_only`、短 `busy_timeout`、
///   不开长事务。（分层就是这个代价：宁可两处各写一遍打开策略，
///   也不让适配层依赖索引层。）
/// - 查之前先用 `sqlite_master` 确认 `session` / `message` / `part` 三张表都在，
///   缺任何一张就返回空并附 warning——上游改名不该让扫描失败。
/// - 轮次正文来自 `part`（一条 message 拆成多段），按 `message_id` 聚合、
///   按 `part.id` 升序拼接；`role` 从 `message.data.role` 白名单归一到
///   `Role::{User, Assistant}`，其余（工具调用、模型思考等）归 `Role::Tool`。
/// - 每条轮次的 `byte_off` = 该 message 的 rowid，`byte_len` = 0（见模块文档）。
/// - `ts_ms` 取 message 的时间列（列名随上游变，用
///   `time_created` / `created_at` / `time` 依次探测，都没有就 None）。
pub fn parse_all(db: &Path) -> Result<(Vec<OpencodeSession>, Vec<String>)> {
    let mut warnings = Vec::new();
    let Some(conn) = open_readonly(db) else {
        warnings.push(format!(
            "opencode session database is not readable, skipped: {}",
            db.display()
        ));
        return Ok((Vec::new(), warnings));
    };

    // 一次取完就断:collect_all 内部所有 Statement 都在它返回前 drop,
    // 这里再显式关掉连接,读锁的存活时间以毫秒计。
    let out = collect_all(&conn, &mut warnings);
    drop(conn);

    match out {
        Ok(sessions) => Ok((sessions, warnings)),
        Err(e) => {
            // 探测过了还是查失败:上游把表换成了我们读不懂的形状。
            // 这仍然只是"这一家读不到",不是整次扫描的错误。
            warnings.push(format!(
                "opencode session database could not be parsed, skipped: {} ({e:#})",
                db.display()
            ));
            Ok((Vec::new(), warnings))
        }
    }
}

/// 三张表读进内存 → 在 Rust 里 join。库是几 MB 量级，全量载入比反复回查便宜，
/// 更重要的是能让所有 `Statement` 在函数返回前统一 drop，不留长读锁。
fn collect_all(conn: &Connection, warnings: &mut Vec<String>) -> Result<Vec<OpencodeSession>> {
    for t in ["session", "message", "part"] {
        if !has_table(conn, t)? {
            warnings.push(format!(
                "opencode session database has no `{t}` table (upstream schema changed?), skipped"
            ));
            return Ok(Vec::new());
        }
    }

    let scols = columns(conn, "session")?;
    let mcols = columns(conn, "message")?;
    let pcols = columns(conn, "part")?;
    for (table, cols, need) in [
        ("session", &scols, &["id"][..]),
        ("message", &mcols, &["id", "session_id", "data"][..]),
        ("part", &pcols, &["id", "message_id", "data"][..]),
    ] {
        for c in need {
            if !cols.iter().any(|x| x == c) {
                warnings.push(format!(
                    "opencode session database is missing `{table}.{c}` \
                     (upstream schema changed?), skipped"
                ));
                return Ok(Vec::new());
            }
        }
    }

    // 会话:title / directory 是"有就用"的锦上添花,缺了不影响轮次。
    let has = |cols: &[String], c: &str| cols.iter().any(|x| x == c);
    let title_sel = if has(&scols, "title") {
        "\"title\""
    } else {
        "NULL"
    };
    let dir_sel = if has(&scols, "directory") {
        "\"directory\""
    } else {
        "NULL"
    };
    let mut sessions: BTreeMap<String, OpencodeSession> = BTreeMap::new();
    {
        let sql = format!("SELECT \"id\", {title_sel}, {dir_sel} FROM \"session\"");
        let mut stmt = conn.prepare(&sql).context("prepare session listing")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .context("query session listing")?;
        for row in rows {
            let (id, title, dir) = row.context("read a session row")?;
            sessions.insert(
                id.clone(),
                OpencodeSession {
                    id,
                    title: non_empty(title),
                    cwd: non_empty(dir),
                    meta: SessionMeta {
                        cwd: None,
                        title: None,
                        turn_count: 0,
                    },
                    turns: Vec::new(),
                },
            );
        }
    }

    // part 先按 (message_id, id) 取全量,再按 message_id 分桶。
    // 桶内顺序即查询顺序,也就是 part.id 升序。
    let mut parts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT \"message_id\", \"data\" FROM \"part\" ORDER BY \"message_id\", \"id\"",
            )
            .context("prepare part listing")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .context("query part listing")?;
        for row in rows {
            let (mid, data) = row.context("read a part row")?;
            parts.entry(mid).or_default().push(data);
        }
    }

    // message 的时间列名随上游变,只在固定白名单里挑,拼进 SQL 的永远是常量。
    let tcol = ["time_created", "created_at", "time"]
        .into_iter()
        .find(|c| has(&mcols, c));
    let (time_sel, order) = match tcol {
        Some(c) => (format!("\"{c}\""), format!("\"{c}\", \"id\"")),
        None => ("NULL".to_string(), "\"id\"".to_string()),
    };
    let sql = format!(
        "SELECT rowid, \"id\", \"session_id\", \"data\", {time_sel} \
         FROM \"message\" ORDER BY {order}"
    );
    let mut stmt = conn.prepare(&sql).context("prepare message listing")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })
        .context("query message listing")?;

    for row in rows {
        let (rowid, mid, sid, data, ts_ms) = row.context("read a message row")?;
        // session 表里没有的 message 是孤儿(上游删会话时的残留),跳过。
        let Some(session) = sessions.get_mut(&sid) else {
            continue;
        };
        let role = serde_json::from_str::<Value>(&data)
            .ok()
            .and_then(|v| v.get("role").and_then(Value::as_str).map(role_of))
            // data 不是 JSON 或没有 role:内容还在,但说不清是谁说的 ⇒ Tool。
            .unwrap_or(Role::Tool);
        let empty: Vec<String> = Vec::new();
        let (text, aux_only) = render_parts(parts.get(&mid).unwrap_or(&empty));
        if text.is_empty() {
            continue; // 纯 step-start/step-finish 的消息,没有可检索正文。
        }
        session.turns.push(TurnRecord {
            seq: session.turns.len() as u32,
            role: if aux_only { Role::Tool } else { role },
            ts_ms,
            // 哨兵:byte_len == 0 ⇒ byte_off 是源库行 id,不是文件偏移。
            byte_off: rowid as u64,
            byte_len: 0,
            text,
        });
    }

    let mut out: Vec<OpencodeSession> = sessions.into_values().collect();
    for s in &mut out {
        s.meta = SessionMeta {
            cwd: s.cwd.clone(),
            title: s.title.clone(),
            turn_count: s.turns.len() as u32,
        };
    }
    Ok(out)
}

/// 按 message rowid 回读一条轮次的正文。`duster open` / `session show`
/// 在 `byte_len == 0` 时走这条。
///
/// 走的是与 [`parse_all`] **同一套**渲染（[`render_parts`]），所以返回值与
/// 索引里那条轮次的 `text` 逐字节相同——回读路径的整个契约就压在这一点上。
///
/// 与 [`parse_all`] 不同，这里读不到就是错误：调用方指名要这一条，
/// 给个空串会被当成"这轮什么都没说"。
pub fn read_turn(db: &Path, message_rowid: i64) -> Result<String> {
    let Some(conn) = open_readonly(db) else {
        bail!(
            "opencode session database is not readable: {}",
            db.display()
        );
    };
    for t in ["message", "part"] {
        if !has_table(&conn, t)? {
            bail!(
                "opencode session database has no `{t}` table: {}",
                db.display()
            );
        }
    }

    let mid: String = conn
        .query_row(
            "SELECT \"id\" FROM \"message\" WHERE rowid = ?1",
            [message_rowid],
            |r| r.get(0),
        )
        .with_context(|| {
            format!(
                "no opencode message at row {message_rowid} in {}",
                db.display()
            )
        })?;

    let mut datas: Vec<String> = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT \"data\" FROM \"part\" WHERE \"message_id\" = ?1 ORDER BY \"id\"")
            .context("prepare part re-read")?;
        let rows = stmt
            .query_map([&mid], |r| r.get::<_, String>(0))
            .context("query part re-read")?;
        for row in rows {
            datas.push(row.context("read a part row")?);
        }
    }
    drop(conn);

    Ok(render_parts(&datas).0)
}

/// 把一条 message 名下的 part JSON 列表渲染成轮次正文。
///
/// 返回 `(正文, 是否只有辅助内容)`。第二个值为 true 表示这条 message 没有
/// 任何 `text` part，正文是 `reasoning`/`tool` 的渲染，调用方应把它记成
/// [`Role::Tool`]。
fn render_parts(parts: &[String]) -> (String, bool) {
    let mut body: Vec<String> = Vec::new();
    let mut aux: Vec<String> = Vec::new();
    for raw in parts {
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            continue; // 坏 JSON 跳过,不影响同一条消息的其他段。
        };
        match v.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = v.get("text").and_then(Value::as_str) {
                    let t = t.trim();
                    if !t.is_empty() {
                        body.push(t.to_string());
                    }
                }
            }
            Some("reasoning") => {
                if let Some(t) = v.get("text").and_then(Value::as_str) {
                    let t = t.trim();
                    if !t.is_empty() {
                        aux.push(t.to_string());
                    }
                }
            }
            Some("tool") => {
                let name = v.get("tool").and_then(Value::as_str).unwrap_or("?");
                // state.output 是工具回显;没有就退到 state.title(人可读摘要)。
                let out = v
                    .pointer("/state/output")
                    .and_then(Value::as_str)
                    .or_else(|| v.pointer("/state/title").and_then(Value::as_str))
                    .unwrap_or("")
                    .trim();
                if out.is_empty() {
                    aux.push(format!("[tool: {name}]"));
                } else {
                    aux.push(format!("[tool: {name}] {out}"));
                }
            }
            // patch / step-start / step-finish / 未知类型:只有哈希、token
            // 计数和文件名,没有可检索正文,整类丢弃。
            _ => {}
        }
    }
    if body.is_empty() {
        (aux.join("\n"), true)
    } else {
        (body.join("\n"), false)
    }
}

/// `message.data.role` 归一。实测只有 `user` / `assistant` 两个值，
/// 其余（上游将来可能加的 `system` / `tool` 等）一律 [`Role::Tool`]。
fn role_of(s: &str) -> Role {
    match s {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => Role::Tool,
    }
}

/// 空串当没有：`session.title` / `session.directory` 是 NOT NULL 列，
/// 上游没值时填的是 `''` 而不是 NULL。
fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|x| !x.trim().is_empty())
}

/// 只读打开一个 agent 自家的 SQLite 库。读不到返回 `None`（不是错误）。
///
/// # 为什么这段代码在这里再写一遍
///
/// `duster_index::foreign::open` 是同一套策略的规范实现，但 duster-index 在
/// 本 crate **上面**（duster-model < duster-fs < duster-index < duster-adapter），
/// 引它就把分层倒过来了。两处各写一遍打开策略，是为了不让适配层依赖索引层——
/// 这份重复是**故意的**，改其中一处时另一处要跟着改。
///
/// 策略（与 `foreign.rs` 逐条对应）：
/// - 先看魔数，省掉给每个非库文件开一次连接；
/// - `SQLITE_OPEN_READ_ONLY`，绝不带 `CREATE`（别人的目录里不该多出一个空库）；
/// - `PRAGMA query_only = 1` 兜底，走错的写语句当场报错而不是悄悄成功；
/// - `busy_timeout` 给 [`BUSY_TIMEOUT_MS`] 这么短，**不做重试循环**：
///   等很久说明对方正忙，让开比排队礼貌；
/// - `PRAGMA schema_version` 真的开一次读事务读 page 1，把"WAL 需要恢复但
///   目录不可写""文件头对但内容是垃圾"这两类问题在这里暴露，而不是留给
///   调用方第一次 SELECT。
///
/// # 调用方的义务：拿了就走
///
/// opencode / omp 的库在 agent 运行时是热的。只要一条 `Statement` 活着，
/// SQLite 就持有读锁，对方就 checkpoint 不了、`-wal` 会一直涨。
/// **prepare → 一次性取完 → 立刻 drop**，不要跨 I/O、不要把 `Rows` 存起来。
pub(crate) fn open_readonly(path: &Path) -> Option<Connection> {
    if !is_sqlite(path) {
        return None;
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .ok()?;
    conn.pragma_update(None, "query_only", true).ok()?;
    conn.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
        .ok()?;
    Some(conn)
}

/// 按文件头魔数判断是不是 SQLite 库。读不到视为"不是"。
pub(crate) fn is_sqlite(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 16];
    f.read_exact(&mut head).is_ok() && &head == MAGIC
}

/// 库里有没有这张表（视图同样算有）。表名走绑定参数，不拼 SQL。
///
/// 上游换 schema 是常态（opencode 的 `migration` 表里已经有 38 条），
/// 所以每个查询之前都要先问一句。
pub(crate) fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
            [table],
            |r| r.get(0),
        )
        .with_context(|| format!("failed to probe table {table:?} in foreign database"))?;
    Ok(n > 0)
}

/// 取某张表的列名集合，按 `cid`（建表声明）顺序。
///
/// `PRAGMA table_info` 的参数是标识符不能绑定，所以先验后拼：
/// 拒绝任何不是 `[A-Za-z0-9_]+` 的名字。表不存在时返回空 `Vec`。
pub(crate) fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    if !table
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || table.is_empty()
    {
        bail!("refusing unsafe table name for PRAGMA table_info: {table:?}");
    }
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .with_context(|| format!("failed to prepare table_info for {table:?}"))?;
    // 列序:cid, name, type, notnull, dflt_value, pk。
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .with_context(|| format!("failed to read columns of {table:?}"))?;
    let mut out = Vec::new();
    for name in rows {
        out.push(name.with_context(|| format!("failed to read a column name of {table:?}"))?);
    }
    Ok(out)
}

/// 造一个最小的 opencode 形状的库，给**跨 crate**的测试当夹具。
///
/// 不加 `#[cfg(test)]`：`cfg(test)` 不跨 crate 传播，duster-core 的
/// `session show` 测试需要一个能被 [`parse_all`] / [`read_turn`] 接受的库，
/// 而它不该、也不能复制本模块对上游 schema 的认知。
///
/// `turns` 是 `(role, text)`；role 直接写进 `message.data.role`。
/// 返回各轮次的 message rowid，按传入顺序——它们就是 `TurnRecord.byte_off`，
/// 也是 [`read_turn`] 的入参。
#[doc(hidden)]
pub fn fixture_db(path: &Path, session_id: &str, turns: &[(&str, &str)]) -> Result<Vec<i64>> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to create fixture db: {}", path.display()))?;
    conn.execute_batch(
        "CREATE TABLE \"session\" (\"id\" TEXT PRIMARY KEY, \"title\" TEXT NOT NULL, \
           \"directory\" TEXT NOT NULL);
         CREATE TABLE \"message\" (\"id\" TEXT PRIMARY KEY, \"session_id\" TEXT NOT NULL, \
           \"time_created\" INTEGER NOT NULL, \"data\" TEXT NOT NULL);
         CREATE TABLE \"part\" (\"id\" TEXT PRIMARY KEY, \"message_id\" TEXT NOT NULL, \
           \"session_id\" TEXT NOT NULL, \"data\" TEXT NOT NULL);",
    )
    .context("failed to create fixture tables")?;
    conn.execute(
        "INSERT INTO \"session\"(\"id\",\"title\",\"directory\") VALUES (?1, ?2, ?3)",
        rusqlite::params![session_id, "fixture session", "/tmp/fixture"],
    )
    .context("failed to insert fixture session")?;

    let mut rowids = Vec::with_capacity(turns.len());
    // message 是普通 rowid 表,插入顺序即 rowid 顺序;但顺序是隐含约定,
    // 这里如实取回而不是假设 1..=n。
    for (i, (role, text)) in turns.iter().enumerate() {
        // id 用零填充,保证字典序与插入顺序一致(上游的真实 id 也有这个性质)。
        let mid = format!("msg_{i:04}");
        conn.execute(
            "INSERT INTO \"message\"(\"id\",\"session_id\",\"time_created\",\"data\") \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                mid,
                session_id,
                1_786_030_000_000i64 + i as i64,
                serde_json::json!({ "role": role }).to_string(),
            ],
        )
        .context("failed to insert fixture message")?;
        rowids.push(conn.last_insert_rowid());
        conn.execute(
            "INSERT INTO \"part\"(\"id\",\"message_id\",\"session_id\",\"data\") \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                format!("prt_{i:04}_0"),
                mid,
                session_id,
                serde_json::json!({ "type": "text", "text": text }).to_string(),
            ],
        )
        .context("failed to insert fixture part")?;
    }
    conn.close()
        .map_err(|(_, e)| e)
        .context("close fixture db")?;
    Ok(rowids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// 造一个更接近真库的样本：两个会话、多段 message、tool/step 噪声。
    fn build(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE `session` (`id` text PRIMARY KEY, `title` text NOT NULL,
               `directory` text NOT NULL);
             CREATE TABLE `message` (`id` text PRIMARY KEY, `session_id` text NOT NULL,
               `time_created` integer NOT NULL, `data` text NOT NULL);
             CREATE TABLE `part` (`id` text PRIMARY KEY, `message_id` text NOT NULL,
               `session_id` text NOT NULL, `data` text NOT NULL);",
        )
        .unwrap();
        for (sid, title, dir) in [("ses_a", "第一个会话", "/tmp/a"), ("ses_b", "", "/tmp/b")] {
            conn.execute(
                "INSERT INTO `session` VALUES (?1, ?2, ?3)",
                params![sid, title, dir],
            )
            .unwrap();
        }
        let msg = |mid: &str, sid: &str, ts: i64, role: &str| {
            conn.execute(
                "INSERT INTO `message` VALUES (?1, ?2, ?3, ?4)",
                params![mid, sid, ts, format!(r#"{{"role":"{role}"}}"#)],
            )
            .unwrap();
        };
        msg("msg_a1", "ses_a", 1000, "user");
        msg("msg_a2", "ses_a", 1001, "assistant");
        msg("msg_a3", "ses_a", 1002, "assistant"); // 只有 tool/step
        msg("msg_a4", "ses_a", 1003, "assistant"); // 只有 step-*,不出轮次
        msg("msg_b1", "ses_b", 2000, "user");
        msg("msg_orphan", "ses_gone", 3000, "user"); // 孤儿,跳过

        let part = |pid: &str, mid: &str, data: &str| {
            conn.execute(
                "INSERT INTO `part` VALUES (?1, ?2, 'x', ?3)",
                params![pid, mid, data],
            )
            .unwrap();
        };
        part("prt_01", "msg_a1", r#"{"type":"text","text":" 你好 "}"#);
        // 多段 text:要按 part.id 升序拼接。
        part(
            "prt_02",
            "msg_a2",
            r#"{"type":"step-start","snapshot":"s"}"#,
        );
        part(
            "prt_03",
            "msg_a2",
            r#"{"type":"reasoning","text":"想一想"}"#,
        );
        part("prt_04", "msg_a2", r#"{"type":"text","text":"第一段"}"#);
        part("prt_05", "msg_a2", r#"{"type":"text","text":"第二段"}"#);
        part("prt_06", "msg_a2", r#"{"type":"step-finish","cost":0}"#);
        part(
            "prt_07",
            "msg_a3",
            r#"{"type":"tool","tool":"bash","state":{"status":"completed","output":"ok"}}"#,
        );
        part("prt_08", "msg_a4", r#"{"type":"patch","files":["/x"]}"#);
        part("prt_09", "msg_b1", r#"{"type":"text","text":"另一个会话"}"#);
        part("prt_10", "msg_orphan", r#"{"type":"text","text":"孤儿"}"#);
        conn.close().unwrap();
    }

    #[test]
    fn 两个会话_多段拼接_角色归一_哨兵为零() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        build(&db);

        let (sessions, warnings) = parse_all(&db).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(sessions.len(), 2);

        let a = &sessions[0];
        assert_eq!(a.id, "ses_a");
        assert_eq!(a.title.as_deref(), Some("第一个会话"));
        assert_eq!(a.cwd.as_deref(), Some("/tmp/a"));
        assert_eq!(a.meta.turn_count, 3, "msg_a4 只有 patch,不成轮次");
        assert_eq!(a.turns.len(), 3);

        assert_eq!(a.turns[0].role, Role::User);
        assert_eq!(a.turns[0].text, "你好", "首尾空白要去掉");
        assert_eq!(a.turns[0].seq, 0);
        assert_eq!(a.turns[0].ts_ms, Some(1000));

        assert_eq!(a.turns[1].role, Role::Assistant);
        assert_eq!(a.turns[1].text, "第一段\n第二段", "多段按 part.id 升序拼接");
        assert_eq!(a.turns[1].seq, 1);

        // 没有 text part、只有工具调用 ⇒ Tool(进索引、不进 FTS)。
        assert_eq!(a.turns[2].role, Role::Tool);
        assert_eq!(a.turns[2].text, "[tool: bash] ok");

        // 哨兵:byte_len 恒 0,byte_off 是 message 的 rowid。
        for t in sessions.iter().flat_map(|s| &s.turns) {
            assert_eq!(t.byte_len, 0, "SQLite 会话没有字节区间");
            assert!(t.byte_off > 0);
        }

        let b = &sessions[1];
        assert_eq!(b.id, "ses_b");
        assert_eq!(b.title, None, "空标题当没有");
        assert_eq!(b.turns.len(), 1);
        assert_eq!(b.turns[0].text, "另一个会话");
    }

    #[test]
    fn read_turn_与解析结果逐字节相同() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
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
    }

    #[test]
    fn 缺表与非库文件_降级成警告而不是错误() {
        let dir = tempfile::tempdir().unwrap();

        // 是 SQLite,但没有我们要的表。
        let empty = dir.path().join("empty.db");
        Connection::open(&empty)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated(x)")
            .unwrap();
        let (s, w) = parse_all(&empty).unwrap();
        assert!(s.is_empty());
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("`session` table"), "{w:?}");

        // 顶着 .db 名字的纯文本。
        let fake = dir.path().join("text.db");
        std::fs::write(&fake, b"not a database at all").unwrap();
        let (s, w) = parse_all(&fake).unwrap();
        assert!(s.is_empty());
        assert!(w[0].contains("not readable"), "{w:?}");

        // 根本不存在的路径同样只是警告。
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
            .execute_batch(
                "CREATE TABLE `session`(`id` text);
                 CREATE TABLE `message`(`id` text, `session_id` text, `payload` text);
                 CREATE TABLE `part`(`id` text, `message_id` text, `data` text);",
            )
            .unwrap();
        let (s, w) = parse_all(&db).unwrap();
        assert!(s.is_empty());
        assert!(w[0].contains("message.data"), "{w:?}");
    }

    /// 活跃 WAL:写方连着、没 checkpoint,读方照样解析,且主库体积不变。
    #[test]
    fn 活跃wal下可读且不改动主库() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("hot.db");
        build(&db);

        let writer = Connection::open(&db).unwrap();
        // PRAGMA journal_mode 会返回一行(新模式名),必须走 query_row;
        // pragma_update 用的是 execute,遇到有结果集的 pragma 会直接报错。
        let mode: String = writer
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        // 关掉自动 checkpoint,保证下面写的帧留在 -wal 里。
        writer
            .pragma_update(None, "wal_autocheckpoint", 0i64)
            .unwrap();
        writer
            .execute(
                "INSERT INTO `message` VALUES ('msg_hot','ses_b',9000,'{\"role\":\"user\"}')",
                [],
            )
            .unwrap();
        writer
            .execute(
                "INSERT INTO `part` VALUES ('prt_hot','msg_hot','ses_b',
                 '{\"type\":\"text\",\"text\":\"热库里的一句\"}')",
                [],
            )
            .unwrap();

        let before = std::fs::metadata(&db).unwrap().len();
        let wal = db.with_file_name("hot.db-wal");
        assert!(wal.exists(), "WAL 应该已经落地");

        let (sessions, warnings) = parse_all(&db).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let b = sessions.iter().find(|s| s.id == "ses_b").unwrap();
        assert_eq!(b.turns.len(), 2, "未 checkpoint 的帧也要读到");
        assert_eq!(b.turns[1].text, "热库里的一句");

        // 只读连接不能改主库:体积一字节都不变。
        assert_eq!(std::fs::metadata(&db).unwrap().len(), before);
        drop(writer);
    }

    /// 跨 crate 夹具:返回的 rowid 必须真的能喂给 read_turn。
    #[test]
    fn fixture_db_可被自己解析() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("fx.db");
        let ids = fixture_db(&db, "ses_fx", &[("user", "问"), ("assistant", "答")]).unwrap();
        assert_eq!(ids, vec![1, 2]);

        let (sessions, w) = parse_all(&db).unwrap();
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(sessions.len(), 1);
        let t = &sessions[0].turns;
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].byte_off, 1);
        assert_eq!(t[1].byte_off, 2);
        assert_eq!(t[0].role, Role::User);
        assert_eq!(t[1].role, Role::Assistant);
        assert_eq!(read_turn(&db, ids[1]).unwrap(), "答");
    }

    #[test]
    fn read_turn_行不存在时报错() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("fx.db");
        fixture_db(&db, "ses_fx", &[("user", "问")]).unwrap();
        assert!(read_turn(&db, 99).is_err());
    }
}
