//! FTS5 trigram 检索 + 偏移量回读 + char 边界安全高亮。
//!
//! # 设计取舍
//!
//! - `fts_turn` 是 contentless（`content=''`）表：正文只进倒排索引不落库，
//!   因此 FTS5 自带的 `snippet()` / `highlight()` **不可用**（无正文可切）。
//!   摘要改为命中后按 `(resource.path, turn.byte_off, byte_len)` 从源文件
//!   回读该行原文，在原文上自行定位与切窗。
//! - 回读的是**原始行**（通常是一行 JSONL），而 FTS 里存的是解析后的正文。
//!   多数情况下正文子串在原始行里原样出现，直接对整行做大小写不敏感的
//!   子串定位即可；若因 JSON 转义（`\n`、`\"`、`\uXXXX` 等）导致定位失败，
//!   降级为「行首 160 char 窗口 + 空 highlights」——不去解析 JSON 重提正文，
//!   把解析职责留在写入侧，检索侧保持零格式假设。
//! - 源文件被删/截短/改写导致回读失败时，snippet 降级为 `[源文件已变动]`，
//!   检索本身不中断——索引是可丢弃派生物，源文件才是事实。
//! - 全程按 char 边界切片与计算区间，中文/emoji 不会 panic 或产生乱码。

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::io::{Read, Seek, SeekFrom};

/// 回读失败（文件已删/截短/非 UTF-8）时的降级摘要。
const SNIPPET_SOURCE_CHANGED: &str = "[源文件已变动]";

/// 命中窗口:子串两侧各保留的 char 数。
const WINDOW_CHARS: usize = 80;

/// 定位失败时的降级窗口:行首 char 数。
const FALLBACK_WINDOW_CHARS: usize = 160;

/// 一条检索命中。
pub struct SearchHit {
    pub tid: i64,
    pub rid: i64,
    pub agent_id: String,
    pub resource_path: String,
    pub seq: i64,
    pub role: String,
    pub byte_off: u64,
    pub byte_len: u64,
    /// 命中行的摘要窗口（源文件回读失败时为降级占位文本）。
    pub snippet: String,
    /// `snippet` 内的高亮区间（字节偏移，保证落在 char 边界上）。
    pub highlights: Vec<(usize, usize)>,
}

/// 检索过滤条件。
pub struct SearchFilter {
    /// 只看某个 agent 的资源;`None` 为全部。
    pub agent: Option<String>,
    /// 最多返回条数。
    pub limit: usize,
}

/// 对 `fts_turn` 做子串检索,按 bm25 相关度排序,回读源文件生成摘要。
///
/// `query` 是朴素子串,不是 FTS5 查询语法——内部会包一层双引号转义,
/// 用户输入里的 `AND` / `*` / `-` 等不会被当作操作符。
pub fn search_turns(
    conn: &Connection,
    query: &str,
    filter: &SearchFilter,
) -> Result<Vec<SearchHit>> {
    // trigram 分词器的最小 token 是 3 个字符:更短的查询串在倒排索引里
    // 根本不存在对应 token,MATCH 只会报错或空转——直接返回空结果。
    if query.chars().count() < 3 {
        return Ok(Vec::new());
    }

    // 双引号包裹 + 内部 `"` 翻倍,把用户输入整体降级为一个字符串词元。
    let fts_query = format!("\"{}\"", query.replace('"', "\"\""));

    // contentless 表不存任何列值(含 UNINDEXED 的 tid),只能靠
    // rowid == turn.tid 的写入约定关联回 turn。
    let mut stmt = conn
        .prepare(
            "SELECT t.tid, t.rid, r.agent_id, r.path, t.seq, t.role, t.byte_off, t.byte_len
             FROM fts_turn
             JOIN turn t ON fts_turn.rowid = t.tid
             JOIN resource r USING (rid)
             WHERE fts_turn MATCH ?1
               AND (?2 IS NULL OR r.agent_id = ?2)
             ORDER BY bm25(fts_turn)
             LIMIT ?3",
        )
        .context("准备检索语句失败")?;

    let rows = stmt
        .query_map(
            rusqlite::params![fts_query, filter.agent, filter.limit as i64],
            |row| {
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
            },
        )
        .context("执行检索失败")?;

    let mut hits = Vec::new();
    for row in rows {
        let (tid, rid, agent_id, path, seq, role, byte_off, byte_len) =
            row.context("读取命中行失败")?;
        let byte_off = byte_off.max(0) as u64;
        let byte_len = byte_len.max(0) as u64;

        let (snippet, highlights) = match read_span(&path, byte_off, byte_len) {
            Some(line) => make_snippet(&line, query),
            // 文件已删/截短/内容不再是合法 UTF-8:降级占位,不中断检索。
            None => (SNIPPET_SOURCE_CHANGED.to_string(), Vec::new()),
        };

        hits.push(SearchHit {
            tid,
            rid,
            agent_id,
            resource_path: path,
            seq: seq.unwrap_or_default(),
            role: role.unwrap_or_default(),
            byte_off,
            byte_len,
            snippet,
            highlights,
        });
    }
    Ok(hits)
}

