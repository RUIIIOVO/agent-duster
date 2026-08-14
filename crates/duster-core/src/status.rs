//! `duster status`:纯读索引,汇总各 agent 体积/资源计数。
//!
//! 只读打开(不建目录、不迁移、不抢写锁),扫描进行中也能看;
//! 库还没建过就先建一次,见 [`crate::freshness::ensure_exists`]。
//!
//! 上一轮改造把「捎一轮他检」从 status 里整体撤下了：issues / deep / ping
//! 三个旗标连同它们伺候的六项检查（明文凭据、skill 元数据、配置语法、
//! 断链、agent 库完整性、MCP 可达性）一起删掉，status 的退出码回到
//! 纯 0/失败。本模块只剩一个入口 [`status`]；agent 的 SQLite 库读不读得动
//! 移去了 `duster doctor` 的自检（见 `crate::doctor::check_sqlite`）。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use duster_index::db::Index;

use crate::freshness;

/// 单个 agent 的索引统计。
#[derive(Debug, Serialize)]
pub struct AgentStatus {
    pub agent_id: String,
    pub display_name: Option<String>,
    /// 上次扫描时间(Unix 毫秒);从未扫过为 None。
    pub last_scan_ms: Option<i64>,
    /// 该 agent 全部资源体积合计。
    pub bytes: u64,
    /// 各 kind 的资源行数,如 `{"mcp": 2, "session": 40}`。
    pub kind_counts: BTreeMap<String, u64>,
    /// 各 kind 的体积合计(字节)。
    pub kind_bytes: BTreeMap<String, u64>,
    /// **可回收**字节按清理级别拆分(`l0`/`l1`/`l2` -> 字节)。
    ///
    /// 记的是"清掉能拿回多少"而非"占了多少":l0 只算 SQLite 空洞。
    /// `install` 永远不出现在这里——软件本体从不参与清理,
    /// 把它算进"能清出多少"是在骗自己。
    pub clean_bytes: BTreeMap<String, u64>,
}

/// `duster status` 的汇总报告。
#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub agents: Vec<AgentStatus>,
    pub total_bytes: u64,
}

/// 读取索引并按 agent 聚合。`index_path` 缺省 `~/.agent-duster/index.db`。
pub fn status(index_path: Option<&Path>) -> Result<StatusReport> {
    let path = freshness::ensure_exists(index_path)?;
    let idx = Index::open_readonly(&path)
        .with_context(|| format!("failed to open index read-only: {}", path.display()))?;
    let conn = idx.conn();

    let mut agents: Vec<AgentStatus> = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT agent_id, display_name, last_scan_ms FROM agent ORDER BY agent_id")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;
        for r in rows {
            let (agent_id, display_name, last_scan_ms) = r?;
            agents.push(AgentStatus {
                agent_id,
                display_name,
                last_scan_ms,
                bytes: 0,
                kind_counts: BTreeMap::new(),
                kind_bytes: BTreeMap::new(),
                clean_bytes: BTreeMap::new(),
            });
        }
    }

    let mut total_bytes = 0u64;
    for a in &mut agents {
        let mut stmt = conn.prepare(
            "SELECT kind, COUNT(*), COALESCE(SUM(size), 0) FROM resource \
             WHERE agent_id = ?1 GROUP BY kind ORDER BY kind",
        )?;
        let rows = stmt.query_map((a.agent_id.as_str(),), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for r in rows {
            let (kind, count, size) = r?;
            a.kind_counts.insert(kind.clone(), count.max(0) as u64);
            a.kind_bytes.insert(kind, size.max(0) as u64);
            a.bytes += size.max(0) as u64;
        }

        // 资源目录**内部**的软件本体（skill 里的 node_modules / dist / bin）。
        // `size` 已经把它扣掉了，这里补回 install 桶——不补的话
        // 本机 ~/.claude/skills 的 1 GB 编译产物会从所有视图里凭空消失，
        // 「SIZE 加起来对不上 du」正是这一列存在的原因。
        let embedded: i64 = conn.query_row(
            "SELECT COALESCE(SUM(install_bytes), 0) FROM resource WHERE agent_id = ?1",
            (a.agent_id.as_str(),),
            |row| row.get(0),
        )?;
        if embedded > 0 {
            *a.kind_bytes.entry("install".to_string()).or_insert(0) += embedded as u64;
            a.bytes += embedded as u64;
        }
        total_bytes += a.bytes;

        // 汇总 reclaimable 而不是 size:l0 的文件里活数据还在,
        // 拿文件大小当回收量会虚报。老库(v3 之前扫的行)该列为 NULL,
        // COALESCE 记 0——宁可少报,不能多报。
        let mut stmt = conn.prepare(
            "SELECT clean_level, COALESCE(SUM(reclaimable), 0) FROM resource \
             WHERE agent_id = ?1 AND clean_level IS NOT NULL \
             GROUP BY clean_level ORDER BY clean_level",
        )?;
        let rows = stmt.query_map((a.agent_id.as_str(),), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for r in rows {
            let (level, reclaimable) = r?;
            a.clean_bytes.insert(level, reclaimable.max(0) as u64);
        }
    }

    Ok(StatusReport {
        agents,
        total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 库不存在不再是错误:自己建一个,给一份空报告。用户从没听说过
    /// 「索引」这个东西,不该在这里被拦下来先学一个概念再回来。
    #[test]
    fn 库不存在时自动建库而不报错() {
        let tmp = tempfile::TempDir::new().unwrap();
        let index = tmp.path().join(".agent-duster").join("index.db");

        let rep = status(Some(&index)).unwrap();
        assert!(index.is_file(), "库该被建出来");
        assert_eq!(rep.total_bytes, 0, "空 home 里扫不出任何东西");
    }
}
