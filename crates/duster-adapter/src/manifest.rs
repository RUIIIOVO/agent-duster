//! 声明式清单（`adapters/<id>.toml`）的解析与校验。
//! 加载顺序：内置（编译进二进制）→ `~/.agent-duster/adapters/*.toml`（用户覆盖）。
//!
//! 清单只描述「资源在哪、用哪个 mapper」，不做任何 IO 解释：
//! 路径一律保留 `~` 开头的字符串，展开由运行时负责；
//! `version_cmd` 在 M0 只存不执行。字段与词汇表的唯一范本见
//! `adapters/claude-code.toml` 文件头注释。

use anyhow::{Context, Result, bail};
use duster_model::ResourceKind;
use serde::Deserialize;
use std::path::Path;

/// 内置清单：编译期嵌入。新增 agent 只需在此加一行。
const BUILTIN: &[(&str, &str)] = &[
    (
        "cc-switch.toml",
        include_str!("../../../adapters/cc-switch.toml"),
    ),
    (
        "claude-code.toml",
        include_str!("../../../adapters/claude-code.toml"),
    ),
    ("codex.toml", include_str!("../../../adapters/codex.toml")),
    (
        "copilot-cli.toml",
        include_str!("../../../adapters/copilot-cli.toml"),
    ),
    ("cursor.toml", include_str!("../../../adapters/cursor.toml")),
    (
        "gemini-cli.toml",
        include_str!("../../../adapters/gemini-cli.toml"),
    ),
    (
        "kimi-cli.toml",
        include_str!("../../../adapters/kimi-cli.toml"),
    ),
    ("omp.toml", include_str!("../../../adapters/omp.toml")),
    (
        "opencode.toml",
        include_str!("../../../adapters/opencode.toml"),
    ),
    ("pi.toml", include_str!("../../../adapters/pi.toml")),
    ("qoder.toml", include_str!("../../../adapters/qoder.toml")),
];

/// 一份完整的适配器清单。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub agent: AgentSection,
    pub probe: ProbeSection,
    /// 资源声明；TOML 里写作 `[[resource]]`。
    #[serde(default, rename = "resource")]
    pub resources: Vec<ResourceSection>,
}

/// `[agent]`：身份。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSection {
    /// 稳定标识，如 `claude-code`。小写字母/数字/连字符。
    pub id: String,
    /// 展示名，如 "Claude Code"。
    pub display_name: String,
}

/// `[probe]`：安装探测。路径存在性为主，二进制/版本命令为辅。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeSection {
    /// 任一路径存在即视为已安装。
    #[serde(default)]
    pub any_of: Vec<String>,
    /// 全部路径存在才视为已安装（与 `any_of` 可并用，均须满足）。
    #[serde(default)]
    pub all_of: Vec<String>,
    /// PATH 里应能找到的二进制名。
    #[serde(default)]
    pub binary: Option<String>,
    /// 取版本号的命令（argv 形式）。M0 只存不执行。
    #[serde(default)]
    pub version_cmd: Option<Vec<String>>,
}

/// `[[resource]]`：一类资源的声明。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSection {
    pub kind: ResourceKind,
    pub scope: ManifestScope,
    /// 文件或目录，一律 `~` 开头，清单层不展开。
    pub path: String,
    /// JSON 文件内定位（RFC 6901），如 `/mcpServers`。
    #[serde(default)]
    pub json_pointer: Option<String>,
    /// TOML 文件内定位（点路径），如 `mcp_servers`。与 `json_pointer` 互斥。
    #[serde(default)]
    pub toml_key: Option<String>,
    pub mapper: MapperName,
    /// 目录型资源的条目匹配模式，如 `*/SKILL.md`。
    #[serde(default)]
    pub glob: Option<String>,
    /// 仅 artifact 资源：清理级别（再生成本）。
    #[serde(default)]
    pub clean_level: Option<CleanLevel>,
}

/// 清单层作用域。与 `duster_model::Scope` 不同：这里 project 不携带具体
/// 项目路径——具体路径由运行时项目扫描填入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestScope {
    Global,
    Project,
}

