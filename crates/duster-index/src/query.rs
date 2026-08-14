//! 只读查询：把索引里的资源行取出来给用例层做删除计划。
//!
//! 存在的理由是分层：`clean` / `prune` / `uninstall` 的计划生成是**纯读索引**
//! 的（不重跑 probe、不重解析清单），而 duster-core 不依赖 rusqlite。
//! 所有 SQL 收在这里，向上只暴露普通结构体。
//!
//! 与 [`crate::search`] 的分工：那边是全文检索（FTS5），这边是结构化查询。

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, ToSql};

/// 一条资源行的完整快照。字段与 `resource` 表一一对应。
#[derive(Debug, Clone)]
pub struct ResourceRecord {
    pub rid: i64,
    pub agent_id: String,
    pub kind: String,
    pub scope: String,
    pub key: String,
    pub path: String,
    /// 占用字节数。skill 行已扣除 `install_bytes`。
    pub size: u64,
    pub mtime_ns: i64,
    /// `l0` / `l1` / `l2`；非 artifact 恒为 None。
    pub clean_level: Option<String>,
    /// 清掉能拿回多少（l0 只算空洞）；非 artifact 恒为 None。
    pub reclaimable: Option<u64>,
    /// 资源目录内部属于软件本体的字节数（skill 里的 node_modules 等）。
    pub install_bytes: Option<u64>,
    /// 清单声明的「保留最新 N 份代际」；只有带 glob 的 stats-only 行有值。
    /// 恒为 ≥ 1（清单校验拒绝 0——那等于连最后一份兜底一起删）。
    pub keep_generations: Option<u32>,
    /// 清单声明的 mapper 名。NULL = 未知（历史行/手写行），读方按
    /// 「不是 stats-only」处理——只有显式声明的 stats-only 才被过滤。
    pub mapper: Option<String>,
    pub hash_content: Option<[u8; 32]>,
}

impl ResourceRecord {
    /// mtime 的 Unix 毫秒表示。
    pub fn mtime_ms(&self) -> i64 {
        self.mtime_ns / 1_000_000
    }
}

/// 资源查询过滤器。空 Vec = 不按该维度过滤。
#[derive(Debug, Clone, Default)]
pub struct ResourceFilter {
    /// 只看这几个 agent 的资源;空 Vec = 全部。
    pub agents: Vec<String>,
    /// `mcp` / `skill` / `memory` / `session` / `artifact` / `install`。
    pub kinds: Vec<String>,
    /// `l0` / `l1` / `l2`。
    pub clean_levels: Vec<String>,
}

/// `resource` 表的全列投影，顺序与 [`row_to_record`] 的下标一一对应。
/// 集中一处，避免每个查询各写一遍列名导致下标漂移。
const RESOURCE_COLS: &str = "rid, agent_id, kind, scope, key, path, size, mtime_ns, \
                             clean_level, reclaimable, install_bytes, keep_generations, mapper, hash_content";

/// 把一行 [`RESOURCE_COLS`] 投影解成 [`ResourceRecord`]。
fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResourceRecord> {
    // hash_content 是 32 字节 BLOB。历史行/写坏的行可能长度不对，
    // 这里静默降级为 None 而不是报错：哈希只是去重的加速手段，
    // 缺一个哈希最多漏掉一对重复，不该让整个列表查询失败。
    let hash: Option<Vec<u8>> = row.get("hash_content")?;
    let hash_content = hash.and_then(|h| <[u8; 32]>::try_from(h.as_slice()).ok());

    Ok(ResourceRecord {
        rid: row.get("rid")?,
        agent_id: row.get("agent_id")?,
        kind: row.get("kind")?,
        scope: row.get("scope")?,
        key: row.get("key")?,
        path: row.get("path")?,
        size: row.get("size")?,
        mtime_ns: row.get("mtime_ns")?,
        clean_level: row.get("clean_level")?,
        reclaimable: row.get("reclaimable")?,
        install_bytes: row.get("install_bytes")?,
        // 历史库/写坏的行可能带非法值（0、负数），降级为 None——缺一个
        // 代际声明最多让 surplus 判定退化回旧行为（按龄清理），不该让
        // 整个列表查询失败。
        keep_generations: row
            .get::<_, Option<i64>>("keep_generations")?
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n >= 1),
        mapper: row.get("mapper")?,
        hash_content,
    })
}

