//! 声明式清单（`adapters/<id>.toml`）的解析与校验。
//! 加载顺序：内置（编译进二进制）→ `~/.agent-duster/adapters/*.toml`（用户覆盖）。
//!
//! 清单只描述「资源在哪、用哪个 mapper」，不做任何 IO 解释：
//! 路径一律保留 `~` 开头的字符串，展开由运行时负责；
//! `version_cmd` 在 M0 只存不执行。字段与词汇表的唯一范本见
//! `adapters/claude-code.toml` 文件头注释。
//!
//! `[uninstall]` 是唯一一个**描述动作**而不是描述资源的小节：卸载要动
//! 四类所有权不同的东西——自己独占的树（删）、写在别人文件里的键（改）、
//! 软件本体的安装方式（只打印命令，绝不代跑）、声明化的已知残留（自己删，
//! 查不到就沉默）。它仍然不做 IO：路径照样留 `~`，`detect` / `command`
//! 照样只是 argv。

use anyhow::{Context, Result, bail};
use duster_model::{CleanLevel, ResourceKind};
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
    /// 卸载声明；TOML 里写作 `[uninstall]`。整节可选，缺省语义见
    /// [`Manifest::owned_roots`]。
    #[serde(default)]
    pub uninstall: Option<UninstallSection>,
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
    /// 仅 artifact 资源：清理级别。artifact 必填，其余 kind 禁写。
    #[serde(default)]
    pub clean_level: Option<CleanLevel>,
    /// 仅 l2 artifact：这份资源是「按代际滚动」的（数据库滚动备份等），
    /// 每份代际一行索引（要求同时声明 `glob`），这里写保留最新几份。
    ///
    /// 语义与 `--older-than` 正交：第 8 份副本冗余不冗余看数量不看年龄，
    /// 所以它绕开年龄阈值，由 `prune --keep-generations` 独立处理。
    /// 其余 kind / 级别禁写（校验层拒绝加载）。
    #[serde(default)]
    pub keep_generations: Option<u32>,
    /// 仅 skill 资源：目录内属于「软件本体」的一级子目录名，如
    /// `["node_modules", "dist", "bin", ".git"]`。
    ///
    /// artifact / install 的二分法在资源**内部**也要成立：一个 skill 目录
    /// 里既有用户内容（可归档、可去重）也有装出来的东西（删了要重装）。
    /// 命中这里的子树只统计进 install 桶，不进 skill 体积、不进归档包、
    /// 不进副本检测的树哈希。写的是**裸目录名**（按名匹配任意层级），不是路径。
    #[serde(default)]
    pub install_paths: Vec<String>,
}

/// `[uninstall]`：卸载这个 agent 要动的四类东西。整节可选。
///
/// 四个字段是四种**所有权**，不是四个档位：
/// `owns` 是随 agent 一起死的树（整棵删）；`shared` 是它写在**别人**文件里
/// 的那几个键（只能逐键动刀，永不删文件）；`package` 是软件本体的安装方式
/// （duster 只打印，永不代跑）；`residue` 是**声明化的已知残留**（以前只记
/// 在注释里、永远靠用户手动删，现在 duster 自己删）。四者互不替代，少一类
/// 就卸不干净。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UninstallSection {
    /// 本 agent 独占的路径，一律 `~` 开头。留空表示「按 [`Manifest::owned_roots`]
    /// 从 `[probe]` + `[[resource]]` 推导」——写清单的人不必把根抄两遍。
    #[serde(default)]
    pub owns: Vec<String>,
    /// 共享文件里的定点改写；TOML 里写作 `[[uninstall.shared]]`。
    #[serde(default)]
    pub shared: Vec<SharedEdit>,
    /// 包管理器线索；TOML 里写作 `[[uninstall.package]]`。
    #[serde(default)]
    pub package: Vec<PackageHint>,
    /// 声明化的已知残留；TOML 里写作 `[[uninstall.residue]]`。
    #[serde(default)]
    pub residue: Vec<ResidueSpec>,
}

/// `[[uninstall.shared]]`：**别的 agent 也拥有**的那个文件里，属于本 agent
/// 的那一个键。
///
/// 与 `owns` 的差别不是粒度而是所有权：这个文件不是我们的，所以只能摘掉
/// 一个键，不能删文件、不能清空表。`json_pointer` / `toml_key` 二选一且必选
/// 一个——没有键的「共享改写」等于重写整个文件，那不叫卸载。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedEdit {
    /// 共享文件路径，`~` 开头、不展开。不得落在任一 `owns` 之内。
    pub path: String,
    /// JSON 文件内定位（RFC 6901），如 `/mcpServers/foo`。与 `toml_key` 互斥。
    #[serde(default)]
    pub json_pointer: Option<String>,
    /// TOML 文件内定位（点路径），如 `mcp_servers.foo`。与 `json_pointer` 互斥。
    #[serde(default)]
    pub toml_key: Option<String>,
    /// 动刀前给用户看的一句话，**英文**：这个键是什么、为什么该跟着一起走。
    pub reason: String,
}

/// `[[uninstall.package]]`：软件本体是怎么装上来的。
///
/// `detect` 是只读探测，duster **可以**执行它来判断这条线索在本机是否适用；
/// `command` 是卸载命令，duster **只打印**。这条边界是刻意的：包管理器的
/// 卸载会动 duster 认领范围之外的文件（PATH 上的 shim、别的项目的依赖），
/// 代跑一次就再也说不清是谁删的。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageHint {
    pub manager: PackageManager,
    /// 只读探测命令（argv）。留空表示这条线索无法探测，只能无条件打印。
    #[serde(default)]
    pub detect: Vec<String>,
    /// 卸载命令（argv）。**永不自动执行**，只打印给用户。不得为空。
    pub command: Vec<String>,
}

/// `[[uninstall.residue]]`：**声明化的已知残留**。
///
/// 为什么需要这个声明段：以前这类残留只写在 adapter 的注释里——它们永远
/// 进不了卸载报告，也永远只能靠用户自己动手删。声明化之后，新发现的残留
/// 只需在清单里加几行 TOML，不用改任何代码；卸载时 duster 自己删，删不掉
/// 就如实报 `Failed`，而不是假装卸干净。
///
/// 与 `shared` 的差别：`shared` 是「别人文件里**属于我们的键**」，语义上
/// 那个文件还有别的主人，必须保留文件本身；`residue` 是「schema 以前表达
/// 不了、但事实存在的既定残留」——shell rc 里的 PATH 行、别的工具数据库里
/// 的整行。两者都只做行级手术，绝不删整个文件。`kind` 是封闭词汇表：
/// 只有 `shell_line` 与 `sqlite_row` 两个值，其余加载即报错。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ResidueSpec {
    /// shell rc 里的整行（如安装脚本追加的 PATH 行）。`files` 里**存在**的
    /// 文件才处理；`match` 是**子串包含**匹配整行；`with_comment_above` 为
    /// true 时，命中行紧邻的上一行若以 `#` 开头（去空白后）则一并删除——
    /// 安装脚本常在 PATH 行上方留一行注释。
    #[serde(rename = "shell_line")]
    ShellLine {
        /// 可能含这一行的 rc 文件，一律 `~` 开头、不展开。不存在的文件跳过。
        files: Vec<String>,
        /// 匹配整行的子串。TOML 里写作 `match`（Rust 关键字，这里改名存）。
        #[serde(rename = "match")]
        match_: String,
        /// 命中行上方紧邻的 `#` 注释行是否一并删。缺省 false。
        #[serde(default)]
        with_comment_above: bool,
        /// 必填：这一行为什么属于这个 agent，进卸载报告。
        why: String,
    },
    /// 别的工具 SQLite 库里的整行（如 cc-switch 的 providers 供应商行）。
    /// `equals` 是 `column` 列的**精确等值**；库文件不存在则跳过。
    #[serde(rename = "sqlite_row")]
    SqliteRow {
        /// 库文件路径，`~` 开头、不展开。库不存在则跳过。
        db: String,
        /// 表名。
        table: String,
        /// 等值匹配的列名。
        column: String,
        /// 该列的精确等值，命中即删整行。
        equals: String,
        /// 必填：这一行为什么属于这个 agent，进卸载报告。
        why: String,
    },
}