/// mapper 封闭词汇表。清单里只能写这七个名字，serde 解析即校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum MapperName {
    /// 标准 JSON `mcpServers` 映射（Claude Code / 多数 agent）。
    #[serde(rename = "mcp/standard-json")]
    McpStandardJson,
    /// Codex `config.toml` 的 `mcp_servers` 表。
    #[serde(rename = "mcp/codex-toml")]
    McpCodexToml,
    /// `SKILL.md` YAML frontmatter（name/description）。
    #[serde(rename = "skill/frontmatter-md")]
    SkillFrontmatterMd,
    /// Markdown 记忆文件（CLAUDE.md / AGENTS.md 等）。
    #[serde(rename = "memory/markdown")]
    MemoryMarkdown,
    /// Claude Code JSONL 会话（原生适配器逃生舱）。
    #[serde(rename = "native/claude-session")]
    NativeClaudeSession,
    /// Codex JSONL 会话（原生适配器逃生舱）。
    #[serde(rename = "native/codex-session")]
    NativeCodexSession,
    /// 不解析内容，只做体积/数量统计。任何 kind 都可用。
    #[serde(rename = "stats-only")]
    StatsOnly,
}

impl MapperName {
    /// 清单里的字面名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::McpStandardJson => "mcp/standard-json",
            Self::McpCodexToml => "mcp/codex-toml",
            Self::SkillFrontmatterMd => "skill/frontmatter-md",
            Self::MemoryMarkdown => "memory/markdown",
            Self::NativeClaudeSession => "native/claude-session",
            Self::NativeCodexSession => "native/codex-session",
            Self::StatsOnly => "stats-only",
        }
    }

    /// mapper 是否适用于该资源类别。`stats-only` 通用，其余按前缀对号入座。
    fn fits(self, kind: ResourceKind) -> bool {
        match self {
            Self::StatsOnly => true,
            Self::McpStandardJson | Self::McpCodexToml => kind == ResourceKind::Mcp,
            Self::SkillFrontmatterMd => kind == ResourceKind::Skill,
            Self::MemoryMarkdown => kind == ResourceKind::Memory,
            Self::NativeClaudeSession | Self::NativeCodexSession => kind == ResourceKind::Session,
        }
    }
}

/// artifact 清理级别：删除后的再生成本，l0 最低（随删随生）、l4 不可再生。
/// 注意没有 l3——级别是离散档位不是连续刻度，留空位给未来细分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CleanLevel {
    L0,
    L1,
    L2,
    L4,
}

impl Manifest {
    /// 结构之外的语义校验，给出人话错误。serde 已挡掉未知字段/非法枚举值。
    pub fn validate(&self) -> Result<()> {
        let id = &self.agent.id;
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            bail!("agent.id `{id}` 不合法：只能用小写字母、数字和连字符，且不能为空");
        }
        if self.agent.display_name.is_empty() {
            bail!("[{id}] agent.display_name 不能为空");
        }

        let p = &self.probe;
        if p.any_of.is_empty() && p.all_of.is_empty() && p.binary.is_none() {
            bail!("[{id}] probe 至少要声明 any_of / all_of / binary 之一，否则无从探测");
        }
        for path in p.any_of.iter().chain(&p.all_of) {
            ensure_tilde(id, "probe 路径", path)?;
        }
        if let Some(cmd) = &p.version_cmd
            && cmd.is_empty()
        {
            bail!("[{id}] probe.version_cmd 不能是空数组");
        }

        for (i, r) in self.resources.iter().enumerate() {
            let at = format!("[{id}] 第 {} 个 resource（path = {}）", i + 1, r.path);
            ensure_tilde(id, "resource.path", &r.path)?;
            if let Some(ptr) = &r.json_pointer
                && !ptr.starts_with('/')
            {
                bail!("{at}：json_pointer `{ptr}` 必须以 `/` 开头（RFC 6901）");
            }
            if r.json_pointer.is_some() && r.toml_key.is_some() {
                bail!("{at}：json_pointer 与 toml_key 互斥，只能声明其一");
            }
            if !r.mapper.fits(r.kind) {
                bail!(
                    "{at}：mapper `{}` 不适用于 kind `{:?}`",
                    r.mapper.as_str(),
                    r.kind
                );
            }
            if r.clean_level.is_some() && r.kind != ResourceKind::Artifact {
                bail!("{at}：clean_level 只有 artifact 资源可以声明");
            }
        }
        Ok(())
    }
}

