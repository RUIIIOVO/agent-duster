//! `duster search` / `duster open`:检索用例,薄封装 duster-index::search。
//!
//! 本模块是 CLI 触达检索能力的唯一合法通道:CLI 只依赖 duster-core,
//! 这里把 `SearchHit` / `SearchFilter` 转出,并补上两件编排层的事——
//! 索引库路径解析(缺省 `~/.agent-duster/index.db`,与 status 一致)与
//! 按字节区间从源文件回读**正文**:搜索摘要与 `open` 全文都走
//! [`read_turn_body`]——索引是 contentless 的,正文永远回源取,
//! 取回来还得按 agent 抽成可读散文,否则印给用户的就是原始 JSONL 行。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_index::db::Index;
use duster_index::search::TurnHit;
use duster_model::TurnBody;

pub use duster_index::search::SearchFilter;

/// 回读失败(文件已删/截短/非 UTF-8)时的降级摘要。
const SNIPPET_SOURCE_CHANGED: &str = "[source file changed]";

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
    /// 命中行的摘要窗口(源文件回读失败时为降级占位文本)。
    pub snippet: String,
    /// `snippet` 内的高亮区间(字节偏移,保证落在 char 边界上)。
    pub highlights: Vec<(usize, usize)>,
}

/// 解析索引库路径并只读打开。库不存在时给人话提示,引导先跑 `duster scan`。
fn open_index(index_path: Option<&Path>) -> Result<Index> {
    let path: PathBuf = match index_path {
        Some(p) => p.to_path_buf(),
        None => duster_fs::path::expand_tilde("~/.agent-duster/index.db"),
    };
    if !path.is_file() {
        bail!(
            "index database not found: {}. Run `duster scan` first to build it.",
            path.display()
        );
    }
    Index::open_readonly(&path)
        .with_context(|| format!("failed to open index read-only: {}", path.display()))
}

/// 全文检索会话正文。`index_path` 缺省 `~/.agent-duster/index.db`。
///
/// 查询语义见 [`duster_index::search::search_turns`]:朴素子串,
/// 不解释 FTS5 操作符;短于 3 个字符的查询直接返回空。
///
/// 索引层只回命中行,摘要在这里回读正文再切窗生成:回读走
/// [`read_turn_body`],`.zst` 压缩包与 `byte_len == 0` 的 SQLite 行 id 哨兵
/// 都由它兜住——prune 压过的会话、history.db 里的轮次因此也能出真实摘要。
pub fn search(
    index_path: Option<&Path>,
    query: &str,
    filter: &SearchFilter,
) -> Result<Vec<SearchHit>> {
    let idx = open_index(index_path)?;
    let hits = duster_index::search::search_turns(idx.conn(), query, filter)?;
    hits.into_iter()
        .map(|h: TurnHit| {
            let (snippet, highlights) = match read_turn_body(
                &h.agent_id,
                &h.resource_path,
                h.byte_off as i64,
                h.byte_len as i64,
            ) {
                Ok(body) => make_snippet(&body.text, query),
                // 文件已删/截短/内容不再是合法 UTF-8:降级占位,不中断检索。
                // 注意只有这里会降级——抽取失败已经在 read_turn_body 内部
                // 兜底成原始区间了,那不叫失败,叫「有内容但抽不动」。
                Err(_) => (SNIPPET_SOURCE_CHANGED.to_string(), Vec::new()),
            };
            Ok(SearchHit {
                tid: h.tid,
                rid: h.rid,
                agent_id: h.agent_id,
                resource_path: h.resource_path,
                seq: h.seq,
                role: h.role,
                byte_off: h.byte_off,
                byte_len: h.byte_len,
                snippet,
                highlights,
            })
        })
        .collect()
}