/// 包管理器封闭词汇表。清单里只能写这五个名字，serde 解析即校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    Npm,
    Brew,
    Curl,
    Pipx,
    Cargo,
}

impl PackageManager {
    /// 清单里的字面名，也是打印给用户看的那个词。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Brew => "brew",
            Self::Curl => "curl",
            Self::Pipx => "pipx",
            Self::Cargo => "cargo",
        }
    }
}

/// 清单层作用域。与 `duster_model::Scope` 不同：这里 project 不携带具体
/// 项目路径——具体路径由运行时项目扫描填入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestScope {
    Global,
    Project,
}

/// mapper 封闭词汇表。清单里只能写这些名字，serde 解析即校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum MapperName {
    /// 标准 JSON `mcpServers` 映射（Claude Code / 多数 agent）。
    #[serde(rename = "mcp/standard-json")]
    McpStandardJson,
    /// Codex `config.toml` 的 `mcp_servers` 表。
    #[serde(rename = "mcp/codex-toml")]
    McpCodexToml,
    /// opencode `opencode.jsonc` 的 `.mcp` 表。第三种方言：条目自带
    /// `type = "local" | "remote"`，`local` 用 `command: [...]`（argv 数组，
    /// 不拆 command/args），`remote` 用 `url`；另有 `enabled` 与 `environment`。
    #[serde(rename = "mcp/opencode-json")]
    McpOpencodeJson,
    /// Gemini CLI `~/.gemini/settings.json` 的 `mcpServers` 表。第四种方言：
    /// 键名与 standard-json 高度重合，但传输判定规则是 Gemini 自己的一套
    /// （`httpUrl` 压过 `url`、`type` 只认 stdio/sse/http、不认的值不报错而是
    /// 退回按端点推断）。详见 [`crate::mapper::mcp::from_gemini_json`]。
    #[serde(rename = "mcp/gemini-json")]
    McpGeminiJson,
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
    /// opencode 的 `opencode.db`（SQLite，一个库装所有会话）。
    #[serde(rename = "native/opencode-session")]
    NativeOpencodeSession,
    /// omp 的 `history.db`（SQLite，活跃 WAL）。
    #[serde(rename = "native/omp-session")]
    NativeOmpSession,
    /// omp 会话 jsonl（`sessions/<路径编码>/<时间戳>_<uuid>.jsonl`）。
    #[serde(rename = "native/omp-jsonl-session")]
    NativeOmpJsonlSession,
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
            Self::McpOpencodeJson => "mcp/opencode-json",
            Self::McpGeminiJson => "mcp/gemini-json",
            Self::SkillFrontmatterMd => "skill/frontmatter-md",
            Self::MemoryMarkdown => "memory/markdown",
            Self::NativeClaudeSession => "native/claude-session",
            Self::NativeCodexSession => "native/codex-session",
            Self::NativeOpencodeSession => "native/opencode-session",
            Self::NativeOmpSession => "native/omp-session",
            Self::NativeOmpJsonlSession => "native/omp-jsonl-session",
            Self::StatsOnly => "stats-only",
        }
    }

    /// mapper 是否适用于该资源类别。`stats-only` 通用，其余按前缀对号入座。
    /// `artifact` / `install` 因此天然只能走 `stats-only`——它们没有可解析的内容模型。
    fn fits(self, kind: ResourceKind) -> bool {
        match self {
            Self::StatsOnly => true,
            Self::McpStandardJson
            | Self::McpCodexToml
            | Self::McpOpencodeJson
            | Self::McpGeminiJson => kind == ResourceKind::Mcp,
            Self::SkillFrontmatterMd => kind == ResourceKind::Skill,
            Self::MemoryMarkdown => kind == ResourceKind::Memory,
            Self::NativeClaudeSession
            | Self::NativeCodexSession
            | Self::NativeOpencodeSession
            | Self::NativeOmpSession
            | Self::NativeOmpJsonlSession => kind == ResourceKind::Session,
        }
    }
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
            bail!(
                "agent.id `{id}` is invalid: only lowercase letters, digits, and hyphens are allowed, and it must not be empty"
            );
        }
        if self.agent.display_name.is_empty() {
            bail!("[{id}] agent.display_name must not be empty");
        }

        let p = &self.probe;
        if p.any_of.is_empty() && p.all_of.is_empty() && p.binary.is_none() {
            bail!(
                "[{id}] probe must declare at least one of any_of / all_of / binary, otherwise nothing can be probed"
            );
        }
        for path in p.any_of.iter().chain(&p.all_of) {
            ensure_tilde(id, "probe path", path)?;
        }
        if let Some(cmd) = &p.version_cmd
            && cmd.is_empty()
        {
            bail!("[{id}] probe.version_cmd must not be an empty array");
        }

        for (i, r) in self.resources.iter().enumerate() {
            let at = format!("[{id}] resource #{} (path = {})", i + 1, r.path);
            ensure_tilde(id, "resource.path", &r.path)?;
            if let Some(ptr) = &r.json_pointer
                && !ptr.starts_with('/')
            {
                bail!("{at}: json_pointer `{ptr}` must start with `/` (RFC 6901)");
            }
            if r.json_pointer.is_some() && r.toml_key.is_some() {
                bail!("{at}: json_pointer and toml_key are mutually exclusive; declare only one");
            }
            if !r.mapper.fits(r.kind) {
                bail!(
                    "{at}: mapper `{}` is not applicable to kind `{:?}`",
                    r.mapper.as_str(),
                    r.kind
                );
            }
            // artifact ⇔ clean_level 是双向绑定：clean_level 是 artifact 的
            // 定义（"可清理，代价是这个"），不是可选修饰。漏写会让 clean 面对
            // 一个不知道该不该动的目录，写在别的 kind 上则是在给不可清理的
            // 东西发清理许可——两个方向都直接拒绝加载。
            match r.kind {
                ResourceKind::Artifact if r.clean_level.is_none() => bail!(
                    "{at}: artifact resources must declare clean_level (l0 / l1 / l2). \
                     If it cannot be cleaned without reinstalling, it is not an artifact — \
                     declare it as kind = \"install\" instead"
                ),
                ResourceKind::Artifact => {}
                _ if r.clean_level.is_some() => bail!(
                    "{at}: clean_level may only be declared on artifact resources; \
                     kind `{:?}` is never cleaned",
                    r.kind
                ),
                _ => {}
            }

            // keep_generations 只在 l2 artifact 上有意义,而且必须带 glob:
            // 「保留最新 N 份代际」的前提是每份代际占一行索引,没有 glob 的
            // stats-only 永远只有一行——N 再大也只会 keep 那唯一一份,声明
            // 就成了静默无效。glob 还必须是直接子项模式(不含 `/`):代际是
            // 「一个目录里滚动的几份文件」,通配跨目录会让同一份资源的两份
            // 代际被 plan 分到两个组里,各留 N 份,与声明的意图不符。两种
            // 都是"写了也不生效"的清单 bug,照例拒绝加载而不是悄悄忽略。
            if let Some(n) = r.keep_generations {
                if r.clean_level != Some(CleanLevel::L2) {
                    bail!(
                        "{at}: keep_generations is only meaningful for l2 artifacts (time- \
                         limited, prune-owned). Declared on level {:?} it either never fires \
                         or gives a non-prune command a second deletion authority — \
                         declare clean_level = \"l2\" or drop the field",
                        r.clean_level
                    );
                }
                if n == 0 {
                    bail!(
                        "{at}: keep_generations = 0 would delete every generation including \
                         the last fallback copy. The minimum is 1 — keep the newest one, \
                         which is what a backup is for"
                    );
                }
                match &r.glob {
                    None => bail!(
                        "{at}: keep_generations requires glob — without it this resource is \
                         a single row, so there are no generations to keep the newest N of"
                    ),
                    Some(g) if g.contains('/') => bail!(
                        "{at}: keep_generations glob `{g}` crosses directories; generations \
                         are the rolling files of one folder, and a `*`-across-subdirs \
                         pattern would split one resource's generations into per-folder \
                         groups. Declare the folder that actually rolls"
                    ),
                    Some(_) => {}
                }
            }

            // install_paths 只在 skill 上有意义:它解的是「一个资源目录内部
            // 混着用户内容与软件本体」这一个问题,而这只在 skill 目录里发生。
            // 写在 artifact 上等于给"清理时跳过一部分"开后门,写在 install
            // 上是同义反复——两种都拒绝加载,免得清单变成许愿池。
            if !r.install_paths.is_empty() {
                if r.kind != ResourceKind::Skill {
                    bail!(
                        "{at}: install_paths may only be declared on skill resources; \
                         kind `{:?}` is either wholly install or wholly cleanable — \
                         split it into two resources instead",
                        r.kind
                    );
                }
                for name in &r.install_paths {
                    if name.is_empty() {
                        bail!("{at}: install_paths must not contain an empty entry");
                    }
                    // 裸目录名,按名匹配任意层级(`gstack/node_modules` 与
                    // `gstack/pkg/a/node_modules` 都要命中)。收路径进来会
                    // 让人以为支持通配/相对定位,实际不支持。
                    if name.contains('/') || name.contains('~') {
                        bail!(
                            "{at}: install_paths entry `{name}` must be a bare directory name, \
                             not a path (no `/`, no `~`); it is matched by name at any depth"
                        );
                    }
                }
            }

            // 重叠路径禁止：体积按声明路径独立聚合，父子同时声明会双算，
            // 「总共多少 GB / 能清多少」当场失真。真要拆细粒度，得先让
            // scan 支持子树扣减；在那之前，清单层直接把它拦在门外。
            for (j, other) in self.resources.iter().enumerate().take(i) {
                if path_contains(&other.path, &r.path) || path_contains(&r.path, &other.path) {
                    bail!(
                        "{at}: path overlaps resource #{} ({}); sizes are aggregated per \
                         declared path, so nesting one inside the other double-counts bytes. \
                         Declare the outer path only",
                        j + 1,
                        other.path
                    );
                }
            }
        }

        // ── [uninstall]：三种所有权各有各的失败模式，全部挡在加载期 ─────
        if let Some(u) = &self.uninstall {
            for path in &u.owns {
                ensure_tilde(id, "uninstall.owns entry", path)?;
            }
            // 拿 owned_roots 而不是 u.owns：`owns` 留空时根是推导出来的，
            // 「共享文件不得落在自己树里」这条对推导出来的根同样要成立。
            let roots = self.owned_roots();
            for (i, s) in u.shared.iter().enumerate() {
                let at = format!("[{id}] uninstall.shared #{} (path = {})", i + 1, s.path);
                ensure_tilde(id, "uninstall.shared.path", &s.path)?;
                match (&s.json_pointer, &s.toml_key) {
                    (Some(_), Some(_)) => bail!(
                        "{at}: json_pointer and toml_key are mutually exclusive; \
                         declare only the one that matches the file's format"
                    ),
                    (None, None) => bail!(
                        "{at}: declare exactly one of json_pointer (RFC 6901, e.g. \
                         \"/mcpServers/foo\") or toml_key (dotted path, e.g. \
                         \"mcp_servers.foo\"). A shared edit with no key would rewrite a \
                         file another agent owns — if the whole path really is ours, \
                         declare it in owns instead"
                    ),
                    (Some(ptr), None) if !ptr.starts_with('/') => bail!(
                        "{at}: json_pointer `{ptr}` must start with `/` (RFC 6901); \
                         write \"/mcpServers/foo\", not \"mcpServers/foo\""
                    ),
                    (None, Some(key)) if key.is_empty() => bail!(
                        "{at}: toml_key must not be empty; an empty dotted path means the \
                         document root, which would clear a file another agent owns. \
                         Write the table that belongs to this agent, e.g. \"mcp_servers.foo\""
                    ),
                    _ => {}
                }
                if s.reason.trim().is_empty() {
                    bail!(
                        "{at}: reason must not be empty; it is the English line shown to the \
                         user right before duster edits someone else's file. Write what the \
                         key is, e.g. \"MCP entry pointing at this agent\""
                    );
                }
                if let Some(root) = roots.iter().find(|r| path_contains(r, &s.path)) {
                    bail!(
                        "{at}: this path sits inside owned root `{root}`, which uninstall \
                         deletes outright — a file is either ours to delete or someone \
                         else's to edit, never both. Drop this shared edit, or narrow owns \
                         so it no longer covers the file"
                    );
                }
            }
            for (i, p) in u.package.iter().enumerate() {
                if p.command.is_empty() {
                    bail!(
                        "[{id}] uninstall.package #{} (manager = {}): command must not be \
                         empty; it is the argv duster prints for the user to run, e.g. \
                         [\"npm\", \"uninstall\", \"-g\", \"@foo/bar\"]. If this agent has no \
                         package-manager uninstall on record, drop the whole \
                         [[uninstall.package]] entry — an empty command is a hint that \
                         hints nothing",
                        i + 1,
                        p.manager.as_str()
                    );
                }
            }
            // residue 是「duster 自己删的已知残留」：删的是行不是文件，所以
            // 定位字段一个都不能空——空 files 是无的放矢，空 match / equals
            // 会匹配到无关行甚至整个文件。路径照样走 ensure_tilde。
            for (i, r) in u.residue.iter().enumerate() {
                let at = format!("[{id}] uninstall.residue #{}", i + 1);
                match r {
                    ResidueSpec::ShellLine {
                        files,
                        match_,
                        with_comment_above: _,
                        why,
                    } => {
                        if files.is_empty() {
                            bail!(
                                "{at}: files must not be empty; a residue that targets no \
                                 file is dead weight — write the rc files that may contain \
                                 the line, or drop the entry"
                            );
                        }
                        for f in files {
                            ensure_tilde(id, "uninstall.residue.files entry", f)?;
                        }
                        if match_.trim().is_empty() {
                            bail!(
                                "{at}: match must not be empty; an empty substring would \
                                 match every line. Write the distinctive fragment of the \
                                 line, e.g. \"/.opencode/bin\""
                            );
                        }
                        if why.trim().is_empty() {
                            bail!(
                                "{at}: why must not be empty; it is the reason shown in the \
                                 uninstall report. Write what this line is and why it \
                                 belongs to this agent"
                            );
                        }
                    }
                    ResidueSpec::SqliteRow {
                        db,
                        table,
                        column,
                        equals,
                        why,
                    } => {
                        ensure_tilde(id, "uninstall.residue.db", db)?;
                        if table.trim().is_empty() {
                            bail!("{at}: table must not be empty");
                        }
                        if column.trim().is_empty() {
                            bail!("{at}: column must not be empty");
                        }
                        if equals.trim().is_empty() {
                            bail!(
                                "{at}: equals must not be empty; an empty value would match \
                                 every row whose {column} is empty — or nothing at all, \
                                 silently. Write the exact row id this agent owns"
                            );
                        }
                        if why.trim().is_empty() {
                            bail!(
                                "{at}: why must not be empty; it is the reason shown in the \
                                 uninstall report. Write what this row is and why it \
                                 belongs to this agent"
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 本 agent 独占的路径根：`uninstall.owns` 有内容就照写，否则由
    /// `[probe]` 的探测路径 + `[[resource]]` 的资源路径推导。
    ///
    /// 推导时会把落在另一个候选之内的路径丢掉，只留最外层——这样调用方拿到
    /// 的一定是互不包含的根，删一遍就够，不会父子各删一次。返回值仍是 `~`
    /// 开头的字符串，展开由调用方负责。
    pub fn owned_roots(&self) -> Vec<String> {
        if let Some(u) = &self.uninstall
            && !u.owns.is_empty()
        {
            return u.owns.clone();
        }
        let candidates: Vec<&String> = self
            .probe
            .any_of
            .iter()
            .chain(&self.probe.all_of)
            .chain(self.resources.iter().map(|r| &r.path))
            .collect();
        let mut out: Vec<String> = Vec::new();
        for (i, c) in candidates.iter().enumerate() {
            let nested = candidates
                .iter()
                .enumerate()
                .any(|(j, o)| j != i && *o != *c && path_contains(o, c));
            if nested || out.iter().any(|kept| kept == *c) {
                continue;
            }
            out.push((*c).clone());
        }
        out
    }
}

/// 清单里的路径必须是 `~` 开头的字符串（不展开、不接受绝对/相对路径）。
fn ensure_tilde(id: &str, what: &str, path: &str) -> Result<()> {
    if path == "~" || path.starts_with("~/") {
        Ok(())
    } else {
        bail!(
            "[{id}] {what} `{path}` must start with `~/` (the manifest layer does not expand paths or accept other forms)"
        );
    }
}

/// `outer` 是否等于 `inner` 或为其祖先目录。按路径分段比较，
/// `~/.claude/cache` 因此不会被误判为 `~/.claude/cache-v2` 的祖先。
fn path_contains(outer: &str, inner: &str) -> bool {
    let outer = outer.trim_end_matches('/');
    match inner.trim_end_matches('/').strip_prefix(outer) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
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
        .map(|(name, src)| {
            parse(src).unwrap_or_else(|e| panic!("built-in manifest {name} is corrupt: {e:#}"))
        })
        .collect()
}

/// 内置清单的原始源文本（文件名, 内容），顺序与 [`BUILTIN`] 声明一致。
///
/// 给上层算「解析规则指纹」用：清单文本决定了同一份源文件被怎么解析，
/// 所以它必须进指纹。给源文本而不是解析后的结构，是为了免掉给整棵
/// `Manifest` 加 `Serialize`；代价是改注释也会让指纹变，可以接受。
pub fn builtin_sources() -> &'static [(&'static str, &'static str)] {
    BUILTIN
}

/// 加载单份用户清单文件。
///
/// 单独开一个入口是给 `duster doctor` 的自检用：[`load_user_dir`] 撞上第一份
/// 坏文件就整体报错，而自检要把用户手写的每一份坏 toml 都点名报出来——
/// 那是他在那份报告里唯一能自己动手修的东西，一次说全才对得起这一项。
pub fn load_user_file(path: &Path) -> Result<Manifest> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read user manifest {}", path.display()))?;
    parse(&src).with_context(|| format!("invalid user manifest: {}", path.display()))
}

/// 加载用户清单目录（`~/.agent-duster/adapters/*.toml`）。
/// 目录不存在视为没有用户清单；单个文件解析失败即整体报错（带文件名）。
pub fn load_user_dir(dir: &Path) -> Result<Vec<Manifest>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read user manifest directory {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    // 按文件名排序,保证加载顺序稳定。
    entries.sort_by_key(|e| e.file_name());

    let mut out = Vec::new();
    for entry in entries {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        out.push(load_user_file(&path)?);
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

        // Install：插件本体不参与清理，clean_level 必须为空。
        let install: Vec<_> = m
            .resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Install)
            .collect();
        assert_eq!(install.len(), 1);
        assert_eq!(install[0].path, "~/.claude/plugins");
        assert_eq!(install[0].clean_level, None);

        // Artifact ×3：全是自动重建的缓存类，一律 l1。
        let levels: Vec<_> = m
            .resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Artifact)
            .map(|r| r.clean_level.expect("artifact 必须有 clean_level"))
            .collect();
        assert_eq!(levels, [CleanLevel::L1, CleanLevel::L1, CleanLevel::L1]);
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
        assert!(
            parse(&src)
                .unwrap_err()
                .to_string()
                .contains("not applicable")
        );

        // probe 三无。
        let src = minimal("").replace("any_of = [\"~/.demo\"]", "");
        assert!(parse(&src).unwrap_err().to_string().contains("probe"));
    }

    /// install_paths 是 skill 专属的「资源内部再分桶」开关：
    /// 用在别的 kind 上，或写成路径，都必须当场拒绝加载。
    #[test]
    fn install_paths_只许写在_skill_上且必须是裸目录名() {
        // 合法：skill + 裸目录名。
        let ok = minimal(
            r#"
[[resource]]
kind = "skill"
scope = "global"
path = "~/.demo/skills"
mapper = "skill/frontmatter-md"
install_paths = ["node_modules", "dist", "bin", ".git"]
"#,
        );
        let m = parse(&ok).expect("skill 上的 install_paths 应当合法");
        assert_eq!(
            m.resources[0].install_paths,
            ["node_modules", "dist", "bin", ".git"]
        );

        // 缺省为空 Vec，不是 None——旧清单不受影响。
        let bare = minimal(
            r#"
[[resource]]
kind = "skill"
scope = "global"
path = "~/.demo/skills"
mapper = "skill/frontmatter-md"
"#,
        );
        assert!(parse(&bare).unwrap().resources[0].install_paths.is_empty());

        // 非 skill：拒绝，并指出该拆成两条资源。
        let wrong_kind = minimal(
            r#"
[[resource]]
kind = "artifact"
scope = "global"
path = "~/.demo/cache"
mapper = "stats-only"
clean_level = "l1"
install_paths = ["node_modules"]
"#,
        );
        let err = parse(&wrong_kind).unwrap_err().to_string();
        assert!(
            err.contains("install_paths") && err.contains("skill"),
            "错误应点名 install_paths 与 skill: {err}"
        );

        // 写成路径：拒绝。
        let with_slash = minimal(
            r#"
[[resource]]
kind = "skill"
scope = "global"
path = "~/.demo/skills"
mapper = "skill/frontmatter-md"
install_paths = ["gstack/node_modules"]
"#,
        );
        let err = parse(&with_slash).unwrap_err().to_string();
        assert!(
            err.contains("bare directory name"),
            "错误应说明只收裸目录名: {err}"
        );

        // 带 `~` 同样拒绝（免得有人以为能写 `~/x`）。
        let with_tilde = with_slash.replace("gstack/node_modules", "~node_modules");
        assert!(
            parse(&with_tilde)
                .unwrap_err()
                .to_string()
                .contains("bare directory name")
        );

        // 空串拒绝：会把整棵树当 install。
        let empty = with_slash.replace("\"gstack/node_modules\"", "\"\"");
        assert!(parse(&empty).unwrap_err().to_string().contains("empty"));
    }

    /// 三份内置清单的 skill 资源必须声明同一套 install_paths——
    /// status 体积口径、归档排除、副本检测的树哈希都读这一个定义，
    /// 任何一份走样都会让「同一个 skill 在两个 agent 下体积不同」。
    #[test]
    fn 内置清单的_skill_install_paths_一致() {
        const EXPECTED: [&str; 4] = ["node_modules", "dist", "bin", ".git"];
        for id in ["claude-code", "codex", "omp"] {
            let m = load_builtin()
                .into_iter()
                .find(|m| m.agent.id == id)
                .unwrap_or_else(|| panic!("内置应包含 {id}"));
            let skill = m
                .resources
                .iter()
                .find(|r| r.kind == ResourceKind::Skill)
                .unwrap_or_else(|| panic!("{id} 应有 skill 资源"));
            assert_eq!(skill.install_paths, EXPECTED, "{id} 的 install_paths 走样");
        }
    }

    /// artifact 必须自带 clean_level：漏写等于让 clean 面对一个不知道
    /// 该不该动的目录，错误信息必须把人引向 `install`。
    #[test]
    fn artifact_without_clean_level_is_rejected() {
        let src = minimal(
            r#"
[[resource]]
kind = "artifact"
scope = "global"
path = "~/.demo/cache"
mapper = "stats-only"
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("must declare clean_level"), "{err}");
        assert!(err.contains("install"), "错误信息要指出正确归类: {err}");
    }

    /// install 是"删了等于卸载"的软件本体：给它发清理许可必须被拒。
    #[test]
    fn install_with_clean_level_is_rejected() {
        let src = minimal(
            r#"
[[resource]]
kind = "install"
scope = "global"
path = "~/.demo/extensions"
mapper = "stats-only"
clean_level = "l1"
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("never cleaned"), "{err}");
    }

    /// `keep_generations` 只在 l2 artifact 上有意义。写在 l1 上等于给
    /// clean 一把第二重删除权（clean 本就整项删，没有"保留 N 份"一说），
    /// 写在别的 kind 上等于给不可清理的东西发代际清理许可——都拒绝加载。
    #[test]
    fn keep_generations_outside_l2_is_rejected() {
        let src = minimal(
            r#"
[[resource]]
kind = "artifact"
scope = "global"
path = "~/.demo/cache"
mapper = "stats-only"
clean_level = "l1"
keep_generations = 2
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("l2"), "{err}");
        assert!(err.contains("keep_generations"), "{err}");

        // 非 artifact 更不许写。
        let src = minimal(
            r#"
[[resource]]
kind = "skill"
scope = "global"
path = "~/.demo/skills"
mapper = "skill/frontmatter-md"
keep_generations = 2
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("l2"), "{err}");
    }

    /// `keep_generations` 必须带直接子项的 glob：没有 glob 的 stats-only
    /// 永远只有一行，「保留最新 N 份」无从谈起；glob 带 `/` 会让同一份
    /// 资源的代际被 plan 按子目录拆组。两种都是写了也不生效的清单 bug。
    #[test]
    fn keep_generations_without_direct_glob_is_rejected() {
        let src = minimal(
            r#"
[[resource]]
kind = "artifact"
scope = "global"
path = "~/.demo/backups"
mapper = "stats-only"
clean_level = "l2"
keep_generations = 2
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("requires glob"), "{err}");

        let src = minimal(
            r#"
[[resource]]
kind = "artifact"
scope = "global"
path = "~/.demo/backups"
mapper = "stats-only"
clean_level = "l2"
glob = "**/*.db"
keep_generations = 2
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("crosses directories"), "{err}");
    }

    /// N = 0 会把唯一副本一起删掉,违反「绝不动唯一副本」红线。
    #[test]
    fn keep_generations_zero_is_rejected() {
        let src = minimal(
            r#"
[[resource]]
kind = "artifact"
scope = "global"
path = "~/.demo/backups"
mapper = "stats-only"
clean_level = "l2"
glob = "*.db"
keep_generations = 0
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("0"), "{err}");
        assert!(err.contains("minimum is 1"), "{err}");
    }

    /// 合法形态:l2 + 直接子项 glob + N ≥ 1。
    #[test]
    fn keep_generations_valid_form_parses() {
        let src = minimal(
            r#"
[[resource]]
kind = "artifact"
scope = "global"
path = "~/.demo/backups"
mapper = "stats-only"
clean_level = "l2"
glob = "*.db"
keep_generations = 2
"#,
        );
        let m = parse(&src).unwrap();
        let r = &m.resources[0];
        assert_eq!(r.keep_generations, Some(2));
    }

    /// 父子路径同时声明会把同一批字节算两遍，直接拒绝加载。
    #[test]
    fn nested_resource_paths_are_rejected() {
        let src = minimal(
            r#"
[[resource]]
kind = "install"
scope = "global"
path = "~/.demo/plugins"
mapper = "stats-only"

[[resource]]
kind = "artifact"
scope = "global"
path = "~/.demo/plugins/cache"
mapper = "stats-only"
clean_level = "l1"
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("double-counts"), "{err}");

        // 只是同前缀、不是同一层级的兄弟目录，必须放行。
        let src = minimal(
            r#"
[[resource]]
kind = "artifact"
scope = "global"
path = "~/.demo/cache"
mapper = "stats-only"
clean_level = "l1"

[[resource]]
kind = "artifact"
scope = "global"
path = "~/.demo/cache-v2"
mapper = "stats-only"
clean_level = "l1"
"#,
        );
        assert!(parse(&src).is_ok());
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

    /// `[uninstall]` 整节可选：老清单一个字不改也要照样加载，
    /// 且 `owned_roots` 要能从 `[probe]` + `[[resource]]` 把根推出来。
    #[test]
    fn 没有_uninstall_节的清单照样加载_且根由_probe_与_resource_推导() {
        let src = minimal(
            r#"
[[resource]]
kind = "skill"
scope = "global"
path = "~/.demo/skills"
mapper = "skill/frontmatter-md"

[[resource]]
kind = "memory"
scope = "global"
path = "~/.config/demo/AGENTS.md"
mapper = "memory/markdown"
"#,
        );
        let m = parse(&src).expect("缺 [uninstall] 不该影响加载");
        assert!(m.uninstall.is_none());
        // ~/.demo/skills 落在 probe 根 ~/.demo 之内被丢掉，只留最外层；
        // ~/.config/demo/AGENTS.md 不在任何 probe 根里，必须留下。
        assert_eq!(m.owned_roots(), ["~/.demo", "~/.config/demo/AGENTS.md"]);
    }

    /// 显式 `owns` 优先于推导：清单作者说了算，不许被 resource 悄悄扩宽。
    #[test]
    fn 显式_owns_覆盖推导结果() {
        let src = minimal(
            r#"
[[resource]]
kind = "memory"
scope = "global"
path = "~/.config/demo/AGENTS.md"
mapper = "memory/markdown"

[uninstall]
owns = ["~/.demo"]
"#,
        );
        let m = parse(&src).unwrap();
        assert_eq!(m.owned_roots(), ["~/.demo"]);
        // 三个字段都缺省成空 Vec，不是 None——消费侧不必到处 unwrap_or_default。
        let u = m.uninstall.as_ref().unwrap();
        assert!(u.shared.is_empty() && u.package.is_empty());
    }

    /// `owns` 走 `ensure_tilde`：绝对路径会让「删整棵树」指向任意位置。
    #[test]
    fn uninstall_owns_必须是波浪号路径() {
        let src = minimal(
            r#"
[uninstall]
owns = ["/opt/demo"]
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("uninstall.owns entry"), "{err}");
        assert!(err.contains("~/"), "错误要说清该写成什么: {err}");
    }

    /// shared 是「别人文件里的一个键」：键必须有且只有一个定位方式。
    #[test]
    fn uninstall_shared_的定位键必须二选一() {
        let both = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.shared]]
path = "~/.other.json"
json_pointer = "/mcpServers/demo"
toml_key = "mcp_servers.demo"
reason = "MCP entry pointing at this agent"
"#,
        );
        let err = parse(&both).unwrap_err().to_string();
        assert!(
            err.contains("json_pointer") && err.contains("toml_key"),
            "错误应同时点名两个字段: {err}"
        );

        // 一个都不写：等于要重写别人的整个文件。
        let neither = both
            .replace("json_pointer = \"/mcpServers/demo\"\n", "")
            .replace("toml_key = \"mcp_servers.demo\"\n", "");
        let err = parse(&neither).unwrap_err().to_string();
        assert!(err.contains("json_pointer"), "{err}");
        assert!(
            err.contains("owns"),
            "错误要指出「整条路径真是我们的就写 owns」: {err}"
        );

        // json_pointer 少了开头的 `/`。
        let bad_ptr = both.replace(
            "json_pointer = \"/mcpServers/demo\"\ntoml_key = \"mcp_servers.demo\"",
            "json_pointer = \"mcpServers/demo\"",
        );
        let err = parse(&bad_ptr).unwrap_err().to_string();
        assert!(
            err.contains("json_pointer") && err.contains("RFC 6901"),
            "{err}"
        );

        // toml_key 空串 = 文档根，会清空别人的文件。
        let empty_key = both.replace(
            "json_pointer = \"/mcpServers/demo\"\ntoml_key = \"mcp_servers.demo\"",
            "toml_key = \"\"",
        );
        let err = parse(&empty_key).unwrap_err().to_string();
        assert!(err.contains("toml_key") && err.contains("root"), "{err}");

        // reason 是动刀前给用户看的唯一一句话，空着等于无声改写别人的文件。
        let no_reason = both
            .replace("toml_key = \"mcp_servers.demo\"\n", "")
            .replace(
                "reason = \"MCP entry pointing at this agent\"",
                "reason = \"   \"",
            );
        let err = parse(&no_reason).unwrap_err().to_string();
        assert!(err.contains("reason"), "{err}");
    }

    /// 一个文件要么是我们的（删），要么是别人的（改键），不可能两者都是。
    /// 这条对显式 `owns` 与推导出来的根同样成立。
    #[test]
    fn uninstall_shared_不许落在_owns_之内() {
        let explicit = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.shared]]
