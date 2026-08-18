//! `duster prune`：清理**陈旧的用户资源**（l2 + 超期 skill / session）。
//!
//! # 为什么是独立的命令，不是 `clean --level l2`
//!
//! 同意模型不同。clean 无需逐项同意，prune 必须逐项过目。肌肉记忆是按命令
//! 建立的，不是按旗标——一个人 `duster clean --yes` 敲过 50 次之后就不再读输出，
//! 这时候把"要读的清单"塞进同一个动词里，那道确认是在跟条件反射较劲。
//! 输出形状也不同：clean 吐一个数字，prune 吐一份要逐行读的清单。
//!
//! # 红线
//!
//! **清完不能影响正常使用。** 只动"长期没碰过"和"过期即无用"的东西，
//! 绝不动软件本体、绝不动唯一副本。默认关闭，必须显式 `--older-than`。
//!
//! # 三种处置方式，按"能不能再生"分
//!
//! - **skill**（不可再生）→ 归档后永久删除。跨 agent 同名 skill 只要**任一**
//!   副本是活跃的就整组保留（避免删掉 link 的源）。
//! - **session**（不可再生且体量大）→ **压缩存档，不删除内容**。压缩包留原地；
//!   原件必须在往返校验一致（解压后 BLAKE3 与原文件逐字节相同）之后才删。
//!   可验证的正确性比可撤销更强，也避免把几百 MB 原件挪进某个"回收站"
//!   再一次没释放空间。
//! - **artifact l2**（可再生但有代价）→ 按同一阈值清；备份类保留最新一份兜底。
//!
//! MCP 的陈旧清理是**改配置文件**不是删文件，依赖 M2 的 Codec 写方向 +
//! schema_guard，本模块不做，只在计划里以 warning 说明。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::plan::{Action, Plan, PlanFilter};

/// prune 的输入。
#[derive(Debug, Clone)]
pub struct PruneOptions {
    pub index_path: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// 只处理这几个 agent；空 Vec = 全部。
    pub agents: Vec<String>,
    /// 陈旧阈值（天）。**必填**——prune 默认关闭。
    pub older_than_days: u32,
    /// 是否把声明了 `keep_generations` 的资源的超编代际纳入 prune。
    /// 默认关——关了与今天行为一致，只多一行点名旗标的收尾报告。
    pub keep_generations: bool,
    /// 导出目录；缺省 `~/agent-duster-exports`。
    pub export_dir: Option<PathBuf>,
    /// true = 只出计划不执行。**默认 true**。
    pub dry_run: bool,
    /// 跳过问句。**不跳过清单打印**。
    pub yes: bool,
    /// 调用方是 `--json` 模式。**`--json` 且无 `--yes` 一律拒绝执行**——
    /// 机器可读输出里没有地方放"用户读过了清单"这件事。
    pub json: bool,
    /// 当前时间（Unix 毫秒）注入口，测试用。
    pub now_ms: Option<i64>,
}

/// 单项处置结果。
#[derive(Debug, Clone, Serialize)]
pub struct PruneOutcome {
    pub path: String,
    pub action: String,
    pub before: u64,
    pub after: u64,
    pub freed: u64,
    /// 压缩存档时的新路径（`.zst`）。
    pub replaced_by: Option<String>,
    pub error: Option<String>,
}

/// prune 的完整报告。
#[derive(Debug, Clone, Serialize)]
pub struct PruneReport {
    pub plan: Plan,
    pub executed: bool,
    pub outcomes: Vec<PruneOutcome>,
    /// 归档回执：路径与实际尺寸必须出现在输出里。
    pub archive_path: Option<String>,
    pub archive_bytes: Option<u64>,
    pub freed_bytes: u64,
    pub warnings: Vec<String>,
}

/// 一兆字节。门槛文案里报 MiB 而不是裸字节——用户要拿这个数做决定。
const MIB: u64 = 1024 * 1024;

/// 生成计划 → 校验同意条件 → 归档 → 执行 → 汇总。
///
/// 执行前的硬门槛，任一未过即返回 Err（CLI 映射退出码 4）：
/// 1. `json && !yes` → 拒绝；
/// 2. **先打包成功再删**：凡要删的不可再生内容一律先进归档包，打包失败即
///    中止整个操作、一个字节不删。没有关闭归档的旗标——`prune` 能删的
///    东西没有可再生的，跳过打包就等于给「删错了」留一个无法撤销的口子。
///
/// 归档预估超过 [`duster_fs::archive::AUTO_ARCHIVE_LIMIT`] **不再拒绝执行**。
/// 旧语义是「超阈值且用户没表态就拒绝」，那扇门依赖 `--archive` /
/// `--no-archive` 三态旗标；旗标没了，门的落点必须重定，而不是悄悄删掉：
/// 超阈值照常打包，但报告里的警告必须把包的预估体积与"超过自动归档上限"
/// 点名说出来。取舍是——prune 本来就先出计划再要 `--yes`，同意建立在
/// 逐行过目的清单上，体积是这份同意的一部分；「拒绝执行」把决定权退给
/// 一枚旗标，而「报数」把决定权留在人读计划的那一步。uninstall 不适用
/// 这条：它没有先出计划再确认的流程，`archive_choice` 三态与拒绝门槛
/// 在那边原样保留。
///
/// 门槛 1 只在**真要动手时**才咬人：dry-run 什么都不执行，也就无所谓
/// "人有没有读过清单"。`--json --dry-run` 是完整预览，必须放行，
/// 否则 §1.2 ② 要的"重定向到文件、看完再回来跑 `--yes`"就没了机器可读的一半。
/// 代价是预览与执行的门槛不同，所以 dry-run 报告必须自己把预估写清楚，
/// 不能让用户到执行那一刻才撞墙。dry-run 不建导出目录、不写包。
pub fn prune(opts: &PruneOptions) -> Result<PruneReport> {
    prune_filtered(opts, &PlanFilter::default())
}