/// 从源文件精确回读 `[off, off+len)` 字节并解码为 UTF-8。
/// 任一步失败(文件没了/不够长/编码坏了)都返回 `None`,由调用方降级。
fn read_span(path: &str, off: u64, len: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(off)).ok()?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// 在回读的原始行里定位 `query`(大小写不敏感,取首个命中),
/// 以命中为中心切 ±[`WINDOW_CHARS`] char 的窗口,并给出窗口内的高亮字节区间。
///
/// 定位失败(正文经 JSON 转义后与原文不再逐字节一致)时降级:
/// 窗口 = 行首 [`FALLBACK_WINDOW_CHARS`] char,highlights 为空。
fn make_snippet(line: &str, query: &str) -> (String, Vec<(usize, usize)>) {
    let Some((m_start, m_end)) = find_ci(line, query) else {
        let end = char_boundary_after(line, 0, FALLBACK_WINDOW_CHARS);
        return (line[..end].to_string(), Vec::new());
    };

    let win_start = char_boundary_before(line, m_start, WINDOW_CHARS);
    let win_end = char_boundary_after(line, m_end, WINDOW_CHARS);
    let snippet = line[win_start..win_end].to_string();
    let highlights = vec![(m_start - win_start, m_end - win_start)];
    (snippet, highlights)
}

/// 大小写不敏感地查找 `needle` 在 `haystack` 中的首个命中,
/// 返回命中在 `haystack` 里的字节区间(天然落在 char 边界上)。
///
/// 逐 char 用 `to_lowercase()` 迭代器比较,不对整串做 lowercase——
/// 某些字符 lowercase 后字节长度会变(如 `İ`),整串转换会破坏偏移映射。
fn find_ci(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    for (start, _) in haystack.char_indices() {
        if let Some(end) = match_ci_at(haystack, start, needle) {
            return Some((start, end));
        }
    }
    None
}

/// 从 `haystack[start..]` 起尝试逐 char 匹配 `needle`,成功返回结束字节偏移。
fn match_ci_at(haystack: &str, start: usize, needle: &str) -> Option<usize> {
    let mut hay = haystack[start..].chars();
    let mut pos = start;
    for nc in needle.chars() {
        let hc = hay.next()?;
        if !hc.to_lowercase().eq(nc.to_lowercase()) {
            return None;
        }
        pos += hc.len_utf8();
    }
    Some(pos)
}

/// 从字节偏移 `from`(必须在 char 边界上)往前退最多 `chars` 个 char,
/// 返回落点的字节偏移。
fn char_boundary_before(s: &str, from: usize, chars: usize) -> usize {
    s[..from]
        .char_indices()
        .rev()
        .nth(chars.saturating_sub(1))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// 从字节偏移 `from`(必须在 char 边界上)往后走最多 `chars` 个 char,
/// 返回落点的字节偏移。
fn char_boundary_after(s: &str, from: usize, chars: usize) -> usize {
    s[from..]
        .char_indices()
        .nth(chars)
        .map(|(i, _)| from + i)
        .unwrap_or(s.len())
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
        SearchFilter { agent: None, limit }
    }

    /// 中文子串命中,高亮区间落在 char 边界:切片不 panic 且等于 query。
    #[test]
    fn cjk_substring_hit_with_char_safe_highlight() {
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
        assert_eq!(hit.highlights.len(), 1);
        let (s, e) = hit.highlights[0];
        // 切片本身就是 char 边界断言:越界或劈开 char 会 panic。
        assert_eq!(&hit.snippet[s..e], "全文检索");
    }

    /// 英文大小写不敏感:FTS 命中 + 回读定位都不区分大小写。
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
        let hit = &hits[0];
        let (s, e) = hit.highlights[0];
        // 高亮的是原文原样大小写,与 query 只在忽略大小写意义下相等。
        assert!(hit.snippet[s..e].eq_ignore_ascii_case("trigram tokenizer"));
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
            agent: Some("codex".to_string()),
            limit: 10,
        };
        let hits = search_turns(&conn, "共享关键词", &filter).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].agent_id, "codex");
    }

    /// 源文件删除后降级为占位摘要,不 panic、不中断。
    #[test]
    fn deleted_source_degrades_gracefully() {
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
        assert_eq!(hits[0].snippet, SNIPPET_SOURCE_CHANGED);
        assert!(hits[0].highlights.is_empty());
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
        let (s, e) = hits[0].highlights[0];
        assert_eq!(&hits[0].snippet[s..e], r#""引号" 与 *"#);

        // 纯操作符词也只是子串:正文里有 AND,能命中。
        let hits = search_turns(&conn, "AND OR NOT", &all(10)).unwrap();
        assert_eq!(hits.len(), 1);
    }

    /// 长行:窗口按 char 截取,中文长文不 panic,且窗口包含命中。
    #[test]
    fn long_line_window_is_char_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db();
        let long = format!("{}目标子串{}", "前缀甲".repeat(100), "后缀乙".repeat(100));
        seed(&conn, dir.path(), "claude", "f.jsonl", &[long.as_str()]);

        let hits = search_turns(&conn, "目标子串", &all(10)).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        // 窗口 = 命中 4 char + 两侧各 80 char。
        assert_eq!(hit.snippet.chars().count(), 4 + WINDOW_CHARS * 2);
        let (s, e) = hit.highlights[0];
        assert_eq!(&hit.snippet[s..e], "目标子串");
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
