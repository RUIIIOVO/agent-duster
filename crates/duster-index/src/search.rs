//! FTS5 trigram 检索（纯 SQL，不碰文件系统）。
//!
//! # 设计取舍
//!
//! - `fts_turn` 是 contentless（`content=''`）表：正文只进倒排索引不落库，
//!   因此 FTS5 自带的 `snippet()` / `highlight()` **不可用**（无正文可切）。
//! - 摘要不再由本层生成：那是**展示层的活**——正文要回读（`.zst` 压缩包 /
//!   SQLite 行 id 哨兵两种形态只有 core 的 `read_turn_text` 认全）还要按
//!   agent 抽成散文，而索引层不碰文件、也不认识正文长什么样。本层只回命中
//!   行（[`TurnHit`]），core 拿 `(path, byte_off, byte_len)` 回读 + 切窗。
//! - 本层因此**零文件 I/O**：源文件删了、改了都不影响检索本身——命中
//!   行照回，摘要好不好是回读层的事。

use anyhow::{Context, Result};
use rusqlite::{Connection, ToSql};

/// 一条检索命中（不含摘要）。摘要由上层回读正文后生成。
pub struct TurnHit {
    pub tid: i64,
    pub rid: i64,
    pub agent_id: String,
    pub resource_path: String,
    pub seq: i64,
    pub role: String,
    pub byte_off: u64,
    pub byte_len: u64,
}

/// 检索过滤条件。
pub struct SearchFilter {
    /// 只看这几个 agent 的资源;空 Vec = 全部。
    pub agents: Vec<String>,
    /// 最多返回条数。
    pub limit: usize,
}

