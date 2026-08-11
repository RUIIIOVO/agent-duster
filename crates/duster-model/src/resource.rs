//! 资源标识与分类。六大资源类是架构地基，新增类别须走 ADR 评审。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Agent 稳定标识，如 `claude-code`、`codex`、`omp`。
pub type AgentId = String;

/// 被管理对象的六大分类。
///
/// `Artifact` 与 `Install` 的分界是清理安全的地基，不容含糊：
/// - `Artifact` = **可清理产物**。缓存、日志、临时目录、运行时残留、
///   可再生的备份与归档。删掉之后 agent 照常启动、照常干活。
///   每一条 artifact 都必须在清单里声明 `clean_level`，没有级别就不是 artifact。
/// - `Install` = **软件本体与安装产物**。扩展、插件、随附二进制、
///   `node_modules`。删掉等于卸载，要重新下载安装才能恢复。
///   只统计体积，`duster clean` 永不触碰。
///
/// 判据是「删了要不要重装」，不是「占了多大」：1 GB 的扩展目录是 Install，
/// 800 MB 的日志库是 Artifact。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceKind {
    Mcp,
    Skill,
    Memory,
    Session,
    Artifact,
    Install,
}

/// artifact 的清理级别：**只描述"清掉的代价是什么"，不描述"占了多大"**。
///
/// | 级别 | 含义 | 展示标签 |
/// |---|---|---|
/// | `l0` | 无损回收：SQLite 空闲页、孤儿 WAL/SHM、`.tmp-*` 崩溃残留。数据一条不少。 | lossless |
/// | `l1` | 可再生：缓存、日志、临时目录、运行时残留。删完 agent 照常用，内容自动重建。 | caches, logs |
/// | `l2` | 有时效：滚动备份、归档。功能不缺，但重建有人工代价（如重新登录、重新下载）。 | stale, asks first |
///
/// 没有更高级别：**"删了等于卸载"的东西根本不是 artifact**，
/// 归 [`ResourceKind::Install`]，clean 永不触碰。
///
/// 放在 model 层而不是 adapter 层：它和 [`ResourceKind`] 是同一种东西——
/// 一张全 workspace 共享的封闭词汇表。清单解析、索引落库、CLI 展示三处
/// 都要用它，任何一处各自抄一份字符串就会像 l3/l4 那样各自漂移。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CleanLevel {
    L0,
    L1,
    L2,
}

impl CleanLevel {
    /// 代价递增序。展示与遍历都按这个顺序，别再另起一份。
    pub const ORDER: [CleanLevel; 3] = [CleanLevel::L0, CleanLevel::L1, CleanLevel::L2];

    /// 清单字面名，同时也是入库字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::L0 => "l0",
            Self::L1 => "l1",
            Self::L2 => "l2",
        }
    }

    /// 给人看的一句话标签（CLI 摘要行用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::L0 => "lossless",
            Self::L1 => "caches, logs",
            Self::L2 => "stale, asks first",
        }
    }
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