/// 按过滤器列出资源行，按 `(agent_id, kind, key)` 稳定排序。
pub fn list_resources(conn: &Connection, filter: &ResourceFilter) -> Result<Vec<ResourceRecord>> {
    // SQL 是拼出来的，但拼进去的**只有生成的占位符序号**（`?1`、`?2`…），
    // 过滤值一律走绑定参数——kind / clean_level 虽然来自内部枚举，
    // agent 却直接来自命令行，绝不能进字符串。
    let mut where_parts: Vec<String> = Vec::new();
    let mut params: Vec<&dyn ToSql> = Vec::new();

    // agent 过滤是动态 IN：占位符个数跟着 `agents` 走，空 Vec = 不设约束
    // （不是"匹配空集"）。值走绑定参数而不是拼进 SQL——agent id 来自
    // `--agent`，拼字符串等于把注入的口子递到命令行手里。
    if !filter.agents.is_empty() {
        let first = params.len() + 1;
        for a in &filter.agents {
            params.push(a);
        }
        let holes = (first..=params.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        where_parts.push(format!("agent_id IN ({holes})"));
    }
    // 空 Vec = 该维度不设约束（不是"匹配空集"），所以只在非空时加子句。
    for (col, values) in [
        ("kind", &filter.kinds),
        ("clean_level", &filter.clean_levels),
    ] {
        if values.is_empty() {
            continue;
        }
        let first = params.len() + 1;
        for v in values.iter() {
            params.push(v);
        }
        let holes = (first..=params.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        where_parts.push(format!("{col} IN ({holes})"));
    }

    let mut sql = format!("SELECT {RESOURCE_COLS} FROM resource");
    if !where_parts.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_parts.join(" AND "));
    }
    sql.push_str(" ORDER BY agent_id, kind, key");

    let mut stmt = conn
        .prepare(&sql)
        .context("failed to prepare resource listing")?;
    let rows = stmt
        .query_map(params.as_slice(), row_to_record)
        .context("failed to execute resource listing")?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read resource rows")
}

/// 索引里已知的全部 agent id，按字典序。
pub fn agent_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT agent_id FROM agent ORDER BY agent_id")
        .context("failed to prepare agent listing")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .context("failed to execute agent listing")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read agent rows")
}

/// agent 的展示名（`agent.display_name`）。
pub fn agent_display_name(conn: &Connection, agent: &str) -> Result<Option<String>> {
    // 两层 Option 摊平成一层：行不存在与 display_name 为 NULL 对调用方
    // 是同一件事（没有可展示的名字），都退回用 agent_id 显示。
    let name: Option<Option<String>> = conn
        .query_row(
            "SELECT display_name FROM agent WHERE agent_id = ?1",
            [agent],
            |r| r.get(0),
        )
        .optional()
        .with_context(|| format!("failed to query display name for agent: {agent}"))?;
    Ok(name.flatten())
}

/// 某资源（会话文件）里最晚一条轮次的时间戳（Unix 毫秒）。
///
/// prune 判定会话是否陈旧的**唯一口径**：取轮次最大 ts，不看文件 mtime——
/// 文件可能因为备份工具、云盘同步被摸过，轮次时间戳才是"人什么时候用的"。
/// 没有任何带 ts 的轮次返回 None，调用方据此**视为最近用过**（宁可漏杀）。
pub fn last_turn_ms(conn: &Connection, rid: i64) -> Result<Option<i64>> {
    // 聚合查询恒返回一行：没有轮次（或 ts_ms 全为 NULL）时该行是 NULL。
    conn.query_row("SELECT MAX(ts_ms) FROM turn WHERE rid = ?1", [rid], |r| {
        r.get::<_, Option<i64>>(0)
    })
    .with_context(|| format!("failed to query last turn ts for resource {rid}"))
}

/// 一条轮次行的元信息。**不含正文**——正文不落库（`fts_turn` 是
/// contentless 的），要看正文必须按 `(byte_off, byte_len)` 回源。
#[derive(Debug, Clone)]
pub struct TurnRow {
    pub tid: i64,
    pub seq: i64,
    pub role: String,
    pub ts_ms: Option<i64>,
    /// 源文件里的字节偏移；**`byte_len == 0` 时它是源库里的行 id**
    /// （SQLite 型会话没有字节区间，见 `duster_adapter::native::opencode_session`）。
    pub byte_off: i64,
    pub byte_len: i64,
}

/// 一个资源的全部轮次，按 `seq` 升序（`seq` 相同再按 `tid`，保证稳定）。
///
/// `session show` / `session export` 的取数口径：先拿区间，再逐条回源。
/// 排序落在 SQL 里而不是调用方，是因为「轮次的顺序」是这张表的语义，
/// 不是某个展示层的偏好。
pub fn list_turns(conn: &Connection, rid: i64) -> Result<Vec<TurnRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT tid, seq, role, ts_ms, byte_off, byte_len
             FROM turn WHERE rid = ?1 ORDER BY seq, tid",
        )
        .context("failed to prepare turn listing statement")?;
    let rows = stmt
        .query_map([rid], |r| {
            Ok(TurnRow {
                tid: r.get(0)?,
                seq: r.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                role: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                ts_ms: r.get(3)?,
                byte_off: r.get::<_, Option<i64>>(4)?.unwrap_or_default(),
                byte_len: r.get::<_, Option<i64>>(5)?.unwrap_or_default(),
            })
        })
        .with_context(|| format!("failed to query turns for resource {rid}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.with_context(|| format!("failed to read a turn row of resource {rid}"))?);
    }
    Ok(out)
}