/// 同 [`prune`]，但在出计划后套一层 [`PlanFilter`]。
///
/// 两个入口用同一份过滤器，各取所需：
/// - `duster session prune`（纵向入口，只许动会话）把边界表达成
///   `only_kind: Some("session")` 的**谓词**——计划层看不见「这个入口叫
///   session」这件事（它只知道这些项陈旧了），这份「入口无权动」的判定
///   只有调用方算得出来。谓词而不是快照黑名单：每一份重算出来的计划都
///   自动受它约束，两次计划之间新冒出来的非会话项也逃不掉；
/// - 交互式 `duster prune` 把用户勾选的清单放进 `filter.allow`（白名单），
///   重算时新冒出来的项自动落在名单外（为什么是白名单而不是黑名单，见
///   [`PlanFilter`] 所在模块的模块头）。
///
/// `skip` 按**解析后的绝对路径**逐项相等匹配，`allow` 按「路径 + 动作」
/// 二元组匹配——不是按 key、不是按前缀：计划项的 `path` 已经是展开过的
/// 绝对路径，调用方也只能从同一份计划里取，两边同源才不会出现「以为
/// 放行了其实没有」。
///
/// 剔除而不是改成 [`Action::Keep`]：被跳过的项根本不该出现在用户要逐行读的
/// 那份清单里，留一堆 "keep" 行只会稀释真正要过目的内容。剔除后
/// `reclaim_bytes` 同步重算，否则报告会承诺一个它不会释放的数字。
pub fn prune_filtered(opts: &PruneOptions, filter: &PlanFilter) -> Result<PruneReport> {
    // 计划阶段纯读索引，只读句柄在 plan_prune 内部开完即弃——
    // 下面要开写句柄，单实例锁容不下第二个。
    let mut plan = crate::plan::plan_prune(&crate::plan::PlanOptions {
        index_path: opts.index_path.clone(),
        home: opts.home.clone(),
        agents: opts.agents.clone(),
        older_than_days: Some(opts.older_than_days),
        keep_generations: opts.keep_generations,
        now_ms: opts.now_ms,
    })?;
    filter.apply(&mut plan);
    let plan = plan;
    let mut warnings = plan.warnings.clone();

    let roots = plan.archive_roots();
    let estimate =
        duster_fs::archive::estimate_bytes(&roots).context("failed to estimate archive size")?;
    let over_limit = estimate > duster_fs::archive::AUTO_ARCHIVE_LIMIT;

    // 门槛 1：`--json` 无 `--yes` 拒绝执行。机器可读的输出流里没有任何地方
    // 能放下"这个人读过清单了"这件事，而没有回收站之后，那一次确认是唯一防线。
    if opts.json && !opts.yes && !opts.dry_run {
        bail!(
            "refusing to execute prune in --json mode without --yes: a machine-readable \
             stream has nowhere to record that a human read the list. Preview it with \
             `duster prune --older-than {}d --dry-run --json`, then rerun with --yes",
            opts.older_than_days
        );
    }

    // 归档预估随报告走，dry-run 与执行两条路径都要带上：prune 恒归档，
    // 「包会有多大」是这份同意的一部分，不能等执行完才让用户看见数字。
    // 超过 [`duster_fs::archive::AUTO_ARCHIVE_LIMIT`] 不再拒绝执行（见
    // [`prune`] 的取舍说明），但阈值本身依旧要出现在报告里——把安全门
    // 从「拒绝」降级成「报数」的前提是数字确实到了用户眼前。
    if !roots.is_empty() {
        let dir = export_dir(opts).display().to_string();
        let tense = if opts.dry_run {
            "would be packed into"
        } else {
            "packed into"
        };
        warnings.push(format!(
            "archive estimate: {} MiB across {} path(s) {tense} {dir}",
            estimate / MIB,
            roots.len(),
        ));
        if over_limit {
            warnings.push(format!(
                "the estimate is above the {} MiB auto-archive limit; it will still be packed \
                 — pruning never deletes without a backup",
                duster_fs::archive::AUTO_ARCHIVE_LIMIT / MIB
            ));
        }
    }

    // 门槛 2：dry-run。一个字节都不动，预估已经写进 warnings，
    // 让"看完报告再回来跑 --yes"这条路上没有意外。
    if opts.dry_run {
        return Ok(PruneReport {
            plan,
            executed: false,
            outcomes: Vec::new(),
            archive_path: None,
            archive_bytes: None,
            freed_bytes: 0,
            warnings,
        });
    }

    // 无事可做时不去抢索引的单实例写锁：拿锁本身会把并行跑的另一个 duster 顶掉。
    if plan.actionable().next().is_none() {
        return Ok(PruneReport {
            plan,
            executed: true,
            outcomes: Vec::new(),
            archive_path: None,
            archive_bytes: None,
            freed_bytes: 0,
            warnings,
        });
    }

    // **先打包成功，再删。顺序不能反**——这是归档机制存在的全部意义。
    // 没有关闭归档的旗标：凡删必先进包，打包失败即中止、一个字节不删。
    let mut archive_path = None;
    let mut archive_bytes = None;
    if !roots.is_empty() {
        let home = resolve_home(opts.home.as_deref())?;
        let receipt = duster_fs::archive::archive_paths(
            "prune",
            &roots,
            &export_dir(opts),
            &home,
        )
        .context(
            "archiving failed, so nothing was deleted; fix the cause and rerun",
        )?;
        archive_path = Some(receipt.path.display().to_string());
        archive_bytes = Some(receipt.bytes);
    }

    let index_path = index_path(opts)?;
    let idx = duster_index::db::Index::open(&index_path)
        .with_context(|| format!("failed to open index for writing: {}", index_path.display()))?;
    let conn = idx.conn();

    let mut outcomes: Vec<PruneOutcome> = Vec::new();
    let mut freed_bytes = 0u64;
    for item in plan.actionable() {
        let mut oc = PruneOutcome {
            path: item.path.display().to_string(),
            action: action_str(item.action).to_string(),
            before: 0,
            after: 0,
            freed: 0,
            replaced_by: None,
            error: None,
        };
        match item.action {
            // 会话资源有两种形态：单个 jsonl 文件（Claude/Codex 的每场会话
            // 一行索引），以及整个目录（`~/.codex/archived_sessions` 走
            // stats-only，只有一行索引代表整棵树）。两者都要能压。
            Action::CompressFile => {
                let done = if item.path.is_dir() {
                    compress_session_dir(&item.path)
                } else {
                    compress_session(&item.path)
                };
                match done {
                    Ok(d) => {
                        oc = d;
                        // 索引行改指 `.zst`。turn 表不动：偏移量描述的是解压后的
                        // 逻辑内容，压缩对检索与回读透明。目录形态不改 path
                        // （目录还在原处），`replaced_by` 为 None 时这一步跳过。
                        if let Some(new) = oc.replaced_by.clone()
                            && item.rid >= 0
                            && let Err(e) =
                                duster_index::query::update_resource_path(conn, item.rid, &new)
                        {
                            oc.error = Some(format!(
                                "compressed, but repointing the index failed: {e:#}"
                            ));
                        }
                    }
                    Err(e) => oc.error = Some(format!("{e:#}")),
                }
            }
            Action::RemoveFile | Action::RemoveDir => {
                oc.before = measure_bytes(&item.path);
                match remove_tree(&item.path, &mut warnings) {
                    Ok(()) => {
                        oc.after = measure_bytes(&item.path);
                        oc.freed = oc.before.saturating_sub(oc.after);
                        if item.rid >= 0
                            && let Err(e) = duster_index::query::delete_resource(conn, item.rid)
                        {
                            oc.error =
                                Some(format!("deleted, but dropping the index row failed: {e:#}"));
                        }
                    }
                    Err(e) => oc.error = Some(format!("{e:#}")),
                }
            }
            // actionable() 已经滤掉 Keep；其余动作是 clean 的地盘，prune 不代劳，
            // 但也不静默吞掉——如实报告比"看起来成功了"有用。
            Action::Keep => continue,
            other => {
                oc.error = Some(format!(
                    "{} is a clean action, not a prune action; skipped",
                    action_str(other)
                ));
            }
        }
        // 单项失败不中断整体，但 freed 只累计**实测**释放量，绝不用预估凑数。
        freed_bytes += oc.freed;
        outcomes.push(oc);
    }
    drop(idx);

    Ok(PruneReport {
        plan,
        executed: true,
        outcomes,
        archive_path,
        archive_bytes,
        freed_bytes,
        warnings,
    })
}