/// 清单里的路径必须是 `~` 开头的字符串（不展开、不接受绝对/相对路径）。
fn ensure_tilde(id: &str, what: &str, path: &str) -> Result<()> {
    if path == "~" || path.starts_with("~/") {
        Ok(())
    } else {
        bail!("[{id}] {what} `{path}` 必须以 `~/` 开头（清单层不展开、不接受其他形式）");
    }
}

/// 解析并校验一份清单文本。
fn parse(src: &str) -> Result<Manifest> {
    let m: Manifest = toml::from_str(src)?;
    m.validate()?;
    Ok(m)
}

/// 加载全部内置清单。内置清单损坏属于程序错误，直接 panic（有测试兜底）。
pub fn load_builtin() -> Vec<Manifest> {
    BUILTIN
        .iter()
        .map(|(name, src)| parse(src).unwrap_or_else(|e| panic!("内置清单 {name} 损坏：{e:#}")))
        .collect()
}

/// 加载用户清单目录（`~/.agent-duster/adapters/*.toml`）。
/// 目录不存在视为没有用户清单；单个文件解析失败即整体报错（带文件名）。
pub fn load_user_dir(dir: &Path) -> Result<Vec<Manifest>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("读取用户清单目录 {} 失败", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    // 按文件名排序,保证加载顺序稳定。
    entries.sort_by_key(|e| e.file_name());

    let mut out = Vec::new();
    for entry in entries {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let src = std::fs::read_to_string(&path)
            .with_context(|| format!("读取用户清单 {} 失败", path.display()))?;
        let m = parse(&src).with_context(|| format!("用户清单 {} 不合法", path.display()))?;
        out.push(m);
    }
    Ok(out)
}

