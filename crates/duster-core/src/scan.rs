//! `duster scan`:探测 agent → 遍历资源 → 写入索引。默认增量,`--full` 全量。
//!
//! 编排流程(每个 agent 独立处理,单个资源/文件失败进 warnings 不中断):
//! 1. 加载清单(内置 + `<home>/.agent-duster/adapters`),逐个 probe;
//! 2. 已安装的按 `[[resource]]` 声明逐类采集并 upsert;
//!    session 走增量:cheap print 未变(`changed=false`)则跳过重解析;
//!    未安装的**不采集但照跑 stale 清理**(空 seen 集合)——用户手动 rm 掉
//!    目录、没走 uninstall 时,旧资源行与 agent 行要能自然消失,列表才不撒谎;
//! 3. 每类处理完调用 `delete_stale_resources` 清掉本轮未见的旧行;
//! 4. 候选目录中未被任何清单认领的计入 unclassified。
//!
//! `home` 可注入(测试传 tempdir 当假 home),清单/资源路径中的 `~`
//! 一律相对注入的 home 展开,绝不触碰真实用户目录。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_adapter::codec::{self, Doc};
use duster_adapter::manifest::{self, Manifest, ManifestScope, MapperName, ResourceSection};
use duster_adapter::mapper::{mcp, skill};
use duster_adapter::native::{
    claude_session, codex_session, omp_jsonl_session, omp_session, opencode_session,
};
use duster_adapter::probe::{self, ProbeSpec};
use duster_fs::walk::{WalkOptions, walk_files, walk_stats};
use duster_index::db::Index;
use duster_index::meta;
use duster_index::sqlite_probe;
use duster_index::upsert::{self, ResourceRow};
use duster_model::{AgentInfo, CleanLevel, ResourceKind};

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
    /// 各资源类的体积合计(kind -> 字节)。
    pub kind_bytes: BTreeMap<String, u64>,
    /// **可回收**字节按清理级别拆分(`l0`/`l1`/`l2` -> 字节)。
    ///
    /// 只有 artifact 行进这里,且记的是"清掉能拿回多少"而不是"占了多少":
    /// l1/l2 两者相同,l0 只算 SQLite 空洞。单看 `kind_bytes["artifact"]`
    /// 既会把无损回收和需确认的删除混为一谈,又会把 l0 的活数据算成收益。
    pub clean_bytes: BTreeMap<String, u64>,
    /// 本轮资源体积合计(mcp 配置行计 0,见 [`ResourceRow`] 约定)。
    pub bytes: u64,
    /// 本轮真正重解析入索引的会话文件数(增量短路的不算)。
    pub sessions_indexed: usize,
    /// 采集过程中的非致命失败(单个文件损坏、格式不符等)。
    pub warnings: Vec<String>,
}

impl AgentReport {
    /// 记一行资源:总数、分类计数、体积一次记齐。
    /// `clean` 非空(仅 artifact)时同时落进分级可回收账本。
    ///
    /// `install` 是**这一行内部**属于软件本体的字节数(skill 目录里的
    /// `node_modules`/`dist`,见清单的 `install_paths`)。它并进 install
    /// 桶与总量,但**不再记一次行数**——那还是同一行资源,分两次 tally
    /// 会让 `resources` 与 `kind_counts` 双算。
    fn tally(&mut self, kind: &str, bytes: u64, clean: Option<(CleanLevel, u64)>, install: u64) {
        self.resources += 1;
        *self.kind_counts.entry(kind.to_string()).or_insert(0) += 1;
        *self.kind_bytes.entry(kind.to_string()).or_insert(0) += bytes;
        if let Some((level, reclaimable)) = clean {
            *self
                .clean_bytes
                .entry(level.as_str().to_string())
                .or_insert(0) += reclaimable;
        }
        if install > 0 {
            *self.kind_bytes.entry("install".to_string()).or_insert(0) += install;
        }
        self.bytes += bytes + install;
    }
}

/// 未被任何清单认领的目录或文件。
#[derive(Debug, Serialize)]
pub struct UnclassifiedDir {
    /// `~` 形式的展示路径,如 `~/.cursor`、`~/.codex/computer-use`。
    pub path: String,
    pub bytes: u64,
    /// 认领了外层目录的 agent id;顶层候选目录(整个 agent 都没清单)为 None。
    ///
    /// 有 agent 归属意味着「这个 agent 的清单漏声明了这一块」,
    /// CLI 据此把字节数算到该 agent 头上,而不是当成孤立垃圾。
    pub agent: Option<String>,
}

/// 一次完整扫描的汇总报告。
#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub agents: Vec<AgentReport>,
    pub unclassified: Vec<UnclassifiedDir>,
    /// agents 与 unclassified 的字节数总和。
    pub total_bytes: u64,
    pub duration_ms: u64,
    /// 解析规则指纹与库内不一致,本轮已自动转全量重解析。
    ///
    /// 首次扫描(库内还没有指纹)不算变更——资源行本来就是空的,全解析是常态。
    pub rules_changed: bool,
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

/// 六大资源类的库内 kind 字符串,顺序与 [`ResourceKind`] 一致。
const ALL_KINDS: [&str; 6] = ["mcp", "skill", "memory", "session", "artifact", "install"];

/// 原生会话解析器的纪元号。
///
/// **改了 `duster_adapter::native::*` 的解析行为就必须 +1**——清单是数据、
/// 改动会自动进指纹,但解析器是代码、哈希看不见,只能靠这个手动闸门。
/// 递增后用户下次 `duster scan` 会自动全量重解析,无需知道 `--full`。
///
/// 3 = 三个 jsonl 解析器开始顺手提取技能调用事件(`skill_event` 表,
/// 见各解析器模块文档)。旧索引的 `skill_event` 是空表,而「空表」会被
/// 误读成「从没调用过」——所以这次纪元递增还带着 `SKILL_EVIDENCE_READY`
/// 标记的落库(见 [`scan`]),双保险缺一不可。
const NATIVE_PARSER_EPOCH: u32 = 3;

