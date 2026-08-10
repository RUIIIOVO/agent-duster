//! Skill 的归一化元数据(SKILL.md frontmatter 模型)。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 一个已发现的 skill。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    /// skill 名(目录名或 frontmatter name,以 frontmatter 优先)。
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// skill 根目录(含 SKILL.md 的目录)。
    pub root: PathBuf,
    /// 目录树语义哈希(剪枝 node_modules/.git 后逐文件 BLAKE3 再聚合)。
    /// 延迟计算,扫描期可为空。
    #[serde(default)]
    pub tree_hash: Option<String>,
    /// frontmatter 中未建模的其余字段(JSON 序列化),迁移时原样带走。
    #[serde(default)]
    pub extra: Option<String>,
}