/// 一个资源的轮次条数。`session list` 每行都要这个数，而列表可能有上千行——
/// 走 COUNT 而不是把区间全取回来再 `.len()`，省的是几万次无谓的行解码。
pub fn turn_count(conn: &Connection, rid: i64) -> Result<u64> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM turn WHERE rid = ?1", [rid], |r| {
            r.get(0)
        })
        .with_context(|| format!("failed to count turns for resource {rid}"))?;
    Ok(n.max(0) as u64)
}

/// 一个资源的**第一条**轮次。
///
/// `session list` 靠它还原 cwd：会话的工作目录只存在于内容里（Claude 的
/// 目录名是不可逆编码，索引也不存这一列），只能回读第一条轮次的原文去找。
/// 单独一个 `LIMIT 1` 而不是复用 [`list_turns`]：列表一行只需要这一条，
/// 把一场几千轮的会话整个取回来只为看第一行，代价全白付。
pub fn first_turn(conn: &Connection, rid: i64) -> Result<Option<TurnRow>> {
    conn.query_row(
        "SELECT tid, seq, role, ts_ms, byte_off, byte_len
         FROM turn WHERE rid = ?1 ORDER BY seq, tid LIMIT 1",
        [rid],
        |r| {
            Ok(TurnRow {
                tid: r.get(0)?,
                seq: r.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                role: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                ts_ms: r.get(3)?,
                byte_off: r.get::<_, Option<i64>>(4)?.unwrap_or_default(),
                byte_len: r.get::<_, Option<i64>>(5)?.unwrap_or_default(),
            })
        },
    )
    .optional()
    .with_context(|| format!("failed to query the first turn of resource {rid}"))
}

/// 一个词元最近一次出现在会话正文里的时间（Unix 毫秒）。
///
/// prune 判定 MCP 是否陈旧的口径。走 FTS 找到含该词元的轮次，
/// 取最大 ts。**查不到证据返回 None**——没证据不等于没用过，调用方决定怎么用。
///
/// 词元短于 trigram 分词器的 3 字符下限时同样返回 None：倒排索引里根本
/// 没有对应 token，检索无从谈起，这也是"没有证据"而不是"没有出现过"。
///
/// 与 crate::search 同一套降级：双引号包裹 + 内部 `"` 翻倍，把词元整体
/// 降级成一个字符串词元，`-`、`*`、`AND` 都不再是操作符。MCP 名字里带
/// `@scope/pkg` 很常见，不能裸拼进 MATCH。
///
/// skill 不再走这条路：正文提到技能名只是引用不是调用，判陈旧看
/// [`skill_last_invoked_ms`] 的调用记录。
fn session_evidence_ms(conn: &Connection, term: &str) -> Result<Option<i64>> {
    if term.chars().count() < 3 {
        return Ok(None);
    }
    let fts_query = format!("\"{}\"", term.replace('"', "\"\""));

    conn.query_row(
        "SELECT MAX(t.ts_ms)
         FROM fts_turn
         JOIN turn t ON fts_turn.rowid = t.tid
         WHERE fts_turn MATCH ?1",
        [&fts_query],
        |r| r.get::<_, Option<i64>>(0),
    )
    .with_context(|| format!("failed to query session evidence for: {term}"))
}

/// MCP server 名字最近一次出现在会话正文里的时间（Unix 毫秒）。
///
/// prune 判定 MCP 是否陈旧的口径。走 FTS 找到含该名字的轮次，取最大 ts。
/// **查不到证据返回 None，调用方必须视为"最近用过"，永不判定为陈旧**——
/// 一个 MCP 可能装着就是为了偶尔用一次，没证据不等于没用过。
pub fn mcp_last_seen_ms(conn: &Connection, server_name: &str) -> Result<Option<i64>> {
    session_evidence_ms(conn, server_name)
}

