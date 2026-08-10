//! `duster search` / `duster open`:检索用例,薄封装 duster-index::search。
//!
//! 本模块是 CLI 触达检索能力的唯一合法通道:CLI 只依赖 duster-core,
//! 这里把 `SearchHit` / `SearchFilter` 原样转出,并补上两件编排层的事——
//! 索引库路径解析(缺省 `~/.agent-duster/index.db`,与 status 一致)与
//! `open` 场景下按字节区间从源文件回读**全文**(search 只给摘要窗口)。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_index::db::Index;

pub use duster_index::search::{SearchFilter, SearchHit};

/// 解析索引库路径并只读打开。库不存在时给人话提示,引导先跑 `duster scan`。
fn open_index(index_path: Option<&Path>) -> Result<Index> {
    let path: PathBuf = match index_path {
        Some(p) => p.to_path_buf(),
        None => duster_fs::path::expand_tilde("~/.agent-duster/index.db"),
    };
    if !path.is_file() {
        bail!(
            "索引库不存在: {}。请先运行 `duster scan` 建立索引。",
            path.display()
        );
    }
    Index::open_readonly(&path).with_context(|| format!("只读打开索引失败: {}", path.display()))
}

/// 全文检索会话正文。`index_path` 缺省 `~/.agent-duster/index.db`。
///
/// 查询语义见 [`duster_index::search::search_turns`]:朴素子串,
/// 不解释 FTS5 操作符;短于 3 个字符的查询直接返回空。
pub fn search(
    index_path: Option<&Path>,
    query: &str,
    filter: &SearchFilter,
) -> Result<Vec<SearchHit>> {
    let idx = open_index(index_path)?;
    duster_index::search::search_turns(idx.conn(), query, filter)
}

/// `duster open <tid>` 的展示数据:轮次元信息 + 源文件回读的正文全文。
#[derive(Debug, Serialize)]
pub struct TurnDetail {
    pub resource_path: String,
    pub agent_id: String,
    pub seq: i64,
    pub role: String,
    pub text: String,
}

/// 按 tid 取轮次详情,并按索引里的 `(byte_off, byte_len)` 从源文件回读原文。
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
        .context("准备查询轮次语句失败")?;
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
        .context("查询轮次失败")?;

    let Some(row) = rows.next() else {
        bail!("tid {tid} 不存在。请先用 `duster search` 找到有效的 tid。");
    };
    let (resource_path, agent_id, seq, role, byte_off, byte_len) = row.context("读取轮次行失败")?;

    let text = read_span(
        &resource_path,
        byte_off.max(0) as u64,
        byte_len.max(0) as u64,
    )
    .with_context(|| {
        format!(
            "回读源文件失败: {resource_path}(文件可能已删除、截短或改写;\
             索引是派生物,可重跑 `duster scan` 修正)"
        )
    })?;

    Ok(TurnDetail {
        resource_path,
        agent_id,
        seq: seq.unwrap_or_default(),
        role: role.unwrap_or_default(),
        text,
    })
}

/// 从源文件精确回读 `[off, off+len)` 字节并解码为 UTF-8。
/// 与检索侧的降级回读不同,open 语义是「给我全文」,任何一步失败都报错。
fn read_span(path: &str, off: u64, len: u64) -> Result<String> {
    let mut f = std::fs::File::open(path).context("打开源文件失败")?;
    f.seek(SeekFrom::Start(off)).context("定位字节偏移失败")?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf)
        .context("源文件长度不足(可能已被截短)")?;
    String::from_utf8(buf).context("该区间不再是合法 UTF-8(内容已改写)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use duster_index::upsert::{self, ResourceRow};
    use duster_model::{Role, TurnRecord};

    /// 测试夹具:tempdir 里自建索引库 + 会话源文件,返回 (根目录, 库路径, 正文)。
    /// 不引 tempfile(core 无该 dev 依赖),沿用 status.rs 的 temp_dir + pid 模式。
    fn build_fixture(tag: &str) -> (PathBuf, PathBuf, &'static str) {
        let dir =
            std::env::temp_dir().join(format!("duster-core-search-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let text = "hello 你好世界,这是一条测试轮次正文";
        let src = dir.join("session.jsonl");
        std::fs::write(&src, text).unwrap();

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
                size: text.len() as u64,
                mtime_ns: 0,
                hash_content: None,
                cheap_print: None,
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
                byte_len: text.len() as u64,
                text: text.to_string(),
            }],
        )
        .unwrap();
        drop(idx); // 释放写锁,让被测函数走只读路径。

        (dir, db, text)
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
        assert!(err.to_string().contains("不存在"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_turn_源文件截短后报错() {
        let (dir, db, _) = build_fixture("truncated");
        std::fs::write(dir.join("session.jsonl"), "x").unwrap();
        let err = open_turn(Some(&db), 1).unwrap_err();
        assert!(format!("{err:#}").contains("回读源文件失败"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_库不存在时提示先跑_scan() {
        let missing = std::env::temp_dir().join(format!(
            "duster-core-search-miss-{}/nope.db",
            std::process::id()
        ));
        let filter = SearchFilter {
            agent: None,
            limit: 20,
        };
        // SearchHit 未派生 Debug,unwrap_err 用不了;走 Option 通道取错误。
        let err = search(Some(&missing), "hello", &filter).err().unwrap();
        assert!(err.to_string().contains("duster scan"), "{err:#}");
    }

    #[test]
    fn search_能命中自建库() {
        let (dir, db, _) = build_fixture("hit");
        let filter = SearchFilter {
            agent: None,
            limit: 20,
        };
        let hits = search(Some(&db), "你好世界", &filter).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tid, 1);
        assert_eq!(hits[0].agent_id, "claude");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