path = "~/.demo/config.json"
json_pointer = "/mcpServers/demo"
reason = "MCP entry pointing at this agent"
"#,
        );
        let err = parse(&explicit).unwrap_err().to_string();
        assert!(err.contains("inside owned root"), "{err}");
        assert!(err.contains("~/.demo"), "错误要点名那个根: {err}");

        // 没写 owns 时根是推导的，同一条规则照样要拦住。
        let derived = explicit.replace("owns = [\"~/.demo\"]\n", "");
        assert!(
            parse(&derived)
                .unwrap_err()
                .to_string()
                .contains("inside owned root")
        );

        // 只是同前缀的兄弟文件，必须放行（~/.demo-shared 不在 ~/.demo 里）。
        let sibling = explicit.replace("~/.demo/config.json", "~/.demo-shared.json");
        assert!(parse(&sibling).is_ok(), "同前缀兄弟路径不该被拦");
    }

    /// package 是「打印给用户去跑的那条命令」：空命令什么都没告诉用户，
    /// manager 只认封闭词汇表里的五个。
    #[test]
    fn uninstall_package_必须有命令且_manager_是封闭词汇表() {
        let empty_cmd = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.package]]
manager = "npm"
detect = ["npm", "ls", "-g", "--depth", "0", "demo"]
command = []
"#,
        );
        let err = parse(&empty_cmd).unwrap_err().to_string();
        assert!(err.contains("command"), "{err}");
        assert!(
            err.contains("[[uninstall.package]]"),
            "错误要指出该整条删掉: {err}"
        );

        // detect 可以为空（无从探测就无条件打印），command 不行。
        let no_detect = empty_cmd
            .replace(
                "detect = [\"npm\", \"ls\", \"-g\", \"--depth\", \"0\", \"demo\"]\n",
                "",
            )
            .replace(
                "command = []",
                "command = [\"npm\", \"uninstall\", \"-g\", \"demo\"]",
            );
        let m = parse(&no_detect).expect("detect 缺省应当合法");
        let p = &m.uninstall.as_ref().unwrap().package[0];
        assert_eq!(p.manager, PackageManager::Npm);
        assert_eq!(p.manager.as_str(), "npm");
        assert!(p.detect.is_empty());

        // 词汇表外的名字：serde 解析即拒绝。
        let bad = no_detect.replace("manager = \"npm\"", "manager = \"yarn\"");
        assert!(parse(&bad).unwrap_err().to_string().contains("yarn"));
    }

    /// residue 是「duster 自己删的已知残留」：两种 kind 各给一份最小合法形态，
    /// 且字段名 `match`（Rust 关键字）要按 serde rename 落回 `match_`。
    #[test]
    fn uninstall_residue_两种_kind_都能解析() {
        let src = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.residue]]