/// 会话压缩存档：`<f>.jsonl` → `<f>.jsonl.zst`，往返校验通过后删原件。
///
/// 步骤不可省、不可换序：
/// 1. 过锁探测；
/// 2. 计算原件 BLAKE3；
/// 3. 压缩到 `<f>.jsonl.zst`（同目录，原子写）；
/// 4. **解压回内存重算 BLAKE3，与第 2 步逐字节相同**才继续；
/// 5. 删除原件；
/// 6. 把索引里该资源行的 `path` 改成 `.zst`（turn 表不动：byte_off/byte_len
///    指向的是解压后的逻辑内容，压缩对检索与回读透明）。
///
/// 任何一步失败：删掉半成品 `.zst`，保留原件，返回 Err。
///
/// 第 4 步是这套设计的支点：**可验证的正确性比可撤销更强**。做成"先挪进回收站
/// 再删"看似更保险，实际是把几百 MB 原件从一个目录搬到另一个目录，一个字节
/// 没释放，还得再养一套 GC。往返校验通过则新文件与原件信息等价，删原件不是
/// 冒险；不通过则原件根本没动过，也不需要撤销。
pub fn compress_session(file: &Path) -> Result<PruneOutcome> {
    // 1. 锁探测。正被 agent 追加写的会话，压出来的快照必然是残的。
    duster_fs::lockprobe::ensure_free(file)?;

    let before = std::fs::metadata(file)
        .with_context(|| format!("failed to stat session file: {}", file.display()))?
        .len();

    // 2. 原件指纹。第 4 步要拿它逐字节对。
    let digest_before = duster_fs::hash::hash_file(file)?;

    let mut dst = file.as_os_str().to_owned();
    dst.push(".zst");
    let dst = PathBuf::from(dst);
    // 目标已存在时提前拦下：后面失败要删半成品，而这个 `.zst` 不是我们造的，
    // 删掉它就是删了别人的东西。
    if dst.exists() {
        bail!(
            "refusing to compress {}: {} already exists. Remove or rename it first",
            file.display(),
            dst.display()
        );
    }

    // 3~5 任一步失败都要回到"原件还在、没有半成品"的状态。
    let outcome = (|| -> Result<PruneOutcome> {
        // 3. 流式压缩，原子写。
        let after = duster_fs::zst::compress_file(file, &dst, duster_fs::zst::ARCHIVE_LEVEL)
            .with_context(|| format!("failed to compress {}", file.display()))?;

        // 4. 解压回内存重算指纹，逐字节相同才允许继续。
        let back = duster_fs::zst::decompress_to_vec(&dst)
            .with_context(|| format!("failed to read back {}", dst.display()))?;
        if blake3::hash(&back) != digest_before {
            bail!(
                "round-trip verification failed for {}: the decompressed content does not \
                 match the original BLAKE3 digest. The original was left untouched",
                file.display()
            );
        }

        // 5. 校验过了才删原件。
        std::fs::remove_file(file)
            .with_context(|| format!("failed to remove original: {}", file.display()))?;

        Ok(PruneOutcome {
            path: file.display().to_string(),
            action: action_str(Action::CompressFile).to_string(),
            before,
            after,
            freed: before.saturating_sub(after),
            replaced_by: Some(dst.display().to_string()),
            error: None,
        })
    })();

    if outcome.is_err() {
        let _ = std::fs::remove_file(&dst);
    }
    outcome
}

