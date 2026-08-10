//! 资源标识与分类。六大资源类是架构地基，新增类别须走 ADR 评审。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Agent 稳定标识，如 `claude-code`、`codex`、`omp`。
pub type AgentId = String;

/// 被管理对象的六大分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceKind {
    Mcp,
    Skill,
    Memory,
    Session,
    Artifact,
}

/// 资源作用域。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Global,
    Project(PathBuf),
    Local,
}

/// 资源唯一标识：跨 agent、跨机器稳定。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceId {
    pub agent: AgentId,
    pub kind: ResourceKind,
    pub scope: Scope,
    /// agent 内的自然键：skill 名 / mcp 名 / session uuid。
    pub key: String,
}

/// 一个被索引的资源实例（骨架，随功能落地逐步扩展）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    /// 物理来源路径。
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
}