kind = "shell_line"
files = ["~/.zshrc", "~/.bashrc"]
match = "/.demo/bin"
with_comment_above = true
why = "PATH entry added by the demo installer"

[[uninstall.residue]]
kind = "sqlite_row"
db = "~/.cc-switch/cc-switch.db"
table = "providers"
column = "id"
equals = "demo-official"
why = "cc-switch keeps one provider row per agent"
"#,
        );
        let m = parse(&src).expect("两种合法 residue 应当能解析");
        let r = &m.uninstall.as_ref().unwrap().residue;
        assert_eq!(r.len(), 2);
        match &r[0] {
            ResidueSpec::ShellLine {
                files,
                match_,
                with_comment_above,
                why,
            } => {
                assert_eq!(*files, ["~/.zshrc".to_string(), "~/.bashrc".to_string()]);
                assert_eq!(*match_, "/.demo/bin");
                assert!(*with_comment_above);
                assert_eq!(*why, "PATH entry added by the demo installer");
            }
            other => panic!("第一条约应是 shell_line, 实际 {other:?}"),
        }
        match &r[1] {
            ResidueSpec::SqliteRow {
                db,
                table,
                column,
                equals,
                why,
            } => {
                assert_eq!(*db, "~/.cc-switch/cc-switch.db");
                assert_eq!(*table, "providers");
                assert_eq!(*column, "id");
                assert_eq!(*equals, "demo-official");
                assert_eq!(*why, "cc-switch keeps one provider row per agent");
            }
            other => panic!("第二条约应是 sqlite_row, 实际 {other:?}"),
        }

        // with_comment_above 缺省 false；residue 整段缺省空 Vec。
        let no_comment = src.replace("with_comment_above = true\n", "");
        let m = parse(&no_comment).expect("缺省字段应当合法");
        let r = &m.uninstall.as_ref().unwrap().residue;
        match &r[0] {
            ResidueSpec::ShellLine {
                with_comment_above,
                ..
            } => assert!(!with_comment_above),
            other => panic!("第一条约应是 shell_line, 实际 {other:?}"),
        }
        assert!(parse(&minimal("")).unwrap().uninstall.is_none());
    }

    /// `kind` 是封闭词汇表：词汇表外的名字 serde 解析即拒绝，不留后门。
    #[test]
    fn uninstall_residue_的_kind_是封闭词汇表() {
        let src = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.residue]]