/// 目录形态的会话资源：逐个 `*.jsonl` 压，**不**把整棵树打成一个包。
///
/// 清单里有两种会话声明：一场会话一个 jsonl（Claude/Codex 走原生适配器，
/// 每个文件一行索引），以及整个目录只有一行索引（`~/.codex/archived_sessions`
/// 走 stats-only，本机 243 MB、153 个 rollout 文件）。后者不能套用单文件路径，
/// 但它恰恰是最该压的那一档。
///
/// 逐文件压是刻意的：整目录打成一个 `.tar.zst` 之后，`duster search` 命中
/// 某一条会话就再也定位不到它了——读一条得先解开几百 MB。逐文件压保住了
/// 「一个会话一个可寻址文件」，`duster open` 照常透明解压回读。
///
/// 单个文件失败不影响其余：失败原因汇进 `error`，成功的那些照样瘦了身。
/// 已经是 `.zst` 的跳过（重跑幂等）。
pub fn compress_session_dir(dir: &Path) -> Result<PruneOutcome> {
    let mut files: Vec<PathBuf> = Vec::new();
    duster_fs::walk::walk_files(dir, &duster_fs::walk::WalkOptions::default(), |p, meta| {
        if meta.is_file() && p.extension().is_some_and(|e| e == "jsonl") {
            files.push(p.to_path_buf());
        }
    })?;
    files.sort(); // 遍历是并行的，排序让报告与重跑结果稳定。

    let mut before = 0u64;
    let mut after = 0u64;
    let mut errors: Vec<String> = Vec::new();
    for f in &files {
        match compress_session(f) {
            Ok(d) => {
                before += d.before;
                after += d.after;
            }
            Err(e) => errors.push(format!("{}: {e:#}", f.display())),
        }
    }

    Ok(PruneOutcome {
        path: dir.display().to_string(),
        action: action_str(Action::CompressFile).to_string(),
        before,
        after,
        freed: before.saturating_sub(after),
        // 目录还在原处，索引行的 path 不必改。
        replaced_by: None,
        error: (!errors.is_empty()).then(|| {
            format!(
                "{} of {} session files could not be compressed: {}",
                errors.len(),
                files.len(),
                errors.join("; ")
            )
        }),
    })
}