/// 计算解析规则指纹。
///
/// 覆盖「同一份源文件的解析结果可能变化」的全部输入:原生解析器纪元、
/// 内置清单源文本、用户清单目录下每个 `.toml` 的文件名与内容。
/// 任何一项变化都会换指纹,scan 据此自动转全量。
fn parser_epoch(user_dir: &Path) -> Result<String> {
    let mut h = blake3::Hasher::new();
    h.update(&NATIVE_PARSER_EPOCH.to_le_bytes());
    for (name, src) in manifest::builtin_sources() {
        h.update(name.as_bytes());
        h.update(src.as_bytes());
    }
    if user_dir.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(user_dir)
            .with_context(|| format!("failed to read {}", user_dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        files.sort(); // 指纹必须与目录遍历顺序无关。
        for f in &files {
            h.update(f.file_name().unwrap_or_default().as_encoded_bytes());
            let src =
                std::fs::read(f).with_context(|| format!("failed to read {}", f.display()))?;
            h.update(&src);
        }
    }
    Ok(h.finalize().to_hex().to_string())
}

/// 执行一次扫描:probe → 采集 → upsert → 清理 stale → unclassified 统计。
pub fn scan(opts: &ScanOptions) -> Result<ScanReport> {
    let started = Instant::now();
    let home = resolve_home(opts.home.as_deref())?;
    let index_path = opts
        .index_path
        .clone()
        .unwrap_or_else(|| default_index_path(&home));

    let adapters_dir = home.join(".agent-duster").join("adapters");
    let manifests = manifest::load_all(Some(&adapters_dir))?;
    let idx = Index::open(&index_path)?;

    // 解析规则变了 → 旧 turn 是用旧规则解出来的,指纹却没变,增量会静默留下
    // 过期数据。这里自动转全量,用户不必知道 `--full` 的存在。
    let epoch = parser_epoch(&adapters_dir)?;
    let stored = meta::get(idx.conn(), meta::PARSER_EPOCH)?;
    let rules_changed = matches!(&stored, Some(s) if s != &epoch);
    let full = opts.full || rules_changed;

    let mut agents = Vec::with_capacity(manifests.len());
    for m in &manifests {
        agents.push(scan_agent(&idx, m, &home, full)?);
    }
    // 只有整轮走完才落指纹:中途失败保持旧值,下次仍会重解析。
    meta::set(idx.conn(), meta::PARSER_EPOCH, &epoch)?;

    // 技能调用证据标记:本轮有会话被(重)解析过,说明 `skill_event` 已反映
    // 语料,「查过、没有记录」从此可以放心当作证据。旧库升级后第一次 scan
    // 因为解析纪元变动必然全量重解析,这里自动点亮;没点亮之前 plan 层对
    // skill 的陈旧判定整体冻结(见 plan::last_used 的 skill 分支)。
    if agents.iter().any(|a| a.sessions_indexed > 0) {
        meta::set(idx.conn(), meta::SKILL_EVIDENCE_READY, "1")?;
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
        rules_changed,
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
        bail!("cannot determine home directory (HOME is not set)");
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

/// 扫描单个 agent：probe 判定安装 → 逐资源采集 → 逐 kind 清 stale。
///
/// 未安装的 agent 不采集（目录都不在，没有可采集的），但**同样清一遍 stale**：
/// 用户手动 rm 掉目录后旧行要能自然消失，见 [`scan_agent`] 内注释。
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
        kind_bytes: BTreeMap::new(),
        clean_bytes: BTreeMap::new(),
        bytes: 0,
        sessions_indexed: 0,
        warnings: Vec::new(),
    };
    if !outcome.installed {
        // 没装：什么都不采集，但**旧索引行必须清**。
        //
        // 用户手动 rm 掉 agent 目录、没走 `duster uninstall` 时，探针立刻
        // 失配——若在这里直接早退，索引里那批指向不存在路径的资源行会永远
        // 躺着，列表就永远脏着。空 seen 集合跑一遍
        // [`upsert::delete_stale_resources`]，把这一轮见到的（零个）之外的
        // 旧行全部清掉，turn / fts_turn 随行级清理一并消失。
        //
        // agent 行本身也删：它是纯派生状态，每次安装态扫描都会由
        // [`upsert::upsert_agent`] 重建，留着只会让 status 渲染一个
        // 「0 字节空壳」——软件都不在了还列着一行，是另一种说谎。
        // 用户重装后下一次扫描自然把行写回来，删除没有任何信息损失。
        for kind in ALL_KINDS {
            upsert::delete_stale_resources(idx.conn(), &agent_id, kind, &[])?;
        }
        duster_index::query::delete_agent(idx.conn(), &agent_id)?;
        return Ok(report);
    }

    let info = AgentInfo {
        id: agent_id.clone(),
        display_name: m.agent.display_name.clone(),
        root: outcome.root.clone().unwrap_or_else(|| home.to_path_buf()),
        version: outcome.version.clone(),
    };
    upsert::upsert_agent(idx.conn(), &info, now_ms())
        .with_context(|| format!("failed to write agent `{agent_id}`"))?;

    // 本轮各 kind 见到的 key,收尾时据此清 stale(没声明的 kind 也要清:
    // 清单撤掉某类资源后,旧行必须跟着消失)。
    let mut seen: BTreeMap<&'static str, Vec<String>> =
        ALL_KINDS.iter().map(|k| (*k, Vec::new())).collect();

    for r in &m.resources {
        let kind = kind_str(r.kind);
        let seen_kind = seen
            .get_mut(kind)
            .expect("ALL_KINDS covers every resource kind");
        if let Err(e) = scan_resource(idx, &agent_id, r, home, full, seen_kind, &mut report) {
            report.warnings.push(format!(
                "failed to collect resource {} ({kind}): {e:#}",
                r.path
            ));
        }
    }

    for kind in ALL_KINDS {
        upsert::delete_stale_resources(idx.conn(), &agent_id, kind, &seen[kind])
            .with_context(|| format!("failed to clean stale {kind} rows for `{agent_id}`"))?;
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
        MapperName::McpStandardJson
        | MapperName::McpCodexToml
        | MapperName::McpOpencodeJson
        | MapperName::McpGeminiJson => scan_mcp(idx, agent_id, r, &path, seen, report),
        MapperName::SkillFrontmatterMd => scan_skills(idx, agent_id, r, &path, seen, report),
        MapperName::MemoryMarkdown => scan_memory(idx, agent_id, r, &path, seen, report),
        MapperName::NativeClaudeSession
        | MapperName::NativeCodexSession
        | MapperName::NativeOmpJsonlSession => {
            scan_sessions(idx, agent_id, r, &path, full, seen, report)
        }
        // SQLite 型会话:一个库装所有会话,粒度是「库里的一行」而不是
        // 「一个文件」,所以走另一条采集路径。
        MapperName::NativeOpencodeSession | MapperName::NativeOmpSession => {
            scan_db_sessions(idx, agent_id, r, &path, full, seen, report)
        }
        // stats-only:任何 kind 通用,只记体积不解析(artifact 只有这一条路)。
        MapperName::StatsOnly => scan_stats_only(idx, agent_id, r, &path, seen, report),
    }
}

/// SQLite 型会话采集(opencode `opencode.db` / omp `history.db`)。
///
/// 与 [`scan_sessions`] 的区别是粒度:那边一个文件一行索引,这边一个库里
/// 有 N 个会话,每个会话一行索引,`path` 全部指向同一个库文件、靠 `key`
/// (库内会话 id)区分。
///
/// 增量:整库一个 cheap print。库没变就整体跳过——单个会话级的增量需要
/// 上游给出稳定的 per-session 版本号,而它们都没有。库变了就全量重解析,
/// 这类库都是几 MB 量级,重解析比猜便宜。
///
/// 轮次的 `byte_off` = 库内行 id、`byte_len` = 0,见
/// [`duster_adapter::native::opencode_session`] 的模块文档。
///
/// # 体积怎么记
///
/// **每个会话行 `size = 0`**,与 [`scan_mcp`] 对"住在同一个共享文件里的
/// 声明"的处理一致。库是一个文件,把它的字节按会话摊开是编造:摊平了
/// 每行都记全量会让 agent 总量翻 N 倍,按轮次数分摊则会让 `prune` 以为
/// 删掉某个会话能拿回几百 KB——而库里删一个会话根本不是文件操作,一个字节
/// 都不会立刻还回来。库文件本身的体积由清单里指向这个库的那条
/// stats-only 声明记一次,这里不重复记。这样 agent 总量永远不会超过真实
/// 文件大小,`scan` 报表与 `status` 聚合(它对 `resource.size` 求和)也对得上。
fn scan_db_sessions(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    db: &Path,
    full: bool,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    if !db.is_file() {
        return Ok(()); // 装了 agent 但还没跑过,库不存在不算异常。
    }
    let cp = duster_fs::hash::cheap_print(db)?;
    let path_str = db.display().to_string();

    // 增量前置闸:一个 cheap print 管整库。库没动过就连解析都不做——
    // 会话在库里,不重解析就拿不到 key,所以这一步必须在解析**之前**
    // 从索引里问出上一轮的 key 集合。
    //
    // 这里比的是 mtime 而不是完整 cheap print:索引的只读查询接口不暴露
    // cheap_print 列(它是 upsert 的内部短路凭据)。对"这个库变了吗"来说
    // mtime 就是全部答案——SQLite 提交必然落盘、落盘必然推进 mtime;
    // cheap print 里另外那两项(size/ino)防的是"同路径换了个文件",
    // 而那同样会改 mtime。真出现 mtime 被外部工具压回去的病态情况,
    // `--full` 是给出来的逃生口。
    let prior: Vec<duster_index::query::ResourceRecord> = duster_index::query::list_resources(
        idx.conn(),
        &duster_index::query::ResourceFilter {
            agents: vec![agent_id.to_string()],
            kinds: vec!["session".to_string()],
            clean_levels: Vec::new(),
        },
    )
    .with_context(|| format!("failed to list indexed sessions of `{agent_id}`"))?
    .into_iter()
    .filter(|rec| rec.path == path_str)
    .collect();

    if !full && !prior.is_empty() && prior.iter().all(|rec| rec.mtime_ns == cp.mtime_ns) {
        // 跳过也必须报 seen:漏报的话 delete_stale_resources 会把整库的
        // 会话连同轮次一起当过期行删掉,下一轮再重建——增量优化变成了
        // 每轮全删全建。
        for rec in &prior {
            seen.push(rec.key.clone());
            report.tally("session", rec.size, None, 0);
        }
        return Ok(());
    }

    // 适配器把「读不到」降级成 warning + 空结果,绝不 Err:一家 agent 的库
    // 打不开不该让整次 scan 失败。
    let (sessions, warnings): (Vec<(String, Vec<duster_model::TurnRecord>)>, Vec<String>) =
        match r.mapper {
            MapperName::NativeOpencodeSession => {
                let (s, w) = opencode_session::parse_all(db)?;
                (s.into_iter().map(|x| (x.id, x.turns)).collect(), w)
            }
            _ => {
                let (s, w) = omp_session::parse_all(db)?;
                (s.into_iter().map(|x| (x.id, x.turns)).collect(), w)
            }
        };
    report.warnings.extend(warnings);

    for (key, turns) in sessions {
        let row = ResourceRow {
            agent_id: agent_id.to_string(),
            kind: "session".to_string(),
            scope: scope_str(r.scope).to_string(),
            key: key.clone(),
            path: path_str.clone(),
            // 见上文「体积怎么记」。
            size: 0,
            mtime_ns: cp.mtime_ns,
            hash_content: None,
            // 同一个库里的每一行都带同一个指纹:库没变 ⇒ 每行都 changed=false。
            cheap_print: Some(cp.to_bytes()),
            clean_level: None,
            reclaimable: None,
            install_bytes: None,
            mapper: Some(r.mapper.as_str().to_string()),
        };
        let outcome = upsert::upsert_resource(idx.conn(), &row)?;
        seen.push(key);
        report.tally("session", 0, None, 0);

        if outcome.changed || full {
            upsert::replace_turns(idx.conn(), outcome.rid, &turns)?;
            report.sessions_indexed += 1;
        }
    }
    Ok(())
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
    let meta = std::fs::metadata(cfg)
        .with_context(|| format!("failed to read metadata: {}", cfg.display()))?;
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
        // opencode 的第三种方言:`.mcp` 表,条目自带 `type = local|remote`,
        // `command` 是整条 argv 数组而不是 command/args 两分。定位方式与
        // standard-json 一样走 json_pointer——它也是 JSON,只是键名与形状不同。
        (MapperName::McpOpencodeJson, Doc::Json(v)) => {
            let node = match &r.json_pointer {
                Some(ptr) => codec::json_pointer(v, ptr),
                None => Some(v),
            };
            node.map(mcp::from_opencode_json)
                .transpose()?
                .unwrap_or_default()
        }
        // Gemini 的第四种方言:键名与 standard-json 重合,传输判定另一套
        // (httpUrl 压 url、type 只认 stdio/sse/http)。定位同样走 json_pointer。
        (MapperName::McpGeminiJson, Doc::Json(v)) => {
            let node = match &r.json_pointer {
                Some(ptr) => codec::json_pointer(v, ptr),
                None => Some(v),
            };
            node.map(mcp::from_gemini_json)
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
            "mapper `{}` does not match the file's actual format: {}",
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
            clean_level: None,
            reclaimable: None,
            install_bytes: None,
            mapper: Some(r.mapper.as_str().to_string()),
        };
        upsert::upsert_resource(idx.conn(), &row)
            .with_context(|| format!("failed to upsert mcp `{}`", s.name))?;
        seen.push(s.name.clone());
        report.tally("mcp", 0, None, 0);
    }
    Ok(())
}