/// skill 最近一次被**真实调用**的时间（Unix 毫秒）。
///
/// 证据是 `skill_event` 表（会话解析器从工具调用记录里提取的事件），不是
/// 会话正文里的名字出现——正文提到技能名只说明被引用，不说明被调用。
/// 匹配声明名**或**目录 basename：7 份真实副本两者不同（`design-taste-frontend`
/// 在 `taste-skill/`、`open-gstack-browser` 在 `connect-chrome/` 里……），
/// codex 的记录带的是目录名，只按声明名匹配会凭空造出新误删。
///
/// 查不到返回 None。调用方（plan 层）在 `skill_evidence_ready` 为假时
/// 根本不问这个问题——空表是「没查过」，不是「从没调用过」。
pub fn skill_last_invoked_ms(
    conn: &Connection,
    declared_name: &str,
    dir_name: &str,
) -> Result<Option<i64>> {
    // 两个名字相同时 IN 列表去重是锦上添花;不去重也只是同一行查两遍,
    // MAX 结果不变,不值得为它拼动态占位符。
    conn.query_row(
        "SELECT MAX(ts_ms) FROM skill_event WHERE skill IN (?1, ?2)",
        [declared_name, dir_name],
        |r| r.get::<_, Option<i64>>(0),
    )
    .with_context(|| {
        format!("failed to query last skill invocation for {declared_name:?}/{dir_name:?}")
    })
}

/// 技能调用证据是否已采集（`meta` 标记，见 [`crate::meta::SKILL_EVIDENCE_READY`]）。
///
/// 旧索引升级而来、尚未重扫时 `skill_event` 是空表——"没查过"不能读成
/// "从没调用过"。plan 层据此冻结 skill 的陈旧判定，直到一次真正的 scan
/// 把表填上。
pub fn skill_evidence_ready(conn: &Connection) -> Result<bool> {
    Ok(crate::meta::get(conn, crate::meta::SKILL_EVIDENCE_READY)?.is_some())
}

/// 全部 `install` 行的体积合计，外加 skill 行里内嵌的 `install_bytes`。
///
/// `agents` 为空时统计全部 agent。这是「软件本体」那一桶的数字，
/// clean / prune 报告都要报它——让用户看见那几个 GB 为什么不动。
pub fn install_total(conn: &Connection, agents: &[String]) -> Result<u64> {
    // 两部分一次查完：整行就是软件本体的 `install` 行取 size，
    // 以及混在资源目录里的本体字节（install_bytes，除 skill 行外恒为 NULL）。
    // artifact / session 行两部分都不贡献，天然被排除。
    //
    // 过滤子句与 list_resources 同一套动态 IN：占位符序号可以拼，
    // 值必须绑定——agent id 来自命令行，拼字符串等于把注入的口子
    // 递到 `--agent` 手里。空 Vec 时不加 WHERE（不是 `IN ()`，那是语法错误）。
    let mut params: Vec<&dyn ToSql> = Vec::new();
    let mut agent_where = String::new();
    if !agents.is_empty() {
        for a in agents {
            params.push(a);
        }
        let holes = (1..=params.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        agent_where = format!(" WHERE agent_id IN ({holes})");
    }
    let total: i64 = conn
        .query_row(
            &format!(
                "SELECT COALESCE(SUM(CASE WHEN kind = 'install' THEN size END), 0)
                      + COALESCE(SUM(install_bytes), 0)
                 FROM resource{agent_where}"
            ),
            params.as_slice(),
            |r| r.get(0),
        )
        .context("failed to sum install bucket")?;
    Ok(total.max(0) as u64)
}

/// 按 skill 名分组的全部 skill 行（跨 agent）。copies 与 link 的输入。
///
/// 注意 key 可能是 `name@目录名` 形式（同名冲突时 scan 会退化命名），
/// 分组要用**还原后的 skill 名**，不是 key 原文。
pub fn skill_groups(conn: &Connection) -> Result<BTreeMap<String, Vec<ResourceRecord>>> {
    let rows = list_resources(
        conn,
        &ResourceFilter {
            kinds: vec!["skill".to_string()],
            ..Default::default()
        },
    )?;

    let mut groups: BTreeMap<String, Vec<ResourceRecord>> = BTreeMap::new();
    for row in rows {
        groups.entry(skill_name_of(&row.key)).or_default().push(row);
    }
    Ok(groups)
}

/// 从资源 key 还原 skill 名：`name@目录名` 取 `name`，无 `@` 取原文。
///
/// 按**最后一个** `@` 切：skill 名本身可能含 `@`（`@scope/pkg` 风格），
/// 而退化后缀总是追加在末尾。切出来的名字为空（key 形如 `@dir`）时
/// 退回原文——那不是退化命名，是一个正经以 `@` 开头的名字。
///
/// 导出给 plan 层：`last_used` 判定 skill 陈旧时要拿还原后的名字去查调用
/// 证据（退化命名的 `name@目录名` 按原名检索，见 [`skill_last_invoked_ms`]）。
pub fn skill_name_of(key: &str) -> String {
    match key.rsplit_once('@') {
        Some((name, _)) if !name.is_empty() => name.to_string(),
        _ => key.to_string(),
    }
}

/// 更新一条资源行的 `path`（会话被压缩成 `.zst` 后指向新文件）。
///
/// 只改 path，不碰 turn 表：byte_off/byte_len 指向的是**解压后**的逻辑内容，
/// 压缩对检索与回读完全透明。
pub fn update_resource_path(conn: &Connection, rid: i64, new_path: &str) -> Result<()> {
    conn.execute(
        "UPDATE resource SET path = ?2 WHERE rid = ?1",
        rusqlite::params![rid, new_path],
    )
    .with_context(|| format!("failed to update path for resource {rid}"))?;
    Ok(())
}

/// 删除一条资源行（连同 `turn` 与 `fts_turn`）。prune / uninstall 删完文件后收尾。
pub fn delete_resource(conn: &Connection, rid: i64) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .context("failed to begin delete_resource transaction")?;

    // 顺序不能反：fts_turn 是 contentless 表，与 turn/resource 之间没有任何
    // 外键关系（见 upsert.rs 顶部说明），必须先靠 turn.tid 反查把它清干净。
    // 漏删会留下孤儿 FTS 行——rowid 被后续 turn 复用时会撞成脏命中。
    tx.execute(
        "DELETE FROM fts_turn WHERE rowid IN (SELECT tid FROM turn WHERE rid = ?1)",
        [rid],
    )
    .context("failed to delete fts_turn rows")?;
    // turn 虽有 ON DELETE CASCADE，仍显式删：foreign_keys pragma 是连接级的，
    // 不依赖调用方开没开。
    tx.execute("DELETE FROM turn WHERE rid = ?1", [rid])
        .context("failed to delete turn rows")?;
    tx.execute("DELETE FROM resource WHERE rid = ?1", [rid])
        .context("failed to delete resource row")?;

    tx.commit()
        .context("failed to commit delete_resource transaction")
}