/// 加载全部清单：内置 + 用户目录，用户清单按 agent.id 覆盖内置。
/// `user_dir` 传 None 时用默认位置 `~/.agent-duster/adapters`。
pub fn load_all(user_dir: Option<&Path>) -> Result<Vec<Manifest>> {
    let mut out = load_builtin();
    let default_dir = dirs::home_dir().map(|h| h.join(".agent-duster").join("adapters"));
    let user = match (user_dir, &default_dir) {
        (Some(d), _) => load_user_dir(d)?,
        (None, Some(d)) => load_user_dir(d)?,
        (None, None) => Vec::new(),
    };
    for m in user {
        match out.iter_mut().find(|b| b.agent.id == m.agent.id) {
            Some(slot) => *slot = m, // 同 id 覆盖内置
            None => out.push(m),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼一份最小合法清单，便于逐项改坏。
    fn minimal(extra_resource: &str) -> String {
        format!(
            r#"
[agent]
id = "demo"
display_name = "Demo"

[probe]
any_of = ["~/.demo"]
{extra_resource}
"#
        )
    }

    #[test]
    fn builtin_claude_code_parses_with_expected_fields() {
        let manifests = load_builtin();
        let m = manifests
            .iter()
            .find(|m| m.agent.id == "claude-code")
            .expect("内置应包含 claude-code");
        assert_eq!(m.agent.display_name, "Claude Code");
        assert_eq!(m.probe.any_of, ["~/.claude", "~/.claude.json"]);
        assert_eq!(m.probe.binary.as_deref(), Some("claude"));
        assert_eq!(m.resources.len(), 9);

        // MCP：文件内定位 + 标准 mapper。
        let mcp = &m.resources[0];
        assert_eq!(mcp.kind, ResourceKind::Mcp);
        assert_eq!(mcp.path, "~/.claude.json");
        assert_eq!(mcp.json_pointer.as_deref(), Some("/mcpServers"));
        assert_eq!(mcp.mapper, MapperName::McpStandardJson);

        // Skill：目录 + glob。
        let skill = &m.resources[1];
        assert_eq!(skill.glob.as_deref(), Some("*/SKILL.md"));
        assert_eq!(skill.mapper, MapperName::SkillFrontmatterMd);

        // Session：原生逃生舱。
        assert!(m.resources.iter().any(
            |r| r.kind == ResourceKind::Session && r.mapper == MapperName::NativeClaudeSession
        ));

        // Artifact ×4：clean_level 依次 l2/l1/l1/l1。
        let levels: Vec<_> = m
            .resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Artifact)
            .map(|r| r.clean_level.unwrap())
            .collect();
        assert_eq!(
            levels,
            [
                CleanLevel::L2,
                CleanLevel::L1,
                CleanLevel::L1,
                CleanLevel::L1
            ]
        );
    }

    #[test]
    fn builtin_has_all_eleven_agents_in_order() {
        let manifests = load_builtin();
        let ids: Vec<_> = manifests.iter().map(|m| m.agent.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "cc-switch",
                "claude-code",
                "codex",
                "copilot-cli",
                "cursor",
                "gemini-cli",
                "kimi-cli",
                "omp",
                "opencode",
                "pi",
                "qoder"
            ]
        );
    }

    #[test]
    fn unknown_field_is_rejected() {
        let src = minimal("").replace("[probe]", "[probe]\ntypo_field = 1");
        let err = parse(&src).unwrap_err();
        assert!(
            err.to_string().contains("typo_field"),
            "错误应点名未知字段: {err}"
        );
    }

    #[test]
    fn illegal_mapper_name_is_rejected() {
        let src = minimal(
            r#"
[[resource]]
kind = "mcp"
scope = "global"
path = "~/.demo.json"
mapper = "mcp/no-such-mapper"
"#,
        );
        let err = parse(&src).unwrap_err();
        assert!(
            err.to_string().contains("no-such-mapper"),
            "错误应点名非法 mapper: {err}"
        );
    }

    #[test]
    fn semantic_validation_gives_human_errors() {
        // 路径没带 ~。
        let src = minimal(
            r#"
[[resource]]
kind = "memory"
scope = "global"
path = "/etc/demo.md"
mapper = "memory/markdown"
"#,
        );
        assert!(parse(&src).unwrap_err().to_string().contains("~/"));

        // clean_level 用在非 artifact 上。
        let src = minimal(
            r#"
[[resource]]
kind = "mcp"
scope = "global"
path = "~/.demo.json"
mapper = "mcp/standard-json"
clean_level = "l1"
"#,
        );
        assert!(parse(&src).unwrap_err().to_string().contains("clean_level"));

        // mapper 与 kind 不匹配。
        let src = minimal(
            r#"
[[resource]]
kind = "skill"
scope = "global"
path = "~/.demo/skills"
mapper = "mcp/standard-json"
"#,
        );
        assert!(parse(&src).unwrap_err().to_string().contains("不适用"));

        // probe 三无。
        let src = minimal("").replace("any_of = [\"~/.demo\"]", "");
        assert!(parse(&src).unwrap_err().to_string().contains("probe"));
    }

    #[test]
    fn user_dir_overrides_builtin_by_id_and_appends_new() {
        let dir = tempfile::tempdir().unwrap();
        // 同 id 覆盖内置。
        std::fs::write(
            dir.path().join("claude-code.toml"),
            r#"
[agent]
id = "claude-code"
display_name = "Claude Code (custom)"

[probe]
any_of = ["~/.claude"]
"#,
        )
        .unwrap();
        // 新 id 追加。
        std::fs::write(
            dir.path().join("other.toml"),
            r#"
[agent]
id = "other-agent"
display_name = "Other"

[probe]
binary = "other"
"#,
        )
        .unwrap();
        // 非 toml 文件忽略。
        std::fs::write(dir.path().join("README.md"), "ignore me").unwrap();

        let all = load_all(Some(dir.path())).unwrap();
        let claude: Vec<_> = all.iter().filter(|m| m.agent.id == "claude-code").collect();
        assert_eq!(claude.len(), 1, "同 id 只保留一份");
        assert_eq!(claude[0].agent.display_name, "Claude Code (custom)");
        assert!(all.iter().any(|m| m.agent.id == "other-agent"));
    }

    #[test]
    fn missing_user_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-dir");
        assert!(load_user_dir(&missing).unwrap().is_empty());
    }
}
