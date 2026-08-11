//! 写入层:agent / resource / turn 的 upsert 与陈旧行清理。
//!
//! 设计要点:
//! - `resource` 的冲突键 = `UNIQUE(agent_id, kind, scope, key)`,与
//!   `duster_model::ResourceId` 一一对应;
//! - `cheap_print`(size+mtime 的廉价指纹)命中即跳过写入,让重复扫描接近零写放大;
//! - `fts_turn` 是 contentless 表(`contentless_delete=1`),删除行必须显式执行,
//!   `turn` 上的外键级联管不到它——所有清理路径都手动同步删 FTS。

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;

/// FTS 正文单轮上限:16 KiB。超长正文截断后入索引,byte_off/byte_len
/// 仍指向源文件完整区间,展示层回读不受影响。
const FTS_BODY_LIMIT: usize = 16 * 1024;

/// 一条待写入的资源行。字段与 `resource` 表一一对应。
pub struct ResourceRow {
    pub agent_id: String,
    pub kind: String,
    pub scope: String,
    pub key: String,
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash_content: Option<[u8; 32]>,
    pub cheap_print: Option<[u8; 24]>,
    /// 清单声明的清理级别（`l0`/`l1`/`l2`）；非 artifact 恒为 None。
    ///
    /// 注意它**不参与** `cheap_print` 短路判断，也不需要：带级别的行全是
    /// stats-only 采集的（artifact 只有这一条路），而 stats-only 从不产出
    /// `cheap_print`，短路条件要求入参指纹存在，所以这类行每轮都走写路径，
    /// 清单改级别下一次 scan 必然落库。
    pub clean_level: Option<String>,
    /// 清掉这一项真正能拿回的字节数；非 artifact 恒为 None。
    /// l1/l2 等于 `size`，l0 只算空洞（见 schema v3 注释）。
    pub reclaimable: Option<u64>,
}

/// [`upsert_resource`] 的结果:行 id + 本次是否真的写了。
pub struct UpsertOutcome {
    pub rid: i64,
    /// false = cheap_print 命中,未写任何列;调用方可据此跳过重新解析会话。
    pub changed: bool,
}

/// 写入(或整行替换)一条 agent 记录。
pub fn upsert_agent(
    conn: &Connection,
    info: &duster_model::AgentInfo,
    last_scan_ms: i64,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO agent(agent_id, display_name, root, version, last_scan_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            info.id,
            info.display_name,
            info.root.to_string_lossy(),
            info.version,
            last_scan_ms,
        ],
    )
    .with_context(|| format!("failed to upsert agent: {}", info.id))?;
    Ok(())
}