/// 删除一条 agent 记录（`uninstall` 收尾）。返回删除行数（0 = 本来就没有）。
///
/// 调用方必须**先**把该 agent 的全部资源行走 [`delete_resource`] 删掉：
/// `resource` 对 `agent` 没有外键，更没有级联，留下来就是一堆悬空行——
/// 下次 scan 又不会重新探测到这个 agent，它们永远不会被 stale 清理捡走。
pub fn delete_agent(conn: &Connection, agent_id: &str) -> Result<u64> {
    let n = conn
        .execute("DELETE FROM agent WHERE agent_id = ?1", [agent_id])
        .with_context(|| format!("failed to delete agent row: {agent_id}"))?;
    Ok(n as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Index;
    use crate::upsert::{self, ResourceRow};
    use duster_model::{Role, TurnRecord};

    /// 真开一个临时库（不是 in-memory）：`Index::open` 还要抢旁路写锁、
    /// 建父目录，这些都只在真文件路径上成立，测试要覆盖同一条路径。
    /// 返回的 TempDir 必须被持有到断言结束，drop 即删目录。
    fn open() -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("index.db")).unwrap();
        (dir, index)
    }

    fn seed(
        conn: &Connection,
        agent: &str,
        kind: &str,
        key: &str,
        size: u64,
        level: Option<&str>,
    ) -> i64 {
        upsert::upsert_resource(
            conn,
            &ResourceRow {
                agent_id: agent.into(),
                kind: kind.into(),
                scope: "global".into(),
                key: key.into(),
                path: format!("/tmp/{agent}/{kind}/{key}"),
                size,
                mtime_ns: 1_700_000_000_000_000_000,
                hash_content: Some([9u8; 32]),
                cheap_print: None,
                clean_level: level.map(str::to_string),
                reclaimable: level.map(|_| size),
                install_bytes: None,
                mapper: None,
            },
        )
        .unwrap()
        .rid
    }

    fn seed_skill(conn: &Connection, agent: &str, key: &str, size: u64, install: u64) -> i64 {
        upsert::upsert_resource(
            conn,
            &ResourceRow {
                agent_id: agent.into(),
                kind: "skill".into(),
                scope: "global".into(),
                key: key.into(),
                path: format!("/tmp/{agent}/skills/{key}"),
                size,
                mtime_ns: 1_700_000_000_000_000_000,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: Some(install),
                mapper: None,
            },
        )
        .unwrap()
        .rid
    }

    fn turn(seq: u32, ts_ms: i64, text: &str) -> TurnRecord {
        TurnRecord {
            seq,
            role: Role::User,
            ts_ms: Some(ts_ms),
            byte_off: 0,
            byte_len: text.len() as u64,
            text: text.into(),
        }
    }

    fn keys(rows: &[ResourceRecord]) -> Vec<&str> {
        rows.iter().map(|r| r.key.as_str()).collect()
    }

    /// 三个过滤维度各自生效、组合生效，且排序恒为 (agent_id, kind, key)。
    #[test]
    fn 过滤器各维度独立且可组合() {
        let (_dir, index) = open();
        let conn = index.conn();
        // 故意乱序插入，验证排序来自 SQL 而不是插入顺序。
        seed(conn, "codex", "artifact", "cache", 10, Some("l1"));
        seed(conn, "claude-code", "session", "s2", 20, None);
        seed(conn, "claude-code", "artifact", "b-log", 30, Some("l2"));
        seed(conn, "claude-code", "artifact", "a-db", 40, Some("l0"));

        let all = list_resources(conn, &ResourceFilter::default()).unwrap();
        assert_eq!(
            keys(&all),
            vec!["a-db", "b-log", "s2", "cache"],
            "必须按 (agent_id, kind, key) 排序"
        );

        let by_agent = list_resources(
            conn,
            &ResourceFilter {
                agents: vec!["codex".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(keys(&by_agent), vec!["cache"]);

        let by_kind = list_resources(
            conn,
            &ResourceFilter {
                kinds: vec!["session".into(), "artifact".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_kind.len(), 4, "多值 kind 走 IN，任一命中即入选");

        let by_level = list_resources(
            conn,
            &ResourceFilter {
                clean_levels: vec!["l0".into(), "l1".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(keys(&by_level), vec!["a-db", "cache"]);

        let combined = list_resources(
            conn,
            &ResourceFilter {
                agents: vec!["claude-code".into()],
                kinds: vec!["artifact".into()],
                clean_levels: vec!["l2".into()],
            },
        )
        .unwrap();
        assert_eq!(keys(&combined), vec!["b-log"]);
        assert_eq!(combined[0].hash_content, Some([9u8; 32]));
    }

    /// 多选 agent 走动态 IN：两个 agent 的行都回来，第三个的进不来。
    /// 最容易悄悄坏掉的就是这里——占位符序号错一位，过滤就静默失效。
    #[test]
    fn 多选agent两个命中一个排除() {
        let (_dir, index) = open();
        let conn = index.conn();
        seed(conn, "codex", "artifact", "a", 10, None);
        seed(conn, "omp", "artifact", "b", 10, None);
        seed(conn, "claude-code", "artifact", "c", 10, None);

        let rows = list_resources(
            conn,
            &ResourceFilter {
                agents: vec!["codex".into(), "omp".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let got: Vec<&str> = rows.iter().map(|r| r.agent_id.as_str()).collect();
        assert_eq!(got, vec!["codex", "omp"], "第三个 agent 不得入选");
    }

    /// 空 agents = 全部，与不设过滤同义。这是交互菜单「All agents 勾选」
    /// 与命令行不传 `--agent` 的共同落点，静默错一个就是该看的行全看不见。
    #[test]
    fn 空agents等于全部() {
        let (_dir, index) = open();
        let conn = index.conn();
        seed(conn, "codex", "artifact", "a", 10, None);
        seed(conn, "omp", "artifact", "b", 10, None);

        let all = list_resources(conn, &ResourceFilter::default()).unwrap();
        let by_empty = list_resources(
            conn,
            &ResourceFilter {
                agents: Vec::new(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(keys(&by_empty), keys(&all));
        assert_eq!(by_empty.len(), 2);
    }

    /// 带引号的 agent id 必须作为参数绑定，而不是拼进 SQL——否则
    /// `x' OR 1=1 --` 这种 id 会把整张 resource 表捞出来。能查到
    /// 空结果本身就证明注入没生效。
    #[test]
    fn 引号agent_id绑定为参数不注入() {
        let (_dir, index) = open();
        let conn = index.conn();
        seed(conn, "codex", "artifact", "a", 10, None);
        seed(conn, "omp", "artifact", "b", 10, None);

        let evil = "x' OR 1=1 --".to_string();
        let rows = list_resources(
            conn,
            &ResourceFilter {
                agents: vec![evil.clone()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(rows.is_empty(), "注入 id 不该匹配到任何行");
        // 同一条 id 走 install_total 也必须被绑住，不是被拼进 WHERE。
        assert_eq!(install_total(conn, &[evil]).unwrap(), 0);
    }

    /// install 桶 = install 行的 size + skill 行的 install_bytes，
    /// artifact / session 行一分不进。
    #[test]
    fn install桶只算本体字节() {
        let (_dir, index) = open();
        let conn = index.conn();
        seed(conn, "claude-code", "install", "npm-pkg", 1_000, None);
        seed(conn, "claude-code", "artifact", "log", 5_000, Some("l1"));
        seed(conn, "claude-code", "session", "s1", 7_000, None);
        seed_skill(conn, "claude-code", "gstack", 200, 800);
        seed(conn, "codex", "install", "bin", 300, None);

        assert_eq!(install_total(conn, &[]).unwrap(), 1_000 + 800 + 300);
        assert_eq!(
            install_total(conn, &["claude-code".to_string()]).unwrap(),
            1_000 + 800
        );
        assert_eq!(install_total(conn, &["nobody".to_string()]).unwrap(), 0);
    }

    /// 退化命名 `name@目录名` 必须与原名归到同一组。
    #[test]
    fn skill分组还原退化命名() {
        let (_dir, index) = open();
        let conn = index.conn();
        seed_skill(conn, "claude-code", "gstack", 10, 0);
        seed_skill(conn, "codex", "gstack@managed-skills", 20, 0);
        seed_skill(conn, "codex", "browse", 30, 0);
        // skill 之外的行不得混进来。
        seed(conn, "codex", "artifact", "gstack", 40, Some("l1"));

        let groups = skill_groups(conn).unwrap();
        assert_eq!(groups.len(), 2);
        let gstack = &groups["gstack"];
        assert_eq!(gstack.len(), 2, "同名 skill 跨 agent 归一组");
        assert_eq!(keys(gstack), vec!["gstack", "gstack@managed-skills"]);
        assert_eq!(groups["browse"].len(), 1);
    }

    /// 删资源要连 turn 与 fts_turn 一起清干净——留下孤儿 FTS 行，
    /// 下次重建索引复用 rowid 时会撞成脏命中。
    #[test]
    fn 删除资源同时清空轮次与fts() {
        let (_dir, index) = open();
        let conn = index.conn();
        let doomed = seed(conn, "claude-code", "session", "s1", 100, None);
        let keeper = seed(conn, "claude-code", "session", "s2", 100, None);
        upsert::replace_turns(
            conn,
            doomed,
            &[turn(0, 1_000, "第一轮"), turn(1, 2_000, "第二轮")],
        )
        .unwrap();
        upsert::replace_turns(conn, keeper, &[turn(0, 3_000, "留下的一轮")]).unwrap();

        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT count(*) FROM fts_turn"), 3);
        assert_eq!(last_turn_ms(conn, doomed).unwrap(), Some(2_000));

        delete_resource(conn, doomed).unwrap();

        assert_eq!(count("SELECT count(*) FROM resource"), 1);
        assert_eq!(count("SELECT count(*) FROM turn"), 1);
        assert_eq!(
            count("SELECT count(*) FROM fts_turn"),
            1,
            "被删资源的 FTS 行必须一起消失"
        );
        assert_eq!(last_turn_ms(conn, doomed).unwrap(), None);
        assert_eq!(last_turn_ms(conn, keeper).unwrap(), Some(3_000));
    }

    /// MCP 证据取正文命中里最晚那条 ts；从未出现过返回 None（= 永不判陈旧）。
    #[test]
    fn mcp证据取最新命中否则无证据() {
        let (_dir, index) = open();
        let conn = index.conn();
        let rid = seed(conn, "claude-code", "session", "s1", 100, None);
        upsert::replace_turns(
            conn,
            rid,
            &[
                turn(0, 1_000, "帮我用 context7 查一下文档"),
                turn(1, 5_000, "再调一次 context7"),
                turn(2, 9_000, "换个话题聊聊别的"),
            ],
        )
        .unwrap();

        assert_eq!(mcp_last_seen_ms(conn, "context7").unwrap(), Some(5_000));
        assert_eq!(mcp_last_seen_ms(conn, "never-installed").unwrap(), None);
        // trigram 下限以下无法检索，同样是"没有证据"。
        assert_eq!(mcp_last_seen_ms(conn, "ab").unwrap(), None);
        // FTS 操作符/引号不得逃逸出词元，只会查无此名而不是报错。
        assert!(mcp_last_seen_ms(conn, "\"OR fts_turn MATCH \"a").is_ok());
    }

    /// 调用证据取最新事件；查不到返回 None；声明名与目录名两个都要匹配。
    #[test]
    fn skill调用证据取最新事件_声明名与目录名都认() {
        let (_dir, index) = open();
        let conn = index.conn();
        let rid = seed(conn, "claude-code", "session", "s1", 100, None);
        upsert::replace_skill_events(
            conn,
            rid,
            &[
                duster_model::SkillInvocation {
                    skill: "weekly-report".into(),
                    ts_ms: 1_000,
                },
                duster_model::SkillInvocation {
                    skill: "weekly-report".into(),
                    ts_ms: 5_000,
                },
                duster_model::SkillInvocation {
                    skill: "taste-skill".into(),
                    ts_ms: 9_000,
                },
            ],
        )
        .unwrap();

        // 声明名命中,取最晚事件。
        assert_eq!(
            skill_last_invoked_ms(conn, "weekly-report", "weekly-report").unwrap(),
            Some(5_000)
        );
        // 目录名命中(声明名 `design-taste-frontend` 与目录 `taste-skill` 不同)。
        assert_eq!(
            skill_last_invoked_ms(conn, "design-taste-frontend", "taste-skill").unwrap(),
            Some(9_000)
        );
        // 两个名字都没有 → 无证据。
        assert_eq!(
            skill_last_invoked_ms(conn, "never-a-skill", "never-a-skill").unwrap(),
            None
        );
        assert_eq!(
            skill_last_invoked_ms(conn, "design-taste-frontend", "other-dir").unwrap(),
            None
        );
    }

    /// 重解析同一会话 = 整体替换事件,不累积重复。
    #[test]
    fn skill事件_重解析会话即替换() {
        let (_dir, index) = open();
        let conn = index.conn();
        let rid = seed(conn, "claude-code", "session", "s1", 100, None);
        let ev = |skill: &str, ts_ms: i64| duster_model::SkillInvocation {
            skill: skill.into(),
            ts_ms,
        };
        upsert::replace_skill_events(conn, rid, &[ev("pdf", 1_000), ev("pdf", 2_000)]).unwrap();
        // 会话重解析后只有新事件(比如那次 pdf 调用其实没发生)。
        upsert::replace_skill_events(conn, rid, &[ev("docx", 3_000)]).unwrap();

        assert_eq!(
            skill_last_invoked_ms(conn, "pdf", "pdf").unwrap(),
            None,
            "替换不是追加:旧事件必须消失"
        );
        assert_eq!(
            skill_last_invoked_ms(conn, "docx", "docx").unwrap(),
            Some(3_000)
        );
        let n: i64 = conn
            .query_row("SELECT count(*) FROM skill_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// 会话资源被删时事件随外键级联消失。
    #[test]
    fn skill事件_随会话删除级联清空() {
        let (_dir, index) = open();
        let conn = index.conn();
        let rid = seed(conn, "claude-code", "session", "s1", 100, None);
        upsert::replace_skill_events(
            conn,
            rid,
            &[duster_model::SkillInvocation {
                skill: "pdf".into(),
                ts_ms: 1_000,
            }],
        )
        .unwrap();

        delete_resource(conn, rid).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM skill_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "孤儿事件会把死技能永远救活,必须级联清掉");
    }

    /// 证据标记:缺省未采集;写入后已采集。
    #[test]
    fn skill证据标记_缺省未采集_写入后已采集() {
        let (_dir, index) = open();
        let conn = index.conn();
        assert!(!skill_evidence_ready(conn).unwrap());
        crate::meta::set(conn, crate::meta::SKILL_EVIDENCE_READY, "1").unwrap();
        assert!(skill_evidence_ready(conn).unwrap());
    }

    /// 压缩后改 path 不动 turn：偏移量指向解压后的逻辑内容，依然有效。
    #[test]
    fn 改写路径不影响轮次() {
        let (_dir, index) = open();
        let conn = index.conn();
        let rid = seed(conn, "claude-code", "session", "s1", 100, None);
        upsert::replace_turns(conn, rid, &[turn(0, 1_000, "hello")]).unwrap();

        update_resource_path(conn, rid, "/tmp/s1.jsonl.zst").unwrap();

        let row = &list_resources(conn, &ResourceFilter::default()).unwrap()[0];
        assert_eq!(row.path, "/tmp/s1.jsonl.zst");
        assert_eq!(last_turn_ms(conn, rid).unwrap(), Some(1_000));
    }

    /// agent 列表按字典序；展示名缺行与缺值都摊平成 None。
    #[test]
    fn agent列表与展示名() {
        let (_dir, index) = open();
        let conn = index.conn();
        for id in ["omp", "claude-code"] {
            upsert::upsert_agent(
                conn,
                &duster_model::AgentInfo {
                    id: id.into(),
                    display_name: format!("{id} display"),
                    root: std::path::PathBuf::from(format!("/tmp/{id}")),
                    version: None,
                },
                0,
            )
            .unwrap();
        }

        assert_eq!(agent_ids(conn).unwrap(), vec!["claude-code", "omp"]);
        assert_eq!(
            agent_display_name(conn, "omp").unwrap().as_deref(),
            Some("omp display")
        );
        assert_eq!(agent_display_name(conn, "absent").unwrap(), None);

        // uninstall 收尾：删掉 agent 行，重复删返回 0 而不是报错。
        assert_eq!(delete_agent(conn, "omp").unwrap(), 1);
        assert_eq!(delete_agent(conn, "omp").unwrap(), 0);
        assert_eq!(agent_ids(conn).unwrap(), vec!["claude-code"]);
    }
}