/// skill:一级子目录发现,每个 skill 一行。
///
/// size 记的是**用户内容**体积:清单声明的 `install_paths`(`node_modules`
/// / `dist` / `bin` / `.git`)属于软件本体,单独记进 `install_bytes`。
/// mtime 取子树内最新文件而非目录自身——skill 的「上次使用」定义如此。
/// hash 留空——目录树哈希开销大,到 `skill list` 才按需计算。
fn scan_skills(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    dir: &Path,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    let skills = skill::discover_skills(dir)?; // 目录不存在返回空,无需预判。

    // 同名冲突退化命名。key 取自 frontmatter `name`,两个目录声明同一个
    // name 就会撞上 UNIQUE(agent_id, kind, scope, key) 塌成一行——本机
    // `~/.claude/skills` 下有两处 `name: open-gstack-browser`,scan 计 77
    // 库里只有 76,被盖住的那份 drift 检测永远看不见。
    //
    // 只给冲突的那几个补 `name@目录名`:唯一的名字保持裸 name,既有索引与
    // 用户可见的 key 不变。**复原规则:按最后一个 `@` 切分**
    // (`duster_index::query::skill_groups` 据此重新分组;skill 名本身允许
    // 含 `@`,目录名不含 `/` 但可能含 `@`,所以只能从右边切一刀)。
    let mut dup: BTreeMap<&str, usize> = BTreeMap::new();
    for s in &skills {
        *dup.entry(s.name.as_str()).or_insert(0) += 1;
    }

    // prune 一次建好复用:install_paths 是整条资源的声明,不随 skill 变。
    let opts = WalkOptions {
        follow_links: false,
        prune_dirs: r.install_paths.clone(),
    };
    let declared_install = !r.install_paths.is_empty();

    for s in &skills {
        let key = if dup[s.name.as_str()] > 1 {
            format!("{}@{}", s.name, file_name_of(&s.root))
        } else {
            s.name.clone()
        };
        // 悬空软链(目标目录已删)walk_stats 会解析不了根路径而报错。这份
        // "副本"**仍然必须入库**:它是 claude 启动时当真会去加载却失败的一项,
        // status 体检已删,`skill list` 是唯一出口;静默跳过或整体失败都会让
        // 用户永远看不见它。size 0 / install_bytes None 是"量不出来"的最诚实
        // 表达——None 而非 Some(0):不是"分过桶且桶里没东西",是根本没量到。
        // 行照样 seen + tally,下一轮 scan 才不会被当 stale 清掉。
        let (size, install_bytes, mtime_ns) = match walk_stats(&s.root, &opts) {
            Ok(stats) => {
                // 没声明 install_paths 就是 None,而不是 Some(0):
                // None = 这条资源没分过桶,Some(0) = 分过桶但里面确实没东西。
                // 上层("这个 skill 有多少是装出来的")靠这个区分未知与零。
                let install_bytes = declared_install.then_some(stats.pruned_bytes);
                let size = stats.total_bytes - stats.pruned_bytes;
                (size, install_bytes, stats.max_mtime_ns)
            }
            Err(e) => {
                report.warnings.push(format!(
                    "failed to collect stats for skill `{key}` (indexed at size 0): {e:#}"
                ));
                (0, None, 0)
            }
        };
        let row = ResourceRow {
            agent_id: agent_id.to_string(),
            kind: "skill".to_string(),
            scope: scope_str(r.scope).to_string(),
            key: key.clone(),
            path: s.root.display().to_string(),
            size,
            mtime_ns,
            hash_content: None,
            cheap_print: None,
            clean_level: None,
            reclaimable: None,
            install_bytes,
            mapper: Some(r.mapper.as_str().to_string()),
        };
        upsert::upsert_resource(idx.conn(), &row)?;
        seen.push(key.clone());
        report.tally("skill", size, None, install_bytes.unwrap_or(0));
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
    let meta = std::fs::metadata(file)
        .with_context(|| format!("failed to read metadata: {}", file.display()))?;
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
        clean_level: None,
        reclaimable: None,
        install_bytes: None,
        mapper: Some(r.mapper.as_str().to_string()),
    };
    upsert::upsert_resource(idx.conn(), &row)?;
    seen.push(key);
    report.tally("memory", meta.len(), None, 0);
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
    // 三种 jsonl 方言,解析器按 mapper 分派;文件名过滤规则也随方言变。
    let parser = match r.mapper {
        MapperName::NativeCodexSession => SessionParser::Codex,
        MapperName::NativeClaudeSession => SessionParser::Claude,
        MapperName::NativeOmpJsonlSession => SessionParser::Omp,
        _ => unreachable!("scan_resource 只把 jsonl 会话 mapper 派到这里"),
    };

    // Claude:projects/<路径编码>/*.jsonl;Codex:sessions/**/rollout-*.jsonl;
    // omp:sessions/<路径编码>/<时间戳>_<uuid>.jsonl。
    // 统一递归遍历再按文件名过滤,兼容各种目录深度。
    //
    // `.jsonl.zst` 同样是会话文件:prune 会把老会话压成 `.zst` 并删掉原文件,
    // 这里漏认它,下一轮 scan 就找不到 `.jsonl`,delete_stale_resources
    // 会把归档过的历史从检索里静默抹掉。
    let mut files: Vec<PathBuf> = Vec::new();
    walk_files(dir, &WalkOptions::default(), |p, meta| {
        if !meta.is_file() {
            return; // 符号链接不当会话文件处理。
        }
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let jsonl = name.ends_with(".jsonl") || name.ends_with(".jsonl.zst");
        let hit = if parser == SessionParser::Codex {
            name.starts_with("rollout-") && jsonl
        } else {
            jsonl
        };
        if hit {
            files.push(p.to_path_buf());
        }
    })?;
    files.sort(); // 遍历是并行的,排序保证 upsert 顺序稳定。

    for f in &files {
        if let Err(e) = index_session_file(idx, agent_id, r, f, full, parser, seen, report) {
            report.warnings.push(format!(
                "failed to index session file {}: {e:#}",
                f.display()
            ));
        }
    }
    Ok(())
}

/// 三种 jsonl 会话方言,决定文件名过滤与解析器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionParser {
    Claude,
    Codex,
    Omp,
}

/// 单个会话文件:upsert 行 → changed(或 full)才重解析 + 重建 turn/FTS。
///
/// 压缩过的 `*.jsonl.zst` 先解压再解析:`byte_off`/`byte_len` 描述的一律是
/// **解压后**的逻辑内容,压缩对检索与回读完全透明。
#[allow(clippy::too_many_arguments)]
fn index_session_file(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    file: &Path,
    full: bool,
    parser: SessionParser,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    let cp = duster_fs::hash::cheap_print(file)?;
    let compressed = duster_fs::zst::is_zst(file);
    // key 取**逻辑**会话名:`x.jsonl.zst` 仍然记 `x.jsonl`。prune 压完只改
    // path 不改 key,key 要是跟着后缀走,下一轮 scan 就会把旧行当 stale 删掉
    // 再建一个新 rid——同一段历史在索引里断一次链,prune 的改 path 也白做。
    let key = if compressed {
        file_name_of(&file.with_extension(""))
    } else {
        file_name_of(file)
    };
    let row = ResourceRow {
        agent_id: agent_id.to_string(),
        kind: "session".to_string(),
        scope: scope_str(r.scope).to_string(),
        key: key.clone(),
        path: file.display().to_string(),
        size: cp.size,
        mtime_ns: cp.mtime_ns,
        hash_content: None,
        // cheap print 取的是**磁盘上那个文件**(压缩后就是 `.zst` 本身):
        // 增量判的是"源文件变没变",压缩本身就是一次变更,该重解析。
        cheap_print: Some(cp.to_bytes()),
        clean_level: None,
        reclaimable: None,
        install_bytes: None,
        mapper: Some(r.mapper.as_str().to_string()),
    };
    let outcome = upsert::upsert_resource(idx.conn(), &row)?;
    seen.push(key);
    report.tally("session", cp.size, None, 0);

    if outcome.changed || full {
        // 原生解析器只吃路径,所以压缩的先落一个临时文件再解析。
        let scratch = if compressed {
            Some(Scratch::write(&duster_fs::zst::decompress_to_vec(file)?)?)
        } else {
            None
        };
        let target = scratch.as_ref().map_or(file, Scratch::path);
        let (_meta, turns, events) = match parser {
            SessionParser::Claude => claude_session::parse(target)?,
            SessionParser::Codex => codex_session::parse(target)?,
            SessionParser::Omp => omp_jsonl_session::parse(target)?,
        };
        upsert::replace_turns(idx.conn(), outcome.rid, &turns)?;
        // 调用事件与轮次同生共死:同一个会话文件,要么两个都重建,
        // 要么都不动——解析器在同一趟里产出,落库也必须同一趟。
        upsert::replace_skill_events(idx.conn(), outcome.rid, &events)?;
        report.sessions_indexed += 1;
    }
    Ok(())
}

/// 解压后的临时会话文件,Drop 即删。
///
/// duster-core 没有 tempfile 依赖(它只是 dev-dependency),用 pid + 进程内
/// 单调序号自己保证唯一:同进程内序号不重,跨进程 pid 不重。
struct Scratch(PathBuf);