/// upsert 一条资源行。
///
/// 冲突键命中且库内 cheap_print 与入参**都存在且相等**时不写任何列,
/// `changed = false`;指纹缺失(任一侧为 None)视为"未知",保守走写入路径。
pub fn upsert_resource(conn: &Connection, row: &ResourceRow) -> Result<UpsertOutcome> {
    // 先查现有行:命中指纹就完全不碰写路径(连 no-op UPDATE 都不发,避免 WAL 增长)。
    let existing: Option<(i64, Option<Vec<u8>>)> = conn
        .query_row(
            "SELECT rid, cheap_print FROM resource
             WHERE agent_id = ?1 AND kind = ?2 AND scope = ?3 AND key = ?4",
            params![row.agent_id, row.kind, row.scope, row.key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .context("failed to query existing resource row")?;

    if let Some((rid, Some(stored))) = &existing
        && let Some(incoming) = &row.cheap_print
        && stored.as_slice() == incoming.as_slice()
    {
        return Ok(UpsertOutcome {
            rid: *rid,
            changed: false,
        });
    }

    let rid: i64 = conn
        .query_row(
            "INSERT INTO resource(agent_id, kind, scope, key, path, size, mtime_ns, hash_content, cheap_print, clean_level, reclaimable)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(agent_id, kind, scope, key) DO UPDATE SET
               path = excluded.path,
               size = excluded.size,
               mtime_ns = excluded.mtime_ns,
               hash_content = excluded.hash_content,
               cheap_print = excluded.cheap_print,
               clean_level = excluded.clean_level,
               reclaimable = excluded.reclaimable
             RETURNING rid",
            params![
                row.agent_id,
                row.kind,
                row.scope,
                row.key,
                row.path,
                row.size,
                row.mtime_ns,
                row.hash_content.as_ref().map(|h| h.as_slice()),
                row.cheap_print.as_ref().map(|p| p.as_slice()),
                row.clean_level.as_deref(),
                row.reclaimable,
            ],
            |r| r.get(0),
        )
        .with_context(|| format!("failed to upsert resource: {}/{}/{}", row.agent_id, row.kind, row.key))?;

    Ok(UpsertOutcome { rid, changed: true })
}

/// 重建某资源(会话文件)的全部轮次:删旧 turn + fts_turn,再批量插入。
///
/// - `fts_turn.rowid` 与 `turn.tid` 严格对齐;
/// - 正文入 FTS 前按 char 边界截断到 16 KiB;
/// - `Role::Tool` 的轮次只进 `turn` 表不进 FTS(见 `duster_model::Role` 文档:
///   工具调用/结果非对话正文,默认不参与全文检索);
/// - 整个替换包在一个事务里,崩溃不会留下半新半旧的索引。
pub fn replace_turns(
    conn: &Connection,
    rid: i64,
    turns: &[duster_model::TurnRecord],
) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .context("failed to begin replace_turns transaction")?;

    // 先删 FTS(依赖 turn.tid 反查),再删 turn——顺序不能反。
    tx.execute(
        "DELETE FROM fts_turn WHERE rowid IN (SELECT tid FROM turn WHERE rid = ?1)",
        [rid],
    )
    .context("failed to delete stale fts_turn rows")?;
    tx.execute("DELETE FROM turn WHERE rid = ?1", [rid])
        .context("failed to delete stale turn rows")?;

    {
        let mut ins_turn = tx.prepare(
            "INSERT INTO turn(rid, seq, role, ts_ms, byte_off, byte_len)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut ins_fts =
            tx.prepare("INSERT INTO fts_turn(rowid, body, tid) VALUES (?1, ?2, ?1)")?;

        for turn in turns {
            let role = match turn.role {
                duster_model::Role::User => "user",
                duster_model::Role::Assistant => "assistant",
                duster_model::Role::Tool => "tool",
            };
            ins_turn.execute(params![
                rid,
                turn.seq,
                role,
                turn.ts_ms,
                turn.byte_off,
                turn.byte_len,
            ])?;
            let tid = tx.last_insert_rowid();

            if turn.role != duster_model::Role::Tool {
                ins_fts.execute(params![tid, truncate_at_char_boundary(&turn.text)])?;
            }
        }
    }

    tx.commit()
        .context("failed to commit replace_turns transaction")
}

/// 删除该 (agent_id, kind) 下本轮扫描没见到的旧资源行,返回删除数。
///
/// 外键级联只覆盖 turn;fts_turn 与 resource/turn 之间没有约束关系,
/// 必须逐 rid 先手动清 FTS,再删 resource(turn 随级联消失)。
pub fn delete_stale_resources(
    conn: &Connection,
    agent_id: &str,
    kind: &str,
    seen_keys: &[String],
) -> Result<u64> {
    let seen: HashSet<&str> = seen_keys.iter().map(String::as_str).collect();

    let tx = conn
        .unchecked_transaction()
        .context("failed to begin delete_stale transaction")?;

    // 全量拉该 (agent, kind) 的 key 在 Rust 侧过滤:seen_keys 可能上千,
    // 拼 IN 子句既有长度上限又难以参数化。
    let stale: Vec<i64> = {
        let mut stmt =
            tx.prepare("SELECT rid, key FROM resource WHERE agent_id = ?1 AND kind = ?2")?;
        let rows = stmt.query_map(params![agent_id, kind], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.filter_map(|row| match row {
            Ok((rid, key)) if !seen.contains(key.as_str()) => Some(Ok(rid)),
            Ok(_) => None,
            Err(e) => Some(Err(e)),
        })
        .collect::<std::result::Result<_, _>>()
        .context("failed to enumerate stale resources")?
    };

    for rid in &stale {
        tx.execute(
            "DELETE FROM fts_turn WHERE rowid IN (SELECT tid FROM turn WHERE rid = ?1)",
            [rid],
        )?;
        // resource 删除后 turn 由 ON DELETE CASCADE 清掉。
        tx.execute("DELETE FROM resource WHERE rid = ?1", [rid])?;
    }

    tx.commit()
        .context("failed to commit delete_stale transaction")?;
    Ok(stale.len() as u64)
}

/// 在不超过 [`FTS_BODY_LIMIT`] 字节的前提下,于 char 边界截断正文。
fn truncate_at_char_boundary(text: &str) -> &str {
    if text.len() <= FTS_BODY_LIMIT {
        return text;
    }
    let mut end = FTS_BODY_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use duster_model::{Role, TurnRecord};

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    fn row(key: &str, size: u64, print: [u8; 24]) -> ResourceRow {
        ResourceRow {
            agent_id: "claude-code".into(),
            kind: "session".into(),
            scope: "global".into(),
            key: key.into(),
            path: format!("/tmp/{key}.jsonl"),
            size,
            mtime_ns: 1_000,
            hash_content: Some([7u8; 32]),
            cheap_print: Some(print),
            clean_level: None,
            reclaimable: None,
        }
    }

    fn turn(seq: u32, role: Role, text: &str) -> TurnRecord {
        TurnRecord {
            seq,
            role,
            ts_ms: Some(1_700_000_000_000),
            byte_off: 0,
            byte_len: text.len() as u64,
            text: text.into(),
        }
    }

    fn fts_hit(conn: &Connection, query: &str) -> Option<i64> {
        conn.query_row(
            "SELECT rowid FROM fts_turn WHERE fts_turn MATCH ?1",
            [query],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    /// 同 cheap_print 二次 upsert:不写、changed=false、rid 稳定。
    #[test]
    fn same_cheap_print_skips_write() {
        let conn = open();
        let r = row("s1", 100, [1u8; 24]);

        let first = upsert_resource(&conn, &r).unwrap();
        assert!(first.changed);

        // 即使 size/path 变了,指纹相同就整行跳过(指纹是唯一判据)。
        let mut same_print = row("s1", 100, [1u8; 24]);
        same_print.path = "/elsewhere".into();
        let second = upsert_resource(&conn, &same_print).unwrap();
        assert!(!second.changed);
        assert_eq!(second.rid, first.rid);

        let path: String = conn
            .query_row(
                "SELECT path FROM resource WHERE rid = ?1",
                [first.rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(path, "/tmp/s1.jsonl", "命中指纹不得写任何列");
    }

    /// 指纹变化(size 变)→ changed=true 且列真的更新了。
    #[test]
    fn changed_cheap_print_updates_columns() {
        let conn = open();
        let first = upsert_resource(&conn, &row("s1", 100, [1u8; 24])).unwrap();

        let bigger = row("s1", 200, [2u8; 24]);
        let second = upsert_resource(&conn, &bigger).unwrap();
        assert!(second.changed);
        assert_eq!(second.rid, first.rid, "冲突键命中必须复用 rid");

        let (size, print): (u64, Vec<u8>) = conn
            .query_row(
                "SELECT size, cheap_print FROM resource WHERE rid = ?1",
                [first.rid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(size, 200);
        assert_eq!(print, vec![2u8; 24]);
    }

    /// 任一侧指纹缺失 → 保守写入(changed=true)。
    #[test]
    fn missing_cheap_print_always_writes() {
        let conn = open();
        let mut r = row("s1", 100, [1u8; 24]);
        r.cheap_print = None;
        assert!(upsert_resource(&conn, &r).unwrap().changed);
        assert!(
            upsert_resource(&conn, &r).unwrap().changed,
            "None 指纹不得判为命中"
        );
    }

    /// replace_turns:新正文可 MATCH,旧正文彻底消失,tid/rowid 对齐。
    #[test]
    fn replace_turns_swaps_fts_content() {
        let conn = open();
        let rid = upsert_resource(&conn, &row("s1", 100, [1u8; 24]))
            .unwrap()
            .rid;

        replace_turns(&conn, rid, &[turn(0, Role::User, "旧的中文正文内容")]).unwrap();
        assert!(fts_hit(&conn, "中文正文").is_some());

        replace_turns(
            &conn,
            rid,
            &[
                turn(0, Role::User, "全新的检索文本"),
                turn(1, Role::Assistant, "assistant reply body"),
                turn(2, Role::Tool, "tool 输出不入全文索引"),
            ],
        )
        .unwrap();

        assert!(fts_hit(&conn, "中文正文").is_none(), "旧正文必须搜不到");
        assert!(fts_hit(&conn, "检索文本").is_some());
        assert!(fts_hit(&conn, "reply").is_some());
        assert!(fts_hit(&conn, "全文索引").is_none(), "tool 轮次不入 FTS");

        // turn 表三行全在;fts rowid 与 tid 一致。
        // 注:contentless 表读列值一律返回 NULL,只有 rowid 真实,故对齐只能凭 rowid 验证。
        let turn_count: i64 = conn
            .query_row("SELECT count(*) FROM turn WHERE rid = ?1", [rid], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(turn_count, 3);
        let aligned: i64 = conn
            .query_row(
                "SELECT count(*) FROM fts_turn f JOIN turn t ON f.rowid = t.tid",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(aligned, 2);
    }

    /// 16 KiB 截断:多字节字符恰好跨在 16384 字节边界上也不 panic,且截断后可检索。
    #[test]
    fn oversized_body_truncates_at_char_boundary() {
        let conn = open();
        let rid = upsert_resource(&conn, &row("s1", 100, [1u8; 24]))
            .unwrap()
            .rid;

        // "汉" 3 字节 x 5462 = 16386 字节:16384 落在第 5462 个字符中间。
        let body = "汉".repeat(5462);
        assert!(body.len() > FTS_BODY_LIMIT);
        assert!(!body.is_char_boundary(FTS_BODY_LIMIT));

        replace_turns(&conn, rid, &[turn(0, Role::User, &body)]).unwrap();

        let stored_len: i64 = conn
            .query_row("SELECT length(CAST(body AS BLOB)) FROM fts_turn", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        // contentless 表不回存正文,长度查询可能为 0;关键断言是不 panic 且可检索。
        let _ = stored_len;
        assert!(fts_hit(&conn, "汉汉汉").is_some());

        // 纯 Rust 侧再验证截断函数本身。
        let cut = truncate_at_char_boundary(&body);
        assert_eq!(cut.len(), 16383, "16384 非 char 边界,应回退到 16383");
        assert!(cut.chars().all(|c| c == '汉'));
    }

    /// delete_stale:未见 key 的行连同 turn + fts 一起消失,返回删除数。
    #[test]
    fn delete_stale_cascades_turn_and_fts() {
        let conn = open();
        let keep = upsert_resource(&conn, &row("keep", 1, [1u8; 24]))
            .unwrap()
            .rid;
        let gone = upsert_resource(&conn, &row("gone", 2, [2u8; 24]))
            .unwrap()
            .rid;
        replace_turns(&conn, keep, &[turn(0, Role::User, "保留的正文段落")]).unwrap();
        replace_turns(&conn, gone, &[turn(0, Role::User, "将被清理的正文")]).unwrap();

        let removed =
            delete_stale_resources(&conn, "claude-code", "session", &["keep".into()]).unwrap();
        assert_eq!(removed, 1);

        let res_count: i64 = conn
            .query_row("SELECT count(*) FROM resource", [], |r| r.get(0))
            .unwrap();
        assert_eq!(res_count, 1);
        let turn_count: i64 = conn
            .query_row("SELECT count(*) FROM turn WHERE rid = ?1", [gone], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(turn_count, 0, "外键级联应清掉 turn");
        assert!(fts_hit(&conn, "被清理").is_none(), "fts 行必须手动清干净");
        assert!(fts_hit(&conn, "正文段落").is_some(), "存活行不受影响");

        // 不同 kind 不受波及。
        let other = upsert_resource(
            &conn,
            &ResourceRow {
                kind: "skill".into(),
                ..row("gone", 3, [3u8; 24])
            },
        )
        .unwrap();
        assert!(other.changed);
        let removed2 =
            delete_stale_resources(&conn, "claude-code", "session", &["keep".into()]).unwrap();
        assert_eq!(removed2, 0);
    }

    /// upsert_agent:INSERT OR REPLACE,二次写覆盖旧值。
    #[test]
    fn upsert_agent_replaces() {
        let conn = open();
        let mut info = duster_model::AgentInfo {
            id: "codex".into(),
            display_name: "Codex".into(),
            root: "/home/u/.codex".into(),
            version: None,
        };
        upsert_agent(&conn, &info, 111).unwrap();
        info.version = Some("1.2.3".into());
        upsert_agent(&conn, &info, 222).unwrap();

        let (n, ver, ts): (i64, Option<String>, i64) = conn
            .query_row(
                "SELECT count(*), version, last_scan_ms FROM agent WHERE agent_id = 'codex'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(ver.as_deref(), Some("1.2.3"));
        assert_eq!(ts, 222);
    }
}