/// `duster open <tid>` 的展示数据:轮次元信息 + 源文件回读的正文全文。
///
/// `tool` / `raw_fallback` 是从 [`TurnBody`] 原样透出的——`open` 与
/// `session show` 现在共用同一套渲染规则(`print_turn_body`),工具轮要
/// 折叠成一行、兜底正文要打明示,这两件事只有回读层知道,不透出来
/// 外壳就得去猜正文是不是原始 JSONL 行。
#[derive(Debug, Serialize)]
pub struct TurnDetail {
    pub resource_path: String,
    pub agent_id: String,
    pub seq: i64,
    pub role: String,
    pub text: String,
    /// 工具轮的工具名;行里带就给,不带是 None(与 [`TurnBody::tool`] 同源)。
    pub tool: Option<String>,
    /// true = `text` 是原始行兜底(抽取失败),外壳必须明示而不是假装散文。
    pub raw_fallback: bool,
}

/// 按 tid 取轮次详情,并按索引里的 `(byte_off, byte_len)` 从源文件回读正文。
///
/// 回读走 [`read_turn_body`]:拿到原始区间后再按 agent 抽成可读散文,
/// 而不是把 JSONL 行原样端给用户。
///
/// - tid 不存在 → 报错(提示用 `duster search` 找有效 tid);
/// - 源文件已删/截短/改写导致回读失败 → 报错说明(索引是派生物,
///   源文件才是事实,重跑 `duster scan` 可修正)。
pub fn open_turn(index_path: Option<&Path>, tid: i64) -> Result<TurnDetail> {
    let idx = open_index(index_path)?;
    let conn = idx.conn();

    let mut stmt = conn
        .prepare(
            "SELECT r.path, r.agent_id, t.seq, t.role, t.byte_off, t.byte_len
             FROM turn t
             JOIN resource r USING (rid)
             WHERE t.tid = ?1",
        )
        .context("failed to prepare turn query statement")?;
    let mut rows = stmt
        .query_map([tid], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .context("failed to query turn")?;

    let Some(row) = rows.next() else {
        bail!("tid {tid} does not exist. Use `duster search` to find a valid tid.");
    };
    let (resource_path, agent_id, seq, role, byte_off, byte_len) =
        row.context("failed to read turn row")?;

    let body =
        read_turn_body(&agent_id, &resource_path, byte_off, byte_len).with_context(|| {
            format!(
                "failed to read back the turn from {resource_path} (the source may have been \
             deleted, truncated, or rewritten; the index is derived data — rerun \
             `duster scan` to repair)"
            )
        })?;

    Ok(TurnDetail {
        resource_path,
        agent_id,
        seq: seq.unwrap_or_default(),
        role: role.unwrap_or_default(),
        text: body.text,
        tool: body.tool,
        raw_fallback: body.raw_fallback,
    })
}

/// 回读一条轮次的正文。**全 workspace 唯一的回读入口**——
/// `duster open` 与 `duster session show` 都走这里。
///
/// 源有三种形态，判据只看索引里那两个数：
/// 1. 普通文件 → `seek` + `read_exact`；
/// 2. `.zst`（`prune` 压过的会话）→ 整体解压再切片；
/// 3. **`byte_len == 0`** → 哨兵：`byte_off` 是**源 SQLite 库里的行 id**，
///    不是文件偏移。opencode 的 `opencode.db`、omp 的 `history.db` 走这条。
///    真实轮次的正文长度永远大于 0，所以这个哨兵不会和前两种撞车。
///
/// 曾经这里只有 1、2 两条，`session show` 自己又抄了一份带第 3 条的：
/// 结果 `duster open` 碰到 opencode 的轮次会拿着行 id 去 seek 一个 `.db`
/// 文件，读出一段二进制垃圾。两份实现只要有一份先长出新形态，
/// 另一份就是错的——所以只留这一份。
pub(crate) fn read_turn_text(
    agent_id: &str,
    path: &str,
    byte_off: i64,
    byte_len: i64,
) -> Result<String> {
    if byte_len == 0 {
        return read_db_turn(agent_id, Path::new(path), byte_off);
    }
    read_span(path, byte_off.max(0) as u64, byte_len.max(0) as u64)
}

/// 回读并抽取一轮的可读正文。**全 workspace 唯一的回读入口**——
/// `duster open` / `duster session show` / 搜索摘要都走这里。
///
/// 先按 [`read_turn_text`] 的三种源形态取原始区间,再按 agent 分派到
/// 适配器把原始行抽成对话正文。抽取返回 None、或抽出来是空的,都兜底成
/// 原始区间(`raw_fallback: true`)——这轮确实有内容,只是没抽动,把原始行
/// 端出去也比假装它没内容强。
///
/// 兜底不是防御性编程:有的 mapper 存的是**整行 JSON** 的区间、有的存的是
/// 更窄的正文区间,而抽取规则只认识整行 JSON;无论哪种形态,原始区间本身
/// 都是这轮内容的忠实写照,兜底照样能看。
pub fn read_turn_body(
    agent_id: &str,
    path: &str,
    byte_off: i64,
    byte_len: i64,
) -> Result<TurnBody> {
    let raw = read_turn_text(agent_id, path, byte_off, byte_len)?;
    let body = duster_adapter::native::body_of_line(agent_id, &raw);
    Ok(match body {
        Some(b) if !b.text.trim().is_empty() => b,
        _ => TurnBody {
            text: raw,
            tool: None,
            raw_fallback: true,
        },
    })
}

/// SQLite 型会话的回读。按 agent 分派到原生适配器。
///
/// 用 agent_id 而不是 mapper 名分派：`resource` 表不存 mapper（那是清单的
/// 属性，不是资源的），而「哪个 agent 的会话住在库里」是一份封闭的短名单。
/// 名单外的 agent 出现这个哨兵只可能是写入方填错了，报错比返回空正文诚实。
fn read_db_turn(agent_id: &str, db: &Path, rowid: i64) -> Result<String> {
    match agent_id {
        "opencode" => duster_adapter::native::opencode_session::read_turn(db, rowid),
        "omp" => duster_adapter::native::omp_session::read_turn(db, rowid),
        other => bail!(
            "this turn is marked as living in a SQLite database (byte_len == 0 means byte_off \
             is a row id), but duster has no database reader for agent `{other}`"
        ),
    }
}

/// 从源文件精确回读 `[off, off+len)` 字节并解码为 UTF-8。
/// 与检索侧的降级回读不同,open 语义是「给我全文」,任何一步失败都报错。
///
/// # 不变量:偏移量永远描述**解压后**的逻辑内容
///
/// `duster prune` 会把陈旧会话就地压成 `.zst`,索引里的 `byte_off` / `byte_len`
/// 一个字节都不改——压缩在这条线以上完全透明。所以这里按后缀分两条路:
/// `.zst` 先整体解压再切片;普通文件走 seek + read_exact 快路径,
/// 不该为了"统一"把几百 MB 的未压缩原件也读进内存。
///
/// 这条分支不是可选的优化:少了它,prune 就成了变相删除——
/// `duster open` / `session show` 会在压缩后集体读不出正文。
fn read_span(path: &str, off: u64, len: u64) -> Result<String> {
    let p = Path::new(path);
    let buf = if duster_fs::zst::is_zst(p) {
        let all = duster_fs::zst::decompress_to_vec(p)
            .context("failed to decompress archived source file")?;
        let end = off
            .checked_add(len)
            .filter(|e| *e <= all.len() as u64)
            .context("source file is too short (it may have been truncated)")?;
        all[off as usize..end as usize].to_vec()
    } else {
        let mut f = std::fs::File::open(p).context("failed to open source file")?;
        f.seek(SeekFrom::Start(off))
            .context("failed to seek to byte offset")?;
        let mut buf = vec![0u8; len as usize];
        f.read_exact(&mut buf)
            .context("source file is too short (it may have been truncated)")?;
        buf
    };
    String::from_utf8(buf).context("byte span is no longer valid UTF-8 (content was rewritten)")
}

/// 在回读的正文里定位 `query`(大小写不敏感,取首个命中),
/// 以命中为中心切 ±[`WINDOW_CHARS`] char 的窗口,并给出窗口内的高亮字节区间。
///
/// 定位失败(正文经 JSON 转义后与原文不再逐字节一致)时降级:
/// 窗口 = 行首 [`FALLBACK_WINDOW_CHARS`] char,highlights 为空。
///
/// 这段逻辑原来住在 duster-index 的检索层;摘要的输入从「原始行」换成了
/// 抽取后的正文之后,它只能跟着正文一起上移——索引层不碰文件、也不认识
/// 正文长什么样,切窗是展示层的活。
pub fn make_snippet(text: &str, query: &str) -> (String, Vec<(usize, usize)>) {
    let Some((m_start, m_end)) = find_ci(text, query) else {
        let end = char_boundary_after(text, 0, FALLBACK_WINDOW_CHARS);
        return (text[..end].to_string(), Vec::new());
    };

    let win_start = char_boundary_before(text, m_start, WINDOW_CHARS);
    let win_end = char_boundary_after(text, m_end, WINDOW_CHARS);
    let snippet = text[win_start..win_end].to_string();
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
    use duster_index::upsert::{self, ResourceRow};
    use duster_model::{Role, TurnRecord};

    /// 会话正文前面刻意留一行控制记录:`byte_off` 因此非零,回读必须真的定位,
    /// 而不是"整个文件读出来正好对上"。
    const PREFIX: &str = "{\"type\":\"session_meta\",\"cwd\":\"/tmp\"}\n";
    const BODY: &str = "hello 你好世界,这是一条测试轮次正文";

    /// 在 `dir` 下建索引库 + 会话源文件,返回 (库路径, 资源行 rid)。
    fn seed_session(dir: &Path) -> (PathBuf, i64) {
        let src = dir.join("session.jsonl");
        std::fs::write(&src, format!("{PREFIX}{BODY}")).unwrap();

        let db = dir.join("index.db");
        let idx = Index::open(&db).unwrap();
        let outcome = upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: "claude".into(),
                kind: "session".into(),
                scope: "user".into(),
                key: "session.jsonl".into(),
                path: src.to_string_lossy().into_owned(),
                size: (PREFIX.len() + BODY.len()) as u64,
                mtime_ns: 0,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
            },
        )
        .unwrap();
        upsert::replace_turns(
            idx.conn(),
            outcome.rid,
            &[TurnRecord {
                seq: 0,
                role: Role::User,
                ts_ms: None,
                byte_off: PREFIX.len() as u64,
                byte_len: BODY.len() as u64,
                text: BODY.to_string(),
            }],
        )
        .unwrap();
        drop(idx); // 释放写锁,让被测函数走只读路径。

        (db, outcome.rid)
    }

    /// 测试夹具:tempdir 里自建索引库 + 会话源文件,返回 (根目录, 库路径, 正文)。
    /// 沿用 status.rs 的 temp_dir + pid 模式(不需要 tempfile 的自动清理)。
    fn build_fixture(tag: &str) -> (PathBuf, PathBuf, &'static str) {
        let dir =
            std::env::temp_dir().join(format!("duster-core-search-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (db, _) = seed_session(&dir);
        (dir, db, BODY)
    }

    fn all() -> SearchFilter {
        SearchFilter {
            agents: Vec::new(),
            limit: 20,
        }
    }

    /// 写一个「一行 JSONL + 抽取后正文」的会话:源文件里是原始行,索引里存的是
    /// 抽取后的散文——与真实 `duster scan` 的落库形状一致,回读时 `body_of_line`
    /// 必须把原始行重新抽回这条散文。
    fn seed_jsonl_session(
        dir: &Path,
        agent_id: &str,
        raw_line: &str,
        body_text: &str,
    ) -> (PathBuf, i64) {
        let src = dir.join("session.jsonl");
        std::fs::write(&src, format!("{raw_line}\n")).unwrap();

        let db = dir.join("index.db");
        let idx = Index::open(&db).unwrap();
        let outcome = upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: agent_id.into(),
                kind: "session".into(),
                scope: "user".into(),
                key: "session.jsonl".into(),
                path: src.to_string_lossy().into_owned(),
                size: (raw_line.len() + 1) as u64,
                mtime_ns: 0,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
            },
        )
        .unwrap();
        upsert::replace_turns(
            idx.conn(),
            outcome.rid,
            &[TurnRecord {
                seq: 0,
                role: Role::User,
                ts_ms: None,
                byte_off: 0,
                byte_len: raw_line.len() as u64,
                text: body_text.to_string(),
            }],
        )
        .unwrap();
        drop(idx); // 释放写锁,让被测函数走只读路径。

        (db, outcome.rid)
    }

    /// prune 把会话压成 `.zst` 之后,`duster open` / `session show` 必须照常
    /// 读得出正文——否则"压缩存档、不删除内容"就是一句空话,prune 等于变相删除。
    #[test]
    fn open_turn_在会话被压缩后依然读得出原文() {
        let dir = tempfile::tempdir().unwrap();
        let (db, rid) = seed_session(dir.path());
        let src = dir.path().join("session.jsonl");

        let before = open_turn(Some(&db), 1).unwrap();
        assert_eq!(before.text, BODY);

        // prune 的处置:压缩 → 往返校验 → 删原件 → 索引行改指 `.zst`。
        let oc = crate::prune::compress_session(&src).unwrap();
        let zst = oc.replaced_by.unwrap();
        assert!(!src.exists(), "原件应已删除");
        let idx = Index::open(&db).unwrap();
        duster_index::query::update_resource_path(idx.conn(), rid, &zst).unwrap();
        drop(idx);

        // byte_off / byte_len 一个字节都没改,读出来必须完全一样。
        let after = open_turn(Some(&db), 1).unwrap();
        assert_eq!(after.text, before.text);
        assert!(after.resource_path.ends_with(".zst"), "{after:?}");
    }

    #[test]
    fn open_turn_回读与写入正文一致() {
        let (dir, db, text) = build_fixture("roundtrip");
        // 全新库里第一条 turn 的 rowid 必为 1。
        let detail = open_turn(Some(&db), 1).unwrap();
        assert_eq!(detail.text, text);
        assert_eq!(detail.agent_id, "claude");
        assert_eq!(detail.seq, 0);
        assert_eq!(detail.role, "user");
        assert!(detail.resource_path.ends_with("session.jsonl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_turn_tid_不存在报错() {
        let (dir, db, _) = build_fixture("missing-tid");
        let err = open_turn(Some(&db), 9999).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_turn_源文件截短后报错() {
        let (dir, db, _) = build_fixture("truncated");
        std::fs::write(dir.join("session.jsonl"), "x").unwrap();
        let err = open_turn(Some(&db), 1).unwrap_err();
        assert!(
            format!("{err:#}").contains("failed to read back the turn from"),
            "{err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_库不存在时提示先跑_scan() {
        let missing = std::env::temp_dir().join(format!(
            "duster-core-search-miss-{}/nope.db",
            std::process::id()
        ));
        // SearchHit 未派生 Debug,unwrap_err 用不了;走 Option 通道取错误。
        let err = search(Some(&missing), "hello", &all()).err().unwrap();
        assert!(err.to_string().contains("duster scan"), "{err:#}");
    }

    #[test]
    fn search_能命中自建库() {
        let (dir, db, _) = build_fixture("hit");
        let hits = search(Some(&db), "你好世界", &all()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tid, 1);
        assert_eq!(hits[0].agent_id, "claude");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 搜索摘要来自抽取后的正文,而不是原始 JSONL 行:命中正文里的词,
    /// 摘要里不该出现 `{` / `"role"` 这类 JSON 痕迹。
    #[test]
    fn search_摘要是散文_不是原始_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let raw = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"lorem"},{"type":"text","text":"先看日志,再定位 docker 配置。"}]},"timestamp":"2026-08-01T10:00:00.000Z"}"#;
        let (db, _) = seed_jsonl_session(
            dir.path(),
            "claude-code",
            raw,
            "先看日志,再定位 docker 配置。",
        );

        let hits = search(Some(&db), "docker", &all()).unwrap();
        assert_eq!(hits.len(), 1);
        let snip = &hits[0].snippet;
        assert!(snip.contains("docker"), "{snip}");
        assert!(
            !snip.contains('{') && !snip.contains("\"role\""),
            "摘要泄漏了 JSON:{snip}"
        );
    }

    /// prune 把会话压成 `.zst` 之后,搜索摘要必须照常出真实正文——旧检索层只认
    /// 普通文件,一碰压缩包就降级成 `[source file changed]`,prune 等于变相让
    /// 会话从搜索结果里消失。
    #[test]
    fn search_命中_zst_压缩会话出真实摘要() {
        let dir = tempfile::tempdir().unwrap();
        let raw = r#"{"type":"user","message":{"role":"user","content":"帮我看看 docker 报错"},"timestamp":"2026-08-01T10:00:00.000Z"}"#;
        let (db, rid) = seed_jsonl_session(dir.path(), "claude-code", raw, "帮我看看 docker 报错");
        let src = dir.path().join("session.jsonl");

        // prune 的处置:压缩 → 往返校验 → 删原件 → 索引行改指 `.zst`。
        let oc = crate::prune::compress_session(&src).unwrap();
        let zst = oc.replaced_by.unwrap();
        assert!(!src.exists(), "原件应已删除");
        let idx = Index::open(&db).unwrap();
        duster_index::query::update_resource_path(idx.conn(), rid, &zst).unwrap();
        drop(idx);

        let hits = search(Some(&db), "docker", &all()).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("docker"), "{}", hits[0].snippet);
        let (s, e) = hits[0].highlights[0];
        assert_eq!(&hits[0].snippet[s..e], "docker");
    }

    /// `history.db` 型轮次(`byte_len == 0`, `byte_off` 是行 id)的搜索摘要
    /// 必须出真实正文——旧检索层拿行 id 去 seek 一个 `.db` 文件,
    /// 读出来不是空就是二进制垃圾。
    #[test]
    fn search_命中_sqlite_行_id_轮次出真实摘要() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("history.db");
        let ids = duster_adapter::native::omp_session::fixture_db(
            &db_path,
            "s1",
            &[("user", "如何用 docker 排查内存泄漏")],
        )
        .unwrap();
        assert_eq!(ids.len(), 1);

        let idx_db = dir.path().join("index.db");
        let idx = Index::open(&idx_db).unwrap();
        let out = upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: "omp".into(),
                kind: "session".into(),
                scope: "user".into(),
                key: "history.db".into(),
                path: db_path.to_string_lossy().into_owned(),
                size: 0,
                mtime_ns: 0,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
            },
        )
        .unwrap();
        upsert::replace_turns(
            idx.conn(),
            out.rid,
            &[TurnRecord {
                seq: 0,
                role: Role::User,
                ts_ms: None,
                byte_off: ids[0] as u64,
                byte_len: 0,
                text: "如何用 docker 排查内存泄漏".to_string(),
            }],
        )
        .unwrap();
        drop(idx);

        let hits = search(Some(&idx_db), "docker", &all()).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("docker"), "{}", hits[0].snippet);
        let (s, e) = hits[0].highlights[0];
        assert_eq!(&hits[0].snippet[s..e], "docker");
    }

    /// 抽取失败(控制记录 / 名单外的 agent)时兜底原始区间,raw_fallback == true——
    /// 这轮有内容,只是没抽动,原始行端出去也比假装没内容强。
    #[test]
    fn read_turn_body_抽不动的行兜底原始区间() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("session.jsonl");
        let raw = r#"{"type":"ai-title","aiTitle":"标题","sessionId":"s1"}"#;
        std::fs::write(&src, raw).unwrap();
        let path = src.to_str().unwrap();

        // 控制记录:原始区间读得回来,但抽不出正文 → 兜底。
        let body = read_turn_body("claude-code", path, 0, raw.len() as i64).unwrap();
        assert!(body.raw_fallback);
        assert_eq!(body.text, raw);

        // 名单外的 agent 同理:抽取规则不认识它,原始区间就是正文。
        let body = read_turn_body("other-agent", path, 0, raw.len() as i64).unwrap();
        assert!(body.raw_fallback);
        assert_eq!(body.text, raw);

        // 能抽动的行:散文正文 + raw_fallback == false,不兜底。
        let raw = r#"{"type":"user","message":{"role":"user","content":"帮我看看 docker 报错"}}"#;
        std::fs::write(&src, raw).unwrap();
        let body = read_turn_body("claude-code", path, 0, raw.len() as i64).unwrap();
        assert!(!body.raw_fallback);
        assert_eq!(body.text, "帮我看看 docker 报错");
    }

    /// 中文子串命中,高亮区间落在 char 边界:切片不 panic 且等于 query。
    #[test]
    fn make_snippet_中文命中_char_边界安全() {
        let text = "今天我们讨论了全文检索的实现方案,还聊到 emoji 🦀 的边界问题";
        let (snippet, highlights) = make_snippet(text, "全文检索");
        assert_eq!(highlights.len(), 1);
        let (s, e) = highlights[0];
        // 切片本身就是 char 边界断言:越界或劈开 char 会 panic。
        assert_eq!(&snippet[s..e], "全文检索");
    }

    /// 英文大小写不敏感:定位与高亮都不区分大小写。
    #[test]
    fn make_snippet_英文大小写不敏感() {
        let text = "We SHOULD use Trigram Tokenizer for CJK";
        let (snippet, highlights) = make_snippet(text, "trigram tokenizer");
        let (s, e) = highlights[0];
        assert!(snippet[s..e].eq_ignore_ascii_case("trigram tokenizer"));
    }

    /// 定位失败(正文经 JSON 转义后与原文不再逐字节一致)降级为
    /// 行首 [`FALLBACK_WINDOW_CHARS`] char 窗口 + 空 highlights。
    #[test]
    fn make_snippet_定位失败_降级行首窗口() {
        let text = "没有目标的正文";
        let (snippet, highlights) = make_snippet(text, "不存在的词");
        assert!(highlights.is_empty());
        assert_eq!(snippet, text); // 短于 160 char,窗口就是全文
    }

    /// 长文本:窗口按 char 截取,中文长文不 panic,且窗口包含命中。
    #[test]
    fn make_snippet_长文本窗口_char_边界() {
        let text = format!("{}目标子串{}", "前缀甲".repeat(100), "后缀乙".repeat(100));
        let (snippet, highlights) = make_snippet(&text, "目标子串");
        // 窗口 = 命中 4 char + 两侧各 80 char。
        assert_eq!(snippet.chars().count(), 4 + WINDOW_CHARS * 2);
        let (s, e) = highlights[0];
        assert_eq!(&snippet[s..e], "目标子串");
    }

    /// 用户输入里的 FTS 语法字符被当作朴素子串,定位照常。
    #[test]
    fn make_snippet_语法字符当子串() {
        let text = r#"包含 AND OR NOT "引号" 与 * 号的正文"#;
        let (snippet, highlights) = make_snippet(text, r#""引号" 与 *"#);
        let (s, e) = highlights[0];
        assert_eq!(&snippet[s..e], r#""引号" 与 *"#);
    }
}