/// 临时文件序号。只要求进程内唯一,不要求跨进程有序。
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    fn write(bytes: &[u8]) -> Result<Self> {
        let n = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("duster-session-{}-{n}.jsonl", std::process::id()));
        std::fs::write(&path, bytes)
            .with_context(|| format!("failed to write scratch file: {}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// stats-only(artifact / install / 格式未证实的 memory·session):
/// 目录聚合体积一行,或单文件一行;不解析内容。
///
/// 声明了 `glob` 的目录资源**每一份命中条目一行**(自己的 size/mtime):
/// 数据库滚动备份这类「一代一份文件」的资源,一行索引代表整棵树会让
/// prune 永远只看见一个 76 MB 的巨物,看不出"第 8 份已经冗余"。
fn scan_stats_only(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    path: &Path,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    if let Some(pattern) = &r.glob
        && path.is_dir()
    {
        return scan_stats_glob(idx, agent_id, r, path, pattern, seen, report);
    }
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

    // key 取清单里声明的 `~` 形式路径,不取 basename:同一 agent 下
    // `~/.x/a/cache` 与 `~/.x/b/cache` 的 basename 相同,会撞上
    // UNIQUE(agent_id, kind, scope, key) 把两个目录塌成一行、体积互相覆盖。
    // 声明路径在单份清单内唯一由 Manifest::validate 的重叠检查保证(路径
    // 相等即互为前缀,直接拒绝加载)。
    let key = r.path.clone();
    let level = r.clean_level;
    // 可回收量 ≠ 占用量。l1/l2 是整个删掉,两者相等;l0 只回收空洞——
    // 一个 784 MB 的日志库里可能只有 774 MB 是空闲页,VACUUM 后剩下的
    // 真实数据还在。估不出来(不是 SQLite / 库被独占)就记 0,绝不拿 size 兜底:
    // 把"占了多少"当"能清多少"报给用户,是这次改造要根除的那类谎。
    let reclaimable = match level {
        None => None,
        Some(CleanLevel::L0) => Some(sqlite_probe::vacuum_reclaimable(path)?.unwrap_or(0)),
        Some(_) => Some(size),
    };
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
        clean_level: level.map(|l| l.as_str().to_string()),
        reclaimable,
        install_bytes: None,
        mapper: Some(r.mapper.as_str().to_string()),
    };
    upsert::upsert_resource(idx.conn(), &row)?;
    upsert::set_keep_generations(
        idx.conn(),
        agent_id,
        kind_str(r.kind),
        scope_str(r.scope),
        &key,
        r.keep_generations,
    )?;
    seen.push(key);
    report.tally(kind_str(r.kind), size, level.zip(reclaimable), 0);
    Ok(())
}

/// 目录 + glob 的 stats-only:每个命中条目一行,size/mtime 都是条目自己的。
///
/// key = 声明路径 + 匹配到的相对路径,沿用上一段的键纪律:相对路径挂在
/// `~` 形式声明路径之后,`~/.cc-switch/backups` 的两份备份是
/// `~/.cc-switch/backups/a.db` 与 `b.db`,互不塌缩,`UNIQUE` 依旧成立;
/// 两棵树里同名文件(不同声明路径)前缀不同,也不会撞。
///
/// `seen` 逐条记 key:某代际文件被用户手删后,下一轮 scan 找不到它,
/// `delete_stale_resources` 据此清掉索引里的旧行——代际资源的增量收敛
/// 与普通资源同一条路。
///
/// 代际声明(`keep_generations`)在清单校验层要求 glob 是**直接子项模式**
/// (不含 `/`,见 manifest.rs),所以相对路径就是裸文件名,key 的父目录
/// 恒等于声明路径——plan 靠 key 父目录分组即是按声明资源分组。
fn scan_stats_glob(
    idx: &Index,
    agent_id: &str,
    r: &ResourceSection,
    dir: &Path,
    pattern: &str,
    seen: &mut Vec<String>,
    report: &mut AgentReport,
) -> Result<()> {
    let level = r.clean_level;
    // 目录不存在视为零条目:装了 agent 但还没产生任何代际,不算异常。
    if !dir.is_dir() {
        return Ok(());
    }
    // 先走一遍收集命中条目,再逐条 upsert:walk 的回调签名是 FnMut,
    // 拿不到 `?`,把可能失败的处理挪回普通循环,错误照旧向上传播。
    let mut hits: Vec<(PathBuf, String, u64, i64)> = Vec::new();
    walk_files(dir, &WalkOptions::default(), |file, meta| {
        let rel = match file.strip_prefix(dir) {
            Ok(rel) => rel,
            Err(_) => return, // 理论不可达:walk 的路径都挂在根之下。
        };
        let rel_str = match rel.to_str() {
            Some(s) => s,
            None => return, // 非 UTF-8 文件名:展示不出 key,跳过不中断。
        };
        if !glob_matches(pattern, rel_str) {
            return;
        }
        hits.push((
            file.to_path_buf(),
            format!("{}/{}", r.path, rel_str),
            meta.len(),
            mtime_ns_of(meta),
        ));
    })
    .with_context(|| format!("failed to walk {}", dir.display()))?;

    for (file, key, size, mtime_ns) in hits {
        let reclaimable = match level {
            None => None,
            Some(CleanLevel::L0) => Some(sqlite_probe::vacuum_reclaimable(&file)?.unwrap_or(0)),
            Some(_) => Some(size),
        };
        let row = ResourceRow {
            agent_id: agent_id.to_string(),
            kind: kind_str(r.kind).to_string(),
            scope: scope_str(r.scope).to_string(),
            key: key.clone(),
            path: file.display().to_string(),
            size,
            mtime_ns,
            hash_content: None,
            cheap_print: None,
            clean_level: level.map(|l| l.as_str().to_string()),
            reclaimable,
            install_bytes: None,
            mapper: Some(r.mapper.as_str().to_string()),
        };
        upsert::upsert_resource(idx.conn(), &row)?;
        upsert::set_keep_generations(
            idx.conn(),
            agent_id,
            kind_str(r.kind),
            scope_str(r.scope),
            &key,
            r.keep_generations,
        )?;
        seen.push(key);
        report.tally(kind_str(r.kind), size, level.zip(reclaimable), 0);
    }
    Ok(())
}

/// 清单 glob 的匹配实现。词汇表只有三种元字符(见 claude-code.toml 头注释):
/// - `*`  匹配任意个**不含 `/`** 的字符(不跨目录);
/// - `?`  匹配任意一个不含 `/` 的字符;
/// - `**` 匹配任意个任意字符(跨目录,唯一的跨目录通配)。
///
/// 其余字节字面匹配。手写回溯而不是引 regex 依赖:duster-core 没有 regex,
/// 而这三种元字符的匹配器只有十几行,为一个字符串比较引入依赖不划算。
fn glob_matches(pattern: &str, rel: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        if p.starts_with(b"**") {
            // `**` 吃掉任意前缀(含空)后继续;对每个切分点尝试。
            let rest = &p[2..];
            (0..=s.len()).any(|i| go(rest, &s[i..]))
        } else if p[0] == b'*' {
            // `*` 最多吃到下一个 `/` 之前:不跨目录是它与 `**` 的分界。
            let stop = s.iter().position(|&b| b == b'/').unwrap_or(s.len());
            (0..=stop).any(|i| go(&p[1..], &s[i..]))
        } else if p[0] == b'?' {
            !s.is_empty() && s[0] != b'/' && go(&p[1..], &s[1..])
        } else {
            !s.is_empty() && s[0] == p[0] && go(&p[1..], &s[1..])
        }
    }
    go(pattern.as_bytes(), rel.as_bytes())
}

/// 未认领统计,两路来源:
/// 1. 顶层候选表里整个没被清单认领的目录(该 agent 压根没适配器);
/// 2. **已认领 agent 目录内部**未声明的子项——`~/.qoder` 被清单认领后,
///    其下没声明的 `canvas/`(用户看板内容)会从每个视图里消失,
///    该 agent 的 SIZE 就小于真实占用。这是 1 覆盖不到的那一半。
fn collect_unclassified(manifests: &[Manifest], home: &Path) -> Vec<UnclassifiedDir> {
    // 清单认领的全部路径前缀:probe 路径 + 资源路径。跨清单一起判:
    // 别家清单声明过的目录不该在这家的报告里当"未认领"。
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
                path: display_path(&dir, home),
                bytes: stats.total_bytes,
                agent: None, // 整个 agent 没清单,谈不上归属。
            });
        }
    }

    // agent 目录内部的漏网子项。以 probe 根为起点:probe 根存在即等于
    // 这个 agent 装着,也就等于这棵树的字节数该有人认领。
    for m in manifests {
        for raw in m.probe.any_of.iter().chain(&m.probe.all_of) {
            let root = expand(raw, home);
            // probe 根写成 `~` 会把整个用户主目录列成"某 agent 的漏网内容",
            // 那不是漏声明,是把 home 当 agent 目录——直接跳过。
            if root == home || !root.is_dir() {
                continue;
            }
            // 判"有没有主"只看**严格落在 root 之下**的声明。root 自身
            // (以及任何 root 的祖先)必然在 claimed 里——正是因为它被认领
            // 才要下钻;把它算成覆盖,整棵树就一条都出不来了。
            let inner: Vec<&Path> = claimed
                .iter()
                .filter(|p| p.starts_with(&root) && p.as_path() != root)
                .map(|p| p.as_path())
                .collect();
            collect_uncovered(&root, &inner, home, &m.agent.id, &mut out);
        }
    }

    // 同一路径被两条 probe 根(或两份清单)撞出来时只留第一条:
    // 报告里出现两次 = total_bytes 双算。
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|u| seen.insert(u.path.clone()));
    out
}

/// 逐层比对 `dir` 的子项:已认领的跳过,认领落在更深处的下钻,
/// 完全没被认领的整块记一条(体积按整棵子树算)。
///
/// 例:`~/.codex` 是 probe 根,`~/.codex/skills` 有声明 → `skills` 有主、
/// 不看;`~/.codex/computer-use` 没人声明 → 整块记一条。
/// 递归深度由最长的已声明路径封顶,不会失控。
fn collect_uncovered(
    dir: &Path,
    claimed: &[&Path],
    home: &Path,
    agent: &str,
    out: &mut Vec<UnclassifiedDir>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // 读不动(权限)就跳过,未认领统计不值得让扫描失败。
    };
    let mut children: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    children.sort(); // 目录读取顺序不保证稳定,报告顺序必须稳定。

    for child in children {
        // 认领落在 child 之上(等于 child,或是它的祖先):整块有主。
        if claimed.iter().any(|p| child.starts_with(p)) {
            continue;
        }
        // 认领只落在 child **之下**:这一层是混的,继续往里看。
        if claimed.iter().any(|p| p.starts_with(&child)) {
            collect_uncovered(&child, claimed, home, agent, out);
            continue;
        }
        // 符号链接按自身大小计(与 walk 的口径一致),不跟进目标,免得双算。
        let Ok(meta) = std::fs::symlink_metadata(&child) else {
            continue;
        };
        let bytes = if meta.is_dir() {
            match walk_stats(&child, &WalkOptions::default()) {
                Ok(s) => s.total_bytes,
                Err(_) => continue,
            }
        } else {
            meta.len()
        };
        out.push(UnclassifiedDir {
            path: display_path(&child, home),
            bytes,
            agent: Some(agent.to_string()),
        });
    }
}