/// 对 `fts_turn` 做子串检索,按 bm25 相关度排序,只回命中行。
///
/// `query` 是朴素子串,不是 FTS5 查询语法——内部会包一层双引号转义,
/// 用户输入里的 `AND` / `*` / `-` 等不会被当作操作符。
///
/// 本函数**不碰文件系统**:摘要（回读 + 切窗 + 高亮）是上层
/// `duster_core::search::search` 的活,那里才认识 `.zst` / SQLite 哨兵
/// 这两种源形态。这里只把 SQL 命中行原样端出去。
pub fn search_turns(conn: &Connection, query: &str, filter: &SearchFilter) -> Result<Vec<TurnHit>> {
    // trigram 分词器的最小 token 是 3 个字符:更短的查询串在倒排索引里
    // 根本不存在对应 token,MATCH 只会报错或空转——直接返回空结果。
    if query.chars().count() < 3 {
        return Ok(Vec::new());
    }

    // 双引号包裹 + 内部 `"` 翻倍,把用户输入整体降级为一个字符串词元。
    let fts_query = format!("\"{}\"", query.replace('"', "\"\""));

    // contentless 表不存任何列值(含 UNINDEXED 的 tid),只能靠
    // rowid == turn.tid 的写入约定关联回 turn。
    //
    // agent 过滤是动态 IN:占位符个数跟着 `agents` 走,过滤值一律绑定参数。
    // 占位符序号是这条 SQL 里唯一被拼出来的内容——agent id 直接来自命令行,
    // 把值拼进字符串等于把 SQL 注入的口子递到 `--agent` 手里。
    let mut params: Vec<&dyn ToSql> = vec![&fts_query];
    let mut agent_clause = String::new();
    if !filter.agents.is_empty() {
        let first = params.len() + 1;
        for a in &filter.agents {
            params.push(a);
        }
        let holes = (first..=params.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        agent_clause = format!(" AND r.agent_id IN ({holes})");
    }
    // limit 的占位符序号在 agents 之后，得等 IN 列表定了才算得出来。
    let limit_hole = params.len() + 1;
    let limit = filter.limit as i64;
    params.push(&limit);

    let mut stmt = conn
        .prepare(&format!(
            "SELECT t.tid, t.rid, r.agent_id, r.path, t.seq, t.role, t.byte_off, t.byte_len
             FROM fts_turn
             JOIN turn t ON fts_turn.rowid = t.tid
             JOIN resource r USING (rid)
             WHERE fts_turn MATCH ?1
               {agent_clause}
             ORDER BY bm25(fts_turn)
             LIMIT ?{limit_hole}"
        ))
        .context("failed to prepare search statement")?;

    let rows = stmt
        .query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .context("failed to execute search")?;

    let mut hits = Vec::new();
    for row in rows {
        let (tid, rid, agent_id, path, seq, role, byte_off, byte_len) =
            row.context("failed to read hit row")?;
        hits.push(TurnHit {
            tid,
            rid,
            agent_id,
            resource_path: path,
            seq: seq.unwrap_or_default(),
            role: role.unwrap_or_default(),
            byte_off: byte_off.max(0) as u64,
            byte_len: byte_len.max(0) as u64,
        });
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use std::io::Write;
    use std::path::Path;

    /// 建库 + 写一个 agent 的一份资源文件,把每行注册进 turn/fts_turn。
    /// 返回 (rid, 每行的 (byte_off, byte_len))。
    fn seed(
        conn: &Connection,
        dir: &Path,
        agent_id: &str,
        file_name: &str,
        lines: &[&str],
    ) -> (i64, Vec<(u64, u64)>) {
        let path = dir.join(file_name);
        let mut file = std::fs::File::create(&path).unwrap();
        let mut spans = Vec::new();
        let mut off = 0u64;
        for line in lines {
            file.write_all(line.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
            spans.push((off, line.len() as u64));
            off += line.len() as u64 + 1;
        }

        conn.execute(
            "INSERT OR IGNORE INTO agent(agent_id) VALUES (?1)",
            [agent_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO resource(agent_id, kind, scope, key, path, size, mtime_ns)
             VALUES (?1, 'session', 'user', ?2, ?3, ?4, 0)",
            rusqlite::params![agent_id, file_name, path.to_str().unwrap(), off as i64],
        )
        .unwrap();
        let rid = conn.last_insert_rowid();

        for (seq, ((line_off, line_len), line)) in spans.iter().zip(lines).enumerate() {
            conn.execute(
                "INSERT INTO turn(rid, seq, role, ts_ms, byte_off, byte_len)
                 VALUES (?1, ?2, 'user', 0, ?3, ?4)",
                rusqlite::params![rid, seq as i64, *line_off as i64, *line_len as i64],
            )
            .unwrap();
            let tid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO fts_turn(rowid, body, tid) VALUES (?1, ?2, ?1)",
                rusqlite::params![tid, line],
            )
            .unwrap();
        }
        (rid, spans)
    }

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    fn all(limit: usize) -> SearchFilter {
        SearchFilter {
            agents: Vec::new(),
            limit,
        }
    }

    /// 中文子串命中:命中行带全部分段信息;摘要/高亮是上层的活,本层不碰。
    #[test]
    fn cjk_substring_hit_returns_turn_hit() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db();
        seed(
            &conn,
            dir.path(),
            "claude",
            "a.jsonl",
            &["今天我们讨论了全文检索的实现方案,还聊到 emoji 🦀 的边界问题"],
        );

        let hits = search_turns(&conn, "全文检索", &all(10)).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.agent_id, "claude");
        assert_eq!(hit.seq, 0);
        assert!(
            hit.resource_path.ends_with("a.jsonl"),
            "{}",
            hit.resource_path
        );
    }

    /// 英文大小写不敏感:FTS 命中不分大小写。
    #[test]
    fn english_search_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db();
        seed(
            &conn,
            dir.path(),
            "codex",
            "b.jsonl",
            &["We SHOULD use Trigram Tokenizer for CJK"],
        );

        let hits = search_turns(&conn, "trigram tokenizer", &all(10)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].agent_id, "codex");
    }

    /// agent 过滤:同一个词在两个 agent 各有命中,过滤后只剩一个。
    #[test]
    fn agent_filter_narrows_results() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db();
        seed(&conn, dir.path(), "claude", "c1.jsonl", &["共享关键词甲"]);
        seed(&conn, dir.path(), "codex", "c2.jsonl", &["共享关键词乙"]);

        let hits = search_turns(&conn, "共享关键词", &all(10)).unwrap();
        assert_eq!(hits.len(), 2);

        let filter = SearchFilter {
            agents: vec!["codex".to_string()],
            limit: 10,
        };
        let hits = search_turns(&conn, "共享关键词", &filter).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].agent_id, "codex");
    }

    /// 源文件在磁盘上**根本不存在**,检索照样回命中行——本层是纯 SQL,
    /// 零文件 I/O;读不读得回正文是上层回读的事,不是检索的事。
    #[test]
    fn 源文件不存在_检索照常返回命中行() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db();
        seed(
            &conn,
            dir.path(),
            "claude",
            "gone.jsonl",
            &["即将消失的正文"],
        );
        std::fs::remove_file(dir.path().join("gone.jsonl")).unwrap();

        let hits = search_turns(&conn, "消失的正文", &all(10)).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].resource_path.ends_with("gone.jsonl"),
            "{}",
            hits[0].resource_path
        );
    }

    /// trigram 下限:不足 3 个字符直接空结果,不碰 FTS。
    #[test]
    fn query_below_trigram_minimum_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db();
        seed(&conn, dir.path(), "claude", "d.jsonl", &["你好世界"]);

        assert!(search_turns(&conn, "你好", &all(10)).unwrap().is_empty());
        assert!(search_turns(&conn, "ab", &all(10)).unwrap().is_empty());
        assert!(search_turns(&conn, "", &all(10)).unwrap().is_empty());
    }

    /// 用户输入里的 FTS 语法字符被当作朴素子串,不报语法错。
    #[test]
    fn fts_operators_in_query_are_literal() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db();
        seed(
            &conn,
            dir.path(),
            "claude",
            "e.jsonl",
            &[r#"包含 AND OR NOT "引号" 与 * 号的正文"#],
        );

        // 语法字符不报错;带引号的子串也能整体命中。
        let hits = search_turns(&conn, r#""引号" 与 *"#, &all(10)).unwrap();
        assert_eq!(hits.len(), 1);

        // 纯操作符词也只是子串:正文里有 AND,能命中。
        let hits = search_turns(&conn, "AND OR NOT", &all(10)).unwrap();
        assert_eq!(hits.len(), 1);
    }

    /// limit 生效。
    #[test]
    fn limit_caps_results() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db();
        seed(
            &conn,
            dir.path(),
            "claude",
            "g.jsonl",
            &["重复正文一号", "重复正文二号", "重复正文三号"],
        );

        let hits = search_turns(&conn, "重复正文", &all(2)).unwrap();
        assert_eq!(hits.len(), 2);
    }
}
