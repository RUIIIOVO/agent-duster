//! `duster mcp`：跨 agent 的 MCP 注册表视图。
//!
//! # 为什么值得做一个视图
//!
//! 同一个 server 常常在三四个 agent 里各声明一遍：Claude 在
//! `~/.claude.json` 的 `mcpServers`、Codex 在 `config.toml` 的
//! `[mcp_servers]`、opencode 在 `opencode.jsonc` 的 `.mcp`。三种方言、
//! 三个位置、一份语义。用户想知道的是「我到底装了几个 MCP」，
//! 不是「我在几个文件里写过 MCP」。
//!
//! 所以 `list` 按 **content hash** 合并：命令、参数、env 归一化之后哈希，
//! 相同即同一个 server，一行展示、右侧列出它出现在哪些 agent 里。
//! 声明得不完全一样的（多一个 `--verbose`、env 少一个键）**不会**被合并——
//! 那正是用户要看见的差异，用 `duster mcp diff` 展开。
//!
//! # ping 的边界
//!
//! duster **不做运行时**。`ping` 只做 stdio 握手：启动进程、发一条
//! `initialize`、读到响应就立刻断开，绝不发第二条消息、绝不常驻。
//! 它回答的是「这条声明还能启动吗」，不是「这个 server 好不好用」。
//! 可全局禁用——在别人的机器上凭一条配置就去 spawn 进程，
//! 得让用户有权说不。
//!
//! # 三种数据来源，各司其职
//!
//! - **索引**（`resource` 表里 `kind = 'mcp'` 的行）：谁在哪里声明了什么名字、
//!   内容哈希是多少。合并与冲突判定**只看它**，不重新解析配置文件——
//!   哈希是 scan 时由 mapper 算好的，`list` 重算一遍既慢又可能与索引打架。
//! - **清单**（`adapters/*.toml`）：那份文件是什么方言、段在哪。索引行不存方言，
//!   只能从清单反查；查不到就如实标成 [`UNKNOWN_DIALECT`]，不编。
//! - **配置文件本身**：只在需要**完整规格**时才读（`show` 的详情、`diff` 的两侧、
//!   `sync` 的源）。索引里没有存规格，这一步无可替代；读出来的规格与索引哈希
//!   对不上就说一句「索引陈旧」，而不是偷偷改口径。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_adapter::codec::{self, Doc};
use duster_adapter::guard;
use duster_adapter::manifest::{self, Manifest, ManifestScope, MapperName, ResourceSection};
use duster_adapter::mapper::mcp as dialect;
use duster_index::db::Index;
use duster_index::meta;
use duster_index::query::{self, ResourceFilter, ResourceRecord};
use duster_model::{McpServerSpec, McpTransport, ResourceKind};

/// 一条声明的出处。
#[derive(Debug, Clone, Serialize)]
pub struct Declaration {
    pub agent_id: String,
    /// 声明所在的配置文件。
    pub path: PathBuf,
    /// 该 agent 用的方言（`mcp/standard-json` / `mcp/codex-toml` /
    /// `mcp/opencode-json` / `mcp/gemini-json`）。
    pub dialect: String,
}

/// 合并后的一个 MCP server。
#[derive(Debug, Clone, Serialize)]
pub struct MergedServer {
    pub name: String,
    /// 归一化内容哈希（hex）。相同即视为同一个 server。
    pub content_hash: String,
    /// 这一份归一化后的规格。多处声明合并时取第一处（它们按定义相同）。
    pub spec: McpServerSpec,
    /// 它出现在哪些 agent 里，按 agent id 排序。
    pub declared_in: Vec<Declaration>,
}

/// `duster mcp list` 的结果。
#[derive(Debug, Clone, Serialize)]
pub struct McpList {
    pub servers: Vec<MergedServer>,
    /// 同名但内容不同的组：`name -> 各自的 content hash`。
    /// 这是最值得用户看一眼的东西——他以为三处一样，其实早就走散了。
    pub conflicts: BTreeMap<String, Vec<String>>,
    pub warnings: Vec<String>,
}

/// 方言查不到时 [`Declaration::dialect`] 的取值。
///
/// 索引行不带方言，只能靠清单反查文件路径。查不到（用户改了清单、
/// 文件被手工搬过位置）就摆明说「不知道」——随便填一个 `mcp/standard-json`
/// 会让 `sync` 的读者以为那份文件是 JSON 的，比空着危险得多。
pub const UNKNOWN_DIALECT: &str = "(unavailable)";

/// 读索引，按 content hash 合并，按 name 排序。
///
/// 纯读索引：`resource` 里 `kind = 'mcp'` 的行已经带了 `hash_content`
/// （scan 时由 mapper 算好），不重新解析配置文件。
///
/// 唯一要回原文件的是 [`MergedServer::spec`]——索引里没有存规格。
/// 读不动的那一组会被略过并记进 `warnings`，但它的名字仍然参与
/// `conflicts` 判定：哈希来自索引，与文件读不读得动无关。
pub fn list(index_path: Option<&std::path::Path>) -> Result<McpList> {
    let (rows, mut cat) = open_view(index_path)?;
    Ok(merge(&rows, &mut cat))
}

/// 单个 server 的详情：完整规格 + 每一处声明的原始形态。
pub fn show(index_path: Option<&std::path::Path>, name: &str) -> Result<MergedServer> {
    let (rows, mut cat) = open_view(index_path)?;
    let view = merge(&rows, &mut cat);

    let mut hits: Vec<MergedServer> = view
        .servers
        .into_iter()
        .filter(|s| s.name == name)
        .collect();
    if hits.len() == 1 {
        return Ok(hits.remove(0));
    }
    if hits.len() > 1 {
        // 同名多组 = 内容已经走散。返回其中任意一组都是在替用户做判断，
        // 而这恰恰是他最需要自己看一眼的情形。
        let hashes: Vec<&str> = hits.iter().map(|s| s.content_hash.as_str()).collect();
        bail!(
            "MCP server `{name}` is declared with {} different contents ({}); \
             inspect them with `duster mcp list` or `duster mcp diff {name} <a> <b>`",
            hits.len(),
            hashes.join(", ")
        );
    }

    // 一条都没合并出来：要么根本没这个名字，要么它的声明读不回来。
    let known: BTreeSet<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    if known.contains(name) {
        bail!(
            "MCP server `{name}` is in the index but none of its declarations could be re-read: {}",
            if view.warnings.is_empty() {
                "no further detail".to_string()
            } else {
                view.warnings.join("; ")
            }
        );
    }
    if known.is_empty() {
        bail!("no MCP server is indexed. Run `duster scan` first to build the index.");
    }
    bail!(
        "unknown MCP server `{name}`. Known servers: {}",
        known.into_iter().collect::<Vec<_>>().join(", ")
    );
}

/// 两个 agent 里同名 server 的差异。复用 [`crate::diff::diff_values`]，
/// **不写第二套比较逻辑**：字段路径的读法必须和 `duster diff` 一致。
pub fn diff(
    index_path: Option<&std::path::Path>,
    name: &str,
    left_agent: &str,
    right_agent: &str,
) -> Result<crate::diff::Diff> {
    let (rows, mut cat) = open_view(index_path)?;
    let left = spec_of(&rows, &mut cat, name, left_agent)?;
    let right = spec_of(&rows, &mut cat, name, right_agent)?;

    // 两侧都转成 JSON 再交给通用引擎：McpServerSpec 是归一化模型，
    // 转出来的字段路径（`command` / `args.0` / `env.API_KEY`）与 `duster diff`
    // 看任何结构化配置时的读法完全一致。
    let lv = serde_json::to_value(&left).context("failed to serialize the left-hand spec")?;
    let rv = serde_json::to_value(&right).context("failed to serialize the right-hand spec")?;
    Ok(crate::diff::diff_values(
        &lv,
        &rv,
        left_agent,
        right_agent,
        &crate::diff::DiffOptions::default(),
    ))
}

/// ping 的结论。
#[derive(Debug, Clone, Serialize)]
pub struct PingResult {
    pub name: String,
    pub agent_id: String,
    /// 握手成功。
    pub ok: bool,
    /// 往返耗时（毫秒）。失败为 None。
    pub ms: Option<u64>,
    /// server 自报的名字与版本（`initialize` 响应里的 `serverInfo`），取不到为 None。
    pub server_info: Option<String>,
    /// 失败原因（人话）。成功为 None。
    pub error: Option<String>,
}

/// ping 的开关与预算。
#[derive(Debug, Clone)]
pub struct PingOptions {
    /// 全局禁用。**默认禁用**：spawn 别人机器上的进程要用户明确点头。
    pub enabled: bool,
    /// 单个 server 的超时（毫秒）。
    pub timeout_ms: u64,
}

impl Default for PingOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            // 3 秒：一个起不来的 server 不该让整条命令卡住，
            // 而一个正常的 stdio server 握手远快于此。
            timeout_ms: 3_000,
        }
    }
}

/// 对一条 stdio 声明做一次握手即断开。
///
/// 硬要求：
/// - 只发 `initialize`，读到响应立刻杀掉子进程，**绝不发第二条**；
/// - 超时即杀，不留孤儿进程（子进程组一起收）；
/// - `transport != stdio` 的一律跳过并说明——HTTP server 的可达性
///   要发真实请求，那是运行时的活，不在本项目范围内；
/// - `opts.enabled == false` 时直接返回「已禁用」，一个进程都不 spawn。
///
/// 计时从 spawn 之前起算：用户问的是「这条声明还能不能起来」，
/// 进程启动本身就是这个问题的一部分。
pub fn ping(spec: &McpServerSpec, agent_id: &str, opts: &PingOptions) -> PingResult {
    let mut out = PingResult {
        name: spec.name.clone(),
        agent_id: agent_id.to_string(),
        ok: false,
        ms: None,
        server_info: None,
        error: None,
    };

    if !opts.enabled {
        out.error = Some(
            "pings are opt-in and currently disabled: duster spawned nothing. \
             Enable pings explicitly to let duster start this server's process."
                .to_string(),
        );
        return out;
    }
    if spec.transport != McpTransport::Stdio {
        out.error = Some(format!(
            "skipped: transport `{}` is not pingable. Reaching an HTTP/SSE endpoint means \
             issuing a real request, which is runtime behaviour and out of scope for duster; \
             only stdio declarations are handshaked.",
            transport_name(&spec.transport)
        ));
        return out;
    }
    let Some(command) = spec.command.as_deref().filter(|c| !c.is_empty()) else {
        out.error = Some("stdio declaration has no command to run".to_string());
        return out;
    };

    let started = Instant::now();
    match handshake(command, spec, opts.timeout_ms) {
        Ok(info) => {
            out.ok = true;
            out.ms = Some(started.elapsed().as_millis() as u64);
            out.server_info = info;
        }
        Err(e) => out.error = Some(format!("{e:#}")),
    }
    out
}

/// `duster mcp sync` 的输入。
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    pub index_path: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// 要分发的 server 名。
    pub name: String,
    /// 源 agent；None 时若只有一处声明就用它，多处则报错要求指名。
    pub from: Option<String>,
    /// 目标 agent 列表。
    pub to: Vec<String>,
    /// true = 只出计划不写。**默认 true**。
    pub dry_run: bool,
    pub yes: bool,
}

/// 一次分发对某个目标的结果。
#[derive(Debug, Clone, Serialize)]
pub struct SyncOutcome {
    pub agent_id: String,
    pub path: String,
    /// `create` / `update` / `skip` / `skipped` / `refused`。
    ///
    /// `skipped` 与 `skip` 的界线：`skip` 是 duster 拒绝覆盖——目标已有
    /// 同名但内容不同的手写声明（或闸门拒写），是分发这一侧的决定，
    /// 带着 `error` 说明原因；`skipped` 是**用户自己没勾**这个目标，
    /// 是交互选择的结果，`error` 恒为 `None`——它不是失败，退出码
    /// 不许为它升级。
    pub action: String,
    /// 被拒绝的原因（schema_guard 触发、格式不支持……）。`skipped` 恒为
    /// `None`：用户没勾不是错误，不该被当成失败渲染或计入退出码。
    pub error: Option<String>,
    /// 改写前的整文件快照路径。
    pub snapshot: Option<String>,
}