/// 绝对路径 -> `~/...` 展示形式;不在 home 之下则原样展示。
fn display_path(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => p.display().to_string(),
    }
}

/// [`ResourceKind`] -> 库内 kind 字符串(与 serde lowercase 一致)。
fn kind_str(k: ResourceKind) -> &'static str {
    match k {
        ResourceKind::Mcp => "mcp",
        ResourceKind::Skill => "skill",
        ResourceKind::Memory => "memory",
        ResourceKind::Session => "session",
        ResourceKind::Artifact => "artifact",
        ResourceKind::Install => "install",
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

    /// 分类学的三条硬契约,一次全测:
    /// 1. `install` 永不进可回收账本——软件本体不是收益;
    /// 2. l0 记的是空洞而不是文件大小——一个 SQLite 库里的活数据不算收益;
    /// 3. scan(进程内累加)与 status(SQL 聚合)对同一份索引必须给出同一个数。
    ///
    /// 这三条是「clean 永不卸载用户软件 / 永不虚报回收量」在 clean 落地前
    /// 唯一的机器守卫。
    #[test]
    fn install_不计回收_l0_只算空洞_scan与status一致() {
        const MANIFEST: &str = r#"
[agent]
id = "taxo-agent"
display_name = "Taxonomy"

[probe]
any_of = ["~/.taxo"]

[[resource]]
kind = "install"
scope = "global"
path = "~/.taxo/extensions"
mapper = "stats-only"

[[resource]]
kind = "artifact"
scope = "global"
path = "~/.taxo/cache"
mapper = "stats-only"
clean_level = "l1"

[[resource]]
kind = "artifact"
scope = "global"
path = "~/.taxo/logs.sqlite"
mapper = "stats-only"
clean_level = "l0"
"#;
        let home = TempHome::new("taxonomy");
        let adapters = home.path().join(".agent-duster/adapters");
        fs::create_dir_all(&adapters).unwrap();
        fs::write(adapters.join("taxo-agent.toml"), MANIFEST).unwrap();

        let root = home.path().join(".taxo");
        // install:一块很大的"软件本体"。
        fs::create_dir_all(root.join("extensions")).unwrap();
        fs::write(root.join("extensions/bin"), vec![0u8; 64 * 1024]).unwrap();
        // l1:整块都能回收的缓存。
        fs::create_dir_all(root.join("cache")).unwrap();
        fs::write(root.join("cache/blob"), vec![0u8; 4096]).unwrap();
        // l0:真的 SQLite 库,借索引层建(省一个 rusqlite 直依赖)。
        let sqlite_path = root.join("logs.sqlite");
        drop(Index::open(&sqlite_path).unwrap());

        let index_path = home.path().join(".agent-duster/index.db");
        let opts = ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        };
        let report = scan(&opts).unwrap();
        let a = report
            .agents
            .iter()
            .find(|x| x.agent_id == "taxo-agent")
            .expect("报告里应有 taxo-agent");
        assert!(a.warnings.is_empty(), "warnings: {:?}", a.warnings);

        // 1. install 占了体积,但一个字节都不算可回收。
        let install_bytes = a.kind_bytes.get("install").copied().unwrap_or(0);
        assert!(install_bytes >= 64 * 1024, "install 应统计体积");
        assert!(
            !a.clean_bytes.contains_key("install"),
            "install 不得出现在分级账本里"
        );
        let clean_total: u64 = a.clean_bytes.values().sum();
        assert!(
            clean_total < install_bytes,
            "可回收量 {clean_total} 不该把 {install_bytes} 的软件本体算进去"
        );

        // 2. l1 整块回收;l0 只回收空洞,必须小于文件本身。
        assert_eq!(a.clean_bytes.get("l1").copied(), Some(4096));
        let sqlite_size = fs::metadata(&sqlite_path).unwrap().len();
        let l0 = a.clean_bytes.get("l0").copied().expect("l0 应有账");
        assert!(sqlite_size > 0);
        assert!(
            l0 < sqlite_size,
            "l0 记了 {l0},文件才 {sqlite_size}——空洞不可能等于整个库"
        );

        // 3. 两条独立的计算路径必须收敛到同一个数。
        let st = crate::status::status(Some(&index_path)).unwrap();
        let s = st
            .agents
            .iter()
            .find(|x| x.agent_id == "taxo-agent")
            .unwrap();
        assert_eq!(
            s.clean_bytes, a.clean_bytes,
            "status 的 SQL 聚合与 scan 的进程内累加对不上"
        );
        assert_eq!(s.kind_bytes.get("install").copied(), Some(install_bytes));
    }

    /// 解析规则变了 → 下次扫描自动全量重解析,用户无需知道 `--full`。
    ///
    /// 这是增量的唯一正确性缺口:清单/解析器改了,文件指纹却没变,
    /// 增量会静默留下按旧规则解析的 turn。
    #[test]
    fn 清单变更后_下次扫描自动全量重解析() {
        let home = TempHome::new("epoch");
        build_fake_home(home.path());
        let index_path = home.path().join(".agent-duster/index.db");
        let opts = ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        };

        // 首次:全解析,但不算「规则变更」——库里本来就没指纹。
        let first = scan(&opts).unwrap();
        assert!(!first.rules_changed, "首次扫描不该报规则变更");
        assert_eq!(fake_of(&first).sessions_indexed, 1);

        // 二次无改动:增量短路。
        let second = scan(&opts).unwrap();
        assert!(!second.rules_changed);
        assert_eq!(fake_of(&second).sessions_indexed, 0);

        // 改用户清单(这里加一行注释就够——源文本进指纹)。
        let adapters = home.path().join(".agent-duster/adapters");
        fs::write(
            adapters.join("fake-agent.toml"),
            format!("{FAKE_MANIFEST}\n# rules tweaked\n"),
        )
        .unwrap();

        // 三次:文件指纹没变,但规则指纹变了 → 自动全量。
        let third = scan(&opts).unwrap();
        assert!(third.rules_changed, "清单改了却没触发全量");
        assert_eq!(fake_of(&third).sessions_indexed, 1);

        // 四次:新指纹已落库,回到增量。
        let fourth = scan(&opts).unwrap();
        assert!(!fourth.rules_changed);
        assert_eq!(fake_of(&fourth).sessions_indexed, 0);
    }

    /// artifact 的体积要能从 scan 与 status 两侧都读到(CLI 用它出「能清多少」)。
    #[test]
    fn artifact_体积在_scan_与_status_两侧一致() {
        let home = TempHome::new("kindbytes");
        build_fake_home(home.path());
        let index_path = home.path().join(".agent-duster/index.db");
        let report = scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        })
        .unwrap();

        // artifacts/cache.bin 是 128 字节。
        let scanned = fake_of(&report)
            .kind_bytes
            .get("artifact")
            .copied()
            .unwrap();
        assert_eq!(scanned, 128);

        let st = crate::status::status(Some(&index_path)).unwrap();
        let a = st
            .agents
            .iter()
            .find(|a| a.agent_id == "fake-agent")
            .unwrap();
        assert_eq!(a.kind_bytes.get("artifact").copied(), Some(128));
        // mcp 行按约定体积计 0,不该混进 artifact。
        assert_eq!(a.kind_bytes.get("mcp").copied(), Some(0));
    }

    /// 取报告里的 fake-agent,测试内多处复用。
    fn fake_of(report: &ScanReport) -> &AgentReport {
        report
            .agents
            .iter()
            .find(|a| a.agent_id == "fake-agent")
            .expect("报告里应有 fake-agent")
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

    /// 未安装 agent 重扫后旧行被清：用户手动 rm 掉目录、没走 uninstall 时，
    /// 探针失配后旧资源行与 agent 行都必须自然消失，列表才不撒谎。
    ///
    /// agent 行也删：它是纯派生状态（重装后下次扫描由 upsert_agent 重建），
    /// 留着只会让 status 渲染一个「0 字节空壳」。
    #[test]
    fn 未安装_agent_重扫后旧行被清() {
        let home = TempHome::new("uninstalledscan");
        put_manifest(home.path(), "skl-agent", SKILL_MANIFEST);
        let skills = home.path().join(".skl/skills");
        put_skill(&skills, "dir-a", "solo");

        let index_path = home.path().join(".agent-duster/index.db");
        let opts = ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        };

        // 第一轮：装着，一行 skill + 一行 agent。
        let first = scan(&opts).unwrap();
        let a = first
            .agents
            .iter()
            .find(|a| a.agent_id == "skl-agent")
            .expect("报告里应有 skl-agent");
        assert!(a.installed);
        assert_eq!(a.kind_counts.get("skill"), Some(&1));
        assert_eq!(skill_keys(&index_path).len(), 1);
        let idx = Index::open_readonly(&index_path).unwrap();
        assert!(
            duster_index::query::agent_ids(idx.conn())
                .unwrap()
                .contains(&"skl-agent".to_string())
        );
        drop(idx);

        // 模拟「用户手动 rm 目录、没走 uninstall」。
        fs::remove_dir_all(home.path().join(".skl")).unwrap();

        // 第二轮：探针失配。不采集，但 stale 清理照跑——
        // 旧 skill 行与 agent 行一并清掉，不留幽灵行、不留 0 字节空壳。
        let second = scan(&opts).unwrap();
        let a2 = second
            .agents
            .iter()
            .find(|a| a.agent_id == "skl-agent")
            .expect("报告里应有 skl-agent");
        assert!(!a2.installed);
        assert_eq!(a2.resources, 0);
        assert_eq!(skill_keys(&index_path).len(), 0, "旧 skill 行必须被清掉");
        let idx = Index::open_readonly(&index_path).unwrap();
        assert!(
            !duster_index::query::agent_ids(idx.conn())
                .unwrap()
                .contains(&"skl-agent".to_string()),
            "agent 行也要清：留着就是 0 字节空壳"
        );
    }

    /// 在假 home 下写一份用户清单。
    fn put_manifest(home: &Path, id: &str, src: &str) {
        let adapters = home.join(".agent-duster/adapters");
        fs::create_dir_all(&adapters).unwrap();
        fs::write(adapters.join(format!("{id}.toml")), src).unwrap();
    }

    /// 造一个 skill 目录:`<skills>/<dir>/SKILL.md` 声明 frontmatter `name`。
    /// 返回 SKILL.md 的字节数。
    fn put_skill(skills: &Path, dir: &str, name: &str) -> u64 {
        let root = skills.join(dir);
        fs::create_dir_all(&root).unwrap();
        let body = format!("---\nname: {name}\ndescription: 演示\n---\n正文");
        fs::write(root.join("SKILL.md"), &body).unwrap();
        body.len() as u64
    }

    /// 取一条 skill 行的 (size, install_bytes)。
    fn skill_row(index_path: &Path, key: &str) -> (u64, Option<u64>) {
        let idx = Index::open_readonly(index_path).unwrap();
        idx.conn()
            .query_row(
                "SELECT size, install_bytes FROM resource WHERE kind = 'skill' AND key = ?1",
                (key,),
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Option<i64>>(1)?)),
            )
            .map(|(s, i)| (s, i.map(|v| v as u64)))
            .unwrap_or_else(|e| panic!("skill 行 `{key}` 应存在: {e}"))
    }

    /// 库里全部 skill key,升序。
    fn skill_keys(index_path: &Path) -> Vec<String> {
        let idx = Index::open_readonly(index_path).unwrap();
        let mut stmt = idx
            .conn()
            .prepare("SELECT key FROM resource WHERE kind = 'skill' ORDER BY key")
            .unwrap();

        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|k| k.unwrap())
            .collect()
    }

    const SKILL_MANIFEST: &str = r#"
