//! `duster scan`:探测 agent → 遍历资源 → 写入索引。默认增量,`--full` 全量。
//!
//! 编排流程(每个 agent 独立处理,单个资源/文件失败进 warnings 不中断):
//! 1. 加载清单(内置 + `<home>/.agent-duster/adapters`),逐个 probe;
//! 2. 已安装的按 `[[resource]]` 声明逐类采集并 upsert;
//!    session 走增量:cheap print 未变(`changed=false`)则跳过重解析;
//! 3. 每类处理完调用 `delete_stale_resources` 清掉本轮未见的旧行;
//! 4. 候选目录中未被任何清单认领的计入 unclassified。
//!
//! `home` 可注入(测试传 tempdir 当假 home),清单/资源路径中的 `~`
//! 一律相对注入的 home 展开,绝不触碰真实用户目录。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_adapter::codec::{self, Doc};
use duster_adapter::manifest::{self, Manifest, ManifestScope, MapperName, ResourceSection};
use duster_adapter::mapper::{mcp, skill};
use duster_adapter::native::{claude_session, codex_session};
use duster_adapter::probe::{self, ProbeSpec};
use duster_fs::walk::{WalkOptions, walk_files, walk_stats};
use duster_index::db::Index;
use duster_index::upsert::{self, ResourceRow};
use duster_model::{AgentInfo, ResourceKind};

/// scan 的输入选项。
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// 假 home 注入口(测试用);缺省用真实用户主目录。
    pub home: Option<PathBuf>,
    /// 索引库路径;缺省 `<home>/.agent-duster/index.db`。
    pub index_path: Option<PathBuf>,
    /// true 时忽略 cheap print 短路,所有会话全部重解析。
    pub full: bool,
}

/// 单个 agent 的扫描结果。
#[derive(Debug, Serialize)]
pub struct AgentReport {
    pub agent_id: String,
    pub installed: bool,
    /// 本轮 upsert 的资源行数(mcp server / skill / 会话文件 / ...)。
    pub resources: usize,
    /// 各资源类的行数(kind -> 数量),零计数的 kind 不出现。
    pub kind_counts: BTreeMap<String, usize>,
    /// 本轮资源体积合计(mcp 配置行计 0,见 [`ResourceRow`] 约定)。
    pub bytes: u64,
    /// 本轮真正重解析入索引的会话文件数(增量短路的不算)。
    pub sessions_indexed: usize,
    /// 采集过程中的非致命失败(单个文件损坏、格式不符等)。
    pub warnings: Vec<String>,
}

impl AgentReport {
    /// 记一行资源:总数、分类计数、体积一次记齐。
    fn tally(&mut self, kind: &str, bytes: u64) {
        self.resources += 1;
        *self.kind_counts.entry(kind.to_string()).or_insert(0) += 1;
        self.bytes += bytes;
    }
}

/// 未被任何清单认领的候选目录。
#[derive(Debug, Serialize)]
pub struct UnclassifiedDir {
    /// `~` 形式的展示路径,如 `~/.cursor`。
    pub path: String,
    pub bytes: u64,
}

/// 一次完整扫描的汇总报告。
#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub agents: Vec<AgentReport>,
    pub unclassified: Vec<UnclassifiedDir>,
    /// agents 与 unclassified 的字节数总和。
    pub total_bytes: u64,
    pub duration_ms: u64,
}

/// 疑似 agent 数据目录的候选表(来源 = todo 附录「unclassified 候选目录」)。
/// 这些 agent 尚无适配器清单;一旦用户/内置清单认领了同名目录即自动退出候选。
const UNCLASSIFIED_CANDIDATES: &[&str] = &[
    "~/.cursor",
    "~/.copilot",
    "~/.kimi",
    "~/.opencode",
    "~/.qoder",
    "~/.cc-switch",
    "~/.pi",
    "~/.gstack",
    "~/.baoyu-skills",
];

/// 五大资源类的库内 kind 字符串,顺序与 [`ResourceKind`] 一致。
const ALL_KINDS: [&str; 5] = ["mcp", "skill", "memory", "session", "artifact"];