/// 陈旧 skill 的整组保留判定。
///
/// 跨 agent 同名 skill 只要**任一**副本活跃（未超期）就整组保留——
/// 三份里删掉两份，剩下那份可能正是 `duster skill link` 的硬链源，
/// 删源等于同时废掉三个 agent 的这个 skill。
pub fn skill_group_is_active(last_used: &[Option<i64>], days: u32, now_ms: i64) -> bool {
    last_used
        .iter()
        .any(|t| !crate::plan::is_stale(*t, days, now_ms))
}

/// 计划动作的库内/输出字符串，与 [`Action`] 的 serde 表示一致。
fn action_str(a: Action) -> &'static str {
    match a {
        Action::Vacuum => "vacuum",
        Action::RemoveSidecar => "remove_sidecar",
        Action::RemoveFile => "remove_file",
        Action::RemoveDir => "remove_dir",
        Action::TruncateFile => "truncate_file",
        Action::CompressFile => "compress_file",
        Action::Keep => "keep",
    }
}

/// 删单个文件或整棵目录树。**删之前必过锁探测**，命中即拒绝，没有 `--force`。
/// 探测能力缺失（没有 lsof）按放行处理，但要让用户知道这一项没被真正检查过。
fn remove_tree(path: &Path, warnings: &mut Vec<String>) -> Result<()> {
    if let duster_fs::lockprobe::LockStatus::Unknown { reason } =
        duster_fs::lockprobe::ensure_free(path)?
    {
        warnings.push(format!(
            "{}: lock check was inconclusive ({reason})",
            path.display()
        ));
    }
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove directory: {}", path.display()))
    } else {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove file: {}", path.display()))
    }
}

/// 路径当前占用的字节数；读不到按 0 计。执行前后各测一次，差值即**实测**释放量。
fn measure_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        duster_fs::walk::walk_stats(path, &duster_fs::walk::WalkOptions::default())
            .map(|s| s.total_bytes)
            .unwrap_or(0)
    } else {
        meta.len()
    }
}

/// 归档落点：显式指定优先，否则 `~/agent-duster-exports`。
fn export_dir(opts: &PruneOptions) -> PathBuf {
    opts.export_dir
        .clone()
        .unwrap_or_else(duster_fs::archive::default_export_dir)
}

/// 索引库路径：显式指定优先，否则 `<home>/.agent-duster/index.db`。
fn index_path(opts: &PruneOptions) -> Result<PathBuf> {
    match &opts.index_path {
        Some(p) => Ok(p.clone()),
        None => Ok(crate::scan::default_index_path(&resolve_home(
            opts.home.as_deref(),
        )?)),
    }
}

/// 确定 home：优先注入值，否则真实用户主目录。
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

#[cfg(test)]
mod tests {
    use super::*;
    use duster_index::db::Index;
    use duster_index::upsert::{self, ResourceRow};
    use duster_model::{AgentInfo, Role, TurnRecord};
    use std::fs;
    use tempfile::TempDir;

    /// 固定的"现在"，2025-10-09。所有陈旧判定都相对它算，不看真实时钟。
    const NOW_MS: i64 = 1_760_000_000_000;
    /// 固定的"很久以前"，2020-09-13，距 NOW_MS 约 1852 天。
    const OLD_MS: i64 = 1_600_000_000_000;

    const SKILL_MD: &str = "---\nname: old-skill\n---\n# 一个很久没碰过的 skill\n";
    const SKILL_NOTES: &str = "笔记正文，用来验证归档包逐字节还原。\n";
    const SESSION_TEXT: &str = "{\"role\":\"user\",\"text\":\"第一行\"}\n{\"role\":\"assistant\",\"text\":\"第二行 hello\"}\n";
    const L2_OLD: &str = "l2 产物：删了要重新登录\n";