[agent]
id = "skl-agent"
display_name = "Skill Agent"

[probe]
any_of = ["~/.skl"]

[[resource]]
kind = "skill"
scope = "global"
path = "~/.skl/skills"
mapper = "skill/frontmatter-md"
install_paths = ["node_modules"]
"#;

    /// 两个目录声明同一个 frontmatter `name` 时必须各占一行。
    ///
    /// 本机实测复现:`~/.claude/skills` 下有两处 `name: open-gstack-browser`,
    /// key 直接取 name 会撞 UNIQUE(agent_id, kind, scope, key) 塌成一行——
    /// scan 计 77 库里只有 76,被盖住的那份 drift 检测永远看不见。
    #[test]
    fn 同名_skill_退化为_name_at_目录名_唯一名不受影响() {
        let home = TempHome::new("skillkey");
        put_manifest(home.path(), "skl-agent", SKILL_MANIFEST);
        let skills = home.path().join(".skl/skills");
        put_skill(&skills, "dir-a", "shared");
        put_skill(&skills, "dir-b", "shared");
        put_skill(&skills, "solo-dir", "solo-skill");

        let index_path = home.path().join(".agent-duster/index.db");
        let report = scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        })
        .unwrap();
        let a = report
            .agents
            .iter()
            .find(|a| a.agent_id == "skl-agent")
            .expect("报告里应有 skl-agent");
        assert!(a.warnings.is_empty(), "warnings: {:?}", a.warnings);

        // 三份 skill = 三行,一行都不许塌。
        assert_eq!(a.kind_counts.get("skill"), Some(&3));
        assert_eq!(
            skill_keys(&index_path),
            ["shared@dir-a", "shared@dir-b", "solo-skill"],
            "冲突的补目录名后缀,唯一的保持裸 name"
        );

        // 两行指向各自的目录,没有互相覆盖 path。
        let idx = Index::open_readonly(&index_path).unwrap();
        let path_a: String = idx
            .conn()
            .query_row(
                "SELECT path FROM resource WHERE key = 'shared@dir-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(path_a.ends_with("dir-a"), "path 串了: {path_a}");
    }

    /// skill 目录内的 install 子路径:体积分桶,不混进 skill。
    ///
    /// 本机实测:`~/.claude/skills` 的 1.1 GB 里 721 MB 是 node_modules,
    /// 真·用户内容只有 36.6 MB。不分桶的话 status 口径失真、
    /// prune 的归档包会被撑爆。
    #[test]
    fn skill_内的_install_子路径_不计入_skill_体积() {
        const BLOB: u64 = 4096;
        let home = TempHome::new("skillinstall");
        put_manifest(home.path(), "skl-agent", SKILL_MANIFEST);
        let skills = home.path().join(".skl/skills");
        let md_a = put_skill(&skills, "dir-a", "heavy");
        let md_b = put_skill(&skills, "dir-b", "light");
        fs::create_dir_all(skills.join("dir-a/node_modules/pkg")).unwrap();
        fs::write(
            skills.join("dir-a/node_modules/pkg/big.bin"),
            vec![0u8; BLOB as usize],
        )
        .unwrap();

        let index_path = home.path().join(".agent-duster/index.db");
        let report = scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        })
        .unwrap();
        let a = report
            .agents
            .iter()
            .find(|a| a.agent_id == "skl-agent")
            .unwrap();
        assert!(a.warnings.is_empty(), "warnings: {:?}", a.warnings);

        // size 只剩用户内容;那 4 KB 单独记在 install_bytes。
        assert_eq!(skill_row(&index_path, "heavy"), (md_a, Some(BLOB)));
        // 声明了 install_paths 但目录里没有 → Some(0),不是 None:
        // "分过桶,里面是空的" ≠ "没分过桶"。
        assert_eq!(skill_row(&index_path, "light"), (md_b, Some(0)));

        // 报告里 4 KB 落 install 桶而不是 skill 桶,行数仍是 2(不双算)。
        assert_eq!(a.kind_bytes.get("skill").copied(), Some(md_a + md_b));
        assert_eq!(a.kind_bytes.get("install").copied(), Some(BLOB));
        assert_eq!(a.kind_counts.get("skill").copied(), Some(2));
        assert_eq!(
            a.kind_counts.get("install"),
            None,
            "内嵌 install 不是独立行"
        );
        assert_eq!(a.resources, 2);
        assert_eq!(a.bytes, md_a + md_b + BLOB, "总量一个字节都不能丢");

        // 同一棵树,清单不声明 install_paths:install_bytes 为 NULL、size 是全量。
        let bare = TempHome::new("skillinstall-bare");
        put_manifest(
            bare.path(),
            "skl-agent",
            &SKILL_MANIFEST.replace("install_paths = [\"node_modules\"]\n", ""),
        );
        let bare_skills = bare.path().join(".skl/skills");
        let md = put_skill(&bare_skills, "dir-a", "heavy");
        fs::create_dir_all(bare_skills.join("dir-a/node_modules/pkg")).unwrap();
        fs::write(
            bare_skills.join("dir-a/node_modules/pkg/big.bin"),
            vec![0u8; BLOB as usize],
        )
        .unwrap();
        let bare_index = bare.path().join(".agent-duster/index.db");
        scan(&ScanOptions {
            home: Some(bare.path().to_path_buf()),
            index_path: Some(bare_index.clone()),
            full: false,
        })
        .unwrap();
        assert_eq!(skill_row(&bare_index, "heavy"), (md + BLOB, None));
    }

    /// 悬空软链 skill(目标目录已删)必须照常入库:size 0、install_bytes None,
    /// 并记一条 warning。它是 claude 启动时当真会去加载却失败的一项,
    /// status 体检已删,`skill list` 是唯一出口——索引里没有这一行,
    /// `skill list` 再聪明也看不见它。
    #[test]
    fn 悬空软链_skill_以_0_字节入库_不中断_scan() {
        let home = TempHome::new("skilldangling");
        put_manifest(home.path(), "skl-agent", SKILL_MANIFEST);
        let skills = home.path().join(".skl/skills");
        put_skill(&skills, "dir-a", "real-skill");
        // 指向不存在目标的软链:发现阶段按目录项名产出,统计阶段量不出体积。
        std::os::unix::fs::symlink(
            home.path().join("gone-target"),
            skills.join("bark-notify"),
        )
        .unwrap();

        let index_path = home.path().join(".agent-duster/index.db");
        let report = scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        })
        .unwrap();
        let a = report
            .agents
            .iter()
            .find(|a| a.agent_id == "skl-agent")
            .expect("报告里应有 skl-agent");

        // 行必须入库:两行都算 skill,不能因为一根悬空链塌成一行或整轮失败。
        assert_eq!(a.kind_counts.get("skill"), Some(&2), "{a:#?}");
        assert_eq!(a.resources, 2);
        // 量不出来 = 0 字节入账,不是 Some(0)("分过桶但桶里没东西"是谎话)。
        assert_eq!(skill_row(&index_path, "bark-notify"), (0, None));
        assert_eq!(skill_row(&index_path, "real-skill").0 > 0, true);
        // 悬空链必须在场,路径就是软链本身(skill list 靠它判 broken)。
        let idx = Index::open_readonly(&index_path).unwrap();
        let path: String = idx
            .conn()
            .query_row(
                "SELECT path FROM resource WHERE kind = 'skill' AND key = 'bark-notify'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(path.ends_with("/bark-notify"), "path 串了: {path}");

        // 记了 warning 但行没丢;整次扫描成功,没把 agent 拖垮。
        assert!(
            a.warnings
                .iter()
                .any(|w| w.contains("bark-notify") && w.contains("size 0")),
            "warnings: {:?}",
            a.warnings
        );

        // 二次扫描:行仍被 seen,不被 delete_stale_resources 清掉。
        scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        })
        .unwrap();
        assert_eq!(
            skill_keys(&index_path),
            ["bark-notify", "real-skill"],
            "悬空行在下一轮必须还在"
        );
    }

    /// skill 的 mtime 取子树内最新文件,不是目录自身的 mtime。
    /// 「上次使用」的定义如此,stale 判定直接依赖它。
    #[test]
    fn skill_的_mtime_取子树内最新文件() {
        let home = TempHome::new("skillmtime");
        put_manifest(home.path(), "skl-agent", SKILL_MANIFEST);
        let skills = home.path().join(".skl/skills");
        put_skill(&skills, "dir-a", "solo");
        // 深层文件拨到 2033:目录自身的 mtime 停在"刚才",不会跟着走。
        let deep = skills.join("dir-a/notes");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("a.md"), b"hi").unwrap();
        let future = UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000);
        fs::File::options()
            .write(true)
            .open(deep.join("a.md"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(future))
            .unwrap();

        let index_path = home.path().join(".agent-duster/index.db");
        scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        })
        .unwrap();

        let idx = Index::open_readonly(&index_path).unwrap();
        let mtime: i64 = idx
            .conn()
            .query_row(
                "SELECT mtime_ns FROM resource WHERE kind = 'skill' AND key = 'solo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mtime, 2_000_000_000_000_000_000);
    }

    /// 认领目录内部未声明的子目录也要进 unclassified,并归到该 agent 名下。
    ///
    /// `~/.qoder` 一旦被清单认领,其下没声明的 `canvas/`(用户看板内容)
    /// 就从每个视图里消失,该 agent 的 SIZE 小于真实占用。
    #[test]
    fn 认领目录内的未声明子项进_unclassified_并归属该_agent() {
        let home = TempHome::new("uncovered");
        // ~/.codex 由内置清单认领,skills 有声明,computer-use 没有。
        let codex = home.path().join(".codex");
        fs::create_dir_all(codex.join("skills/demo")).unwrap();
        fs::write(
            codex.join("skills/demo/SKILL.md"),
            "---\nname: demo\n---\n正文",
        )
        .unwrap();
        fs::create_dir_all(codex.join("computer-use")).unwrap();
        fs::write(codex.join("computer-use/board.bin"), vec![0u8; 256]).unwrap();
        // 未声明的**文件**同样要露出来。
        fs::write(codex.join("stray.log"), vec![0u8; 32]).unwrap();

        let report = scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(home.path().join(".agent-duster/index.db")),
            full: false,
        })
        .unwrap();

        let hit = report
            .unclassified
            .iter()
            .find(|u| u.path == "~/.codex/computer-use")
            .expect("~/.codex/computer-use 应进 unclassified");
        assert_eq!(hit.bytes, 256);
        assert_eq!(hit.agent.as_deref(), Some("codex"));

        let stray = report
            .unclassified
            .iter()
            .find(|u| u.path == "~/.codex/stray.log")
            .expect("未声明的文件也要露出来");
        assert_eq!(stray.bytes, 32);

        // 已声明的 skills 不得重复出现——它已经算在 agent 的资源里了。
        assert!(
            !report
                .unclassified
                .iter()
                .any(|u| u.path.starts_with("~/.codex/skills")),
            "已认领路径不该进 unclassified: {:?}",
            report.unclassified
        );
        // 未认领的字节数进了总量,agent 的 SIZE 不再小于真实占用。
        assert!(report.total_bytes >= 256 + 32);
    }

    /// prune 压过的 `*.jsonl.zst` 必须照常被发现、解析、检索到,
    /// 且 `byte_off`/`byte_len` 描述的是**解压后**的逻辑内容。
    ///
    /// 漏掉这条:下一轮 scan 找不到 `.jsonl`,delete_stale_resources
    /// 会把归档过的历史从检索里静默抹掉。
    #[test]
    fn 压缩会话被发现并按解压后的偏移建索引() {
        const LINE: &str = r#"{"type":"user","cwd":"/tmp/demo","message":{"role":"user","content":"压缩会话里的独特口令 zstd-needle"},"timestamp":"2026-08-01T10:00:00.000Z"}"#;

        let home = TempHome::new("zstsession");
        build_fake_home(home.path()); // 顺带带来一个未压缩的 session-1.jsonl
        let proj = home
            .path()
            .join(".fake/projects/-Users-tester-projects-demo");

        // 造一个"prune 压完"的现场:只留 .zst,原文件不存在。
        let raw = proj.join("compressed-1.jsonl");
        fs::write(&raw, format!("{LINE}\n")).unwrap();
        let zst = proj.join("compressed-1.jsonl.zst");
        duster_fs::zst::compress_file(&raw, &zst, duster_fs::zst::ARCHIVE_LEVEL).unwrap();
        fs::remove_file(&raw).unwrap();

        let index_path = home.path().join(".agent-duster/index.db");
        let report = scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        })
        .unwrap();
        let a = fake_of(&report);
        assert!(a.warnings.is_empty(), "warnings: {:?}", a.warnings);
        assert_eq!(a.kind_counts.get("session"), Some(&2), "压缩的也算一行");
        assert_eq!(a.sessions_indexed, 2);

        // key 用逻辑名(去掉 .zst),path 指向真正躺在盘上的 .zst。
        let idx = Index::open_readonly(&index_path).unwrap();
        let path: String = idx
            .conn()
            .query_row(
                "SELECT path FROM resource WHERE kind = 'session' AND key = 'compressed-1.jsonl'",
                [],
                |r| r.get(0),
            )
            .expect("压缩会话的 key 应是去掉 .zst 的逻辑名");
        assert!(path.ends_with(".jsonl.zst"), "path 应指向 .zst: {path}");
        drop(idx);

        // 压缩会话可检索,且偏移是解压后的逻辑区间。
        let filter = crate::search::SearchFilter {
            agents: Vec::new(),
            limit: 10,
        };
        let hits = crate::search::search(Some(&index_path), "zstd-needle", &filter).unwrap();
        assert_eq!(hits.len(), 1, "压缩会话没进检索");
        assert!(hits[0].resource_path.ends_with(".jsonl.zst"));
        assert_eq!(hits[0].byte_off, 0);
        assert_eq!(
            hits[0].byte_len,
            LINE.len() as u64,
            "偏移必须描述解压后的逻辑内容,而不是 .zst 的物理字节"
        );

        // 同目录里的普通 .jsonl 照常工作——4 个真实轮次仍在。
        let plain = crate::search::search(Some(&index_path), "帮我看看", &filter).unwrap();
        assert!(
            plain
                .iter()
                .any(|h| h.resource_path.ends_with("session-1.jsonl")),
            "未压缩的会话被压缩支持带坏了"
        );

        // 二次扫描:cheap print 没变,两个都短路。
        let again = scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        })
        .unwrap();
        assert_eq!(fake_of(&again).sessions_indexed, 0);
        assert_eq!(fake_of(&again).kind_counts.get("session"), Some(&2));
    }

    /// omp 会话走同一个 `.zst` 透明路径:prune 压缩后,偏移仍是解压后的
    /// 逻辑区间,`open` 读回的正是压缩前的那一行。
    #[test]
    fn omp_压缩会话_偏移与回读_按解压后的内容() {
        const SESSION_LINE: &str = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-08-01T10:00:00.000Z","cwd":"/tmp/demo"}"#;
        const LINE: &str = r#"{"type":"message","id":"m1","parentId":"root","timestamp":"2026-08-01T10:00:00.200Z","message":{"role":"user","content":[{"type":"text","text":"omp 压缩会话里的独特口令 omp-zstd-needle"}]}}"#;

        let home = TempHome::new("ompzst");
        let adapters = home.path().join(".agent-duster/adapters");
        fs::create_dir_all(&adapters).unwrap();
        fs::write(
            adapters.join("omp-fake.toml"),
            r#"
[agent]
id = "omp-fake"
display_name = "Omp Fake"

[probe]
any_of = ["~/.omp-fake"]

[[resource]]
kind = "session"
scope = "global"
path = "~/.omp-fake/sessions"
mapper = "native/omp-jsonl-session"
"#,
        )
        .unwrap();

        // 造一个"prune 压完"的现场:只有 .zst,原文件不存在。
        let proj = home.path().join(".omp-fake/sessions/-Documents-Code-demo");
        fs::create_dir_all(&proj).unwrap();
        let raw = proj.join("2026-08-01T10-00-00-000Z_s1.jsonl");
        fs::write(&raw, format!("{SESSION_LINE}\n{LINE}\n")).unwrap();
        let zst = proj.join("2026-08-01T10-00-00-000Z_s1.jsonl.zst");
        duster_fs::zst::compress_file(&raw, &zst, duster_fs::zst::ARCHIVE_LEVEL).unwrap();
        fs::remove_file(&raw).unwrap();

        let index_path = home.path().join(".agent-duster/index.db");
        let report = scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        })
        .unwrap();
        let a = report
            .agents
            .iter()
            .find(|a| a.agent_id == "omp-fake")
            .expect("报告里应有 omp-fake");
        assert!(a.warnings.is_empty(), "warnings: {:?}", a.warnings);
        assert_eq!(a.kind_counts.get("session"), Some(&1));

        // 压缩会话可检索;偏移是解压后的逻辑区间(首行是 session 控制记录,
        // 轮次一定不在偏移 0)。
        let filter = crate::search::SearchFilter {
            agents: Vec::new(),
            limit: 10,
        };
        let hits = crate::search::search(Some(&index_path), "omp-zstd-needle", &filter).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].resource_path.ends_with(".jsonl.zst"));
        assert_eq!(
            hits[0].byte_off,
            (SESSION_LINE.len() + 1) as u64,
            "偏移描述解压后的逻辑内容"
        );
        assert_eq!(hits[0].byte_len, LINE.len() as u64);

        // open 回读同一行,正文与压缩前逐字节相同。
        let detail = crate::search::open_turn(Some(&index_path), hits[0].tid).unwrap();
        assert!(detail.text.contains("omp-zstd-needle"));
        assert_eq!(detail.role, "user");

        // 二次扫描:cheap print 没变,短路。
        let again = scan(&ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        })
        .unwrap();
        let a2 = again
            .agents
            .iter()
            .find(|a| a.agent_id == "omp-fake")
            .expect("报告里应有 omp-fake");
        assert_eq!(a2.sessions_indexed, 0);
        assert_eq!(a2.kind_counts.get("session"), Some(&1));
    }

    /// SQLite 型会话:一个库 N 个会话 ⇒ N 行索引,`path` 全指向同一个库文件。
    ///
    /// 库是用适配器自带的 `fixture_db` 造的——duster-core 不该复制上游 schema
    /// 的知识,那是适配层的事。
    const DB_MANIFEST: &str = r#"
