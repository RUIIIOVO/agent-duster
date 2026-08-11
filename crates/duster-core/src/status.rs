//! `duster status`:纯读索引,汇总各 agent 体积/资源计数。
//!
//! 只读打开(不建目录、不迁移、不抢写锁),扫描进行中也能看;
//! 库不存在时给出人话提示,引导先跑 `duster scan`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_index::db::Index;

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

    #[test]
    fn 库不存在时提示先跑_scan() {
        let missing = std::env::temp_dir().join(format!(
            "duster-core-status-miss-{}/nope.db",
            std::process::id()
        ));
        let err = status(Some(&missing)).unwrap_err();
        assert!(err.to_string().contains("duster scan"), "{err:#}");
    }
}