/// 执行一次扫描:probe → 采集 → upsert → 清理 stale → unclassified 统计。
pub fn scan(opts: &ScanOptions) -> Result<ScanReport> {
    let started = Instant::now();
    let home = resolve_home(opts.home.as_deref())?;
    let index_path = opts
        .index_path
        .clone()
        .unwrap_or_else(|| default_index_path(&home));

    let manifests = manifest::load_all(Some(&home.join(".agent-duster").join("adapters")))?;
    let idx = Index::open(&index_path)?;

    let mut agents = Vec::with_capacity(manifests.len());
    for m in &manifests {
        agents.push(scan_agent(&idx, m, &home, opts.full)?);
    }
    drop(idx); // 尽早释放单实例写锁,unclassified 统计不需要索引。

    let unclassified = collect_unclassified(&manifests, &home);
    let total_bytes = agents.iter().map(|a| a.bytes).sum::<u64>()
        + unclassified.iter().map(|u| u.bytes).sum::<u64>();

    Ok(ScanReport {
        agents,
        unclassified,
        total_bytes,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// 缺省索引库位置:`<home>/.agent-duster/index.db`。
pub(crate) fn default_index_path(home: &Path) -> PathBuf {
    home.join(".agent-duster").join("index.db")
}

/// 确定 home:优先注入值,否则真实用户主目录(经 duster-fs 的 `~` 展开)。
fn resolve_home(injected: Option<&Path>) -> Result<PathBuf> {
    if let Some(h) = injected {
        return Ok(h.to_path_buf());
    }
    let h = duster_fs::path::expand_tilde("~");
    if h == Path::new("~") {
        bail!("无法确定用户主目录(HOME 未设置)");
    }
    Ok(h)
}

/// 把 `~`/`~/...` 相对指定 home 展开;其余形式原样返回。
fn expand(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

/// 扫描单个 agent:probe 判定安装 → 逐资源采集 → 逐 kind 清 stale。
fn scan_agent(idx: &Index, m: &Manifest, home: &Path, full: bool) -> Result<AgentReport> {
    let agent_id = m.agent.id.clone();
    let spec = ProbeSpec {
        any_of: m.probe.any_of.clone(),
        all_of: m.probe.all_of.clone(),
        binary: m.probe.binary.clone(),
        version_cmd: m.probe.version_cmd.clone(),
    };
    let outcome = probe::probe(&spec, home);

    let mut report = AgentReport {
        agent_id: agent_id.clone(),
        installed: outcome.installed,
        resources: 0,
        kind_counts: BTreeMap::new(),
        bytes: 0,
        sessions_indexed: 0,
        warnings: Vec::new(),
    };
    if !outcome.installed {
        return Ok(report);
    }

    let info = AgentInfo {
        id: agent_id.clone(),
        display_name: m.agent.display_name.clone(),
        root: outcome.root.clone().unwrap_or_else(|| home.to_path_buf()),
        version: outcome.version.clone(),
    };
    upsert::upsert_agent(idx.conn(), &info, now_ms())
        .with_context(|| format!("写入 agent `{agent_id}` 失败"))?;

    // 本轮各 kind 见到的 key,收尾时据此清 stale(没声明的 kind 也要清:
    // 清单撤掉某类资源后,旧行必须跟着消失)。
    let mut seen: BTreeMap<&'static str, Vec<String>> =
        ALL_KINDS.iter().map(|k| (*k, Vec::new())).collect();

    for r in &m.resources {
        let kind = kind_str(r.kind);
        let seen_kind = seen.get_mut(kind).expect("ALL_KINDS 覆盖全部资源类");
        if let Err(e) = scan_resource(idx, &agent_id, r, home, full, seen_kind, &mut report) {
            report
                .warnings
                .push(format!("资源 {}({kind})采集失败: {e:#}", r.path));
        }
    }

    for kind in ALL_KINDS {
        upsert::delete_stale_resources(idx.conn(), &agent_id, kind, &seen[kind])
            .with_context(|| format!("清理 `{agent_id}` 的 stale {kind} 失败"))?;
    }
    Ok(report)
}

/// 按 mapper 分派单个 `[[resource]]` 声明。
fn scan_resource(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    home: &Path,
    full: bool,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    let path = expand(&r.path, home);
    match r.mapper {
        MapperName::McpStandardJson | MapperName::McpCodexToml => {
            scan_mcp(idx, agent_id, r, &path, seen, report)
        }
        MapperName::SkillFrontmatterMd => scan_skills(idx, agent_id, r, &path, seen, report),
        MapperName::MemoryMarkdown => scan_memory(idx, agent_id, r, &path, seen, report),
        MapperName::NativeClaudeSession | MapperName::NativeCodexSession => {
            scan_sessions(idx, agent_id, r, &path, full, seen, report)
        }
        // stats-only:任何 kind 通用,只记体积不解析(artifact 只有这一条路)。
        MapperName::StatsOnly => scan_stats_only(idx, agent_id, r, &path, seen, report),
    }
}

/// mcp:读配置文件 → pointer/toml_key 定位 → mapper 归一化,每个 server 一行。
fn scan_mcp(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    cfg: &Path,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    if !cfg.is_file() {
        return Ok(()); // 装了 agent 但没配过 MCP,不算异常。
    }
    let meta =
        std::fs::metadata(cfg).with_context(|| format!("读取元数据失败: {}", cfg.display()))?;
    let mtime_ns = mtime_ns_of(&meta);
    let doc = codec::read_file(cfg)?;

    let servers = match (r.mapper, &doc) {
        (MapperName::McpStandardJson, Doc::Json(v)) => {
            let node = match &r.json_pointer {
                Some(ptr) => codec::json_pointer(v, ptr),
                None => Some(v),
            };
            // pointer 落空 = 文件里还没有 mcpServers 段,视为零个 server。
            node.map(mcp::from_standard_json)
                .transpose()?
                .unwrap_or_default()
        }
        (MapperName::McpCodexToml, Doc::Toml(v)) => {
            let node = match &r.toml_key {
                Some(key) => codec::toml_path(v, key),
                None => Some(v),
            };
            node.map(mcp::from_codex_toml)
                .transpose()?
                .unwrap_or_default()
        }
        _ => bail!(
            "mapper `{}` 与文件实际格式不匹配: {}",
            r.mapper.as_str(),
            cfg.display()
        ),
    };

    for s in &servers {
        let row = ResourceRow {
            agent_id: agent_id.to_string(),
            kind: "mcp".to_string(),
            scope: scope_str(r.scope).to_string(),
            key: s.name.clone(),
            path: cfg.display().to_string(),
            // mcp 是配置文件里的一段声明,不占独立磁盘体积,size 记 0。
            size: 0,
            mtime_ns,
            hash_content: Some(*s.content_hash().as_bytes()),
            cheap_print: None,
        };
        upsert::upsert_resource(idx.conn(), &row)
            .with_context(|| format!("upsert mcp `{}` 失败", s.name))?;
        seen.push(s.name.clone());
        report.tally("mcp", 0);
    }
    Ok(())
}

/// skill:一级子目录发现,每个 skill 一行,size 为目录聚合体积。
/// hash 留空——目录树哈希开销大,M1 dedupe 时才按需计算。
fn scan_skills(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    dir: &Path,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    let skills = skill::discover_skills(dir)?; // 目录不存在返回空,无需预判。
    for s in &skills {
        let result = (|| -> Result<()> {
            let stats = walk_stats(&s.root, &WalkOptions::default())?;
            let meta = std::fs::metadata(&s.root)?;
            let row = ResourceRow {
                agent_id: agent_id.to_string(),
                kind: "skill".to_string(),
                scope: scope_str(r.scope).to_string(),
                key: s.name.clone(),
                path: s.root.display().to_string(),
                size: stats.total_bytes,
                mtime_ns: mtime_ns_of(&meta),
                hash_content: None,
                cheap_print: None,
            };
            upsert::upsert_resource(idx.conn(), &row)?;
            seen.push(s.name.clone());
            report.tally("skill", stats.total_bytes);
            Ok(())
        })();
        if let Err(e) = result {
            report
                .warnings
                .push(format!("skill `{}` 统计失败: {e:#}", s.name));
        }
    }
    Ok(())
}

/// memory:单文件资源,存在才记一行。
fn scan_memory(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    file: &Path,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    if !file.is_file() {
        return Ok(());
    }
    let meta =
        std::fs::metadata(file).with_context(|| format!("读取元数据失败: {}", file.display()))?;
    let key = file_name_of(file);
    let row = ResourceRow {
        agent_id: agent_id.to_string(),
        kind: "memory".to_string(),
        scope: scope_str(r.scope).to_string(),
        key: key.clone(),
        path: file.display().to_string(),
        size: meta.len(),
        mtime_ns: mtime_ns_of(&meta),
        hash_content: None,
        cheap_print: None,
    };
    upsert::upsert_resource(idx.conn(), &row)?;
    seen.push(key);
    report.tally("memory", meta.len());
    Ok(())
}

/// session:递归枚举 jsonl,每文件一行;cheap print 未变则跳过重解析(增量核心)。
fn scan_sessions(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    dir: &Path,
    full: bool,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let is_codex = r.mapper == MapperName::NativeCodexSession;

    // Claude:projects/<路径编码>/*.jsonl;Codex:sessions/**/rollout-*.jsonl。
    // 统一递归遍历再按文件名过滤,兼容两种目录深度。
    let mut files: Vec<PathBuf> = Vec::new();
    walk_files(dir, &WalkOptions::default(), |p, meta| {
        if !meta.is_file() {
            return; // 符号链接不当会话文件处理。
        }
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let hit = if is_codex {
            name.starts_with("rollout-") && name.ends_with(".jsonl")
        } else {
            name.ends_with(".jsonl")
        };
        if hit {
            files.push(p.to_path_buf());
        }
    })?;
    files.sort(); // 遍历是并行的,排序保证 upsert 顺序稳定。

    for f in &files {
        if let Err(e) = index_session_file(idx, agent_id, r, f, full, is_codex, seen, report) {
            report
                .warnings
                .push(format!("会话文件 {} 索引失败: {e:#}", f.display()));
        }
    }
    Ok(())
}

/// 单个会话文件:upsert 行 → changed(或 full)才重解析 + 重建 turn/FTS。
#[allow(clippy::too_many_arguments)]
fn index_session_file(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    file: &Path,
    full: bool,
    is_codex: bool,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    let cp = duster_fs::hash::cheap_print(file)?;
    let key = file_name_of(file);
    let row = ResourceRow {
        agent_id: agent_id.to_string(),
        kind: "session".to_string(),
        scope: scope_str(r.scope).to_string(),
        key: key.clone(),
        path: file.display().to_string(),
        size: cp.size,
        mtime_ns: cp.mtime_ns,
        hash_content: None,
        cheap_print: Some(cp.to_bytes()),
    };
    let outcome = upsert::upsert_resource(idx.conn(), &row)?;
    seen.push(key);
    report.tally("session", cp.size);

    if outcome.changed || full {
        let (_meta, turns) = if is_codex {
            codex_session::parse(file)?
        } else {
            claude_session::parse(file)?
        };
        upsert::replace_turns(idx.conn(), outcome.rid, &turns)?;
        report.sessions_indexed += 1;
    }
    Ok(())
}

/// stats-only(含 artifact):目录聚合体积一行,或单文件一行;不解析内容。
fn scan_stats_only(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    path: &Path,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    let (size, mtime_ns) = if path.is_dir() {
        let stats = walk_stats(path, &WalkOptions::default())?;
        let meta = std::fs::metadata(path)?;
        (stats.total_bytes, mtime_ns_of(&meta))
    } else if path.is_file() {
        let meta = std::fs::metadata(path)?;
        (meta.len(), mtime_ns_of(&meta))
    } else {
        return Ok(());
    };

    let key = file_name_of(path);
    let row = ResourceRow {
        agent_id: agent_id.to_string(),
        kind: kind_str(r.kind).to_string(),
        scope: scope_str(r.scope).to_string(),
        key: key.clone(),
        path: path.display().to_string(),
        size,
        mtime_ns,
        hash_content: None,
        cheap_print: None,
    };
    upsert::upsert_resource(idx.conn(), &row)?;
    seen.push(key);
    report.tally(kind_str(r.kind), size);
    Ok(())
}

/// 候选目录中未被任何清单认领的,统计体积进 unclassified。
fn collect_unclassified(manifests: &[Manifest], home: &Path) -> Vec<UnclassifiedDir> {
    // 清单认领的全部路径前缀:probe 路径 + 资源路径。
    let claimed: Vec<PathBuf> = manifests
        .iter()
        .flat_map(|m| {
            m.probe
                .any_of
                .iter()
                .chain(&m.probe.all_of)
                .chain(m.resources.iter().map(|r| &r.path))
        })
        .map(|raw| expand(raw, home))
        .collect();

    let mut out = Vec::new();
    for cand in UNCLASSIFIED_CANDIDATES {
        let dir = expand(cand, home);
        if !dir.is_dir() {
            continue;
        }
        // 任何清单路径落在该目录之下即视为已认领。
        if claimed.iter().any(|p| p.starts_with(&dir)) {
            continue;
        }
        if let Ok(stats) = walk_stats(&dir, &WalkOptions::default()) {
            out.push(UnclassifiedDir {
                path: (*cand).to_string(),
                bytes: stats.total_bytes,
            });
        }
    }
    out
}

/// [`ResourceKind`] -> 库内 kind 字符串(与 serde lowercase 一致)。
fn kind_str(k: ResourceKind) -> &'static str {
    match k {
        ResourceKind::Mcp => "mcp",
        ResourceKind::Skill => "skill",
        ResourceKind::Memory => "memory",
        ResourceKind::Session => "session",
        ResourceKind::Artifact => "artifact",
    }
}

/// [`ManifestScope`] -> 库内 scope 字符串。
fn scope_str(s: ManifestScope) -> &'static str {
    match s {
        ManifestScope::Global => "global",
        ManifestScope::Project => "project",
    }
}

/// 文件/目录名兜底提取(无 file_name 时退化为完整路径展示)。
fn file_name_of(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// 元数据 mtime -> 纳秒;拿不到(平台/时钟异常)记 0,不中断扫描。
fn mtime_ns_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// 当前 Unix 毫秒。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 手工临时目录守卫:duster-core 无 tempfile 依赖,用 std 自建 + Drop 清理。
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("duster-core-{tag}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 迷你清单:mcp + skill + session + artifact 各一段,probe 指向 ~/.fake。
    const FAKE_MANIFEST: &str = r#"
[agent]
id = "fake-agent"
display_name = "Fake Agent"

[probe]
any_of = ["~/.fake"]

[[resource]]
kind = "mcp"
scope = "global"
path = "~/.fake/mcp.json"
json_pointer = "/mcpServers"
mapper = "mcp/standard-json"

[[resource]]
kind = "skill"
scope = "global"
path = "~/.fake/skills"
mapper = "skill/frontmatter-md"

[[resource]]
kind = "session"
scope = "global"
path = "~/.fake/projects"
mapper = "native/claude-session"

[[resource]]
kind = "artifact"
scope = "global"
path = "~/.fake/artifacts"
mapper = "stats-only"
clean_level = "l1"
"#;

    /// 永远探测不到的 agent,用于 installed=false 断言。
    const GHOST_MANIFEST: &str = r#"
[agent]
id = "ghost-agent"
display_name = "Ghost"

[probe]
any_of = ["~/.ghost-nowhere"]
"#;

    /// 在假 home 下搭一个迷你 .fake agent:1 mcp + 1 skill + 1 session + artifact。
    fn build_fake_home(home: &Path) {
        let adapters = home.join(".agent-duster/adapters");
        fs::create_dir_all(&adapters).unwrap();
        fs::write(adapters.join("fake-agent.toml"), FAKE_MANIFEST).unwrap();
        fs::write(adapters.join("ghost-agent.toml"), GHOST_MANIFEST).unwrap();

        let fake = home.join(".fake");
        fs::create_dir_all(&fake).unwrap();
        fs::write(
            fake.join("mcp.json"),
            r#"{"mcpServers":{"echo-srv":{"command":"echo","args":["hi"]}}}"#,
        )
        .unwrap();

        let skill_dir = fake.join("skills/demo-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: 演示\n---\n正文若干",
        )
        .unwrap();

        // session 样本从 fixtures 拷贝(4 个真实轮次 + 若干控制记录/坏行)。
        let proj = fake.join("projects/-Users-tester-projects-demo");
        fs::create_dir_all(&proj).unwrap();
        let sample = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/sessions/claude-basic.jsonl");
        fs::copy(&sample, proj.join("session-1.jsonl")).unwrap();

        let artifacts = fake.join("artifacts");
        fs::create_dir_all(&artifacts).unwrap();
        fs::write(artifacts.join("cache.bin"), vec![0u8; 128]).unwrap();

        // unclassified 候选:存在且没被任何清单认领(gstack 无内置清单)。
        let gstack = home.join(".gstack");
        fs::create_dir_all(&gstack).unwrap();
        fs::write(gstack.join("junk.log"), b"0123456789").unwrap();
    }

    #[test]
    fn 扫描_增量_full_status_全链路() {
        let home = TempHome::new("scan");
        build_fake_home(home.path());
        let index_path = home.path().join(".agent-duster/index.db");
        let opts = ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        };

        // 首次扫描:全部资源入索引。
        let report = scan(&opts).unwrap();
        let fake = report
            .agents
            .iter()
            .find(|a| a.agent_id == "fake-agent")
            .expect("报告里应有 fake-agent");
        assert!(fake.installed);
        assert!(fake.warnings.is_empty(), "warnings: {:?}", fake.warnings);
        assert_eq!(fake.resources, 4, "mcp+skill+session+artifact 各 1 行");
        for k in ["mcp", "skill", "session", "artifact"] {
            assert_eq!(fake.kind_counts.get(k), Some(&1), "kind_counts[{k}]");
        }
        assert!(fake.bytes > 0);
        assert_eq!(fake.sessions_indexed, 1);
        assert!(report.total_bytes > 0);

        let ghost = report
            .agents
            .iter()
            .find(|a| a.agent_id == "ghost-agent")
            .unwrap();
        assert!(!ghost.installed);
        assert_eq!(ghost.resources, 0);

        // unclassified:~/.gstack 存在且未被认领。
        let q = report
            .unclassified
            .iter()
            .find(|u| u.path == "~/.gstack")
            .expect("~/.gstack 应进 unclassified");
        assert!(q.bytes >= 10);

        // 索引行确实落库:4 行 resource + 4 个轮次的 turn。
        {
            let idx = Index::open_readonly(&index_path).unwrap();
            let res: i64 = idx
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM resource WHERE agent_id = 'fake-agent'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(res, 4);
            let turns: i64 = idx
                .conn()
                .query_row("SELECT COUNT(*) FROM turn", [], |r| r.get(0))
                .unwrap();
            assert_eq!(turns, 4, "claude-basic 样本含 4 个真实对话轮次");
        }

        // 二次扫描(无变动):cheap print 短路,sessions_indexed=0。
        let report2 = scan(&opts).unwrap();
        let fake2 = report2
            .agents
            .iter()
            .find(|a| a.agent_id == "fake-agent")
            .unwrap();
        assert_eq!(
            fake2.sessions_indexed, 0,
            "增量短路失效: {:?}",
            fake2.warnings
        );
        assert_eq!(fake2.resources, 4);

        // full=true:忽略短路,强制重解析。
        let full_opts = ScanOptions {
            full: true,
            ..opts.clone()
        };
        let report3 = scan(&full_opts).unwrap();
        let fake3 = report3
            .agents
            .iter()
            .find(|a| a.agent_id == "fake-agent")
            .unwrap();
        assert_eq!(fake3.sessions_indexed, 1);

        // status 聚合与扫描结果一致。
        let st = crate::status::status(Some(&index_path)).unwrap();
        let a = st
            .agents
            .iter()
            .find(|a| a.agent_id == "fake-agent")
            .expect("status 应含 fake-agent");
        assert!(a.last_scan_ms.unwrap() > 0);
        assert!(a.bytes > 0);
        assert_eq!(a.kind_counts.get("mcp").copied(), Some(1));
        assert_eq!(a.kind_counts.get("skill").copied(), Some(1));
        assert_eq!(a.kind_counts.get("session").copied(), Some(1));
        assert_eq!(a.kind_counts.get("artifact").copied(), Some(1));
        assert!(st.total_bytes > 0);
    }

    #[test]
    fn 资源消失后_二次扫描清掉_stale_行() {
        let home = TempHome::new("stale");
        build_fake_home(home.path());
        let index_path = home.path().join(".agent-duster/index.db");
        let opts = ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        };
        scan(&opts).unwrap();

        // 删掉 skill 目录,再扫:skill 行应被 delete_stale_resources 清掉。
        fs::remove_dir_all(home.path().join(".fake/skills")).unwrap();
        let report = scan(&opts).unwrap();
        let fake = report
            .agents
            .iter()
            .find(|a| a.agent_id == "fake-agent")
            .unwrap();
        assert_eq!(fake.resources, 3);

        let idx = Index::open_readonly(&index_path).unwrap();
        let skills: i64 = idx
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM resource WHERE agent_id = 'fake-agent' AND kind = 'skill'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(skills, 0);
    }
}