/// 一次分发的完整计划：源声明的内容指纹 + 逐目标的结果。
///
/// 指纹是这一份计划区别于「同名同目标的另一份计划」的证据——用户过目
/// 的是屏幕上渲染出来的 payload，`(agent_id, path, action)` 三个坐标
/// 证明不了「内容没变过」，指纹才能。执行前先比它（见 [`apply_sync`]）：
/// 源声明在两次调用之间被改过（命令换了、env 换了 token），三元组
/// 一模一样也照样拒绝，让用户对新的内容重新过目一次。
#[derive(Debug, Clone, Serialize)]
pub struct SyncPlan {
    /// 源声明的内容指纹（语义哈希的 hex，与 `mcp show` 的 content 行同源）。
    pub src_hash: String,
    /// 逐目标的结果，顺序与 `opts.to` 去重后一致。
    pub targets: Vec<SyncOutcome>,
}

/// 先把一次分发算成**计划**，一个字节都不写。
///
/// 这是「确认链路」的第一半：调用方把返回值摆到屏幕上让用户逐项过目，
/// 用户点头后把同一份结果原样交给 [`apply_sync`] 去执行。
///
/// 计划按定义就是只出计划，所以**无视** `opts.dry_run` 与 `opts.yes`：
/// 不要求调用方先把两个旗标摆对，也不因为 `!dry_run && !yes` 而 bail。
/// 索引恒以只读打开——预览不该抢单实例写锁，也不该被正在跑的扫描挡住。
///
/// 返回的 [`SyncPlan`] 除逐目标结果外还带着**源声明的内容指纹**（与
/// `mcp show` 的 content 行同一个哈希）：用户过目的是屏幕上渲染出来的
/// payload，三元组证明不了「内容没变过」，指纹才是。
///
/// 每个目标返回一条 [`SyncOutcome`]，`action` 只会是 `create` / `skip` /
/// `refused` 三种（词汇表见 `sync_one` 的文档）。
pub fn plan_sync(opts: &SyncOptions) -> Result<SyncPlan> {
    let (idx, cat, home, src, src_agent) = prepare_sync(opts, true)?;
    let targets = dedup_targets(&opts.to);
    // dry-run 从不落快照，op-id 只是占位；生成它的成本可忽略。
    let op_id = duster_fs::snapshot::new_op_id(std::time::SystemTime::now());
    let targets = run_plan(
        SyncCtx {
            idx: &idx,
            cat: &cat,
            home: &home,
            op_id: &op_id,
            src: &src,
            src_agent: &src_agent,
            dry_run: true,
        },
        &targets,
    );
    Ok(SyncPlan {
        // 指纹取源规格的语义哈希，与 list/show 展示的 content hash 同源：
        // 用户看过的 `content  c42fc525…` 那一行，就是执行前要核对的这个值。
        src_hash: src.content_hash().to_hex().to_string(),
        targets,
    })
}

/// 执行用户逐项过目并点头的那份计划。
///
/// # 漂移校验：为什么落盘前要重算一遍
///
/// 用户同意的是**屏幕上那一份**计划，不是执行那一刻重新算出来的另一份。
/// 从点头到落盘之间隔着一整条交互链路，两侧都可能已经被改过——目标
/// 配置文件（另一个进程、另一次编辑，甚至另一个 duster），**以及源声明**。
/// 所以先把计划按执行时的现实重算一遍，再逐层核对，顺序固定：
///
/// 1. **源指纹**（[`SyncPlan::src_hash`]）：源 agent 的那条声明在用户看完
///    计划之后被改过（命令换了、env 换了 token），`(agent_id, path, action)`
///    三元组看不出任何变化——落点没漂，payload 漂了。指纹不等直接拒绝：
///    用户没看过新内容，不能替他点头。这是唯一能证明「屏幕上那份」还是
///    不是当前这份的证据。
/// 2. **逐目标三元组**（[`ensure_current`]）：按 `(agent_id, path, action)`
///    **按顺序逐项**比对，长度不等或任一项不等就什么都不写。
///
/// **任何校验都在任何写盘之前**——宁可多问一次，不可少拦一次。
///
/// `approved` 必须是 [`plan_sync`] 原样返回的那份 [`SyncPlan`]。`allow` 只
/// 缩窄**执行**范围：`None` = 全部目标照 `approved` 真写盘；`Some(ids)` =
/// 只有 `agent_id` 在 `ids` 里的目标真写盘，未入选的目标在返回值里保留
/// 一条 `action = "skipped"` 的占位（`error` 恒为 `None`——用户没勾不是
/// 失败）。漂移校验不看 `allow`——过目的是整份计划，不是其中几行。
///
/// `opts.dry_run` 对这个函数没有意义（它按定义就是执行），误传直接拒绝；
/// `!opts.yes` 同样直接拒绝：这是往别人的配置文件里写字，库层不该替
/// 调用方假设用户同意过。一次 `apply_sync` 全程共用一个 op-id：还原时
/// 用户要回到的是「这一次分发之前」。
pub fn apply_sync(
    opts: &SyncOptions,
    approved: &SyncPlan,
    allow: Option<&[String]>,
) -> Result<Vec<SyncOutcome>> {
    if opts.dry_run {
        bail!(
            "dry_run has no meaning when executing an approved plan: \
             call plan_sync to preview instead"
        );
    }
    if !opts.yes {
        bail!(
            "refusing to rewrite agent configuration without confirmation: \
             preview it with a dry run first, then re-run with --yes"
        );
    }

    let (idx, cat, home, src, src_agent) = prepare_sync(opts, false)?;
    // 先比源声明指纹，再比逐目标三元组。顺序不能换：用户点头时屏幕上
    // 渲染的是这份 payload，源内容变了，任何三元组相等都不作数。
    let src_hash = src.content_hash().to_hex().to_string();
    if approved.src_hash != src_hash {
        bail!(
            "the source declaration `{}` in agent `{}` changed after you reviewed \
             the plan (content hash {} -> {}); re-run the plan and review it again",
            opts.name,
            src_agent,
            approved.src_hash,
            src_hash
        );
    }
    // 一次 apply_sync 共用一个 op-id：还原时用户要回到的是「这一次分发之前」。
    let op_id = duster_fs::snapshot::new_op_id(std::time::SystemTime::now());
    let targets = dedup_targets(&opts.to);

    // 再重算一份计划（只算不写），和用户过目的那份逐项比对。
    let recomputed = run_plan(
        SyncCtx {
            idx: &idx,
            cat: &cat,
            home: &home,
            op_id: &op_id,
            src: &src,
            src_agent: &src_agent,
            dry_run: true,
        },
        &targets,
    );
    ensure_current(&recomputed, &approved.targets)?;

    // 漂移校验通过，才轮到真写盘；`allow` 只在这一步起作用。
    let picked: Option<BTreeSet<&str>> = allow.map(|ids| ids.iter().map(|s| s.as_str()).collect());
    let mut ctx = SyncCtx {
        idx: &idx,
        cat: &cat,
        home: &home,
        op_id: &op_id,
        src: &src,
        src_agent: &src_agent,
        dry_run: false,
    };
    let mut out = Vec::with_capacity(targets.len());
    for (agent, plan_outcome) in targets.iter().zip(&recomputed) {
        let chosen = match &picked {
            None => true,
            Some(ids) => ids.contains(agent),
        };
        if !chosen {
            // 占位不是错误：用户没勾的目标不算失败，`error` 必须留空，
            // 否则跨过模块边界会被当成「这次分发没做完」而升级退出码。
            out.push(SyncOutcome {
                agent_id: (*agent).to_string(),
                path: plan_outcome.path.clone(),
                action: "skipped".to_string(),
                error: None,
                snapshot: None,
            });
            continue;
        }
        out.push(sync_one(&mut ctx, agent));
    }
    Ok(out)
}

/// 出计划与执行共用的准备前缀：校验入参、开索引、载入清单、定位源声明。
///
/// [`plan_sync`] 与 [`apply_sync`] 共享同一套推导——源是谁、目标是什么方言，
/// 全靠这几步决定。两者唯一的分歧在索引的打开方式：出计划只读（预览不该
/// 抢单实例写锁，也不该被正在跑的扫描挡住），执行要读写（guard 基准与
/// 快照元数据都要落库）。`readonly` 参数只影响这一步，其余推导一字不差。
fn prepare_sync(
    opts: &SyncOptions,
    readonly: bool,
) -> Result<(Index, Catalog, PathBuf, McpServerSpec, String)> {
    if opts.name.trim().is_empty() {
        bail!("no MCP server name given");
    }
    if opts.to.is_empty() {
        bail!("no target agent given: pass at least one target to sync into");
    }

    let home = resolve_home(opts.home.as_deref())?;
    let index_path = opts
        .index_path
        .clone()
        .unwrap_or_else(|| home.join(".agent-duster").join("index.db"));
    if !index_path.is_file() {
        bail!(
            "index database not found: {}. Run `duster scan` first to build it.",
            index_path.display()
        );
    }

    let idx = if readonly {
        Index::open_readonly(&index_path)
            .with_context(|| format!("failed to open index read-only: {}", index_path.display()))?
    } else {
        Index::open(&index_path)
            .with_context(|| format!("failed to open index: {}", index_path.display()))?
    };
    let rows = mcp_rows(&idx)?;
    // 写路径上清单读不动就是硬错误：目标文件是什么方言全靠它说了算，
    // 悄悄退回内置清单等于换了一套写入规则。
    let cat = Catalog::load_strict(&home)?;

    let decls: Vec<&ResourceRecord> = rows.iter().filter(|r| r.key == opts.name).collect();
    if decls.is_empty() {
        bail!(
            "no agent declares MCP server `{}` in the index. Run `duster scan` if it was added recently.",
            opts.name
        );
    }
    let src_row = match &opts.from {
        Some(a) => match decls.iter().find(|r| &r.agent_id == a) {
            Some(r) => *r,
            None => bail!(
                "agent `{a}` does not declare MCP server `{}`; it is declared by: {}",
                opts.name,
                agent_list(&decls)
            ),
        },
        None if decls.len() == 1 => decls[0],
        None => bail!(
            "MCP server `{}` is declared by {} agents ({}); name the source explicitly",
            opts.name,
            decls.len(),
            agent_list(&decls)
        ),
    };

    let mut cat = cat;
    let src_spec = spec_of_row(src_row, &mut cat)?;
    if is_broken(&src_spec) {
        bail!(
            "the declaration of `{}` in agent `{}` failed to parse at scan time; \
             fix it before syncing it anywhere",
            opts.name,
            src_row.agent_id
        );
    }
    Ok((idx, cat, home, src_spec, src_row.agent_id.clone()))
}

/// 漂移校验：重算出来的计划与用户过目的那份逐项比对，对不上就拒绝执行。
///
/// 比对的是 `(agent_id, path, action)` 三元组，**按顺序**——位置都变了的
/// 计划当然也不作数。任何一处不一致都要点名是哪个 agent、哪里变了，
/// 并明确要求重跑一遍：执行那一刻的现实已经不是用户点头时的那一份了。
///
/// 这层只挡「落点」漂移；「同一落点、payload 换了」的源声明漂移由
/// [`apply_sync`] 里的源指纹核对先拦下——三层都过了才轮到这里的逐项比对。
fn ensure_current(recomputed: &[SyncOutcome], approved: &[SyncOutcome]) -> Result<()> {
    if recomputed.len() != approved.len() {
        bail!(
            "the plan you approved is no longer current: it had {} target(s), \
             the re-run has {}; re-run the plan and approve it again",
            approved.len(),
            recomputed.len()
        );
    }
    for (i, (got, want)) in recomputed.iter().zip(approved).enumerate() {
        if got.agent_id != want.agent_id || got.path != want.path || got.action != want.action {
            bail!(
                "the plan you approved is no longer current: target #{} (agent `{}`) was \
                 `{}` at `{}` when approved, but is now `{}` at `{}`; re-run the plan and \
                 approve it again",
                i + 1,
                got.agent_id,
                want.action,
                want.path,
                got.action,
                got.path
            );
        }
    }
    Ok(())
}