    fn touch(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn row(kind: &str, key: &str, path: &Path, size: u64) -> ResourceRow {
        ResourceRow {
            agent_id: "claude".into(),
            kind: kind.into(),
            scope: "user".into(),
            key: key.into(),
            path: path.to_string_lossy().into_owned(),
            size,
            // 索引里的 mtime 就是 skill / artifact 的"最后使用时间"口径。
            mtime_ns: OLD_MS * 1_000_000,
            hash_content: None,
            cheap_print: None,
            clean_level: None,
            reclaimable: None,
            install_bytes: None,
            mapper: None,
        }
    }

    /// 造一个 fixture home：陈旧 skill + 陈旧会话 + l2 产物 + 软件本体。
    /// `big` 为真时额外放一个 210 MiB 的稀疏 skill，用来触发归档阈值。
    fn seed(big: bool) -> (TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();

        let skill = h.join(".claude/skills/old-skill");
        touch(&skill.join("SKILL.md"), SKILL_MD);
        touch(&skill.join("notes.md"), SKILL_NOTES);

        let session = h.join(".claude/projects/proj/old.jsonl");
        touch(&session, SESSION_TEXT);

        // l2 产物成组出现：同一父目录下的最新一份是「兜底副本」，计划会留着它。
        // 只放一个的话整组都会被判为最新，l2 这一档就等于没被测到。
        let l2_old = h.join(".claude/run/profile-2020");
        touch(&l2_old.join("cookies.bin"), L2_OLD);
        let l2_new = h.join(".claude/run/profile-2025");
        touch(&l2_new.join("cookies.bin"), "最新一份，必须留着兜底\n");

        // 软件本体：只统计、永不清理。整个测试的对照组。
        let install = h.join(".claude/plugins/vendor");
        touch(&install.join("payload.bin"), "软件本体，一个字节都不许动\n");

        let db = h.join(".agent-duster/index.db");
        let idx = Index::open(&db).unwrap();
        let conn = idx.conn();
        upsert::upsert_agent(
            conn,
            &AgentInfo {
                id: "claude".into(),
                display_name: "Claude Code".into(),
                root: h.join(".claude"),
                version: None,
            },
            OLD_MS,
        )
        .unwrap();

        upsert::upsert_resource(
            conn,
            &row(
                "skill",
                "old-skill",
                &skill,
                (SKILL_MD.len() + SKILL_NOTES.len()) as u64,
            ),
        )
        .unwrap();

        let sess = upsert::upsert_resource(
            conn,
            &row("session", "old.jsonl", &session, SESSION_TEXT.len() as u64),
        )
        .unwrap();
        upsert::replace_turns(
            conn,
            sess.rid,
            &[TurnRecord {
                seq: 0,
                role: Role::User,
                // 会话的陈旧口径是轮次最大 ts，不是文件 mtime。
                ts_ms: Some(OLD_MS),
                byte_off: 0,
                byte_len: SESSION_TEXT.len() as u64,
                text: SESSION_TEXT.to_string(),
            }],
        )
        .unwrap();

        let mut l2_row = row("artifact", "run-2020", &l2_old, L2_OLD.len() as u64);
        l2_row.clean_level = Some("l2".into());
        l2_row.reclaimable = Some(L2_OLD.len() as u64);
        upsert::upsert_resource(conn, &l2_row).unwrap();
        let mut l2_keep = row("artifact", "run-2025", &l2_new, 40);
        l2_keep.clean_level = Some("l2".into());
        l2_keep.reclaimable = Some(40);
        // 一天前才写过：同组里最新的那份，兜底不删。
        l2_keep.mtime_ns = (NOW_MS - 86_400_000) * 1_000_000;
        upsert::upsert_resource(conn, &l2_keep).unwrap();

        upsert::upsert_resource(conn, &row("install", "plugins", &install, 40)).unwrap();

        if big {
            let big_dir = h.join(".claude/skills/big-skill");
            touch(&big_dir.join("SKILL.md"), "---\nname: big-skill\n---\n");
            // 稀疏文件：逻辑长度 210 MiB，实际不占盘。归档预估看的是 len()。
            let blob = fs::File::create(big_dir.join("blob.bin")).unwrap();
            blob.set_len(210 * MIB).unwrap();
            drop(blob);
            upsert::upsert_resource(conn, &row("skill", "big-skill", &big_dir, 210 * MIB)).unwrap();
        }

        // skill 陈旧判定只看调用记录:证据未采集时整组冻结为 Keep,这些夹具
        // 断言的是「陈旧 skill 该删」,必须处于「已采集、确实没有调用」状态。
        duster_index::meta::set(conn, duster_index::meta::SKILL_EVIDENCE_READY, "1").unwrap();

        drop(idx); // 释放单实例写锁，让被测的 prune 自己开写句柄。
        (home, db)
    }

    fn opts(home: &TempDir, db: &Path) -> PruneOptions {
        PruneOptions {
            index_path: Some(db.to_path_buf()),
            home: Some(home.path().to_path_buf()),
            agents: Vec::new(),
            older_than_days: 30,
            keep_generations: false,
            export_dir: Some(home.path().join("exports")),
            dry_run: true,
            yes: false,
            json: false,
            now_ms: Some(NOW_MS),
        }
    }

    #[test]
    fn compress_session_往返一致后删原件() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("rollout.jsonl");
        let body: String = (0..500)
            .map(|i| format!("{{\"seq\":{i},\"text\":\"第 {i} 轮 hello world\"}}\n"))
            .collect();
        fs::write(&src, &body).unwrap();

        let oc = compress_session(&src).unwrap();
        let zst = src.with_extension("jsonl.zst");

        assert!(!src.exists(), "原件应已删除");
        assert!(zst.is_file(), "应生成 {}", zst.display());
        assert_eq!(oc.replaced_by.as_deref(), Some(&*zst.to_string_lossy()));
        assert_eq!(oc.before, body.len() as u64);
        assert_eq!(oc.after, fs::metadata(&zst).unwrap().len());
        assert_eq!(oc.freed, oc.before - oc.after);
        assert!(oc.error.is_none());
        // 唯一真正重要的断言：解压回来逐字节相同。
        assert_eq!(
            duster_fs::zst::decompress_to_vec(&zst).unwrap(),
            body.as_bytes()
        );
    }

    #[test]
    fn compress_session_目标已存在时保留原件并报错() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("rollout.jsonl");
        fs::write(&src, SESSION_TEXT).unwrap();
        let zst = dir.path().join("rollout.jsonl.zst");
        fs::write(&zst, "别人的东西").unwrap();