[agent]
id = "dbfake"
display_name = "DB Fake"

[probe]
any_of = ["~/.dbfake"]

[[resource]]
kind = "session"
scope = "global"
path = "~/.dbfake/opencode.db"
mapper = "native/opencode-session"
"#;

    #[test]
    fn sqlite型会话_每会话一行_库未变则不重解析() {
        let home = TempHome::new("dbscan");
        let adapters = home.path().join(".agent-duster/adapters");
        fs::create_dir_all(&adapters).unwrap();
        fs::write(adapters.join("dbfake.toml"), DB_MANIFEST).unwrap();

        let root = home.path().join(".dbfake");
        fs::create_dir_all(&root).unwrap();
        let db = root.join("opencode.db");
        // 一个库两个会话:先建第一个,再往同一个库里补第二个。
        duster_adapter::native::opencode_session::fixture_db(
            &db,
            "ses_one",
            &[
                ("user", "第一个会话的提问"),
                ("assistant", "第一个会话的回答"),
            ],
        )
        .unwrap();
        append_second_session(&db, "ses_two", "第二个会话只有一句");

        let index_path = home.path().join(".agent-duster/index.db");
        let opts = ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        };
        fn pick(rep: &ScanReport) -> &AgentReport {
            rep.agents
                .iter()
                .find(|a| a.agent_id == "dbfake")
                .expect("报告里应有 dbfake")
        }

        // 首次:两个会话两行,都真解析过。
        let first = scan(&opts).unwrap();
        let a = pick(&first);
        assert!(a.warnings.is_empty(), "warnings: {:?}", a.warnings);
        assert_eq!(a.kind_counts.get("session"), Some(&2), "一个会话一行");
        assert_eq!(a.sessions_indexed, 2);
        // 库文件的字节不摊到会话行上,agent 总量不会超过真实文件大小。
        assert_eq!(a.bytes, 0);
        assert!(a.bytes <= fs::metadata(&db).unwrap().len());

        // 两行共享同一个 path,靠 key 区分;轮次带行 id 哨兵。
        let idx = Index::open_readonly(&index_path).unwrap();
        let rows = duster_index::query::list_resources(
            idx.conn(),
            &duster_index::query::ResourceFilter {
                agents: vec!["dbfake".to_string()],
                kinds: vec!["session".to_string()],
                clean_levels: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "ses_one");
        assert_eq!(rows[1].key, "ses_two");
        for r in &rows {
            assert_eq!(r.path, db.display().to_string(), "两行指向同一个库文件");
            assert_eq!(r.size, 0, "共享文件里的声明不单独记体积");
        }
        drop(idx);

        // 会话正文真的进了 FTS。
        let hits = crate::search::search(
            Some(&index_path),
            "第二个会话",
            &crate::search::SearchFilter {
                agents: Vec::new(),
                limit: 10,
            },
        )
        .unwrap();
        assert!(!hits.is_empty(), "库里的会话正文应可检索");

        // 二次扫描:库一个字节没动 ⇒ 整个资源短路,一次都不重解析。
        let second = scan(&opts).unwrap();
        let b = pick(&second);
        assert_eq!(b.sessions_indexed, 0, "库未变却重解析了: {:?}", b.warnings);
        assert_eq!(b.kind_counts.get("session"), Some(&2), "短路也要报 seen");

        // --full 强制重解析,行数不变。
        let third = scan(&ScanOptions {
            full: true,
            ..opts.clone()
        })
        .unwrap();
        assert_eq!(pick(&third).sessions_indexed, 2);

        // 库里新增一个会话:mtime 变了 ⇒ 重解析,多出一行。
        append_second_session(&db, "ses_three", "第三个会话");
        let fourth = scan(&opts).unwrap();
        let d = pick(&fourth);
        assert_eq!(d.kind_counts.get("session"), Some(&3));
        assert_eq!(d.sessions_indexed, 3, "库变了就整库重解析");
    }

    /// 往一个已存在的 opencode 夹具库里再塞一个会话(一条 message + 一段 text)。
    ///
    /// 用 CLI `sqlite3` 而不是 rusqlite:duster-core 不依赖 rusqlite,也不该
    /// 为了造测试数据破例——它连库都只能通过 duster-index 摸。
    fn append_second_session(db: &Path, sid: &str, text: &str) {
        let sql = format!(
            "INSERT INTO \"session\"(\"id\",\"title\",\"directory\") \
               VALUES ('{sid}','t','/tmp/fixture');\n\
             INSERT INTO \"message\"(\"id\",\"session_id\",\"time_created\",\"data\") \
               VALUES ('m_{sid}','{sid}',1786030999000,'{{\"role\":\"user\"}}');\n\
             INSERT INTO \"part\"(\"id\",\"message_id\",\"session_id\",\"data\") \
               VALUES ('p_{sid}','m_{sid}','{sid}',\
                       json_object('type','text','text','{text}'));\n"
        );
        let out = std::process::Command::new("sqlite3")
            .arg(db)
            .arg(&sql)
            .output()
            .expect("sqlite3 CLI is required by this test");
        assert!(
            out.status.success(),
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // 写入本身就会推进 mtime(纳秒精度),增量闸据此判定库变了。
    }

    /// stats-only + glob:每份备份一行索引(自己的体积/mtime),
    /// key = 声明路径 + 相对路径;文件消失后二次扫描清掉对应行。
    const GLOB_MANIFEST: &str = r#"
[agent]
id = "backupfake"
display_name = "Backup Fake"

[probe]
any_of = ["~/.backupfake"]

[[resource]]
kind = "artifact"
scope = "global"
path = "~/.backupfake/backups"
glob = "*.db"
mapper = "stats-only"
clean_level = "l2"
keep_generations = 2
"#;

    #[test]
    fn 代际备份_每份一行_消失后二次扫描清行() {
        let home = TempHome::new("globscan");
        let adapters = home.path().join(".agent-duster/adapters");
        fs::create_dir_all(&adapters).unwrap();
        fs::write(adapters.join("backupfake.toml"), GLOB_MANIFEST).unwrap();

        let dir = home.path().join(".backupfake/backups");
        fs::create_dir_all(&dir).unwrap();
        for i in 0..10 {
            fs::write(
                dir.join(format!("backup-{i:02}.db")),
                vec![b'x'; 1000 + i * 100],
            )
            .unwrap();
        }
        // 不匹配 glob 的文件不得混入:不同扩展名、或尾巴对不上。
        fs::write(dir.join("notes.txt"), b"n").unwrap();
        fs::write(dir.join("backup-00.db.tmp"), b"z").unwrap();

        let index_path = home.path().join(".agent-duster/index.db");
        let opts = ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index_path.clone()),
            full: false,
        };

        let report = scan(&opts).unwrap();
        let a = report
            .agents
            .iter()
            .find(|a| a.agent_id == "backupfake")
            .expect("报告里应有 backupfake");
        assert!(a.warnings.is_empty(), "warnings: {:?}", a.warnings);
        assert_eq!(a.kind_counts.get("artifact"), Some(&10), "只有命中的 10 份");
        // 每行记自己的体积,agent 总量 = 命中之和,不含 notes.txt。
        let expect: u64 = (0..10).map(|i| 1000 + i * 100).sum();
        assert_eq!(a.bytes, expect);

        let idx = Index::open_readonly(&index_path).unwrap();
        let rows = duster_index::query::list_resources(
            idx.conn(),
            &duster_index::query::ResourceFilter {
                agents: vec!["backupfake".to_string()],
                kinds: vec!["artifact".to_string()],
                clean_levels: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 10);
        let mut sizes: Vec<u64> = rows.iter().map(|r| r.size).collect();
        sizes.sort_unstable();
        assert_eq!(sizes, (0..10).map(|i| 1000 + i * 100).collect::<Vec<u64>>());
        for r in &rows {
            // key = 声明路径 + 相对路径,父目录恒等于声明路径(plan 按它分组)。
            assert_eq!(
                Path::new(&r.key).parent().unwrap().to_string_lossy(),
                "~/.backupfake/backups",
                "{}",
                r.key
            );
            assert_eq!(r.clean_level.as_deref(), Some("l2"));
            assert_eq!(
                r.keep_generations,
                Some(2),
                "keep_generations 随行落库: {}",
                r.key
            );
            assert!(
                r.path.starts_with(&dir.display().to_string()),
                "path 指向真实文件: {}",
                r.path
            );
        }
        drop(idx);

        // 手删两份再扫:对应行被 seen 记账清掉,剩下的照常。
        fs::remove_file(dir.join("backup-00.db")).unwrap();
        fs::remove_file(dir.join("backup-01.db")).unwrap();
        let report2 = scan(&opts).unwrap();
        let a2 = report2
            .agents
            .iter()
            .find(|a| a.agent_id == "backupfake")
            .expect("报告里应有 backupfake");
        assert_eq!(
            a2.kind_counts.get("artifact"),
            Some(&8),
            "消失的备份行被清掉"
        );
    }
}