/// 去重后的目标列表，保持 `opts.to` 的原始顺序。
///
/// 同一个目标写两遍毫无意义，第二遍还会拿到自己刚写完的文件。
fn dedup_targets(to: &[String]) -> Vec<&str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::with_capacity(to.len());
    for agent in to {
        if seen.insert(agent.as_str()) {
            out.push(agent.as_str());
        }
    }
    out
}

/// 对每个目标跑一遍 [`sync_one`]，返回按目标顺序排列的结果。
///
/// `dry_run` 决定这一步写不写盘：出计划（[`plan_sync`]、[`apply_sync`] 的
/// 漂移校验那一遍）传 `true`，真正落盘才传 `false`。目标必须先去重
/// （见 [`dedup_targets`]），同一个 agent 只允许出现一次。
fn run_plan(mut ctx: SyncCtx<'_>, targets: &[&str]) -> Vec<SyncOutcome> {
    let mut out = Vec::with_capacity(targets.len());
    for agent in targets {
        out.push(sync_one(&mut ctx, agent));
    }
    out
}

// ---------------------------------------------------------------------------
// 索引视图
// ---------------------------------------------------------------------------

/// 打开只读索引，取出全部 mcp 行，并按索引所在的 home 载入清单。
fn open_view(index_path: Option<&Path>) -> Result<(Vec<ResourceRecord>, Catalog)> {
    let path = match index_path {
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
    let rows = mcp_rows(&idx)?;
    Ok((rows, Catalog::load_lenient(&home_of_index(&path))))
}

fn mcp_rows(idx: &Index) -> Result<Vec<ResourceRecord>> {
    query::list_resources(
        idx.conn(),
        &ResourceFilter {
            kinds: vec!["mcp".to_string()],
            ..Default::default()
        },
    )
}

/// 从索引路径反推 home。
///
/// 索引按约定住在 `<home>/.agent-duster/index.db`，所以「指定一个索引」同时
/// 也就指定了「用谁的清单目录」。不这么做的话，`--index` 指向别处（测试、
/// 另一台机器拷回来的库）时会去读**本机真实 home** 的用户清单，
/// 反查出来的方言张冠李戴。形状对不上就退回真实 home。
fn home_of_index(index_path: &Path) -> PathBuf {
    if let Some(dir) = index_path.parent()
        && dir.file_name() == Some(OsStr::new(".agent-duster"))
        && let Some(home) = dir.parent()
    {
        return home.to_path_buf();
    }
    duster_fs::path::expand_tilde("~")
}

fn resolve_home(injected: Option<&Path>) -> Result<PathBuf> {
    if let Some(h) = injected {
        return Ok(h.to_path_buf());
    }
    let h = duster_fs::path::expand_tilde("~");
    if h == Path::new("~") {
        bail!("cannot determine the home directory");
    }
    Ok(h)
}

/// 清单侧的一处 MCP 声明位置：某个 agent 的某份配置文件。
struct McpSite {
    agent_id: String,
    /// 已按 home 展开的绝对路径。
    path: PathBuf,
    section: ResourceSection,
}

/// 清单反查表 + 配置文件解析缓存。
struct Catalog {
    sites: Vec<McpSite>,
    warnings: Vec<String>,
    /// 配置文件 → 里面的全部 server（或读取失败的原因）。同一份文件只解析一次。
    cache: BTreeMap<PathBuf, std::result::Result<Vec<McpServerSpec>, String>>,
}

impl Catalog {
    fn build(manifests: &[Manifest], home: &Path, warnings: Vec<String>) -> Self {
        let mut sites = Vec::new();
        for m in manifests {
            for r in &m.resources {
                if r.kind == ResourceKind::Mcp {
                    sites.push(McpSite {
                        agent_id: m.agent.id.clone(),
                        path: expand(&r.path, home),
                        section: r.clone(),
                    });
                }
            }
        }
        Self {
            sites,
            warnings,
            cache: BTreeMap::new(),
        }
    }

    /// 只读视图用：清单读不动就退回内置清单并记一条 warning。
    /// 一份坏掉的用户清单不该让 `duster mcp list` 整个瞎掉。
    fn load_lenient(home: &Path) -> Self {
        let dir = adapters_dir(home);
        match manifest::load_all(Some(&dir)) {
            Ok(m) => Self::build(&m, home, Vec::new()),
            Err(e) => {
                let w = format!(
                    "failed to load adapter manifests from {}: {e:#}; \
                     falling back to the built-in manifests, dialects may be missing",
                    dir.display()
                );
                Self::build(&manifest::load_builtin(), home, vec![w])
            }
        }
    }

    /// 写路径用：清单读不动直接失败。
    fn load_strict(home: &Path) -> Result<Self> {
        let dir = adapters_dir(home);
        let manifests = manifest::load_all(Some(&dir))
            .with_context(|| format!("failed to load adapter manifests from {}", dir.display()))?;
        Ok(Self::build(&manifests, home, Vec::new()))
    }

    /// 精确匹配「哪个 agent 的哪份文件」。索引行存的就是展开后的绝对路径。
    fn site_at(&self, agent: &str, path: &Path) -> Option<usize> {
        self.sites
            .iter()
            .position(|s| s.agent_id == agent && s.path == path)
    }

    /// 某个 agent 用来放 MCP 的文件。多处声明时优先 global——
    /// project 级的那份属于某个具体项目，不是「这个 agent 的全局注册表」。
    fn site_of(&self, agent: &str) -> Option<usize> {
        self.sites
            .iter()
            .position(|s| s.agent_id == agent && s.section.scope == ManifestScope::Global)
            .or_else(|| self.sites.iter().position(|s| s.agent_id == agent))
    }

    /// 解析（并缓存）一处声明位置上的全部 server。
    fn servers_at(&mut self, idx: usize) -> std::result::Result<&[McpServerSpec], String> {
        let path = self.sites[idx].path.clone();
        if !self.cache.contains_key(&path) {
            let parsed = read_site(&self.sites[idx]).map_err(|e| {
                format!(
                    "cannot re-read MCP declarations from {}: {e:#}",
                    path.display()
                )
            });
            if let Err(msg) = &parsed {
                self.warnings.push(msg.clone());
            }
            self.cache.insert(path.clone(), parsed);
        }
        match &self.cache[&path] {
            Ok(v) => Ok(v.as_slice()),
            Err(e) => Err(e.clone()),
        }
    }
}

fn adapters_dir(home: &Path) -> PathBuf {
    home.join(".agent-duster").join("adapters")
}

/// 把清单里的 `~` 路径按指定 home 展开；其余形式原样返回。
fn expand(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

/// 读一处声明位置的配置文件并归一化。读法与 `scan` 完全一致，
/// 否则同一份文件在 scan 与 list 眼里会是两个东西。
fn read_site(site: &McpSite) -> Result<Vec<McpServerSpec>> {
    let doc = codec::read_file(&site.path)?;
    servers_in_doc(&doc, &site.section)
}

/// 从已解析的文档里按清单定位取出全部 server。
///
/// 定位落空（文件里还没有 `mcpServers` 段）视为零个 server，不是错误——
/// 那是一份还没写过 MCP 的正常配置。
fn servers_in_doc(doc: &Doc, section: &ResourceSection) -> Result<Vec<McpServerSpec>> {
    match (section.mapper, doc) {
        (MapperName::McpStandardJson, Doc::Json(v)) => {
            let node = match &section.json_pointer {
                Some(p) => codec::json_pointer(v, p),
                None => Some(v),
            };
            Ok(node
                .map(dialect::from_standard_json)
                .transpose()?
                .unwrap_or_default())
        }
        (MapperName::McpOpencodeJson, Doc::Json(v)) => {
            let node = match &section.json_pointer {
                Some(p) => codec::json_pointer(v, p),
                None => Some(v),
            };
            Ok(node
                .map(dialect::from_opencode_json)
                .transpose()?
                .unwrap_or_default())
        }
        (MapperName::McpGeminiJson, Doc::Json(v)) => {
            let node = match &section.json_pointer {
                Some(p) => codec::json_pointer(v, p),
                None => Some(v),
            };
            Ok(node
                .map(dialect::from_gemini_json)
                .transpose()?
                .unwrap_or_default())
        }
        (MapperName::McpCodexToml, Doc::Toml(v)) => {
            let node = match &section.toml_key {
                Some(k) => codec::toml_path(v, k),
                None => Some(v),
            };
            Ok(node
                .map(dialect::from_codex_toml)
                .transpose()?
                .unwrap_or_default())
        }
        (m, _) => bail!(
            "mapper `{}` cannot read this file: the manifest and the file format disagree",
            m.as_str()
        ),
    }
}

/// 一条索引行加上「能补上的一切」。
struct Decl<'a> {
    row: &'a ResourceRecord,
    /// 内容哈希（hex）。索引里有就用索引的，缺失才退回重读出来的规格自己算。
    hash: Option<String>,
    spec: Option<McpServerSpec>,
    dialect: Option<String>,
}

fn resolve<'a>(row: &'a ResourceRecord, cat: &mut Catalog) -> Decl<'a> {
    let site = cat.site_at(&row.agent_id, Path::new(&row.path));
    let dialect = site.map(|i| cat.sites[i].section.mapper.as_str().to_string());
    if site.is_none() {
        cat.warnings.push(format!(
            "no adapter manifest declares {} as an MCP file for agent `{}`; \
             the dialect and full spec of `{}` are unavailable",
            row.path, row.agent_id, row.key
        ));
    }
    let spec = site.and_then(|i| {
        cat.servers_at(i)
            .ok()
            .and_then(|v| v.iter().find(|s| s.name == row.key).cloned())
    });

    let from_file = spec.as_ref().map(|s| *s.content_hash().as_bytes());
    // 索引与文件对不上 = 索引陈旧。合并仍按索引口径走（那是 list 的定义），
    // 但必须说出来，否则用户读到的是一份过期的「现状」。
    if let (Some(a), Some(b)) = (row.hash_content, from_file)
        && a != b
    {
        cat.warnings.push(format!(
            "the indexed content hash of `{}` in agent `{}` no longer matches {}; \
             run `duster scan` to refresh the index",
            row.key, row.agent_id, row.path
        ));
    }
    let hash = row
        .hash_content
        .or(from_file)
        .map(|h| blake3::Hash::from_bytes(h).to_hex().to_string());

    Decl {
        row,
        hash,
        spec,
        dialect,
    }
}