kind = "file_line"
files = ["~/.zshrc"]
match = "/.demo/bin"
why = "PATH entry added by the demo installer"
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("file_line"), "{err}");
        assert!(
            err.contains("shell_line") && err.contains("sqlite_row"),
            "错误要点名可选值: {err}"
        );

        // 字段拼写错误同样当场拒绝——静默忽略会让「删行」悄悄失效。
        let typo = src.replace("files = [\"~/.zshrc\"]", "file = \"~/.zshrc\"");
        let err = parse(&typo).unwrap_err().to_string();
        assert!(err.contains("file"), "拼错的字段要被点名: {err}");
    }

    /// `why` 是进卸载报告的唯一一句话，空着等于又退回「无声残留」。
    #[test]
    fn uninstall_residue_的_why_不许为空() {
        let src = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.residue]]
kind = "sqlite_row"
db = "~/.cc-switch/cc-switch.db"
table = "providers"
column = "id"
equals = "demo-official"
why = "   "
"#,
        );
        let err = parse(&src).unwrap_err().to_string();
        assert!(err.contains("why"), "{err}");
    }

    /// residue 的路径字段（`files` / `db`）与别的路径声明同一条规矩：
    /// 必须 `~` 开头——绝对路径会让「删行」指向系统文件。
    #[test]
    fn uninstall_residue_的路径必须是波浪号() {
        let bad_file = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.residue]]
