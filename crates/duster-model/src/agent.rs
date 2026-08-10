//! Agent 探测结果。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 一个被探测到的 agent 安装。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: crate::AgentId,
    /// 展示名,如 "Claude Code"。
    pub display_name: String,
    /// 数据根目录,如 `~/.claude`。
    pub root: PathBuf,
    /// 探测到的版本;探测不到为 None(不阻塞使用)。
    #[serde(default)]
    pub version: Option<String>,
}