fn merge(rows: &[ResourceRecord], cat: &mut Catalog) -> McpList {
    let decls: Vec<Decl<'_>> = rows.iter().map(|r| resolve(r, cat)).collect();

    // 冲突判定只看哈希，与「文件还读不读得动」无关：一处声明读不回来，
    // 不该让「这三处早就走散了」这条最值钱的结论跟着消失。
    let mut by_name: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for d in &decls {
        if let Some(h) = &d.hash {
            by_name
                .entry(d.row.key.as_str())
                .or_default()
                .insert(h.as_str());
        }
    }
    let conflicts: BTreeMap<String, Vec<String>> = by_name
        .iter()
        .filter(|(_, hs)| hs.len() > 1)
        .map(|(n, hs)| {
            (
                (*n).to_string(),
                hs.iter().map(|h| (*h).to_string()).collect(),
            )
        })
        .collect();

    // BTreeMap 的键序即 (name, hash) 字典序，servers 因此天然按 name 排好。
    let mut groups: BTreeMap<(&str, &str), Vec<&Decl<'_>>> = BTreeMap::new();
    for d in &decls {
        let Some(h) = d.hash.as_deref() else {
            // 既没有索引哈希也读不回规格：连「它是不是那一个」都判断不了。
            cat.warnings.push(format!(
                "MCP server `{}` in agent `{}` has no content hash and its declaration \
                 could not be re-read; omitted from the merged view",
                d.row.key, d.row.agent_id
            ));
            continue;
        };
        groups.entry((d.row.key.as_str(), h)).or_default().push(d);
    }

    let mut servers = Vec::with_capacity(groups.len());
    for ((name, hash), ds) in groups {
        let Some(spec) = ds.iter().find_map(|d| d.spec.clone()) else {
            cat.warnings.push(format!(
                "MCP server `{name}` ({hash}) is indexed for {} but none of its declarations \
                 could be re-read; omitted from the merged view",
                ds.iter()
                    .map(|d| d.row.agent_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            continue;
        };
        let mut declared_in: Vec<Declaration> = ds
            .iter()
            .map(|d| Declaration {
                agent_id: d.row.agent_id.clone(),
                path: PathBuf::from(&d.row.path),
                dialect: d
                    .dialect
                    .clone()
                    .unwrap_or_else(|| UNKNOWN_DIALECT.to_string()),
            })
            .collect();
        declared_in.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        servers.push(MergedServer {
            name: name.to_string(),
            content_hash: hash.to_string(),
            spec,
            declared_in,
        });
    }

    McpList {
        servers,
        conflicts,
        warnings: std::mem::take(&mut cat.warnings),
    }
}

/// 取某个 agent 声明的某个 server 的完整规格。
fn spec_of(
    rows: &[ResourceRecord],
    cat: &mut Catalog,
    name: &str,
    agent: &str,
) -> Result<McpServerSpec> {
    match rows.iter().find(|r| r.key == name && r.agent_id == agent) {
        Some(row) => spec_of_row(row, cat),
        None => {
            let others: Vec<&str> = rows
                .iter()
                .filter(|r| r.key == name)
                .map(|r| r.agent_id.as_str())
                .collect();
            if others.is_empty() {
                bail!("no agent declares MCP server `{name}`");
            }
            bail!(
                "agent `{agent}` does not declare MCP server `{name}`; it is declared by: {}",
                others.join(", ")
            )
        }
    }
}

fn spec_of_row(row: &ResourceRecord, cat: &mut Catalog) -> Result<McpServerSpec> {
    let Some(i) = cat.site_at(&row.agent_id, Path::new(&row.path)) else {
        bail!(
            "no adapter manifest declares {} as an MCP file for agent `{}`; \
             duster cannot tell which dialect it is written in",
            row.path,
            row.agent_id
        );
    };
    let path = cat.sites[i].path.clone();
    let servers = cat.servers_at(i).map_err(anyhow::Error::msg)?;
    match servers.iter().find(|s| s.name == row.key) {
        Some(s) => Ok(s.clone()),
        None => bail!(
            "agent `{}` no longer declares MCP server `{}` in {}; \
             the index is stale, run `duster scan`",
            row.agent_id,
            row.key,
            path.display()
        ),
    }
}

/// 占位条目（mapper 解析失败时的降级产物）不是一份规格，不许参与分发。
fn is_broken(spec: &McpServerSpec) -> bool {
    spec.extra
        .as_deref()
        .and_then(|e| serde_json::from_str::<serde_json::Value>(e).ok())
        .is_some_and(|v| v.get("_parse_error").is_some())
}

fn agent_list(rows: &[&ResourceRecord]) -> String {
    rows.iter()
        .map(|r| r.agent_id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// ping
// ---------------------------------------------------------------------------

/// 我们发出的那一条 `initialize` 的 id。响应必须带着同一个 id 回来，
/// 否则那是 server 自己推的日志或通知，不是给我们的答复。
const REQUEST_ID: u64 = 1;

/// 声明使用的 MCP 协议版本。server 不认识这个版本也必须回一条
/// `initialize` 响应（里面报它自己支持的版本），所以握手照样成立。
const PROTOCOL_VERSION: &str = "2025-06-18";

fn transport_name(t: &McpTransport) -> &'static str {
    match t {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
        McpTransport::Sse => "sse",
    }
}

/// 起进程、发一条 `initialize`、读一条响应、杀干净。返回 `serverInfo` 摘要。
///
/// MCP 的 stdio 帧格式是**行分隔 JSON**：一条消息一行。所以这里手写
/// 两行代码就够了，不需要为它引一个依赖。
fn handshake(command: &str, spec: &McpServerSpec, timeout_ms: u64) -> Result<Option<String>> {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": REQUEST_ID,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "duster", "version": env!("CARGO_PKG_VERSION") },
        }
    });

    let mut cmd = Command::new(command);
    cmd.args(&spec.args)
        .envs(&spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // stderr 直接丢：server 的启动日志不是我们要回答的问题，
        // 而接一根没人读的管子会在它写满时把对方堵死。
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        // 自成进程组。MCP server 常常是 `sh -c "..."` 或 npx 包一层，
        // 只杀直接子进程会把真正干活的孙子留成孤儿。
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{command}`"))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stdin = child.stdin.take().expect("stdin is piped");

    // 读放在另一个线程里：管子是阻塞的，超时预算只能靠 recv_timeout 兑现。
    // 超时后我们先杀进程，管子随之 EOF，这个线程自己就结束了。
    let (tx, rx) = mpsc::channel::<std::result::Result<String, String>>();
    let reader = thread::spawn(move || {
        let mut buf = String::new();
        let mut r = BufReader::new(stdout);
        loop {
            buf.clear();
            match r.read_line(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(Err(
                        "the server closed its stdout without answering `initialize`".to_string(),
                    ));
                    return;
                }
                Ok(_) => {
                    let line = buf.trim();
                    if line.is_empty() {
                        continue;
                    }
                    // 往 stdout 打日志、先推一条 notification 都很常见。
                    // 只认 id 对得上的那一条，其余一律略过。
                    match serde_json::from_str::<serde_json::Value>(line) {
                        Ok(v)
                            if v.get("id").and_then(serde_json::Value::as_u64)
                                == Some(REQUEST_ID) =>
                        {
                            let _ = tx.send(Ok(line.to_string()));
                            return;
                        }
                        _ => continue,
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("failed to read from the server: {e}")));
                    return;
                }
            }
        }
    });

    let mut line = serde_json::to_string(&request).expect("the request is a literal object");
    line.push('\n');
    let write_err = stdin
        .write_all(line.as_bytes())
        .and_then(|()| stdin.flush())
        .err();

    // 写失败也要走完收尾：进程已经起来了，不能因为写不进去就撒手不管。
    let received = if write_err.is_none() {
        Some(rx.recv_timeout(Duration::from_millis(timeout_ms)))
    } else {
        None
    };

    // stdin 撑到这里才关：不少 server 把 stdin 的 EOF 当成「关门」，
    // 提前关会让它在答复之前就退出。
    drop(stdin);
    kill_and_reap(&mut child);
    let _ = reader.join();

    if let Some(e) = write_err {
        bail!("failed to send `initialize`: {e}");
    }
    let answer = match received.expect("received is Some when the write succeeded") {
        Ok(Ok(line)) => line,
        Ok(Err(e)) => bail!("{e}"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!("no response to `initialize` within {timeout_ms} ms; the process was killed")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("the reader stopped before producing a response")
        }
    };

    let v: serde_json::Value = serde_json::from_str(&answer).with_context(|| {
        format!("the server answered with something that is not JSON: {answer}")
    })?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no message");
        bail!("the server rejected `initialize`: {msg}");
    }
    Ok(v.pointer("/result/serverInfo").map(|si| {
        let name = si
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unnamed");
        match si.get("version").and_then(serde_json::Value::as_str) {
            Some(ver) => format!("{name} {ver}"),
            None => name.to_string(),
        }
    }))
}

/// 杀掉子进程（连同它的进程组）并回收，绝不留僵尸。
fn kill_and_reap(child: &mut Child) {
    #[cfg(unix)]
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// 给整个进程组发 SIGKILL。
///
/// 负号 pid 就是进程组；子进程已经用 `process_group(0)` 自成一组，组 id
/// 等于它的 pid，所以这一发覆盖它拉起来的孙子进程。借 `/bin/kill` 而不是
/// `libc::kill`，是因为 duster-core 不允许新增依赖；拿不到这个命令时
/// 还有 `Child::kill` 兜底（只是管不到孙子），所以失败一律忽略。
#[cfg(unix)]
fn kill_group(pid: u32) {
    let _ = Command::new("/bin/kill")
        .arg("-KILL")
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------
// sync
// ---------------------------------------------------------------------------

/// 回写时肯读的配置文件上限（8 MiB）。
///
/// 与 codec 读路径的默认上限同一个数量级：一个正常的 agent 配置远小于它，
/// 超过这个尺寸的多半不是配置文件，把它整个吸进内存再改写既慢又危险。
const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;

/// 一次分发的共享上下文。攒成一个结构体而不是十个参数。
struct SyncCtx<'a> {
    idx: &'a Index,
    cat: &'a Catalog,
    home: &'a Path,
    op_id: &'a str,
    src: &'a McpServerSpec,
    src_agent: &'a str,
    dry_run: bool,
}

/// 目标文件里「写到哪」的计划。在快照之前就得算出来——
/// 一个注定写不进去的目标不该留下一份快照垃圾。
enum WritePlan {
    Json { pointer: String },
    Toml { dotted: String },
}

/// 对单个目标执行一次分发推导，并视 `dry_run` 决定落不落盘。
///
/// 硬要求，缺一不可：
/// 1. **写前过 schema_guard**（[`duster_adapter::guard::check`]）。指纹对不上
///    就拒绝这一个目标并如实报告，不中断其余目标——一个 agent 升级了格式，
///    不该连累别人。
/// 2. **写前留整文件快照**（[`duster_fs::snapshot::snapshot_file`]）。
///    改的是还装着二十条别的声明的文件，用户同意的是「加一条」，
///    写坏了丢的是整个文件。
/// 3. **保守回写**：只碰目标键，注释、键序、无关内容逐字节不变。
/// 4. 目标已存在同名且内容不同 → 默认 `skip` 并报告差异，
///    绝不静默覆盖别人手写的配置。
///
/// # `action` 词汇表的现状
///
/// 落地的动作只有 `create` / `skip` / `refused` 三种。`update` 保留但**当前
/// 走不到**：唯一会命中它的场景是「目标已有同名但内容不同」，而那一条按
/// 要求 4 恒为 `skip`。覆盖别人手写的声明需要它自己的显式开关，
/// `yes`（"这次写入我同意了"）不是那个开关，不该被借用成那个意思。
/// `skipped` 不出现在本函数里——它是 [`apply_sync`] 给 `allow` 未入选
/// 目标留的占位，代表「用户没勾」，不是分发这一侧的结论。
///
/// # 方言转换的有损边界
///
/// [`McpServerSpec::extra`] **不跨方言搬运**：它装的正是「本方言里我们没建模的
/// 字段」（opencode 的 `enabled`、codex 的自定义键），按定义只对源方言有意义，
/// 塞进目标文件就是往别人家的配置里写它不认识的键。解析失败的占位条目
/// （`extra._parse_error`）更是直接拒绝分发——那不是一份规格，是一份错误报告。
fn sync_one(ctx: &mut SyncCtx<'_>, agent: &str) -> SyncOutcome {
    let mk =
        |path: String, action: &str, error: Option<String>, snapshot: Option<String>| SyncOutcome {
            agent_id: agent.to_string(),
            path,
            action: action.to_string(),
            error,
            snapshot,
        };

    if agent == ctx.src_agent {
        return mk(
            String::new(),
            "skip",
            Some("source and target are the same agent".to_string()),
            None,
        );
    }
    let Some(i) = ctx.cat.site_of(agent) else {
        return mk(
            String::new(),
            "refused",
            Some(format!(
                "no adapter manifest declares an MCP file for agent `{agent}`"
            )),
            None,
        );
    };
    let site = &ctx.cat.sites[i];
    let path = site.path.clone();
    let shown = path.display().to_string();
    if !path.is_file() {
        return mk(
            shown.clone(),
            "refused",
            Some(format!(
                "{shown} does not exist; duster only adds a declaration to a config file \
                 the agent already has"
            )),
            None,
        );
    }

    let doc = match codec::read_file(&path) {
        Ok(d) => d,
        Err(e) => return mk(shown, "refused", Some(format!("{e:#}")), None),
    };
    let existing = match servers_in_doc(&doc, &site.section) {
        Ok(v) => v,
        Err(e) => return mk(shown, "refused", Some(format!("{e:#}")), None),
    };

    // 闸门在最前面：形状变了就连「有没有同名」都不该再下结论，
    // 因为我们对这份文件的理解已经过期了。
    let slot = format!("guard:{agent}:mcp:{shown}");
    let conn = ctx.idx.conn();
    let verdict = guard::check(
        &doc,
        || meta::get(conn, &slot),
        |fp| {
            if ctx.dry_run {
                // 预览一个字节都不写，连基准都不落——否则一次 dry-run
                // 就把「第一次见到的形状」定死了。
                Ok(())
            } else {
                meta::set(conn, &slot, fp)
            }
        },
    );
    match verdict {
        Ok(guard::Verdict::Drifted {
            expected,
            actual,
            reason,
        }) => {
            return mk(
                shown,
                "refused",
                Some(format!("{reason} (fingerprint {expected} -> {actual})")),
                None,
            );
        }
        Ok(_) => {}
        Err(e) => return mk(shown, "refused", Some(format!("schema guard: {e:#}")), None),
    }

    if let Some(old) = existing.iter().find(|s| s.name == ctx.src.name) {
        if old.content_hash() == ctx.src.content_hash() {
            return mk(
                shown,
                "skip",
                Some("already declared with identical content".to_string()),
                None,
            );
        }
        // 用户手写的声明只报差异，绝不覆盖。差异走同一套 diff 引擎，
        // 字段路径的读法与 `duster mcp diff` 一字不差。
        let fields = match (serde_json::to_value(old), serde_json::to_value(ctx.src)) {
            (Ok(l), Ok(r)) => crate::diff::diff_values(
                &l,
                &r,
                agent,
                ctx.src_agent,
                &crate::diff::DiffOptions::default(),
            )
            .entries
            .iter()
            .map(|e| e.key.clone())
            .collect::<Vec<_>>()
            .join(", "),
            _ => "unknown fields".to_string(),
        };
        return mk(
            shown,
            "skip",
            Some(format!(
                "already declared with different content (differs at: {fields}); \
                 refusing to overwrite a hand-written declaration"
            )),
            None,
        );
    }

    let entry = match native_entry(ctx.src, site.section.mapper) {
        Ok(v) => v,
        Err(e) => return mk(shown, "refused", Some(format!("{e:#}")), None),
    };
    let plan = match write_plan(&site.section, &ctx.src.name) {
        Ok(p) => p,
        Err(e) => return mk(shown, "refused", Some(format!("{e:#}")), None),
    };

    // 原文文本要另读一遍：codec 的写入口是**文本进文本出**（它不碰文件系统，
    // 正是为了让「guard → 快照 → 落盘」这个次序由调用方强制执行），
    // 而上面那份 Doc 是解析后的树，回不到原文的注释与键序。
    let src_text = match codec::read_to_string_capped(&path, MAX_CONFIG_BYTES) {
        Ok(t) => t,
        Err(e) => return mk(shown, "refused", Some(format!("{e:#}")), None),
    };
    let rewritten = match plan {
        WritePlan::Json { pointer } => codec::set_json_pointer(&src_text, &pointer, &entry),
        WritePlan::Toml { dotted } => {
            // duster-core 不依赖 toml crate，也不该依赖它（分层向下）。
            // 目标类型由 set_toml_path 的参数推断出来，serde 负责搭桥。
            match serde_json::from_value(entry) {
                Ok(v) => codec::set_toml_path(&src_text, &dotted, &v),
                Err(e) => {
                    Err(anyhow::Error::new(e).context("cannot express this declaration as TOML"))
                }
            }
        }
    };
    let new_text = match rewritten {
        Ok(t) => t,
        Err(e) => return mk(shown, "refused", Some(format!("{e:#}")), None),
    };

    if ctx.dry_run {
        // 计划算到了「新文件长什么样」才敢说 create：预览要能预告失败，
        // 否则用户看完一份全绿的计划，执行时才撞上写入口的报错。
        return mk(shown, "create", None, None);
    }
    if new_text == src_text {
        // 写入口一个字节都没改。走到这里说明目标其实已经有一模一样的内容，
        // 前面的同名判定没认出来（比如它藏在别的段里）。不动，如实说。
        return mk(
            shown,
            "skip",
            Some("the file already contains this declaration byte-for-byte".to_string()),
            None,
        );
    }

    // 快照失败即放弃改写：写不出退路就不动手。
    let root = ctx.home.join(".agent-duster").join("snapshots");
    let receipt = match duster_fs::snapshot::snapshot_file(&root, ctx.op_id, &path, ctx.home) {
        Ok(r) => r,
        Err(e) => {
            return mk(
                shown.clone(),
                "refused",
                Some(format!("refusing to rewrite {shown}: {e:#}")),
                None,
            );
        }
    };
    // GC 放在留完快照之后：先保住新的，再回收旧的。
    let _ = duster_fs::snapshot::prune_old(&root, duster_fs::snapshot::KEEP);
    let snap = Some(receipt.path.display().to_string());

    match duster_fs::atomic::write_atomic(&path, new_text.as_bytes()) {
        Ok(()) => {
            relearn_guard(ctx, &path, &slot);
            mk(shown, "create", None, snap)
        }
        // 快照路径照样带上：写坏了的话，用户第一句话就是「原来那份在哪」。
        Err(e) => mk(shown, "refused", Some(format!("{e:#}")), snap),
    }
}

/// 写完之后把闸门的基准换成新文件的形状。
///
/// 少了这一步，duster 的保守回写会绊倒它自己：注册表从「一条声明」变成
/// 「两条声明」时结构指纹本来就会变（见 [`duster_adapter::guard::fingerprint`]
/// 里 `{*:…}` 折叠那一节），于是下一次 sync 把我们刚按计划加的那一笔
/// 当成别人动过手脚。闸门要防的是**别人**改了形状，不是我们自己写的那一条。
///
/// 刷新失败不回滚也不报错：后果只是下一次 sync 因指纹对不上而拒写，
/// 那是安全的一侧——宁可多问一次，不可少拦一次。
fn relearn_guard(ctx: &SyncCtx<'_>, path: &Path, slot: &str) {
    if let Ok(doc) = codec::read_file(path) {
        let _ = meta::set(ctx.idx.conn(), slot, &guard::fingerprint(&doc));
    }
}

/// 目标文件里这条声明该落在哪个键上。
fn write_plan(section: &ResourceSection, name: &str) -> Result<WritePlan> {
    match section.mapper {
        MapperName::McpStandardJson | MapperName::McpOpencodeJson | MapperName::McpGeminiJson => {
            let base = section.json_pointer.clone().unwrap_or_default();
            Ok(WritePlan::Json {
                pointer: format!("{base}/{}", escape_pointer(name)),
            })
        }
        MapperName::McpCodexToml => {
            // `toml_path` 按 `.` 简单切分，不支持带引号的段，所以名字里有 `.`
            // 就没法表达。这时候宁可拒绝，也不能写到一个错位置去。
            if name.contains('.') {
                bail!(
                    "server name `{name}` contains a dot; the TOML dotted path cannot express it"
                );
            }
            let base = section.toml_key.clone().unwrap_or_default();
            Ok(WritePlan::Toml {
                dotted: if base.is_empty() {
                    name.to_string()
                } else {
                    format!("{base}.{name}")
                },
            })
        }
        m => bail!(
            "mapper `{}` is not an MCP dialect duster can write",
            m.as_str()
        ),
    }
}

/// RFC 6901 转义：`~` -> `~0`、`/` -> `~1`。顺序不能反。
fn escape_pointer(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

/// 把归一化规格变回目标方言的原生条目。
///
/// 返回 JSON 表示：TOML 目标由 serde 在写入口转过去（见 [`sync_one`]），
/// 两条方言路径因此共用同一份「该写哪些键」的定义。
/// 表达不了的组合一律报错，绝不悄悄丢字段——丢掉的可能正是那个 API key。
fn native_entry(spec: &McpServerSpec, mapper: MapperName) -> Result<serde_json::Value> {
    let mut obj = serde_json::Map::new();
    match (mapper, &spec.transport) {
        (MapperName::McpStandardJson, McpTransport::Stdio) => {
            obj.insert("command".into(), stdio_command(spec)?.into());
            if !spec.args.is_empty() {
                obj.insert("args".into(), str_array(&spec.args));
            }
            if !spec.env.is_empty() {
                obj.insert("env".into(), str_map(&spec.env));
            }
        }
        (MapperName::McpStandardJson, t) => {
            obj.insert("type".into(), transport_name(t).into());
            obj.insert("url".into(), remote_url(spec)?.into());
            if !spec.headers.is_empty() {
                obj.insert("headers".into(), str_map(&spec.headers));
            }
        }
        (MapperName::McpCodexToml, McpTransport::Stdio) => {
            obj.insert("command".into(), stdio_command(spec)?.into());
            if !spec.args.is_empty() {
                obj.insert("args".into(), str_array(&spec.args));
            }
            if !spec.env.is_empty() {
                obj.insert("env".into(), str_map(&spec.env));
            }
        }
        (MapperName::McpCodexToml, McpTransport::Http) => {
            obj.insert("url".into(), remote_url(spec)?.into());
            if !spec.headers.is_empty() {
                // Codex 的请求头键名是 http_headers。
                obj.insert("http_headers".into(), str_map(&spec.headers));
            }
        }
        (MapperName::McpCodexToml, McpTransport::Sse) => bail!(
            "codex-toml has no way to say `sse`: it infers the transport from the presence \
             of `url`, so an SSE declaration would silently become plain HTTP"
        ),
        (MapperName::McpOpencodeJson, McpTransport::Stdio) => {
            // 这个方言把 command 和 args 合成一条 argv 数组。
            let mut argv = Vec::with_capacity(spec.args.len() + 1);
            argv.push(serde_json::Value::from(stdio_command(spec)?));
            argv.extend(
                spec.args
                    .iter()
                    .map(|a| serde_json::Value::from(a.as_str())),
            );
            obj.insert("type".into(), "local".into());
            obj.insert("command".into(), serde_json::Value::Array(argv));
            if !spec.env.is_empty() {
                // 环境变量的键名是 environment，不是 env。
                obj.insert("environment".into(), str_map(&spec.env));
            }
        }
        (MapperName::McpOpencodeJson, t) => {
            if *t == McpTransport::Sse {
                bail!(
                    "opencode's `remote` type cannot express SSE: the dialect has no field \
                     that distinguishes it from plain HTTP"
                );
            }
            if !spec.headers.is_empty() {
                bail!(
                    "opencode's `remote` type has no field for request headers; \
                     {} header(s) would be dropped",
                    spec.headers.len()
                );
            }
            obj.insert("type".into(), "remote".into());
            obj.insert("url".into(), remote_url(spec)?.into());
        }
        (MapperName::McpGeminiJson, McpTransport::Stdio) => {
            obj.insert("command".into(), stdio_command(spec)?.into());
            if !spec.args.is_empty() {
                obj.insert("args".into(), str_array(&spec.args));
            }
            if !spec.env.is_empty() {
                obj.insert("env".into(), str_map(&spec.env));
            }
        }
        (MapperName::McpGeminiJson, t) => {
            // `url` + 显式 `type`，两种远程传输对称——正是
            // `gemini mcp add --transport http|sse` 自己写出来的形状
            // （0.54.4 包内 `packages/cli/src/commands/mcp/add.ts`）。跟目标
            // 工具自己的写法保持一致，是「它一定读得回去」最硬的保证。
            //
            // 不写 `httpUrl`：它在 0.54.4 里已被标记弃用（同时出现 `url` 时
            // 会每次启动都打一条迁移警告），我们没有理由往别人的配置里种一颗
            // 疣。显式 `type` 也顺手消掉了歧义——含义在版本之间变过的只有
            // **光秃秃的** `url`，而我们从不写那种。
            obj.insert("url".into(), remote_url(spec)?.into());
            obj.insert("type".into(), transport_name(t).into());
            if !spec.headers.is_empty() {
                obj.insert("headers".into(), str_map(&spec.headers));
            }
        }
        (m, _) => bail!(
            "mapper `{}` is not an MCP dialect duster can write",
            m.as_str()
        ),
    }
    Ok(serde_json::Value::Object(obj))
}

fn stdio_command(spec: &McpServerSpec) -> Result<&str> {
    spec.command
        .as_deref()
        .filter(|c| !c.is_empty())
        .with_context(|| format!("stdio server `{}` has no command", spec.name))
}

fn remote_url(spec: &McpServerSpec) -> Result<&str> {
    spec.url
        .as_deref()
        .filter(|u| !u.is_empty())
        .with_context(|| format!("remote server `{}` has no url", spec.name))
}

fn str_array(v: &[String]) -> serde_json::Value {
    serde_json::Value::Array(
        v.iter()
            .map(|s| serde_json::Value::from(s.as_str()))
            .collect(),
    )
}

fn str_map(m: &BTreeMap<String, String>) -> serde_json::Value {
    serde_json::Value::Object(
        m.iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::from(v.as_str())))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ---------------------- 夹具 ----------------------

    /// 一份迷你 agent 清单。probe 根与资源路径都在假 home 内。
    fn manifest_with(id: &str, mapper: &str) -> String {
        format!(
            r#"
[agent]
id = "{id}"
display_name = "{id}"

[probe]
any_of = ["~/.{id}"]

[[resource]]
kind = "mcp"
scope = "global"
path = "~/.{id}/mcp.json"
json_pointer = "/mcpServers"
mapper = "{mapper}"
"#
        )
    }

    struct Fake {
        home: TempDir,
    }

    impl Fake {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            fs::create_dir_all(home.path().join(".agent-duster/adapters")).unwrap();
            Self { home }
        }

        fn path(&self) -> &Path {
            self.home.path()
        }

        fn index(&self) -> PathBuf {
            self.path().join(".agent-duster/index.db")
        }

        /// 落一份清单 + 一份配置文件，返回配置文件路径。
        fn agent(&self, id: &str, body: &str) -> PathBuf {
            self.agent_with(id, "mcp/standard-json", body)
        }

        /// 同上，但指定方言——目标文件的写法由清单里的 mapper 决定。
        fn agent_with(&self, id: &str, mapper: &str, body: &str) -> PathBuf {
            fs::write(
                self.path()
                    .join(".agent-duster/adapters")
                    .join(format!("{id}.toml")),
                manifest_with(id, mapper),
            )
            .unwrap();
            let dir = self.path().join(format!(".{id}"));
            fs::create_dir_all(&dir).unwrap();
            let file = dir.join("mcp.json");
            fs::write(&file, body).unwrap();
            file
        }

        /// 按 scan 的口径播种索引行：key = server 名，path = 配置文件，哈希 = 语义哈希。
        fn seed(&self, rows: &[(&str, PathBuf, McpServerSpec)]) {
            let idx = Index::open(&self.index()).unwrap();
            for (agent, path, spec) in rows {
                let row = duster_index::upsert::ResourceRow {
                    agent_id: (*agent).to_string(),
                    kind: "mcp".to_string(),
                    scope: "global".to_string(),
                    key: spec.name.clone(),
                    path: path.display().to_string(),
                    size: 0,
                    mtime_ns: 0,
                    hash_content: Some(*spec.content_hash().as_bytes()),
                    cheap_print: None,
                    clean_level: None,
                    reclaimable: None,
                    install_bytes: None,
                };
                duster_index::upsert::upsert_resource(idx.conn(), &row).unwrap();
            }
        }
    }

    fn stdio(name: &str, cmd: &str, args: &[&str]) -> McpServerSpec {
        McpServerSpec {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: Some(cmd.to_string()),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            extra: None,
        }
    }

    fn body(args: &str) -> String {
        format!(r#"{{"mcpServers":{{"echo":{{"command":"echo","args":[{args}]}}}}}}"#)
    }

    // ---------------------- list / show / diff ----------------------

    /// 三个 agent 声明得一模一样 → 合成一行、三处出处、零冲突。
    #[test]
    fn list_三处相同声明合并成一行() {
        let f = Fake::new();
        let spec = stdio("echo", "echo", &["hi"]);
        let rows: Vec<_> = ["a1", "a2", "a3"]
            .iter()
            .map(|id| (*id, f.agent(id, &body(r#""hi""#)), spec.clone()))
            .collect();
        f.seed(&rows);

        let out = list(Some(&f.index())).unwrap();
        assert!(out.warnings.is_empty(), "warnings: {:?}", out.warnings);
        assert_eq!(out.servers.len(), 1);
        assert!(out.conflicts.is_empty());

        let s = &out.servers[0];
        assert_eq!(s.name, "echo");
        assert_eq!(s.spec.args, ["hi"]);
        let agents: Vec<&str> = s.declared_in.iter().map(|d| d.agent_id.as_str()).collect();
        assert_eq!(agents, ["a1", "a2", "a3"], "出处按 agent id 排序");
        assert!(
            s.declared_in
                .iter()
                .all(|d| d.dialect == "mcp/standard-json"),
            "方言应当从清单反查得到"
        );
    }

    /// 同名不同参 → 两行，并且 conflicts 点名这个名字下有两个哈希。
    #[test]
    fn list_同名不同内容不合并且进冲突表() {
        let f = Fake::new();
        let hi = stdio("echo", "echo", &["hi"]);
        let bye = stdio("echo", "echo", &["bye"]);
        f.seed(&[
            ("a1", f.agent("a1", &body(r#""hi""#)), hi.clone()),
            ("a2", f.agent("a2", &body(r#""bye""#)), bye.clone()),
        ]);

        let out = list(Some(&f.index())).unwrap();
        assert_eq!(out.servers.len(), 2, "内容不同不许合并");
        let hashes = out.conflicts.get("echo").expect("echo 应当在冲突表里");
        assert_eq!(hashes.len(), 2);
        assert_ne!(hashes[0], hashes[1]);

        // show 在这种情形下必须拒绝替用户挑一个。
        let err = show(Some(&f.index()), "echo").unwrap_err().to_string();
        assert!(err.contains("2 different contents"), "{err}");

        let err = show(Some(&f.index()), "nope").unwrap_err().to_string();
        assert!(err.contains("unknown MCP server"), "{err}");
        assert!(err.contains("echo"), "未知名字要列出已知名字: {err}");
    }

    /// diff 走通用引擎，报的是「哪个字段不一样」。
    #[test]
    fn diff_点名差异字段() {
        let f = Fake::new();
        f.seed(&[
            (
                "a1",
                f.agent("a1", &body(r#""hi""#)),
                stdio("echo", "echo", &["hi"]),
            ),
            (
                "a2",
                f.agent("a2", &body(r#""bye""#)),
                stdio("echo", "echo", &["bye"]),
            ),
        ]);

        let d = diff(Some(&f.index()), "echo", "a1", "a2").unwrap();
        assert!(!d.identical);
        assert_eq!(d.left_label, "a1");
        let keys: Vec<&str> = d.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["args.0"], "只有第一个参数不同");
        assert_eq!(d.entries[0].left.as_deref(), Some("hi"));
        assert_eq!(d.entries[0].right.as_deref(), Some("bye"));

        let err = diff(Some(&f.index()), "echo", "a1", "ghost")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`ghost` does not declare"), "{err}");
    }

    // ---------------------- ping ----------------------

    /// 禁用状态下一个进程都不许起：命令指向一个「跑起来就会留痕」的脚本。
    #[cfg(unix)]
    #[test]
    fn ping_禁用时不启动任何进程() {
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("was-spawned");
        let script = format!("touch {}", marker.display());
        let spec = stdio("m", "/bin/sh", &["-c", &script]);

        let r = ping(&spec, "a1", &PingOptions::default());
        assert!(!r.ok);
        assert!(r.ms.is_none());
        let err = r.error.unwrap();
        assert!(err.contains("opt-in"), "{err}");
        assert!(!marker.exists(), "禁用状态下不得启动任何进程");
    }

    /// 非 stdio 一律跳过并说明理由，同样不起进程。
    #[test]
    fn ping_非_stdio_跳过() {
        let mut spec = stdio("m", "", &[]);
        spec.transport = McpTransport::Http;
        spec.command = None;
        spec.url = Some("https://example.invalid/mcp".to_string());

        let r = ping(
            &spec,
            "a1",
            &PingOptions {
                enabled: true,
                timeout_ms: 100,
            },
        );
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert!(err.contains("not pingable"), "{err}");
        assert!(err.contains("runtime"), "{err}");
    }

    /// 一个只答一条 initialize、之后永远阻塞的 stdio server：
    /// 握手成功，并且函数返回时那个进程已经不在了。
    #[cfg(unix)]
    #[test]
    fn ping_握手成功且不留活着的进程() {
        if !Path::new("/bin/sh").is_file() {
            return; // 没有 POSIX shell 的平台上这条契约无从验证
        }
        const SERVER_SH: &str = r#"#!/bin/sh
echo $$ > "__PID__"
read -r _line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"fake","version":"9.9"}}}'
exec sleep 300
"#;
        let d = tempfile::tempdir().unwrap();
        let pidfile = d.path().join("pid");
        let script = d.path().join("server.sh");
        fs::write(
            &script,
            SERVER_SH.replace("__PID__", &pidfile.display().to_string()),
        )
        .unwrap();

        let spec = stdio("fake", "/bin/sh", &[script.to_str().unwrap()]);
        let r = ping(
            &spec,
            "a1",
            &PingOptions {
                enabled: true,
                timeout_ms: 10_000,
            },
        );
        assert!(r.ok, "error: {:?}", r.error);
        assert_eq!(r.server_info.as_deref(), Some("fake 9.9"));
        assert!(r.ms.is_some());

        let pid: u32 = fs::read_to_string(&pidfile)
            .expect("脚本应当写下自己的 pid")
            .trim()
            .parse()
            .unwrap();
        // 信号落地有一点延迟，给它 1 秒;超过就是真的没收干净。
        let mut alive = true;
        for _ in 0..50 {
            if !pid_alive(pid) {
                alive = false;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive, "子进程 {pid} 在 ping 返回后还活着");
    }

    /// 超时的 server 同样要被杀干净，并且报的是超时而不是别的什么。
    #[cfg(unix)]
    #[test]
    fn ping_超时即杀() {
        if !Path::new("/bin/sh").is_file() {
            return;
        }
        const MUTE_SH: &str = r#"#!/bin/sh
echo $$ > "__PID__"
exec sleep 300
"#;
        let d = tempfile::tempdir().unwrap();
        let pidfile = d.path().join("pid");
        let script = d.path().join("mute.sh");
        fs::write(
            &script,
            MUTE_SH.replace("__PID__", &pidfile.display().to_string()),
        )
        .unwrap();

        let spec = stdio("mute", "/bin/sh", &[script.to_str().unwrap()]);
        let r = ping(
            &spec,
            "a1",
            &PingOptions {
                enabled: true,
                timeout_ms: 300,
            },
        );
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert!(err.contains("no response"), "{err}");

        let pid: u32 = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..50 {
            if !pid_alive(pid) {
                alive = false;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive, "超时的子进程 {pid} 没被收掉");
    }

    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // ---------------------- sync ----------------------

    /// 目标文件：带注释、带无关键、已经有一条别的声明。
    const TARGET_JSONC: &str = r#"{
  // 用户手写的注释，必须逐字节还在
  "theme": "dark",
  "mcpServers": {
    "other": { "command": "other-bin" }
  }
}
"#;

    /// 造一个「源 a1 有 echo，目标 b1 没有」的局面。
    fn sync_fixture() -> (Fake, PathBuf) {
        let f = Fake::new();
        let src = f.agent("a1", &body(r#""hi""#));
        let dst = f.agent("b1", TARGET_JSONC);
        f.seed(&[
            ("a1", src, stdio("echo", "echo", &["hi"])),
            ("b1", dst.clone(), stdio("other", "other-bin", &[])),
        ]);
        (f, dst)
    }

    fn sync_opts(f: &Fake) -> SyncOptions {
        SyncOptions {
            index_path: Some(f.index()),
            home: Some(f.path().to_path_buf()),
            name: "echo".to_string(),
            from: Some("a1".to_string()),
            to: vec!["b1".to_string()],
            dry_run: true,
            yes: false,
        }
    }

    /// plan_sync 无视 `dry_run` / `yes` 两个旗标：哪怕调用方摆的是
    /// `dry_run: false, yes: false`，它也照出完整计划，一个字节都不写、
    /// 一份快照都不留。
    #[test]
    fn plan_sync_不写一个字节_也不要求_yes() {
        let (f, dst) = sync_fixture();
        let before = fs::read(&dst).unwrap();

        let mut o = sync_opts(&f);
        o.dry_run = false; // plan_sync 按定义就是只出计划，旗标应当被无视
        let out = plan_sync(&o).unwrap();
        assert_eq!(out.targets.len(), 1);
        assert_eq!(out.targets[0].action, "create", "error: {:?}", out.targets[0].error);
        assert!(out.targets[0].snapshot.is_none());

        assert_eq!(fs::read(&dst).unwrap(), before, "plan_sync 改了目标文件");
        assert!(
            !f.path().join(".agent-duster/snapshots").exists(),
            "plan_sync 不该留快照"
        );
    }

    /// 存下一个对不上的指纹 → 拒绝这个目标，原文件一字节没动。
    /// 闸门在出计划阶段就触发：执行前的漂移校验本来也该拦下这一单。
    #[test]
    fn plan_sync_指纹漂移即拒写() {
        let (f, dst) = sync_fixture();
        let before = fs::read(&dst).unwrap();
        {
            let idx = Index::open(&f.index()).unwrap();
            let slot = format!("guard:b1:mcp:{}", dst.display());
            meta::set(idx.conn(), &slot, "0123456789abcdef").unwrap();
        }

        let mut o = sync_opts(&f);
        o.dry_run = false;
        o.yes = true;
        let out = plan_sync(&o).unwrap();

        assert_eq!(out.targets.len(), 1);
        assert_eq!(out.targets[0].action, "refused");
        let err = out.targets[0].error.clone().unwrap();
        assert!(err.contains("structure changed"), "{err}");
        assert!(err.contains("0123456789abcdef"), "要说出期望指纹: {err}");
        assert_eq!(fs::read(&dst).unwrap(), before, "被拒绝的目标不许被改");
    }

    /// 真写一次：新增条目落地，注释与无关键逐字节保留，快照留在 home 下。
    #[test]
    fn sync_写入保留注释与无关键并留快照() {
        let (f, dst) = sync_fixture();

        let mut o = sync_opts(&f);
        o.dry_run = false;
        o.yes = true;
        let approved = plan_sync(&o).unwrap();
        assert_eq!(
            approved.targets[0].action, "create",
            "error: {:?}",
            approved.targets[0].error
        );
        let out = apply_sync(&o, &approved, None).unwrap();
        assert_eq!(out[0].action, "create", "error: {:?}", out[0].error);

        let after = fs::read_to_string(&dst).unwrap();
        assert!(
            after.contains("// 用户手写的注释，必须逐字节还在"),
            "注释没了:\n{after}"
        );
        assert!(
            after.contains(r#""theme": "dark""#),
            "无关键被改写:\n{after}"
        );
        assert!(after.contains("other-bin"), "别人的声明没了:\n{after}");

        // 新条目按 standard-json 方言落地。
        let doc = codec::read_file(&dst).unwrap();
        let Doc::Json(v) = &doc else {
            panic!("目标应当仍是 JSON")
        };
        let echo = v.pointer("/mcpServers/echo").expect("新条目不在");
        assert_eq!(echo.pointer("/command").unwrap(), "echo");
        assert_eq!(echo.pointer("/args/0").unwrap(), "hi");

        // 快照落在 ~/.agent-duster/snapshots/<op-id>/ 下。
        let snap = out[0].snapshot.clone().expect("必须留快照");
        assert!(Path::new(&snap).is_file(), "快照文件不存在: {snap}");
        assert!(
            snap.starts_with(
                &f.path()
                    .join(".agent-duster/snapshots")
                    .display()
                    .to_string()
            ),
            "快照落点不对: {snap}"
        );
        // 快照留的是改写**之前**的原文。
        assert_eq!(fs::read_to_string(&snap).unwrap(), TARGET_JSONC);

        // 同一条再同步一次 = 已经一样了，跳过。
        let again = plan_sync(&o).unwrap();
        let out2 = apply_sync(&o, &again, None).unwrap();
        assert_eq!(out2[0].action, "skip");
        assert!(out2[0].error.clone().unwrap().contains("identical"));
    }

    /// 目标已有同名但内容不同：报差异、跳过，绝不覆盖。
    #[test]
    fn sync_同名不同内容只报差异不覆盖() {
        let f = Fake::new();
        let src = f.agent("a1", &body(r#""hi""#));
        let dst = f.agent(
            "b1",
            r#"{"mcpServers":{"echo":{"command":"echo","args":["bye"]}}}"#,
        );
        f.seed(&[
            ("a1", src, stdio("echo", "echo", &["hi"])),
            ("b1", dst.clone(), stdio("echo", "echo", &["bye"])),
        ]);
        let before = fs::read(&dst).unwrap();

        let mut o = sync_opts(&f);
        o.dry_run = false;
        o.yes = true;
        let approved = plan_sync(&o).unwrap();
        assert_eq!(approved.targets[0].action, "skip");
        let out = apply_sync(&o, &approved, None).unwrap();

        assert_eq!(out[0].action, "skip");
        let err = out[0].error.clone().unwrap();
        assert!(err.contains("args.0"), "要点名差异字段: {err}");
        assert!(err.contains("refusing to overwrite"), "{err}");
        assert_eq!(fs::read(&dst).unwrap(), before);
    }

    /// 没点头就想写 → 直接拒绝，连索引都不开。
    #[test]
    fn apply_sync_无_yes_直接拒绝() {
        let (f, _) = sync_fixture();
        let mut o = sync_opts(&f);
        o.dry_run = false;
        o.yes = false;
        // 拒绝发生在读 approved 之前，喂一份空计划即可。
        let empty = SyncPlan {
            src_hash: String::new(),
            targets: Vec::new(),
        };
        let err = apply_sync(&o, &empty, None).unwrap_err().to_string();
        assert!(err.contains("without confirmation"), "{err}");
    }

    /// 用户点头之后、执行之前，目标文件被人改了：重算出来的计划与
    /// 用户过目的那份对不上，一个字都不能写，直接报错让用户重跑。
    #[test]
    fn apply_sync_计划漂移就拒绝执行() {
        let (f, dst) = sync_fixture();

        let mut o = sync_opts(&f);
        o.dry_run = false;
        o.yes = true;
        let approved = plan_sync(&o).unwrap();
        assert_eq!(approved.targets[0].action, "create");

        // 两次调用之间，目标文件里出现了一条同名但内容不同的手写声明：
        // 重算结果从 create 变成 skip，批准的那份计划已经不作数了。
        let drifted = r#"{"mcpServers":{"other":{"command":"other-bin"},"echo":{"command":"echo","args":["bye"]}}}"#;
        fs::write(&dst, drifted).unwrap();

        let err = apply_sync(&o, &approved, None).unwrap_err().to_string();
        assert!(err.contains("no longer current"), "{err}");
        assert!(err.contains("re-run"), "{err}");
        assert!(err.contains("b1"), "要点名是哪个 agent 漂移了: {err}");
        // 比的是**漂移后**那份内容:那是磁盘上的现状,也是这一趟必须原样留下
        // 的东西。拿漂移前那一版来比是比错了对象——它是测试自己覆盖掉的,
        // 任何实现都不可能让它回来,断言只会永远失败。
        assert_eq!(
            fs::read(&dst).unwrap(),
            drifted.as_bytes(),
            "漂移时一个字都不许写"
        );
    }

    /// 用户点头之后、执行之前，**源** agent 的这条声明被改了：命令换掉、
    /// 名字不变、目标依旧缺失——`(agent_id, path, action)` 三元组完全相等，
    /// 落点没漂，只有 payload 漂了。这正是三元组挡不住、只有源指纹挡得住的
    /// 那一种漂移：不拦下它，用户没看过的 payload 就被写进别人的配置。
    #[test]
    fn apply_sync_源声明被改过就拒绝执行() {
        let (f, dst) = sync_fixture();

        let mut o = sync_opts(&f);
        o.dry_run = false;
        o.yes = true;
        let approved = plan_sync(&o).unwrap();
        assert_eq!(approved.targets[0].action, "create");
        let before = fs::read(&dst).unwrap();

        // 计划到手后改源：只动 payload（args 换掉），server 名与目标现状
        // 都不变。重算出来的三元组仍是 (b1, 目标路径, create)。
        let src_path = f.path().join(".a1/mcp.json");
        let edited = r#"{"mcpServers":{"echo":{"command":"echo","args":["bye"]}}}"#;
        fs::write(&src_path, edited).unwrap();

        let err = apply_sync(&o, &approved, None).unwrap_err().to_string();
        assert!(err.contains("source"), "要点名是源声明漂移: {err}");
        assert!(err.contains("re-run"), "要要求重跑一遍: {err}");
        assert!(
            err.contains("changed after you reviewed"),
            "要说清是看完计划之后被改的: {err}"
        );
        // 源漂移时一个字都不许写：目标配置必须逐字节未变。
        assert_eq!(fs::read(&dst).unwrap(), before, "源漂移时目标不许被改");
    }

    /// `allow` 只缩窄执行范围：没入选的目标一个字节都不写，返回值里
    /// 保留一条 `skipped` 占位（`error` 为 `None`，不是失败），入选的
    /// 目标照写。
    #[test]
    fn apply_sync_未入选的目标一个字节都不写() {
        let f = Fake::new();
        let src = f.agent("a1", &body(r#""hi""#));
        let dst_b = f.agent("b1", TARGET_JSONC);
        let dst_c = f.agent("c1", TARGET_JSONC);
        f.seed(&[
            ("a1", src, stdio("echo", "echo", &["hi"])),
            ("b1", dst_b.clone(), stdio("other", "other-bin", &[])),
            ("c1", dst_c.clone(), stdio("other", "other-bin", &[])),
        ]);
        let before_c = fs::read(&dst_c).unwrap();

        let mut o = sync_opts(&f);
        o.dry_run = false;
        o.yes = true;
        o.to = vec!["b1".to_string(), "c1".to_string()];
        let approved = plan_sync(&o).unwrap();
        assert_eq!(approved.targets.len(), 2);
        assert!(approved.targets.iter().all(|r| r.action == "create"));

        let out = apply_sync(&o, &approved, Some(&["b1".to_string()])).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].agent_id, "b1");
        assert_eq!(out[0].action, "create", "error: {:?}", out[0].error);
        assert_eq!(out[1].agent_id, "c1");
        assert_eq!(out[1].action, "skipped");
        // 占位不是失败：error 必须留空，退出码才不会被它顶上去。
        assert_eq!(out[1].error, None);
        assert!(out[1].snapshot.is_none());

        assert_eq!(
            fs::read(&dst_c).unwrap(),
            before_c,
            "未入选的目标一个字节都不能动"
        );
        assert!(
            fs::read_to_string(&dst_b).unwrap().contains(r#""echo""#),
            "入选的目标应当真的写进去了"
        );
    }

    // ---------------------- 方言转换 ----------------------

    /// 三种方言各自的原生形状。转换是 sync 的全部意义所在。
    #[test]
    fn native_entry_三种方言各写各的形状() {
        let mut spec = stdio("echo", "npx", &["-y", "srv"]);
        spec.env.insert("K".to_string(), "V".to_string());

        let std_json = native_entry(&spec, MapperName::McpStandardJson).unwrap();
        assert_eq!(std_json["command"], "npx");
        assert_eq!(std_json["args"][1], "srv");
        assert_eq!(std_json["env"]["K"], "V");

        let codex = native_entry(&spec, MapperName::McpCodexToml).unwrap();
        assert_eq!(codex["command"], "npx");
        assert_eq!(codex["args"][0], "-y");

        let oc = native_entry(&spec, MapperName::McpOpencodeJson).unwrap();
        assert_eq!(oc["type"], "local");
        assert_eq!(oc["command"][0], "npx", "argv 数组的第 0 位是程序名");
        assert_eq!(oc["command"][2], "srv");
        assert_eq!(oc["environment"]["K"], "V", "键名是 environment 不是 env");
        assert!(oc.get("args").is_none(), "这个方言没有 args 字段");
    }

    /// 表达不了就报错，绝不悄悄丢字段。
    #[test]
    fn native_entry_有损转换一律拒绝() {
        let mut sse = stdio("s", "", &[]);
        sse.transport = McpTransport::Sse;
        sse.command = None;
        sse.url = Some("https://x.invalid/sse".to_string());
        assert!(native_entry(&sse, MapperName::McpCodexToml).is_err());
        assert!(native_entry(&sse, MapperName::McpOpencodeJson).is_err());
        // standard-json 有 type 字段，表达得了。
        let ok = native_entry(&sse, MapperName::McpStandardJson).unwrap();
        assert_eq!(ok["type"], "sse");

        let mut http = sse.clone();
        http.transport = McpTransport::Http;
        http.headers
            .insert("Authorization".to_string(), "Bearer x".to_string());
        assert!(
            native_entry(&http, MapperName::McpOpencodeJson).is_err(),
            "opencode 放不下请求头，必须拒绝而不是丢掉凭据"
        );
        let codex = native_entry(&http, MapperName::McpCodexToml).unwrap();
        assert_eq!(codex["http_headers"]["Authorization"], "Bearer x");
    }

    /// Gemini 的远程条目写成 `url` + 显式 `type`，两种传输对称。
    ///
    /// 这正是 `gemini mcp add --transport http|sse` 自己写出来的形状（0.54.4
    /// 包内 `packages/cli/src/commands/mcp/add.ts`）。反过来说：**不能**写
    /// `{"type":"http","url":…}` 之外的两种诱人写法——
    /// - 光秃秃的 `url`：含义在 Gemini 的版本之间变过（老版本读成 SSE，
    ///   0.54.4 读成 streamable HTTP），写它就是在赌用户的版本号；
    /// - `httpUrl`：0.54.4 已标记弃用，是给老配置留的读入口，不该由我们往
    ///   别人的配置里新种一个。
    #[test]
    fn native_entry_gemini_写_url_加_type_而不是_httpurl() {
        let mut http = stdio("stitch", "", &[]);
        http.transport = McpTransport::Http;
        http.command = None;
        http.url = Some("https://stitch.googleapis.com/mcp".to_string());
        http.headers
            .insert("X-Goog-Api-Key".to_string(), "AQ.secret".to_string());

        let e = native_entry(&http, MapperName::McpGeminiJson).unwrap();
        assert_eq!(e["url"], "https://stitch.googleapis.com/mcp");
        assert_eq!(e["type"], "http");
        assert_eq!(e["headers"]["X-Goog-Api-Key"], "AQ.secret");
        assert!(
            e.get("httpUrl").is_none(),
            "httpUrl 已弃用，不该由我们新写进去"
        );

        let mut sse = http.clone();
        sse.transport = McpTransport::Sse;
        let e = native_entry(&sse, MapperName::McpGeminiJson).unwrap();
        assert_eq!(e["url"], "https://stitch.googleapis.com/mcp");
        assert_eq!(e["type"], "sse");
        assert!(e.get("httpUrl").is_none());

        // stdio 与 standard-json 同形：command / args / env。
        let mut local = stdio("echo", "npx", &["-y", "srv"]);
        local.env.insert("K".to_string(), "V".to_string());
        let e = native_entry(&local, MapperName::McpGeminiJson).unwrap();
        assert_eq!(e["command"], "npx");
        assert_eq!(e["args"][1], "srv");
        assert_eq!(e["env"]["K"], "V");
        assert!(
            e.get("type").is_none(),
            "stdio 不必写 type，Gemini 靠 command 认"
        );

        // 端点缺失时报错而不是写出一条连不上的声明。
        let mut no_url = http.clone();
        no_url.url = None;
        assert!(native_entry(&no_url, MapperName::McpGeminiJson).is_err());
    }

    /// 往返：Gemini 原文 -> spec -> Gemini 原生条目 -> spec，语义必须一字不差。
    ///
    /// 用的是本机 `~/.gemini/settings.json` 里 stitch 的真实形态（`httpUrl` +
    /// headers + timeout）。写回时 `httpUrl` 归一成 `url` + `type: "http"`
    /// ——键换了，Gemini 读出来的东西没换，这才是「同一条声明」。
    #[test]
    fn gemini_条目往返一趟语义不变() {
        let src: serde_json::Value = serde_json::from_str(
            r#"{
                "stitch": {
                    "httpUrl": "https://stitch.googleapis.com/mcp",
                    "headers": { "X-Goog-Api-Key": "AQ.secret" },
                    "timeout": 60000,
                    "trust": true
                }
            }"#,
        )
        .unwrap();
        let first = dialect::from_gemini_json(&src).unwrap();
        assert_eq!(first.len(), 1);
        // 未建模的键活着到了 extra，没有在归一化里蒸发。
        let extra: serde_json::Value =
            serde_json::from_str(first[0].extra.as_deref().unwrap()).unwrap();
        assert_eq!(extra["timeout"], 60000);
        assert_eq!(extra["trust"], true);

        let written = native_entry(&first[0], MapperName::McpGeminiJson).unwrap();
        let round = dialect::from_gemini_json(&serde_json::json!({ "stitch": written })).unwrap();

        assert_eq!(round[0].transport, first[0].transport);
        assert_eq!(round[0].url, first[0].url);
        assert_eq!(round[0].headers, first[0].headers);
        assert_eq!(round[0].command, first[0].command);
        assert_eq!(round[0].args, first[0].args);
        assert_eq!(round[0].env, first[0].env);
        assert_eq!(
            round[0].content_hash(),
            first[0].content_hash(),
            "往返后语义哈希必须不变，否则 list 会把它拆成两行"
        );

        // 再写一次已经稳定：`url` + `type` 是这条方言的不动点。
        let again = native_entry(&round[0], MapperName::McpGeminiJson).unwrap();
        assert_eq!(again, written);

        // SSE 同样是不动点——它才是「写错就静默降级成 HTTP」的那一种。
        let mut sse = first[0].clone();
        sse.transport = McpTransport::Sse;
        let w = native_entry(&sse, MapperName::McpGeminiJson).unwrap();
        let back = dialect::from_gemini_json(&serde_json::json!({ "stitch": w })).unwrap();
        assert_eq!(back[0].transport, McpTransport::Sse);
        assert_eq!(back[0].url, sse.url);
        assert_eq!(back[0].headers, sse.headers);
    }

    /// 端到端跑一次 `sync --to <gemini 方言的 agent>`：不只看 `native_entry`
    /// 的返回值，而是看落到磁盘上的那份 JSON 到底长什么样。
    ///
    /// 这条路径以前会把 HTTP 写成 `{"type":"http","url":…}`——在 Gemini 0.54.4
    /// 里读起来照样是 HTTP，可 SSE 写成同样的形状就会被读成 HTTP。现在两种
    /// 传输都带显式 `type`，谁也不会被读错。
    #[test]
    fn sync_写进_gemini_方言的目标是_url_加_type() {
        for (transport, want_type) in [(McpTransport::Http, "http"), (McpTransport::Sse, "sse")] {
            let f = Fake::new();
            // 源必须真的声明这种传输：sync 读的是源文件，不是索引里的哈希。
            let src = f.agent(
                "a1",
                &format!(
                    r#"{{ "mcpServers": {{ "remote": {{ "type": "{want_type}",
                     "url": "https://x.example.com/mcp",
                     "headers": {{ "Authorization": "Bearer x" }} }} }} }}"#
                ),
            );
            let dst = f.agent_with(
                "g1",
                "mcp/gemini-json",
                "{\n  // gemini 的注释也得留着\n  \"mcpServers\": {}\n}\n",
            );

            let mut spec = stdio("remote", "", &[]);
            spec.transport = transport.clone();
            spec.command = None;
            spec.url = Some("https://x.example.com/mcp".to_string());
            spec.headers
                .insert("Authorization".to_string(), "Bearer x".to_string());
            f.seed(&[("a1", src, spec)]);

            let opts = SyncOptions {
                index_path: Some(f.index()),
                home: Some(f.path().to_path_buf()),
                name: "remote".to_string(),
                from: Some("a1".to_string()),
                to: vec!["g1".to_string()],
                dry_run: false,
                yes: true,
            };
            let approved = plan_sync(&opts).unwrap();
            assert_eq!(
                approved.targets[0].action, "create",
                "error: {:?}",
                approved.targets[0].error
            );
            let out = apply_sync(&opts, &approved, None).unwrap();
            assert_eq!(out[0].action, "create", "error: {:?}", out[0].error);

            let text = fs::read_to_string(&dst).unwrap();
            assert!(text.contains("// gemini 的注释也得留着"), "{text}");

            let Doc::Json(v) = codec::read_file(&dst).unwrap() else {
                panic!("目标应当仍是 JSON")
            };
            let e = v.pointer("/mcpServers/remote").expect("新条目不在");
            assert_eq!(e.pointer("/url").unwrap(), "https://x.example.com/mcp");
            assert_eq!(e.pointer("/type").unwrap(), want_type);
            assert_eq!(e.pointer("/headers/Authorization").unwrap(), "Bearer x");
            assert!(
                e.get("httpUrl").is_none(),
                "不该往别人的配置里种一个已弃用的键:\n{text}"
            );

            // 最后一关：Gemini 自己的读法把它读回原来的传输方式。
            let back = dialect::from_gemini_json(v.pointer("/mcpServers").unwrap()).unwrap();
            assert_eq!(back[0].transport, transport);
        }
    }

    /// JSON Pointer 段要按 RFC 6901 转义；TOML 点路径表达不了带点的名字。
    #[test]
    fn write_plan_转义与拒绝() {
        let json_section = ResourceSection {
            kind: ResourceKind::Mcp,
            scope: ManifestScope::Global,
            path: "~/x.json".to_string(),
            json_pointer: Some("/mcpServers".to_string()),
            toml_key: None,
            mapper: MapperName::McpStandardJson,
            glob: None,
            clean_level: None,
            keep_generations: None,
            install_paths: Vec::new(),
        };
        let WritePlan::Json { pointer } = write_plan(&json_section, "a/b~c").unwrap() else {
            panic!("JSON 方言应当出 JSON 计划");
        };
        assert_eq!(pointer, "/mcpServers/a~1b~0c");

        let toml_section = ResourceSection {
            json_pointer: None,
            toml_key: Some("mcp_servers".to_string()),
            mapper: MapperName::McpCodexToml,
            path: "~/x.toml".to_string(),
            ..json_section
        };
        let WritePlan::Toml { dotted } = write_plan(&toml_section, "echo").unwrap() else {
            panic!("TOML 方言应当出 TOML 计划");
        };
        assert_eq!(dotted, "mcp_servers.echo");
        assert!(
            write_plan(&toml_section, "a.b").is_err(),
            "带点的名字在点路径里无法表达"
        );
    }
}