kind = "shell_line"
files = ["/etc/zshrc"]
match = "/.demo/bin"
why = "PATH entry added by the demo installer"
"#,
        );
        let err = parse(&bad_file).unwrap_err().to_string();
        assert!(err.contains("uninstall.residue.files entry"), "{err}");
        assert!(err.contains("~/"), "错误要说清该写成什么: {err}");

        let bad_db = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.residue]]
kind = "sqlite_row"
db = "/var/lib/cc-switch.db"
table = "providers"
column = "id"
equals = "demo-official"
why = "cc-switch keeps one provider row per agent"
"#,
        );
        let err = parse(&bad_db).unwrap_err().to_string();
        assert!(err.contains("uninstall.residue.db"), "{err}");
        assert!(err.contains("~/"), "错误要说清该写成什么: {err}");
    }

    /// 定位字段空着会让「删行」变成无的放矢或整表误伤——当场拒绝加载。
    #[test]
    fn uninstall_residue_的定位字段不许为空() {
        // files 空：这条残留不指向任何文件，纯死重。
        let no_files = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.residue]]
kind = "shell_line"
files = []
match = "/.demo/bin"
why = "PATH entry added by the demo installer"
"#,
        );
        let err = parse(&no_files).unwrap_err().to_string();
        assert!(err.contains("files"), "{err}");

        // match 空串：子串匹配会命中每一行，等于要改整个 rc 文件。
        let no_match = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.residue]]