        let err = compress_session(&src).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err:#}");
        // 原件必须原封不动，别人的 .zst 也不许被当成半成品删掉。
        assert_eq!(fs::read_to_string(&src).unwrap(), SESSION_TEXT);
        assert_eq!(fs::read(&zst).unwrap(), "别人的东西".as_bytes());
    }

    #[test]
    fn prune_json_无_yes_拒绝执行() {
        let (home, db) = seed(false);
        let mut o = opts(&home, &db);
        o.dry_run = false;
        o.json = true;
        o.yes = false;

        let err = prune(&o).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--json"), "{msg}");
        assert!(msg.contains("--yes"), "{msg}");
        // 拒绝就是一个字节都没动。
        assert!(
            home.path()
                .join(".claude/skills/old-skill/SKILL.md")
                .is_file()
        );
    }

    /// 归档是硬保证，不是旗标：`--archive` / `--no-archive` 已不存在，类型上
    /// 就没有「跳过打包」这个输入，没有任何输入能让 prune 删了东西却不先
    /// 打包。超过自动归档阈值照样打包——旧的「未表态就拒绝」门随旗标一起
    /// 撤了，取而代之的是报告里点名的预估体积与超限说明（见 [`prune`] 的
    /// 取舍）。删掉的内容必须能在 `<home>/agent-duster-exports/` 的包里
    /// 原样解出。
    #[test]
    fn prune_超阈值照样归档且包内内容逐字节还原() {
        let (home, db) = seed(true);
        let mut o = opts(&home, &db);
        o.dry_run = false;
        o.yes = true;
        o.export_dir = Some(home.path().join("agent-duster-exports"));

        let report = prune(&o).unwrap();
        assert!(report.executed);
        let w = report.warnings.join("\n");
        assert!(w.contains("archive estimate"), "{w}");
        assert!(w.contains("auto-archive limit"), "{w}");

        // 陈旧 skill 连同 210 MiB 的 blob 都已删，但必须先躺在包里。
        let big = home.path().join(".claude/skills/big-skill");
        assert!(!big.exists(), "陈旧 skill 应已删除: {}", big.display());
        let archive = PathBuf::from(
            report
                .archive_path
                .as_deref()
                .expect("恒归档:执行后必须产出归档包"),
        );
        assert!(archive.is_file(), "{}", archive.display());
        assert!(
            archive.starts_with(home.path().join("agent-duster-exports")),
            "包必须落在 <home>/agent-duster-exports/: {}",
            archive.display()
        );
        assert_eq!(
            report.archive_bytes,
            Some(fs::metadata(&archive).unwrap().len())
        );

        let dest = tempfile::tempdir().unwrap();
        duster_fs::archive::extract_to(&archive, dest.path()).unwrap();

        // 稀疏 blob:set_len 出来的内容恒为全零。逐字节校验长度与内容
        // （流式，不把 210 MiB 整块搬进内存）。
        let blob = dest.path().join(".claude/skills/big-skill/blob.bin");
        let meta = fs::metadata(&blob).unwrap();
        assert_eq!(meta.len(), 210 * MIB, "blob 长度必须原样");
        let mut f = fs::File::open(&blob).unwrap();
        let mut buf = vec![0u8; 1 << 20];
        let mut n = 0u64;
        let mut all_zero = true;
        loop {
            let r = std::io::Read::read(&mut f, &mut buf).unwrap();
            if r == 0 {
                break;
            }
            n += r as u64;
            all_zero &= buf[..r].iter().all(|&b| b == 0);
        }
        assert_eq!(n, 210 * MIB);
        assert!(all_zero, "blob 内容必须原样（全零）");

        // 普通文本文件逐字节一致。
        assert_eq!(
            fs::read_to_string(dest.path().join(".claude/skills/big-skill/SKILL.md")).unwrap(),
            "---\nname: big-skill\n---\n"
        );
        assert!(report.freed_bytes > 0, "freed 必须是实测值，不能是 0");
    }

    #[test]
    fn prune_dry_run_不动任何文件() {
        let (home, db) = seed(false);
        // 只快照 agent 数据树：索引库自己的 WAL/shm 旁路文件会因为只读打开
        // 而增删，那不是"动了用户的东西"。
        let data = home.path().join(".claude");
        let before = tree_snapshot(&data);

        let report = prune(&opts(&home, &db)).unwrap();
        assert!(!report.executed);
        assert_eq!(report.freed_bytes, 0);
        assert!(report.archive_path.is_none());
        assert!(report.outcomes.is_empty());
        assert!(
            !report.plan.items.is_empty(),
            "计划不该是空的，否则后面的断言什么也没证明"
        );
        assert_eq!(tree_snapshot(&data), before);
    }

    /// dry-run 什么都不执行，所以"人有没有读过清单"这道门槛不该咬人：
    /// `--json --dry-run` 必须是完整预览，还要把执行时会打的包有多大、
    /// 是否超过自动归档上限提前说清楚——恒归档之后「拒绝」撤了，
    /// 「报数」顶上（见 [`prune`] 的取舍）。
    #[test]
    fn prune_json_dry_run_是完整预览且报出归档预估() {
        let (home, db) = seed(true);
        let mut o = opts(&home, &db);
        o.json = true;
        o.yes = false;

        let report = prune(&o).unwrap();
        assert!(!report.executed);
        assert!(!report.plan.items.is_empty());
        let w = report.warnings.join("\n");
        assert!(w.contains("archive estimate"), "{w}");
        assert!(w.contains("auto-archive limit"), "{w}");
        assert!(
            home.path()
                .join(".claude/skills/big-skill/blob.bin")
                .is_file()
        );
        assert!(!home.path().join("exports").exists(), "预览不许打包");
    }

    #[test]
    fn prune_归档包解开后与被删内容逐字节一致() {
        let (home, db) = seed(false);
        let mut o = opts(&home, &db);
        o.dry_run = false;
        o.yes = true;

        let report = prune(&o).unwrap();
        assert!(report.executed);
        for oc in &report.outcomes {
            assert!(oc.error.is_none(), "{oc:?}");
        }

        let archive = PathBuf::from(
            report
                .archive_path
                .as_deref()
                .expect("归档路径必须出现在报告里"),
        );
        assert!(archive.is_file(), "{}", archive.display());
        assert_eq!(
            report.archive_bytes,
            Some(fs::metadata(&archive).unwrap().len())
        );

        // 陈旧 skill 与陈旧 l2 产物都已删，归档包解开后逐字节还原——
        // 这是归档机制唯一的机器守卫。
        let skill = home.path().join(".claude/skills/old-skill");
        let l2_old = home.path().join(".claude/run/profile-2020");
        assert!(!skill.exists(), "陈旧 skill 应已删除");
        assert!(!l2_old.exists(), "陈旧 l2 产物应已删除");
        // 同组最新的一份是兜底副本，绝不能跟着走。
        assert!(
            home.path()
                .join(".claude/run/profile-2025/cookies.bin")
                .is_file(),
            "最新一份 l2 必须留着兜底"
        );
        // 会话是**原地压缩**不是删除，所以不进归档包，但内容必须还在。
        let zst = home.path().join(".claude/projects/proj/old.jsonl.zst");
        assert!(zst.is_file(), "会话应被压缩存档");
        assert_eq!(
            duster_fs::zst::decompress_to_vec(&zst).unwrap(),
            SESSION_TEXT.as_bytes()
        );

        let dest = tempfile::tempdir().unwrap();
        let n = duster_fs::archive::extract_to(&archive, dest.path()).unwrap();
        assert!(n > 0);
        let restored = dest.path().join(".claude/skills/old-skill");
        assert_eq!(
            fs::read_to_string(restored.join("SKILL.md")).unwrap(),
            SKILL_MD
        );
        assert_eq!(
            fs::read_to_string(restored.join("notes.md")).unwrap(),
            SKILL_NOTES
        );
        assert_eq!(
            fs::read_to_string(dest.path().join(".claude/run/profile-2020/cookies.bin")).unwrap(),
            L2_OLD
        );
        assert!(report.freed_bytes > 0, "freed 必须是实测值，不能是 0");
    }

    #[test]
    fn prune_绝不触碰_install_路径() {
        let (home, db) = seed(false);
        let install = home.path().join(".claude/plugins");

        // ① dry-run 计划里 install 行必须出现，但恒为 not cleanable。
        let planned = prune(&opts(&home, &db)).unwrap().plan;
        let install_items: Vec<_> = planned
            .items
            .iter()
            .filter(|i| i.kind == "install")
            .collect();
        assert!(!install_items.is_empty(), "计划里必须看得见软件本体");
        for i in &install_items {
            assert!(!i.cleanable, "install 永远 not cleanable: {i:?}");
            assert_eq!(i.action, Action::Keep);
        }
        // ② 任何会被执行的项都不得落在 install 子树里。
        for i in planned.actionable() {
            assert!(
                !i.path.starts_with(&install),
                "prune 计划碰到了软件本体: {}",
                i.path.display()
            );
        }

        // ③ 真跑一遍，软件本体的文件必须还在。
        let mut o = opts(&home, &db);
        o.dry_run = false;
        o.yes = true;
        let report = prune(&o).unwrap();
        for oc in &report.outcomes {
            assert!(
                !Path::new(&oc.path).starts_with(&install),
                "prune 动了软件本体: {}",
                oc.path
            );
        }
        assert_eq!(
            fs::read_to_string(install.join("vendor/payload.bin")).unwrap(),
            "软件本体，一个字节都不许动\n"
        );
    }

    /// 整棵 home 的 (相对路径, 字节数) 快照，用来断言"一个字节都没动"。
    fn tree_snapshot(root: &Path) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let meta = e.metadata().unwrap();
                if meta.is_dir() {
                    stack.push(p);
                } else {
                    out.push((
                        p.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                        meta.len(),
                    ));
                }
            }
        }
        out.sort();
        out
    }
}