kind = "shell_line"
files = ["~/.zshrc"]
match = ""
why = "PATH entry added by the demo installer"
"#,
        );
        let err = parse(&no_match).unwrap_err().to_string();
        assert!(err.contains("match"), "{err}");

        // equals 空串：WHERE id = '' 要么全不命中（静默失败）要么误删。
        let no_equals = minimal(
            r#"
[uninstall]
owns = ["~/.demo"]

[[uninstall.residue]]
kind = "sqlite_row"
db = "~/.cc-switch/cc-switch.db"
table = "providers"
column = "id"
equals = ""
why = "cc-switch keeps one provider row per agent"
"#,
        );
        let err = parse(&no_equals).unwrap_err().to_string();
        assert!(err.contains("equals"), "{err}");
    }

    /// 内置清单的 `[uninstall]` 必须**实测填满**，不许空着混过加载校验。
    /// 断言写成本机 2026-08-11 的实测结论：谁归 mise 的 npm 包、谁归 brew cask、
    /// 谁查不到包管理器所以整条不写。清单被水改时这里会当场红。
    #[test]
    fn 内置清单的_uninstall_节全部实测填满() {
        let manifests = load_builtin();
        assert_eq!(manifests.len(), 11);

        for m in &manifests {
            let u = m
                .uninstall
                .as_ref()
                .unwrap_or_else(|| panic!("{} 缺 [uninstall] 节", m.agent.id));
            assert!(!u.owns.is_empty(), "{} 的 owns 不许为空", m.agent.id);
            // owns 必须与 M1 `uninstall --data-only` 的删除根一致：
            // plan_uninstall 拿 probe 里实际存在的路径当根，所以 owns 就该是
            // probe 的那几条，一条不多一条不少。
            let probe_roots: Vec<&String> = m.probe.any_of.iter().chain(&m.probe.all_of).collect();
            assert_eq!(
                u.owns.iter().collect::<Vec<_>>(),
                probe_roots,
                "{} 的 owns 与 probe 根不一致（会和 M1 --data-only 删的东西对不上）",
                m.agent.id
            );
            assert_eq!(m.owned_roots(), u.owns);
            // 用户能看见的字符串必须是英文：reason 会在改别人文件前打印出来。
            for s in &u.shared {
                assert!(
                    s.reason.is_ascii() && !s.reason.trim().is_empty(),
                    "{} 的 shared.reason 必须是非空 ASCII 英文",
                    m.agent.id
                );
            }
        }

        let get = |id: &str| {
            manifests
                .iter()
                .find(|m| m.agent.id == id)
                .unwrap_or_else(|| panic!("内置应包含 {id}"))
                .uninstall
                .clone()
                .unwrap()
        };

        // 多根：OpenCode 的三处 XDG 目录一条都不能少。
        assert_eq!(
            get("opencode").owns,
            [
                "~/.opencode",
                "~/.config/opencode",
                "~/.local/share/opencode"
            ]
        );
        assert_eq!(get("claude-code").owns, ["~/.claude", "~/.claude.json"]);

        // shared：本机实测只有 cc-switch 一家把自己的键写进了别人的文件
        // （~/.codex/config.toml 的 model_catalog_json 指向它生成的目录）。
        let with_shared: Vec<&str> = manifests
            .iter()
            .filter(|m| !m.uninstall.as_ref().unwrap().shared.is_empty())
            .map(|m| m.agent.id.as_str())
            .collect();
        assert_eq!(with_shared, ["cc-switch"]);
        let ccs = get("cc-switch");
        assert_eq!(ccs.shared.len(), 1);
        assert_eq!(ccs.shared[0].path, "~/.codex/config.toml");
        assert_eq!(
            ccs.shared[0].toml_key.as_deref(),
            Some("model_catalog_json")
        );
        assert_eq!(ccs.shared[0].json_pointer, None);

        // residue：cc-switch 的 providers 行 ×3 + opencode 的 shell rc PATH 行。
        // 2026-08-18 实测落库（claude-official / codex-official /
        // gemini-official 三行都在，opencode 在 providers 里没有行——它只
        // 有 mcp_servers / skills 两表的 enabled_opencode 布尔列，列级残留
        // 表达不了，仍记注释）。清单被水改时这里会当场红。
        let with_residue: Vec<&str> = manifests
            .iter()
            .filter(|m| !m.uninstall.as_ref().unwrap().residue.is_empty())
            .map(|m| m.agent.id.as_str())
            .collect();
        assert_eq!(with_residue, ["claude-code", "codex", "gemini-cli", "opencode"]);
        let provider_ids = [
            ("claude-code", "claude-official"),
            ("codex", "codex-official"),
            ("gemini-cli", "gemini-official"),
        ];
        for (id, expected) in provider_ids {
            let u = get(id);
            assert_eq!(u.residue.len(), 1, "{id} 应恰好一条 residue");
            match &u.residue[0] {
                ResidueSpec::SqliteRow {
                    db,
                    table,
                    column,
                    equals,
                    why,
                } => {
                    assert_eq!(*db, "~/.cc-switch/cc-switch.db");
                    assert_eq!(*table, "providers");
                    assert_eq!(*column, "id");
                    assert_eq!(*equals, expected);
                    assert!(!why.trim().is_empty());
                }
                other => panic!("{id} 的 residue 应是 sqlite_row, 实际 {other:?}"),
            }
        }
        match &get("opencode").residue[0] {
            ResidueSpec::ShellLine {
                files,
                match_,
                with_comment_above,
                why,
            } => {
                assert_eq!(
                    *files,
                    ["~/.zshrc", "~/.zprofile", "~/.bashrc", "~/.bash_profile"]
                );
                assert_eq!(*match_, "/.opencode/bin");
                assert!(*with_comment_above);
                assert!(!why.trim().is_empty());
            }
            other => panic!("opencode 的 residue 应是 shell_line, 实际 {other:?}"),
        }

        // package：五个 mise 管的 npm 包 + 一个 brew cask，其余五家查不到
        // 包管理器，整条不写（空列表是诚实，猜一条 npm uninstall -g 是危险）。
        let npm_managed = [
            ("claude-code", "npm:@anthropic-ai/claude-code", "claude"),
            ("codex", "npm:@openai/codex", "codex"),
            ("gemini-cli", "npm:@google/gemini-cli", "gemini"),
            ("omp", "npm:@oh-my-pi/pi-coding-agent", "omp"),
            ("pi", "npm:@earendil-works/pi-coding-agent", "pi"),
        ];
        for (id, pkg, bin) in npm_managed {
            let u = get(id);
            assert_eq!(u.package.len(), 1, "{id} 应恰好一条 package 线索");
            let p = &u.package[0];
            assert_eq!(p.manager, PackageManager::Npm);
            assert_eq!(p.detect, ["mise", "which", bin]);
            assert_eq!(p.command, ["mise", "unuse", "--global", pkg]);
        }

        let brew = get("cc-switch");
        assert_eq!(brew.package.len(), 1);
        assert_eq!(brew.package[0].manager, PackageManager::Brew);
        assert_eq!(
            brew.package[0].command,
            ["brew", "uninstall", "--cask", "cc-switch"]
        );

        for id in ["opencode", "cursor", "qoder", "kimi-cli", "copilot-cli"] {
            assert!(
                get(id).package.is_empty(),
                "{id} 在本机查不到包管理器，package 必须留空"
            );
        }
    }
}
