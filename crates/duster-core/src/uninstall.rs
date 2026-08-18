//! `duster uninstall <agent>`：卸载整个 agent 的数据。
//!
//! # 语义与 clean 相反，这是它不能做成 `clean --level l3` 的根本原因
//!
//! clean 操作清单**声明过**的路径（白名单，没声明的绝不碰）；
//! uninstall 操作 agent 的**根目录整棵树**——没声明的子目录
//! （`~/.codex/computer-use` 61 MB、`generated_images` 13 MB、`~/.qoder/canvas`）
//! **也必须删**，否则卸载不干净。一个命令不可能同时遵守两套所有权模型。
//!
//! 铁律因此精确化为：**`duster clean` 永不卸载软件；卸载是单独命名、
//! 单独指名目标、单独同意模型的动词。**
//!
//! # 同意模型
//!
//! 要求**逐字输入 agent id 确认**，不接受裸 `--yes`。这不是仪式感：
//! `--yes` 会被写进脚本、会被 shell history 补全，而这个动作删的是
//! 用户所有的历史会话与记忆。
//!
//! # 三个模式，一个动词
//!
//! - **owns**：agent 独占的目录与文件，整棵删（含清单没声明的子目录）。
//!   `--data-only` 的含义就是「只做这一项」。
//! - **shared**：别人家文件里指向本 agent 的那一个键，外科式删除——
//!   先过 schema_guard，再留整文件快照，最后原子写。同文件内其他键、
//!   注释、键序、缩进、换行符逐字节不动。
//! - **package**：软件当初是怎么装上的。**duster 永不代跑包管理器**：
//!   命令原样打出来，跑不跑是用户的事。`--run-package-manager` 是显式的
//!   例外，而且照样先把 argv 打出来再执行。
//! - **residue**：**声明化的已知残留**——以前只记在注释里、永远靠用户
//!   手动删的既定事实（安装脚本追加的 PATH 行、别的工具数据库里的供应商
//!   行）。声明化之后 duster 自己删：删行不删文件，删不掉就如实报
//!   `Failed`，绝不假装卸干净。
//!
//! 后三个模式全部由清单的 `[uninstall]` 段驱动。没声明就没有——
//! duster 不去猜别人家的配置里哪个键是它的。

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_adapter::manifest::{self, PackageHint, ResidueSpec, SharedEdit};
use duster_adapter::{codec, guard};
use duster_fs::archive::{self, ArchiveReceipt};
use duster_fs::lockprobe::{self, LockStatus};
use duster_fs::walk::{self, WalkOptions};
use duster_index::db::Index;
use duster_index::meta;
use duster_index::query::{self, ResourceFilter};

use crate::plan::{Plan, human_bytes};

/// uninstall 的输入。
#[derive(Debug, Clone, Default)]
pub struct UninstallOptions {
    pub index_path: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// 目标 agent id。
    pub agent: String,
    /// 只动 agent 独占的目录与文件，跳过清单 `[uninstall]` 段里的 `shared`
    /// （别人家文件里指向本 agent 的键）、`package`（安装方式提示）与
    /// `residue`（声明化的已知残留）。
    ///
    /// 默认 false，也就是四个模式全做：留着一条指向已删程序的 MCP 声明
    /// 不叫卸载干净，那正是下一个「为什么这个 agent 启动时报错」的来源。
    pub data_only: bool,
    /// 用户逐字输入的确认串，必须与 `agent` 完全相等。
    pub confirm: Option<String>,
    /// sessions / memory 打包到导出目录。**默认 true**。
    pub export_first: bool,
    /// 保留原地不删的资源类，如 `["sessions", "memory"]`。
    pub keep: Vec<String>,
    /// 归档决定：`None` = 未表态，预估超阈值时拒绝执行（见 [`check_data_escape`]）；
    /// `Some(true)` 强制打包，`Some(false)` 显式接受永久丢失。prune 已撤掉
    /// 三态、恒归档，不经过这里。
    pub archive: Option<bool>,
    pub export_dir: Option<PathBuf>,
    /// 允许 duster 代跑清单里的包管理器卸载命令。**默认 false**——
    /// 那时命令只打印不执行。开了也只是「先打印 argv 再执行」，
    /// 探测命令（`detect`）则任何时候都可能跑，它是只读的。
    pub run_package_manager: bool,
    /// true = 只出计划不执行。**默认 true**。
    pub dry_run: bool,
}

/// 一项前置检查的结果。
#[derive(Debug, Clone, Serialize)]
pub struct PreflightCheck {
    /// `cross-reference` / `data-escape` / `credentials`。
    pub name: String,
    pub passed: bool,
    /// 人话结论。未通过时必须说清"卡在哪、怎么解"。
    pub detail: String,
}

/// 一处共享文件改键的下场。
///
/// `Planned` 只在 dry-run 里出现（"真跑会删掉它"）；真跑时每一条最终
/// 都会落在其余四个之一。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SharedAction {
    /// 键在文件里，真跑会把它删掉。
    Planned,
    /// 已删掉。
    Removed,
    /// 键本来就不在了。这是成功，不是错误——卸载两遍不该报错。
    Absent,
    /// 文件不存在。
    Missing,
    /// 拒绝改写（指纹漂移、解析不了、快照写不出）。整个文件一个字节没动。
    Refused,
}

impl SharedAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Removed => "removed",
            Self::Absent => "absent",
            Self::Missing => "missing",
            Self::Refused => "refused",
        }
    }
}

/// 共享文件里一处改键的结果。
#[derive(Debug, Clone, Serialize)]
pub struct SharedEditOutcome {
    /// 展开后的绝对路径。
    pub path: String,
    /// 被点名的那个键，原样带上定位方式：`/mcpServers/foo` 或 `mcp_servers.foo`。
    pub key: String,
    /// 清单里那句英文理由，一字不改地带给用户——他有权知道
    /// duster 凭什么去动别人的文件。
    pub reason: String,
    pub action: SharedAction,
    /// 不是 `Removed` 时说明为什么；`Removed` 时为 None。
    pub detail: Option<String>,
    /// 改写前的整文件快照落点。
    pub snapshot: Option<String>,
}

/// 包还在不在。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DetectStatus {
    Installed,
    NotInstalled,
    /// 清单没给探测命令，或者探测命令自己跑不起来。
    Unknown,
}

impl DetectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::NotInstalled => "not-installed",
            Self::Unknown => "unknown",
        }
    }
}

/// duster 对一条包管理器提示做了什么。**默认永远是 `Printed`。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageAction {
    /// 只打印。这是默认，也是这条规矩的全部内容。
    Printed,
    /// 探测说它根本没装，跳过。
    Skipped,
    /// `--run-package-manager` 下真跑了，且退出码为 0。
    Executed,
    /// 真跑了但失败。
    Failed,
}

impl PackageAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Printed => "printed",
            Self::Skipped => "skipped",
            Self::Executed => "executed",
            Self::Failed => "failed",
        }
    }
}

/// 一条包管理器提示的结果。
#[derive(Debug, Clone, Serialize)]
pub struct PackageOutcome {
    /// `npm` / `brew` / `curl` / `pipx` / `cargo`。
    pub manager: String,
    /// 可以直接粘回终端的一整行。
    pub command: String,
    /// 同一条命令的 argv 形式，给 `--json` 的消费者。
    pub argv: Vec<String>,
    pub detected: DetectStatus,
    pub action: PackageAction,
    pub detail: Option<String>,
    /// 真跑时的合并输出（stdout + stderr）。只打印时为 None。
    pub output: Option<String>,
}

/// 一处清单声明的残留（`[[uninstall.residue]]`）的下场。
#[derive(Debug, Clone, Serialize)]
pub struct ResidueOutcome {
    /// 人话：删的是哪一行/哪一行里的什么。
    pub what: String,
    /// 被处理的文件。shell_line：rc 文件；sqlite_row：db 文件。
    pub path: PathBuf,
    /// 清单里那句理由，一字不改地带给用户——他有权知道 duster 凭什么
    /// 去动这份文件/这个库。
    pub why: String,
    pub action: ResidueAction,
}

/// 一条残留的下场。
///
/// `Planned` 只在 dry-run 里出现（"真跑会删掉它"）；真跑时每一条最终
/// 都会落在其余三个之一。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResidueAction {
    /// 已删掉。
    Removed,
    /// dry-run：真跑会删掉它。
    Planned,
    /// 本来就找不到（文件不在 / 行不在 / 库不在 / 0 行受影响）。
    /// 这是成功，不是错误——卸载两遍不该报错。
    NotFound,
    /// 该删但没删成（快照失败、读不了、表或列不存在……）。
    /// 那个文件一个字节没动。
    Failed(String),
}

/// uninstall 的完整报告。
#[derive(Debug, Clone, Serialize)]
pub struct UninstallReport {
    pub plan: Plan,
    pub checks: Vec<PreflightCheck>,
    pub executed: bool,
    pub removed_bytes: u64,
    pub archive_path: Option<String>,
    pub archive_bytes: Option<u64>,
    /// 删除后仍存在的路径（应为空；非空即卸载不干净，必须报出来）。
    pub leftovers: Vec<String>,
    /// 共享文件改键，逐条。`--data-only` 或清单没声明时为空。
    pub shared: Vec<SharedEditOutcome>,
    /// 包管理器提示，逐条。**这一栏默认只是打印出来的命令。**
    pub packages: Vec<PackageOutcome>,
    /// 声明化的残留（shell 行 / SQLite 行），逐条。`--data-only` 或清单
    /// 没声明时为空。`Failed` 与 `leftovers` 同属「没删干净」——
    /// 残留还在，索引就不该清，agent 继续列在列表里。
    pub residues: Vec<ResidueOutcome>,
    pub warnings: Vec<String>,
}

/// `--keep` 能指名保留的资源类。只有这两类是**不可再生**的用户数据，
/// 其余（缓存、日志、插件本体）留着没有意义——它们的存在理由是那个
/// 已经被卸载掉的程序。
const KEEPABLE_KINDS: [&str; 2] = ["session", "memory"];

/// 遍历**别人地盘**时不深入的目录。
///
/// 剪掉它们不影响结论的正确性，只影响"能不能定位到外部副本"：
/// 硬链的判定靠 A 侧的 `st_nlink` 计数（见 [`check_cross_reference`]），
/// 外部遍历只负责找出那一份副本在哪。找不到就退化为提示而非失败，
/// 而 pnpm 往 `~/.pnpm-store` 打的硬链、`.git` 里的对象，本来就不是
/// 跨 agent 引用。为它们每次卸载多走一个 GB 不划算。
const OUTSIDE_PRUNE_DIRS: [&str; 2] = ["node_modules", ".git"];

/// 三项前置检查，**缺一不可，任一未过即中止**。
///
/// 1. **交叉引用**：`duster skill link` 的 CAS 硬链源、cc-switch 持有的
///    claude 配置备份等——卸载 A 不得打断 B。命中则先实体化（把硬链/软链
///    换成独立副本）再删。
/// 2. **数据逃生**：sessions / memory 默认 `--export-first` 打包到
///    `~/agent-duster-exports/`（本机 codex sessions 352 MB +
///    archived_sessions 242 MB，zstd 后约一个数量级更小），
///    或 `--keep sessions,memory` 保留原地。
/// 3. **凭据**：跑一遍 [`crate::doctor::scan_secrets`]，报出"本次删除的文件中
///    含 N 处明文凭据，建议去 platform 撤销 token"。
///    **只删本地文件不撤销 token 等于没卸干净。**
///
/// `opts.dry_run` 为 true 时本函数不产生任何副作用：不实体化、不打包，
/// 只把"将要做什么"写进 `detail`。
pub fn preflight(opts: &UninstallOptions) -> Result<Vec<PreflightCheck>> {
    Ok(run_preflight(opts)?.checks)
}

/// 计划 → 确认校验 → 前置检查 → 导出 →（非 dry-run 时）删除 → 复核残留
/// → 共享文件改键 → 残留清理 → 包管理器提示 → 清索引。
///
/// 硬要求：
/// - `confirm` 与 `agent` 不完全相等即报错，**不接受 `--yes` 替代**；
/// - `dry_run` 恒返回 `Ok`（`executed = false`），**检查没过也照样返回**——
///   预览就是用来看"卡在哪、怎么解"的，那时候给一句 Err 等于把唯一一次
///   能看清单的机会也拿掉；真执行时才把未过的检查变成中止；
/// - 预览必须把 `shared`、`packages` 与 `residues` **逐条列全**（连同清单里
///   那句 reason / why），而且一个字节都不写。确认清单是这个动词唯一的
///   防线，看不见就等于没有；
/// - 删除完成后**逐条复核**：计划里的路径应全部消失，残留写进 `leftovers`；
/// - 共享改键排在整棵树删除**之后**。反过来的话，树没删成就白改了别人的
///   文件；而"引用还在、被指的东西已经没了"是两者之间明显更安全的中间态；
/// - 单条共享改键失败（指纹漂移、解析不了、快照写不出）**只作废它自己**，
///   删除与其余改键照走：一个 agent 升级了配置格式，不该连累整场卸载；
/// - **删干净了才清索引**：删除后的残留、`--keep` 保住的路径、删不掉的
///   residue，任何一样还在就保留索引行——agent 继续显示在列表里，直到它
///   真的被删干净（残留还在却从列表消失，正是「列表说谎」）。清索引时才
///   删该 agent 的全部行（资源行 + agent 行）。
pub fn uninstall(opts: &UninstallOptions) -> Result<UninstallReport> {
    // ① 同意模型：逐字输入 agent id。裸 `--yes` 明确不接受。
    if opts.confirm.as_deref() != Some(opts.agent.as_str()) {
        bail!(
            "confirmation required: type the agent id verbatim, `--confirm {agent}`. \
             A bare `--yes` is deliberately not accepted here — this deletes every session and \
             memory {agent} ever wrote, and `--yes` is exactly the flag that ends up in shell \
             history, aliases and scripts.",
            agent = opts.agent
        );
    }

    let home = resolve_home(opts)?;
    let idx_path = resolve_index_path(opts, &home);

    let pf = run_preflight(opts)?;

    let pf_checks_failed = pf.checks.iter().filter(|c| !c.passed).count();

    let mut warnings = pf.plan.warnings.clone();
    warnings.extend(pf.warnings.iter().cloned());

    let mut report = UninstallReport {
        archive_path: pf.receipt.as_ref().map(|r| r.path.display().to_string()),
        archive_bytes: pf.receipt.as_ref().map(|r| r.bytes),
        plan: pf.plan,
        checks: pf.checks,
        executed: false,
        removed_bytes: 0,
        leftovers: Vec::new(),
        shared: Vec::new(),
        packages: Vec::new(),
        residues: Vec::new(),
        warnings,
    };

    // ② dry-run 先返回，**哪怕有检查没过**。
    //
    //    "先看报告再执行"是这个动词的主路径，而"卡住了"恰恰是最需要看见的
    //    那份报告：未过的检查带着 detail 一起回去（怎么解都写在里面），
    //    用户据此补上 `--export-first` 或 `--keep` 再跑一次。
    //    在预览阶段把它变成一句 Err，等于用户唯一能拿到清单的时候拿不到清单。
    //
    //    两个新模式在这里走一遍**只读**预演：读文件、比指纹、算出删掉那个键
    //    之后文本会不会变，但不落基准、不写盘、不跑包管理器。residue 同样
    //    只读预演：照读文件、只读查库，算出"真跑会删什么"，不建快照、
    //    一个字节都不写。
    if opts.dry_run {
        let baselines = load_baselines(&idx_path, &pf.shared, &home);
        let mut learned = Vec::new();
        report.shared = apply_shared(&pf.shared, &home, &baselines, &mut learned, &pf.op_id, true);
        report.packages = package_outcomes(&pf.packages, false);
        report.residues = residue_outcomes(&pf.residues, &home, &pf.op_id, true);
        return Ok(report);
    }

    // ③ 真执行前：任一前置检查未过即中止。detail 里已经写了"卡在哪、怎么解"，
    //    原样带出去。
    if pf_checks_failed > 0 {
        let lines: Vec<String> = report
            .checks
            .iter()
            .filter(|c| !c.passed)
            .map(|c| format!("  [{}] {}", c.name, c.detail))
            .collect();
        bail!(
            "uninstall preflight failed ({} of {} checks); nothing was removed:\n{}\n\
             Re-run with --dry-run to see the full plan alongside these checks.",
            pf_checks_failed,
            report.checks.len(),
            lines.join("\n")
        );
    }

    // ④ 删除目标：计划里真正要动的项，去掉互为子路径的重复
    //    （install 行常常就落在 root 里，删过 root 再删它只会得到 NotFound）。
    let targets = dedup_top_paths(report.plan.actionable().map(|i| i.path.clone()).collect());

    // ⑤ **先把全部目标探一遍锁，再动第一个字节**。逐个探逐个删的话，第三个
    //    目标被占用时前两个已经没了——卸载是全有或全无的语义，不能删一半。
    for t in &targets {
        match lockprobe::ensure_free(t)? {
            LockStatus::Unknown { reason } => report.warnings.push(format!(
                "lock probe inconclusive for {}: {reason}",
                t.display()
            )),
            LockStatus::Free | LockStatus::Locked { .. } => {}
        }
    }

    // ⑥ 逐个删除。removed_bytes 取「删前实测 − 删后实测」，不是计划里的预估：
    //    预估来自索引，而索引可能是几天前那次 scan 的快照。
    for t in &targets {
        let before = measured_bytes(t);
        if let Err(e) = remove_path_except(t, &pf.keep_paths) {
            // 不中断：其余目标该删还得删。删不掉的会在下一步以 leftover 的
            // 形式如实出现，不会被这条 warning 顶替掉。
            report
                .warnings
                .push(format!("failed to remove {}: {e:#}", t.display()));
        }
        let after = measured_bytes(t);
        report.removed_bytes += before.saturating_sub(after);
    }

    // ⑦ 逐条复核。`--keep` 保住的路径与它们的祖先目录不算残留，其余一律算。
    let mut leftovers: Vec<PathBuf> = Vec::new();
    for t in &targets {
        collect_leftovers(t, &pf.keep_paths, &mut leftovers);
    }
    leftovers.sort();
    leftovers.dedup();
    report.leftovers = leftovers.iter().map(|p| p.display().to_string()).collect();
    if !report.leftovers.is_empty() {
        report.warnings.push(format!(
            "uninstall was not clean: {} path(s) survived removal and are listed in `leftovers`",
            report.leftovers.len()
        ));
    }

    // ⑧ 共享文件改键。自己的东西删干净了，才轮到去别人家里摘掉那一个键。
    let baselines = load_baselines(&idx_path, &pf.shared, &home);
    let mut learned: Vec<(String, String)> = Vec::new();
    report.shared = apply_shared(
        &pf.shared,
        &home,
        &baselines,
        &mut learned,
        &pf.op_id,
        false,
    );
    if let Err(e) = store_baselines(&idx_path, &learned) {
        report.warnings.push(format!(
            "the shared-file edits went through, but their new structural fingerprints could not \
             be recorded ({e:#}); the next run will simply learn the shape again"
        ));
    }
    for o in &report.shared {
        if o.action == SharedAction::Refused {
            report.warnings.push(format!(
                "refused to remove `{}` from {}: {}",
                o.key,
                o.path,
                o.detail.as_deref().unwrap_or("no reason recorded")
            ));
        }
    }

    // ⑧b 残留清理。与 shared 同一类动作：别人地盘上的东西，先整文件快照
    //     再原子写。单条失败（快照写不出、库读不了、表或列不存在）只作废
    //     它自己，其余照走——但 `Failed` 会被 [`removal_was_clean`] 看见，
    //     索引不清，agent 继续列在列表里，直到真的删干净。
    report.residues = residue_outcomes(&pf.residues, &home, &pf.op_id, false);
    for r in &report.residues {
        if let ResidueAction::Failed(detail) = &r.action {
            report.warnings.push(format!(
                "could not remove residue {} at {}: {}",
                r.what,
                r.path.display(),
                detail
            ));
        }
    }

    // ⑨ 包管理器。默认只把命令打出来；`--run-package-manager` 才真跑，
    //    而且 argv 原样躺在 `argv` 字段里，渲染层先打印再报输出。
    report.packages = package_outcomes(&pf.packages, opts.run_package_manager);
    for p in &report.packages {
        if p.action == PackageAction::Failed {
            report.warnings.push(format!(
                "`{}` failed: {}",
                p.command,
                p.detail.as_deref().unwrap_or("no detail recorded")
            ));
        }
    }

    // ⑩ 收尾清索引。**必须在这里才开可写句柄**：`Index::open` 抢单实例排他锁，
    //    而前置检查/计划阶段的只读句柄要先 drop 掉（它们都收在 run_preflight
    //    的作用域里，此刻已经释放）。
    //
    //    只有「真的删干净了」才清索引（判定见 [`removal_was_clean`]）：残留
    //    还在时清索引，agent 就会从列表里消失而它其实还活在盘上——列表说了谎。
    //    留着索引行，agent 继续可见，直到它真的被删干净。duster 自己删，
    //    删不掉的如实说一句还列着的原因，而不是叫用户去查。
    if removal_was_clean(&report, &pf.keep_paths) {
        purge_index(&idx_path, &opts.agent)?;
    } else {
        let survived = report.leftovers.len() + pf.keep_paths.len();
        let keep_note = if pf.keep_paths.is_empty() {
            String::new()
        } else {
            format!(" ({} of them kept via --keep)", pf.keep_paths.len())
        };
        report.warnings.push(format!(
            "`{}` is still listed: {survived} path(s) survived the removal{keep_note}; its \
             index rows were kept so the agent stays visible until it is really gone",
            opts.agent
        ));
    }

    report.executed = true;
    Ok(report)
}

// ---------------------------------------------------------------------------
// 前置检查
// ---------------------------------------------------------------------------

/// 前置检查的完整产物。
///
/// [`preflight`] 只向外暴露 `checks`，但 [`uninstall`] 还要用到同一趟里
/// 算出来的计划、归档回执、保留路径与清单声明。分成两层是为了**只跑一趟**：
/// 第二次调用 `preflight` 会把 sessions 再打一个包。
struct Preflight {
    checks: Vec<PreflightCheck>,
    plan: Plan,
    /// `--keep` 指名保留、不参与删除的绝对路径。
    keep_paths: Vec<PathBuf>,
    receipt: Option<ArchiveReceipt>,
    warnings: Vec<String>,
    /// 本次运行的快照批次号。实体化与共享改键共用同一个，
    /// 一次卸载里被改写过的文件全躺在同一个 `<op-id>/` 目录下。
    op_id: String,
    /// 清单 `[uninstall].shared`；`--data-only` 时为空。
    shared: Vec<SharedEdit>,
    /// 清单 `[uninstall].package`；`--data-only` 时为空。
    packages: Vec<PackageHint>,
    /// 清单 `[uninstall].residue`；`--data-only` 时为空。
    residues: Vec<ResidueSpec>,
}

fn run_preflight(opts: &UninstallOptions) -> Result<Preflight> {
    let home = resolve_home(opts)?;
    let idx_path = resolve_index_path(opts, &home);

    let plan_opts = crate::plan::PlanOptions {
        index_path: Some(idx_path.clone()),
        home: Some(home.clone()),
        agents: vec![opts.agent.clone()],
        older_than_days: None,
        keep_generations: false,
        now_ms: None,
    };
    let plan = crate::plan::plan_uninstall(&plan_opts, &opts.agent)?;

    // "A 的地盘" = 计划里真正要动的顶层路径。用它做包含判定，
    // 后面三项检查的"里/外"口径才是同一个。
    let roots = dedup_top_paths(plan.actionable().map(|i| i.path.clone()).collect());

    // shared / package / residue 三个模式的全部输入都在清单的
    // `[uninstall]` 段里。没有这一段就没有这些模式——duster 不去猜
    // 别人家的配置里哪个键是它的。
    let section = load_uninstall_section(&home, &opts.agent)?;
    let (shared, packages, residues) = match (&section, opts.data_only) {
        (Some(s), false) => (s.shared.clone(), s.package.clone(), s.residue.clone()),
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };

    let mut warnings: Vec<String> = Vec::new();
    // `--data-only` 跳过的东西必须说出来。用户以为卸干净了，而别人的配置里
    // 还留着一条指向已删程序的声明——这正是「卸载不干净」的现代形态。
    if opts.data_only
        && let Some(s) = &section
        && !(s.shared.is_empty() && s.package.is_empty() && s.residue.is_empty())
    {
        warnings.push(format!(
            "--data-only: skipped {} shared-file edit(s), {} package hint(s) and {} residue(s) \
             that the manifest declares for `{}`. Re-run without --data-only to also remove its \
             keys from other agents' config files, see how it was installed, and clean up the \
             residue it left behind.",
            s.shared.len(),
            s.package.len(),
            s.residue.len(),
            opts.agent
        ));
    }

    // 实体化会**原地改写别的 agent 的文件**。用户确认的是"卸载 A"，
    // 可能失去的却是 B 的那份副本——这正是 snapshot 存在的那类失败模式，
    // 确认清单里看不见它，所以它无条件执行、也不问。
    let op_id = duster_fs::snapshot::new_op_id(std::time::SystemTime::now());
    let c1 = check_cross_reference(
        &home,
        &opts.agent,
        &roots,
        opts.dry_run,
        &op_id,
        &mut warnings,
    )?;
    let (c2, receipt, keep_paths) = check_data_escape(opts, &home, &idx_path, &roots)?;
    let c3 = check_credentials(&home, &roots, &keep_paths)?;

    Ok(Preflight {
        checks: vec![c1, c2, c3],
        plan,
        keep_paths,
        receipt,
        warnings,
        op_id,
        shared,
        packages,
        residues,
    })
}

/// 取本 agent 清单里的 `[uninstall]` 段。
///
/// agent 未知这件事 [`crate::plan::plan_uninstall`] 已经报过错了（它先跑），
/// 走到这里还找不到只可能是清单目录在两次读取之间被人动了。那时按
/// 「没有这一段」处理：退回只删 owns，比在删除中途炸掉强。
fn load_uninstall_section(home: &Path, agent: &str) -> Result<Option<manifest::UninstallSection>> {
    let dir = home.join(".agent-duster").join("adapters");
    Ok(manifest::load_all(Some(&dir))?
        .into_iter()
        .find(|m| m.agent.id == agent)
        .and_then(|m| m.uninstall))
}

// ---------------------------------------------------------------------------
// shared：别人家文件里的那一个键
// ---------------------------------------------------------------------------

/// 共享配置文件的读取上限。超过这个尺寸的多半不是配置文件。
/// 与 [`crate::mcp`] 用的是同一个数。
const MAX_SHARED_BYTES: u64 = 8 * 1024 * 1024;

/// 键在文件里的定位方式。清单层保证二选一，这里只是把它变成类型。
enum Located {
    Json(String),
    Toml(String),
}

impl Located {
    /// 给用户看的那一串。JSON 指针与 TOML 点路径的写法本来就互不混淆，
    /// 不必再加前缀说明它是哪一种。
    fn shown(&self) -> &str {
        match self {
            Self::Json(s) | Self::Toml(s) => s,
        }
    }
}

/// 闸门基准在 `meta` 表里的键。
///
/// **按文件路径记，不按 agent 记**：结构指纹是这份文件的属性，而共享文件
/// 恰恰没有唯一的归属 agent（这就是它"共享"的含义）。按路径记还带来一个
/// 实打实的好处：卸载 A 时学到的形状，卸载 B 动同一个文件时就能用上。
fn guard_slot(path: &Path) -> String {
    format!("guard:shared:{}", path.display())
}

/// 一次把这批共享文件的闸门基准读出来。
///
/// 不让 [`apply_shared`] 边走文件 IO 边攥着索引连接：`Index::open` 是单实例
/// 排他锁，攥着它做几十次读写只会让并发的 duster 干等。索引开不了就当没有
/// 基准——那等价于 guard 自己定义的"首次见到"，是它的放行态。
fn load_baselines(idx_path: &Path, edits: &[SharedEdit], home: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if edits.is_empty() {
        return out;
    }
    let Ok(idx) = Index::open_readonly(idx_path) else {
        return out;
    };
    for e in edits {
        let slot = guard_slot(&expand_home(home, &e.path));
        if let Ok(Some(v)) = meta::get(idx.conn(), &slot) {
            out.insert(slot, v);
        }
    }
    out
}

/// 把新学到的形状写回 `meta`。
///
/// 只在真跑之后调用。dry-run 一个基准都不落——否则一次预览就把"第一次见到的
/// 形状"定死了，等于用预览替用户做了决定。
fn store_baselines(idx_path: &Path, pairs: &[(String, String)]) -> Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    let idx = Index::open(idx_path)
        .with_context(|| format!("failed to open index: {}", idx_path.display()))?;
    for (k, v) in pairs {
        meta::set(idx.conn(), k, v)?;
    }
    Ok(())
}

/// 逐条处理清单声明的共享改键。
///
/// `dry_run` 为真时全程只读：照样读文件、比指纹、算出删键之后的新文本，
/// 但不写盘、不落基准。预览与真跑因此走的是同一段判断，预览说会删的，
/// 真跑就会删。
fn apply_shared(
    edits: &[SharedEdit],
    home: &Path,
    baselines: &BTreeMap<String, String>,
    learned: &mut Vec<(String, String)>,
    op_id: &str,
    dry_run: bool,
) -> Vec<SharedEditOutcome> {
    edits
        .iter()
        .map(|e| apply_one_shared(e, home, baselines, learned, op_id, dry_run))
        .collect()
}

fn apply_one_shared(
    e: &SharedEdit,
    home: &Path,
    baselines: &BTreeMap<String, String>,
    learned: &mut Vec<(String, String)>,
    op_id: &str,
    dry_run: bool,
) -> SharedEditOutcome {
    let path = expand_home(home, &e.path);
    let shown = path.display().to_string();
    let mk = |key: &str, action: SharedAction, detail: Option<String>| SharedEditOutcome {
        path: shown.clone(),
        key: key.to_string(),
        reason: e.reason.clone(),
        action,
        detail,
        snapshot: None,
    };

    // 清单校验已经保证二选一。仍然把两个坏情况写全：`SharedEdit` 是个公开
    // 结构体，不是只能由清单造出来的。
    let key = match (e.json_pointer.as_deref(), e.toml_key.as_deref()) {
        (Some(p), None) => Located::Json(p.to_string()),
        (None, Some(k)) => Located::Toml(k.to_string()),
        (Some(p), Some(k)) => {
            return mk(
                p,
                SharedAction::Refused,
                Some(format!(
                    "this entry declares both json_pointer `{p}` and toml_key `{k}`; \
                     exactly one of them may locate the key"
                )),
            );
        }
        (None, None) => {
            return mk(
                "(none)",
                SharedAction::Refused,
                Some(
                    "this entry declares neither json_pointer nor toml_key, so there is no key \
                     to remove"
                        .to_string(),
                ),
            );
        }
    };
    let shown_key = key.shown().to_string();

    if !path.is_file() {
        return mk(
            &shown_key,
            SharedAction::Missing,
            Some("this file is not on disk, so there is nothing to remove from it".to_string()),
        );
    }

    let doc = match codec::read_file(&path) {
        Ok(d) => d,
        Err(err) => {
            return mk(
                &shown_key,
                SharedAction::Refused,
                Some(format!(
                    "cannot parse this file, so duster will not rewrite it: {err:#}"
                )),
            );
        }
    };

    // 闸门在最前面：形状变了就连"这个键还是不是我们以为的那个键"都不该再
    // 下结论。降级只读，说出来，卸载的其余部分照走。
    let slot = guard_slot(&path);
    let verdict = guard::check(
        &doc,
        || Ok(baselines.get(&slot).cloned()),
        |fp| {
            learned.push((slot.clone(), fp.to_string()));
            Ok(())
        },
    );
    match verdict {
        Ok(guard::Verdict::Drifted {
            expected,
            actual,
            reason,
        }) => {
            return mk(
                &shown_key,
                SharedAction::Refused,
                Some(format!(
                    "the structure of this file drifted since duster last read it, so the key may \
                     no longer mean what it did: {reason} (fingerprint {expected} -> {actual}). \
                     Remove the entry by hand, or re-run after `duster scan` has seen the new shape"
                )),
            );
        }
        Ok(_) => {}
        Err(err) => {
            return mk(
                &shown_key,
                SharedAction::Refused,
                Some(format!("schema guard could not run: {err:#}")),
            );
        }
    }

    // 原文要另读一遍：codec 的写入口是文本进文本出（它不碰文件系统，正是为了
    // 让"闸门 → 快照 → 落盘"这个次序由调用方强制执行），而上面那份 Doc 是
    // 解析后的树，回不到原文的注释与键序。
    let src = match codec::read_to_string_capped(&path, MAX_SHARED_BYTES) {
        Ok(t) => t,
        Err(err) => {
            return mk(
                &shown_key,
                SharedAction::Refused,
                Some(format!("cannot read this file: {err:#}")),
            );
        }
    };
    let rewritten = match &key {
        Located::Json(ptr) => codec::remove_json_pointer(&src, ptr),
        Located::Toml(dotted) => codec::remove_toml_path(&src, dotted),
    };
    let new_text = match rewritten {
        Ok(t) => t,
        Err(err) => {
            return mk(
                &shown_key,
                SharedAction::Refused,
                Some(format!("cannot remove this key: {err:#}")),
            );
        }
    };

    // 两个写入口在"路径本来就不存在"时逐字节原样返回输入。所以文本没变就是
    // 键本来就不在——卸载第二遍不该报错。
    if new_text == src {
        return mk(
            &shown_key,
            SharedAction::Absent,
            Some("this key is not in the file (already removed); nothing to do".to_string()),
        );
    }

    if dry_run {
        return mk(&shown_key, SharedAction::Planned, None);
    }

    // 快照失败即放弃这一条：写不出退路就不动手。别人的文件更是如此——
    // 用户同意的是"卸载 A"，写坏了丢的却是 B 手里那份。
    let root = home.join(".agent-duster").join("snapshots");
    let receipt = match duster_fs::snapshot::snapshot_file(&root, op_id, &path, home) {
        Ok(r) => r,
        Err(err) => {
            return mk(
                &shown_key,
                SharedAction::Refused,
                Some(format!(
                    "refusing to rewrite it without a snapshot: {err:#}"
                )),
            );
        }
    };
    // GC 放在留完快照之后：先保住新的，再回收旧的。
    let _ = duster_fs::snapshot::prune_old(&root, duster_fs::snapshot::KEEP);
    let snapshot = Some(receipt.path.display().to_string());

    if let Err(err) = duster_fs::atomic::write_atomic(&path, new_text.as_bytes()) {
        let mut out = mk(
            &shown_key,
            SharedAction::Refused,
            // 快照路径照样带上：写坏了的话，用户第一句话就是"原来那份在哪"。
            Some(format!("failed to write it back: {err:#}")),
        );
        out.snapshot = snapshot;
        return out;
    }

    // 把基准换成我们刚写出来的形状。少了这一步，duster 的保守回写会绊倒它
    // 自己：注册表从两条声明掉到一条，结构指纹本来就会变，下一次动同一个
    // 文件的人会把我们自己删的那一笔当成别人动过手脚。
    if let Ok(d) = codec::read_file(&path) {
        learned.push((slot, guard::fingerprint(&d)));
    }

    let mut out = mk(&shown_key, SharedAction::Removed, None);
    out.snapshot = snapshot;
    out
}

// ---------------------------------------------------------------------------
// package：打印，不代跑
// ---------------------------------------------------------------------------

/// 逐条处理清单声明的包管理器提示。
///
/// `run` 只有在用户显式给了 `--run-package-manager` 且不是 dry-run 时才为真。
/// 无论真假，`detect` 都可能被执行——它是只读的探测，存在的意义就是让报告
/// 能说出"这个包其实早就不在了"。`command` 在 `run` 为假时**永不执行**。
fn package_outcomes(hints: &[PackageHint], run: bool) -> Vec<PackageOutcome> {
    hints.iter().map(|h| package_one(h, run)).collect()
}

fn package_one(h: &PackageHint, run: bool) -> PackageOutcome {
    let mut notes: Vec<String> = Vec::new();
    let mut out = PackageOutcome {
        manager: h.manager.as_str().to_string(),
        command: shell_join(&h.command),
        argv: h.command.clone(),
        detected: DetectStatus::Unknown,
        action: PackageAction::Printed,
        detail: None,
        output: None,
    };

    if h.command.is_empty() {
        out.action = PackageAction::Skipped;
        out.detail =
            Some("the manifest declares no uninstall command for this package manager".to_string());
        return out;
    }

    out.detected = if h.detect.is_empty() {
        notes.push(
            "the manifest declares no detect command, so duster cannot tell whether this package \
             is still installed"
                .to_string(),
        );
        DetectStatus::Unknown
    } else {
        match Command::new(&h.detect[0]).args(&h.detect[1..]).output() {
            Ok(o) if o.status.success() => DetectStatus::Installed,
            Ok(_) => DetectStatus::NotInstalled,
            Err(err) => {
                notes.push(format!(
                    "could not run `{}` to check whether it is installed: {err}",
                    shell_join(&h.detect)
                ));
                DetectStatus::Unknown
            }
        }
    };

    if out.detected == DetectStatus::NotInstalled {
        out.action = PackageAction::Skipped;
        notes.push("it is not installed, so there is nothing to uninstall".to_string());
        out.detail = Some(notes.join("; "));
        return out;
    }

    if !run {
        out.action = PackageAction::Printed;
        notes.push(
            "duster never runs a package manager for you: copy the command above and run it \
             yourself, or re-run with --run-package-manager"
                .to_string(),
        );
        out.detail = Some(notes.join("; "));
        return out;
    }

    // 输出是捕获而不是继承的。继承确实能做到"边跑边看"，但它会把包管理器的
    // 每一行都灌进 stdout，而 `--json` 的约定是 stdout 上**只有一行**信封。
    // 捕获下来交给渲染层，人类模式照样逐行打，JSON 模式则老老实实进字段。
    match Command::new(&h.command[0]).args(&h.command[1..]).output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            out.output = (!text.trim().is_empty()).then_some(text);
            if o.status.success() {
                out.action = PackageAction::Executed;
            } else {
                out.action = PackageAction::Failed;
                notes.push(match o.status.code() {
                    Some(c) => format!("exited with status {c}"),
                    None => "killed by a signal".to_string(),
                });
            }
        }
        Err(err) => {
            out.action = PackageAction::Failed;
            notes.push(format!("could not run it: {err}"));
        }
    }
    out.detail = (!notes.is_empty()).then(|| notes.join("; "));
    out
}

/// 把 argv 拼成一行**可以原样粘回终端**的命令。
///
/// 这一行是这个模式的全部产出，所以引号必须是对的：带空格或 shell 元字符的
/// 参数套单引号，参数里自带的单引号按 `'\''` 转义。拼错了用户粘过去会跑出
/// 另一件事，那比不打印更糟。
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let safe = !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c));
            if safe {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// residue：声明化的已知残留
// ---------------------------------------------------------------------------

/// 逐条处理清单声明的残留（`[[uninstall.residue]]`）。
///
/// 与 `shared` / `package` 同级，受同一个 `--data-only` 开关管辖：数据模式
/// 只动 agent 独占的树，别人的文件与别人的库一行都不碰。
///
/// `dry_run` 为真时全程只读：shell 行照读文件、sqlite 行只读查库，
/// 算出「真跑会删什么」，但不建快照、不写盘、不动 db。预览与真跑走
/// 同一段判断——预览说会删的，真跑就会删。
fn residue_outcomes(
    specs: &[ResidueSpec],
    home: &Path,
    op_id: &str,
    dry_run: bool,
) -> Vec<ResidueOutcome> {
    specs
        .iter()
        .flat_map(|s| match s {
            ResidueSpec::ShellLine {
                files,
                match_,
                with_comment_above,
                why,
            } => files
                .iter()
                .map(|f| {
                    shell_line_one(
                        home,
                        f,
                        match_,
                        *with_comment_above,
                        why,
                        op_id,
                        dry_run,
                    )
                })
                .collect(),
            ResidueSpec::SqliteRow {
                db,
                table,
                column,
                equals,
                why,
            } => vec![sqlite_row_one(home, db, table, column, equals, why, op_id, dry_run)],
        })
        .collect()
}

/// shell_line 单文件处理。一个文件一条结果。
///
/// 逐字节规矩：命中行整行删（行终止符跟着它一起走），未命中的行
/// **原样保留**——包括行尾空白与最后一行有没有换行，一个字节都不改。
/// 这靠按字节切行、按字节拼回实现：`split_inclusive('\n')` 让每一行都
/// 带着自己的终止符，拼回时只丢掉 `drop` 表里标中的那些。
fn shell_line_one(
    home: &Path,
    raw: &str,
    match_: &str,
    with_comment_above: bool,
    why: &str,
    op_id: &str,
    dry_run: bool,
) -> ResidueOutcome {
    let path = expand_home(home, raw);
    let what = if with_comment_above {
        format!("shell line containing `{match_}` (and the comment directly above it, if any)")
    } else {
        format!("shell line containing `{match_}`")
    };
    let mk = |action: ResidueAction| ResidueOutcome {
        what: what.clone(),
        path: path.clone(),
        why: why.to_string(),
        action,
    };

    // `files` 里不存在的文件跳过：没有它就没有可删的行，这不是错误。
    if !path.is_file() {
        return mk(ResidueAction::NotFound);
    }
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => return mk(ResidueAction::Failed(format!("cannot read this file: {e:#}"))),
    };
    if bytes.len() as u64 > MAX_SHARED_BYTES {
        return mk(ResidueAction::Failed(format!(
            "file is larger than the {} byte config-file ceiling",
            MAX_SHARED_BYTES
        )));
    }
    let needle = match_.as_bytes();
    if needle.is_empty() {
        return mk(ResidueAction::Failed("match pattern is empty".to_string()));
    }

    // 按字节切行。每一行自带终止符（`\r\n` 或 `\n`），最后一行可以没有。
    let lines: Vec<&[u8]> = bytes.split_inclusive(|&b| b == b'\n').collect();
    let mut drop = vec![false; lines.len()];
    let mut hits = 0usize;
    for (i, line) in lines.iter().enumerate() {
        // 子串包含匹配整行（含终止符在内都不影响路径类子串的命中）。
        if line.windows(needle.len()).any(|w| w == needle) {
            drop[i] = true;
            hits += 1;
            // 命中行紧邻的上一行若去空白后以 `#` 开头，是安装脚本留在
            // 上面的注释，一并删。去空白用 trim_ascii：`\r` 也算空白，
            // CRLF 文件的注释行同样认得出。
            if with_comment_above && i > 0 && lines[i - 1].trim_ascii().starts_with(b"#") {
                drop[i - 1] = true;
            }
        }
    }
    if hits == 0 {
        return mk(ResidueAction::NotFound);
    }
    if dry_run {
        return mk(ResidueAction::Planned);
    }

    // 删前整文件快照。写不出退路就不动手：快照失败这一条作废，
    // 那个文件一个字节都不动（调用方把原因记进 warnings）。
    if let Err(e) = snapshot_before_rewrite(&path, home, op_id) {
        return mk(ResidueAction::Failed(format!("{e:#}")));
    }
    let kept: Vec<&[u8]> = lines
        .iter()
        .zip(&drop)
        .filter(|(_, d)| !**d)
        .map(|(l, _)| *l)
        .collect();
    if let Err(e) = duster_fs::atomic::write_atomic(&path, &kept.concat()) {
        return mk(ResidueAction::Failed(format!("{e:#}")));
    }
    mk(ResidueAction::Removed)
}

/// sqlite_row 单条处理。
///
/// 顺序是刻意的：**先只读问「这一行还在吗」，不在就 NotFound 了事**——
/// 快照是写的前提，不是报告的前提，为一场根本没发生的删除建快照是浪费。
/// 问完再快照再删；检查与删除之间若被别人抢先删掉（0 行受影响），
/// 如实报 NotFound，不报错。
fn sqlite_row_one(
    home: &Path,
    raw_db: &str,
    table: &str,
    column: &str,
    equals: &str,
    why: &str,
    op_id: &str,
    dry_run: bool,
) -> ResidueOutcome {
    let path = expand_home(home, raw_db);
    let what = format!("row `{equals}` in `{table}`.`{column}`");
    let mk = |action: ResidueAction| ResidueOutcome {
        what: what.clone(),
        path: path.clone(),
        why: why.to_string(),
        action,
    };

    // 库不在 = 残留本来就不在。这不是错误，卸载两遍不该报错。
    if !path.is_file() {
        return mk(ResidueAction::NotFound);
    }
    let count = match duster_index::foreign::count_rows(&path, table, column, equals) {
        Ok(Some(n)) => n,
        // 库读不到（不是 SQLite、被锁、损坏、加密）≠ 本来就没了。
        Ok(None) => {
            return mk(ResidueAction::Failed(format!(
                "cannot read {}: not a SQLite database, or it is locked, corrupt or encrypted",
                path.display()
            )));
        }
        // 表/列不存在、标识符不合法——清单声明跟库的实际形状对不上。
        Err(e) => return mk(ResidueAction::Failed(format!("{e:#}"))),
    };
    if count == 0 {
        return mk(ResidueAction::NotFound);
    }
    if dry_run {
        return mk(ResidueAction::Planned);
    }

    // 删前整文件快照。写不出退路就不动手。
    if let Err(e) = snapshot_before_rewrite(&path, home, op_id) {
        return mk(ResidueAction::Failed(format!("{e:#}")));
    }
    match duster_index::foreign::delete_row(&path, table, column, equals) {
        Ok(n) if n > 0 => mk(ResidueAction::Removed),
        Ok(_) => mk(ResidueAction::NotFound),
        Err(e) => mk(ResidueAction::Failed(format!("{e:#}"))),
    }
}

/// 检查一：交叉引用。卸载 A 不得打断 B。
///
/// # 三类引用，危险程度并不相同
///
/// - **软链**（B 的目录里有个链接指进 A）：删掉 A 就是断链，B 真的坏掉。
///   必须实体化，实体化失败即不可解，检查失败。
/// - **硬链**（B 的文件与 A 的文件共用 inode，`duster skill link` 的 CAS 就是
///   这个形态）：POSIX 语义下删掉 A 那一条链接**不会**销毁内容，B 侧照常可读。
///   所以它其实不构成损坏。仍然实体化，是为了让 B 手里那份变成真正独立的副本
///   （链接计数归位、后续谁都别再受谁影响）；定位不到外部那一份时退化为提示，
///   不判失败——为一个不会造成数据丢失的情况阻断整个卸载不成比例。
/// - **cc-switch 备份**：那是独立副本，本来就不受影响，只报告，不处理。
///
/// # 判定方式
///
/// 硬链不做全盘搜索：走一遍 A 的树，按 `(dev, ino)` 归组，
/// 某组的 `st_nlink` 大于组内路径数，就说明还有链接在 A 之外。
/// 外部遍历只用来**定位**那一份在哪，范围限于 CAS、cc-switch 与其他
/// agent 的清单声明路径。
fn check_cross_reference(
    home: &Path,
    agent: &str,
    roots: &[PathBuf],
    dry_run: bool,
    op_id: &str,
    warnings: &mut Vec<String>,
) -> Result<PreflightCheck> {
    // A 侧：收集多链接文件的 (dev, ino) → 路径。
    let mut inside: BTreeMap<(u64, u64), Vec<PathBuf>> = BTreeMap::new();
    let mut nlink_of: BTreeMap<(u64, u64), u64> = BTreeMap::new();
    for root in roots {
        let Ok(meta) = fs::symlink_metadata(root) else {
            continue;
        };
        if meta.is_dir() {
            walk::walk_files(root, &WalkOptions::default(), |p, m| {
                if m.is_file() && m.nlink() > 1 {
                    inside
                        .entry((m.dev(), m.ino()))
                        .or_default()
                        .push(p.to_path_buf());
                    nlink_of.insert((m.dev(), m.ino()), m.nlink());
                }
            })?;
        } else if meta.is_file() && meta.nlink() > 1 {
            inside
                .entry((meta.dev(), meta.ino()))
                .or_default()
                .push(root.clone());
            nlink_of.insert((meta.dev(), meta.ino()), meta.nlink());
        }
    }
    // 只留下"还有链接在 A 之外"的那些组。
    inside.retain(|k, paths| nlink_of.get(k).copied().unwrap_or(1) > paths.len() as u64);

    let search_roots = outside_search_roots(home, agent, roots)?;

    // 外部一趟遍历同时干两件事：按 inode 建索引、挑出指进 A 的软链。
    let mut outside_by_inode: BTreeMap<(u64, u64), Vec<PathBuf>> = BTreeMap::new();
    let mut dangling_symlinks: Vec<PathBuf> = Vec::new();
    let walk_opts = WalkOptions {
        follow_links: false,
        prune_dirs: OUTSIDE_PRUNE_DIRS.iter().map(|s| s.to_string()).collect(),
    };
    let mut visit = |p: &Path, m: &fs::Metadata| {
        if m.is_symlink() {
            if let Ok(target) = fs::canonicalize(p)
                && is_under_any(&target, roots)
            {
                dangling_symlinks.push(p.to_path_buf());
            }
        } else if m.is_file() && m.nlink() > 1 && inside.contains_key(&(m.dev(), m.ino())) {
            outside_by_inode
                .entry((m.dev(), m.ino()))
                .or_default()
                .push(p.to_path_buf());
        }
    };
    for sr in &search_roots {
        let Ok(meta) = fs::symlink_metadata(sr) else {
            continue;
        };
        if meta.is_dir() {
            walk::walk_files(sr, &walk_opts, &mut visit)?;
        } else {
            visit(sr, &meta);
        }
    }

    let mut resolved: Vec<String> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // 软链：必须实体化，失败即不可解。
    dangling_symlinks.sort();
    for link in &dangling_symlinks {
        if dry_run {
            resolved.push(format!(
                "symlink {} resolves into {agent}; would be materialised into an independent copy",
                link.display()
            ));
            continue;
        }
        match materialise_symlink(link, home, op_id) {
            Ok(bytes) => resolved.push(format!(
                "symlink {} → materialised into an independent copy ({bytes} bytes)",
                link.display()
            )),
            Err(e) => unresolved.push(format!(
                "symlink {} resolves into {agent} and could not be materialised: {e:#}. \
                 Copy or repoint it yourself, then re-run",
                link.display()
            )),
        }
    }

    // 硬链：能定位就实体化，定位不到只提示。
    for (key, in_paths) in &inside {
        match outside_by_inode.get(key) {
            Some(out_paths) => {
                for out in out_paths {
                    if dry_run {
                        resolved.push(format!(
                            "hard link {} shares an inode with {}; would be materialised",
                            out.display(),
                            in_paths[0].display()
                        ));
                        continue;
                    }
                    match materialise_hardlink(out, home, op_id) {
                        Ok(bytes) => resolved.push(format!(
                            "hard link {} (shared with {}) → materialised into an independent \
                             copy ({bytes} bytes)",
                            out.display(),
                            in_paths[0].display()
                        )),
                        Err(e) => unresolved.push(format!(
                            "hard link {} shares an inode with {} and could not be \
                             materialised: {e:#}",
                            out.display(),
                            in_paths[0].display()
                        )),
                    }
                }
            }
            None => notes.push(format!(
                "{} has {} link(s) outside {agent} that could not be located in the CAS store, \
                 ~/.cc-switch or other agents' declared paths — content survives the deletion \
                 either way (hard links keep the inode alive), so this is informational",
                in_paths[0].display(),
                nlink_of.get(key).copied().unwrap_or(1) - in_paths.len() as u64
            )),
        }
    }

    // cc-switch 的配置备份：独立副本，不受卸载影响，只报告。
    for p in cc_switch_backups(home, agent) {
        notes.push(format!(
            "~/.cc-switch holds a backup copy of {agent}'s config at {} — it is an independent \
             file and survives the uninstall untouched",
            p.display()
        ));
    }

    if search_roots.is_empty() && !inside.is_empty() {
        warnings.push(
            "no outside search roots exist on this machine; cross-reference detection could only \
             use link counts"
                .to_string(),
        );
    }

    let passed = unresolved.is_empty();
    let detail = if !passed {
        format!(
            "{} unresolvable cross-reference(s); removing {agent} would break another agent:\n{}",
            unresolved.len(),
            bullet(&unresolved)
        )
    } else if resolved.is_empty() && notes.is_empty() {
        format!(
            "no other agent references {agent}'s files (hard links, symlinks, cc-switch backups all clear)"
        )
    } else {
        let mut s = format!(
            "{} reference(s) materialised, {} informational:",
            resolved.len(),
            notes.len()
        );
        if !resolved.is_empty() {
            s.push('\n');
            s.push_str(&bullet(&resolved));
        }
        if !notes.is_empty() {
            s.push('\n');
            s.push_str(&bullet(&notes));
        }
        s
    };

    Ok(PreflightCheck {
        name: "cross-reference".to_string(),
        passed,
        detail,
    })
}

/// 检查二：数据逃生。sessions / memory 是不可再生的，必须有出路。
///
/// 出路只有两条，缺一即失败：`--export-first` 打包带走，或 `--keep` 留在原地。
/// 第三条"什么都不做直接删"不存在——那不是选项，那是事故。
///
/// 归档阈值：预估超过 [`archive::AUTO_ARCHIVE_LIMIT`] 而用户没表态时拒绝
/// 执行。prune 已撤掉这道门（恒归档、超阈值只把体积报出来），uninstall
/// 仍保留——卸载是一次性的，没有「先出计划再确认」的流程兜底。
/// 本机 codex 是 sessions 352 MB + archived_sessions 242 MB，替他决定
/// 打不打包都是错的。
fn check_data_escape(
    opts: &UninstallOptions,
    home: &Path,
    idx_path: &Path,
    roots: &[PathBuf],
) -> Result<(PreflightCheck, Option<ArchiveReceipt>, Vec<PathBuf>)> {
    let keep: Vec<&'static str> = opts.keep.iter().filter_map(|k| normalize_keep(k)).collect();
    let unknown: Vec<&String> = opts
        .keep
        .iter()
        .filter(|k| normalize_keep(k).is_none())
        .collect();

    // 索引里该 agent 的 session / memory 行就是"不可再生数据在哪"的唯一口径。
    // 只读打开：写锁留给收尾阶段的 purge_index。
    let mut by_kind: BTreeMap<&'static str, Vec<PathBuf>> = BTreeMap::new();
    {
        let idx = Index::open_readonly(idx_path)
            .with_context(|| format!("failed to open index read-only: {}", idx_path.display()))?;
        let filter = ResourceFilter {
            agents: vec![opts.agent.clone()],
            kinds: KEEPABLE_KINDS.iter().map(|k| k.to_string()).collect(),
            clean_levels: Vec::new(),
        };
        for r in query::list_resources(idx.conn(), &filter)? {
            let Some(kind) = normalize_keep(&r.kind) else {
                continue;
            };
            let p = PathBuf::from(&r.path);
            // 只关心真的要被删掉的那些：不在删除范围内的行与本次卸载无关。
            if fs::symlink_metadata(&p).is_ok() && is_under_any(&p, roots) {
                by_kind.entry(kind).or_default().push(p);
            }
        }
    }

    let mut keep_paths: Vec<PathBuf> = Vec::new();
    let mut to_export: Vec<PathBuf> = Vec::new();
    let mut kept_kinds: Vec<&str> = Vec::new();
    let mut exposed_kinds: Vec<&str> = Vec::new();
    // 按 KEEPABLE_KINDS 的顺序走而不是 by_kind 的字典序：提示里给出的
    // `--keep sessions,memory` 要和文档、CLI 帮助里的写法一模一样。
    for kind in KEEPABLE_KINDS {
        let Some(paths) = by_kind.get(kind) else {
            continue;
        };
        if keep.contains(&kind) {
            keep_paths.extend(paths.iter().cloned());
            kept_kinds.push(kind);
        } else {
            to_export.extend(paths.iter().cloned());
            exposed_kinds.push(kind);
        }
    }
    keep_paths.sort();
    keep_paths.dedup();
    to_export.sort();
    to_export.dedup();

    let mut detail = String::new();
    if !unknown.is_empty() {
        detail.push_str(&format!(
            "ignored unknown --keep value(s) {:?} (only `sessions` and `memory` can be kept); ",
            unknown
        ));
    }
    if !kept_kinds.is_empty() {
        detail.push_str(&format!(
            "keeping {} in place ({} path(s) excluded from the deletion plan); ",
            kept_kinds.join(" + "),
            keep_paths.len()
        ));
    }

    let mk = |passed: bool, detail: String| PreflightCheck {
        name: "data-escape".to_string(),
        passed,
        detail,
    };

    if to_export.is_empty() {
        detail.push_str("no irreplaceable session or memory data is in the deletion plan");
        return Ok((mk(true, detail), None, keep_paths));
    }

    if !opts.export_first {
        detail.push_str(&format!(
            "{} is irreplaceable and would be deleted with no way back. Two ways out: \
             `--export-first` packs it to {} before deleting, or `--keep {}` leaves it on disk",
            exposed_kinds.join(" + "),
            export_dir(opts).display(),
            exposed_kinds
                .iter()
                .map(|k| keep_token(k).to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
        return Ok((mk(false, detail), None, keep_paths));
    }

    let estimate = archive::estimate_bytes(&to_export)?;

    // 显式 --no-archive：用户自己表了态，放行，但把话说到最难听。
    if opts.archive == Some(false) {
        detail.push_str(&format!(
            "--no-archive: {} ({}) will be permanently deleted with no export. \
             This is unrecoverable",
            exposed_kinds.join(" + "),
            human_bytes(estimate)
        ));
        return Ok((mk(true, detail), None, keep_paths));
    }

    if estimate > archive::AUTO_ARCHIVE_LIMIT && opts.archive.is_none() {
        detail.push_str(&format!(
            "{} of {} to pack, above the {} auto-archive limit. duster will not decide this for \
             you: pass `--archive` to pack it anyway, or `--no-archive` to accept losing it",
            human_bytes(estimate),
            exposed_kinds.join(" + "),
            human_bytes(archive::AUTO_ARCHIVE_LIMIT)
        ));
        return Ok((mk(false, detail), None, keep_paths));
    }

    if opts.dry_run {
        detail.push_str(&format!(
            "dry-run: nothing was packed. A real run would pack {} of {} into {}",
            human_bytes(estimate),
            exposed_kinds.join(" + "),
            export_dir(opts).display()
        ));
        return Ok((mk(true, detail), None, keep_paths));
    }

    let out_dir = export_dir(opts);
    let receipt = archive::archive_paths(
        &format!("uninstall-{}", opts.agent),
        &to_export,
        &out_dir,
        home,
    )
    .with_context(|| {
        format!(
            "failed to pack {}'s sessions/memory before deleting them; nothing was removed",
            opts.agent
        )
    })?;
    detail.push_str(&format!(
        "packed {} entries ({} → {}) to {}",
        receipt.entries,
        human_bytes(receipt.source_bytes),
        human_bytes(receipt.bytes),
        receipt.path.display()
    ));
    Ok((mk(true, detail), Some(receipt), keep_paths))
}

/// 检查三：凭据。**只删本地文件不撤销 token 等于没卸干净。**
///
/// 这一项**有命中也算通过**——它是提醒，不是闸门。但 N > 0 时 `detail`
/// 绝不能是空的：报出来的价值全在那句"去哪撤销"上。
fn check_credentials(
    home: &Path,
    roots: &[PathBuf],
    keep_paths: &[PathBuf],
) -> Result<PreflightCheck> {
    let mk = |detail: String| PreflightCheck {
        name: "credentials".to_string(),
        // 恒为 true：见函数文档。
        passed: true,
        detail,
    };

    // roots 为空时不能往下走：scan_secrets 的空 roots 语义是"扫已知位置"，
    // 那会把别的 agent 的凭据算到这次删除头上。
    if roots.is_empty() {
        return Ok(mk("nothing to delete, no credentials scanned".to_string()));
    }

    let report = crate::doctor::scan_secrets(Some(home), roots)?;
    let hits: Vec<_> = report
        .hits
        .into_iter()
        .filter(|h| !is_under_any(Path::new(&h.path), keep_paths))
        .collect();

    if hits.is_empty() {
        return Ok(mk(format!(
            "scanned {} file(s) in the deletion plan, no plaintext credentials found",
            report.scanned_files
        )));
    }

    let lines: Vec<String> = hits
        .iter()
        .map(|h| {
            let key = if h.key.is_empty() {
                String::new()
            } else {
                format!("{} = ", h.key)
            };
            format!(
                "{}:{} [{}] {key}{} → {}",
                h.path, h.line, h.kind, h.masked, h.advice
            )
        })
        .collect();
    Ok(mk(format!(
        "{} plaintext credential(s) sit in the files this run deletes. Revoke the tokens at the platform — deleting the local file does not revoke anything:\n{}",
        hits.len(),
        bullet(&lines)
    )))
}

// ---------------------------------------------------------------------------
// 实体化
// ---------------------------------------------------------------------------

/// 把软链换成独立副本：读出链接指向的内容，原子写回链接**自己所在的路径**。
///
/// `write_atomic` 是同目录 temp + rename，rename 不跟随符号链接，
/// 所以落地的是"链接位置上出现了一个真文件"，正是要的效果。
/// 指向目录的软链走递归复制。
fn materialise_symlink(link: &Path, home: &Path, op_id: &str) -> Result<u64> {
    let target = fs::canonicalize(link)
        .with_context(|| format!("cannot resolve symlink target: {}", link.display()))?;
    let meta = fs::metadata(&target)
        .with_context(|| format!("cannot stat symlink target: {}", target.display()))?;
    if meta.is_dir() {
        fs::remove_file(link).with_context(|| format!("failed to unlink {}", link.display()))?;
        return copy_dir_recursive(&target, link);
    }
    let bytes = fs::read(&target)
        .with_context(|| format!("failed to read symlink target: {}", target.display()))?;
    snapshot_before_rewrite(link, home, op_id)?;
    duster_fs::atomic::write_atomic(link, &bytes)?;
    Ok(bytes.len() as u64)
}

/// 把硬链换成独立副本：原样重写一遍，rename 换掉的是目录项指向的 inode，
/// 共享关系就此断开，A 侧再怎么删都影响不到这一份。
fn materialise_hardlink(path: &Path, home: &Path, op_id: &str) -> Result<u64> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    snapshot_before_rewrite(path, home, op_id)?;
    duster_fs::atomic::write_atomic(path, &bytes)?;
    Ok(bytes.len() as u64)
}

/// 原地改写**别人的**文件之前留一份整文件快照。
///
/// 与归档是两码事，两者都要有。归档保护的是「这块内容将整个消失」——
/// 用户在确认清单上看得见它。快照保护的是另一种失败：实体化写的是
/// 另一个 agent 目录里的一份文件，用户同意的是"卸载 A"，写坏了丢的却是
/// B 的那一份。这种损失在确认清单上根本不出现，所以它不问、不可关，
/// 无条件执行。落点 `~/.agent-duster/snapshots/<op-id>/`，保留最近 50 次。
///
/// 快照失败即返回 Err：写不出退路就不动手。
fn snapshot_before_rewrite(file: &Path, home: &Path, op_id: &str) -> Result<()> {
    let root = home.join(".agent-duster").join("snapshots");
    duster_fs::snapshot::snapshot_file(&root, op_id, file, home).with_context(|| {
        format!(
            "refusing to rewrite {}: could not snapshot it first",
            file.display()
        )
    })?;
    // GC 放在写完之后：先保住新快照，再回收旧的。
    let _ = duster_fs::snapshot::prune_old(&root, duster_fs::snapshot::KEEP);
    Ok(())
}

/// 递归复制目录（不跟随内部符号链接，符号链接按链接本身复制）。
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<u64> {
    fs::create_dir_all(dst).with_context(|| format!("failed to create {}", dst.display()))?;
    let mut total = 0u64;
    for entry in
        fs::read_dir(src).with_context(|| format!("failed to read dir {}", src.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = fs::symlink_metadata(&from)?;
        if meta.is_dir() {
            total += copy_dir_recursive(&from, &to)?;
        } else if meta.is_symlink() {
            let target = fs::read_link(&from)?;
            std::os::unix::fs::symlink(&target, &to)
                .with_context(|| format!("failed to recreate symlink {}", to.display()))?;
        } else {
            total += fs::copy(&from, &to)
                .with_context(|| format!("failed to copy {}", from.display()))?;
        }
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// 路径与索引小工具
// ---------------------------------------------------------------------------

fn resolve_home(opts: &UninstallOptions) -> Result<PathBuf> {
    match &opts.home {
        Some(h) => Ok(h.clone()),
        None => {
            let h = duster_fs::path::expand_tilde("~");
            if h == Path::new("~") {
                bail!("cannot determine the home directory");
            }
            Ok(h)
        }
    }
}

fn resolve_index_path(opts: &UninstallOptions, home: &Path) -> PathBuf {
    opts.index_path
        .clone()
        .unwrap_or_else(|| home.join(".agent-duster/index.db"))
}

fn export_dir(opts: &UninstallOptions) -> PathBuf {
    opts.export_dir
        .clone()
        .unwrap_or_else(archive::default_export_dir)
}

/// 清单里的 `~/...` 相对**给定 home** 展开（而非真实用户目录），
/// 假 home 注入才有意义。
fn expand_home(home: &Path, raw: &str) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

fn normalize_keep(s: &str) -> Option<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "session" | "sessions" => Some("session"),
        "memory" | "memories" => Some("memory"),
        _ => None,
    }
}

/// 规范化后的 kind 反查成 `--keep` 真正接受的那个词。
///
/// 存在的理由只有一条：`memory` 的复数不是 `memorys`。错误提示里给出的
/// 命令必须能原样粘回去跑，拼错一个字母整条建议就作废了。
fn keep_token(kind: &str) -> &'static str {
    // 入参来自 `normalize_keep` 的输出，只可能是 `session` / `memory`。
    if kind == "session" {
        "sessions"
    } else {
        "memory"
    }
}

fn is_under(child: &Path, ancestor: &Path) -> bool {
    child == ancestor || child.starts_with(ancestor)
}

fn is_under_any(child: &Path, ancestors: &[PathBuf]) -> bool {
    ancestors.iter().any(|a| is_under(child, a))
}

/// 去掉互为子路径的重复，只留顶层。删过父目录再删子目录只会得到 NotFound，
/// 更要命的是体积会被算两遍。
fn dedup_top_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let mut tops: Vec<PathBuf> = Vec::new();
    for p in paths {
        if tops.iter().any(|t| is_under(&p, t)) {
            continue;
        }
        tops.push(p);
    }
    tops
}

/// 实测体积：目录走并行遍历，文件/软链取自身长度，读不到按 0 计。
fn measured_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        walk::walk_stats(path, &WalkOptions::default())
            .map(|s| s.total_bytes)
            .unwrap_or(0)
    } else {
        meta.len()
    }
}

/// 删除 `target`，但绕开 `keeps` 指名保留的路径及其祖先目录。
///
/// 没有保留项时就是一句 `remove_dir_all` / `remove_file`；有保留项时
/// 逐层下钻，把不含保留项的兄弟整棵删掉——`--keep sessions` 的语义是
/// "别的都删，这一份留着"，而不是"整个根目录都别动"。
fn remove_path_except(target: &Path, keeps: &[PathBuf]) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(target) else {
        return Ok(()); // 已经不在了（父目录先被删掉的常见情形）
    };
    if is_under_any(target, keeps) {
        return Ok(());
    }
    if keeps.iter().any(|k| k.starts_with(target)) {
        for entry in fs::read_dir(target)
            .with_context(|| format!("failed to read dir {}", target.display()))?
        {
            remove_path_except(&entry?.path(), keeps)?;
        }
        return Ok(());
    }
    if meta.is_dir() {
        fs::remove_dir_all(target)
            .with_context(|| format!("failed to remove directory {}", target.display()))
    } else {
        fs::remove_file(target)
            .with_context(|| format!("failed to remove file {}", target.display()))
    }
}

/// 复核残留：`target` 子树里除了保留项与它们的祖先目录之外，还剩什么。
fn collect_leftovers(target: &Path, keeps: &[PathBuf], out: &mut Vec<PathBuf>) {
    if fs::symlink_metadata(target).is_err() {
        return;
    }
    if is_under_any(target, keeps) {
        return;
    }
    if keeps.iter().any(|k| k.starts_with(target)) {
        if let Ok(rd) = fs::read_dir(target) {
            for entry in rd.flatten() {
                collect_leftovers(&entry.path(), keeps, out);
            }
        }
        return;
    }
    out.push(target.to_path_buf());
}

/// 交叉引用要搜的"A 之外"的范围：CAS store、cc-switch、其他 agent 的
/// 清单声明路径。刻意不搜整个 home——那既慢又会把用户自己的目录卷进来。
fn outside_search_roots(home: &Path, agent: &str, roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = vec![home.join(".agent-duster/cas"), home.join(".cc-switch")];
    for m in manifest::load_all(None)? {
        if m.agent.id == agent {
            continue;
        }
        for raw in m.probe.any_of.iter().chain(m.probe.all_of.iter()) {
            out.push(expand_home(home, raw));
        }
        for r in &m.resources {
            out.push(expand_home(home, &r.path));
        }
    }
    out.sort();
    out.dedup();
    // 落在 A 里面的、或者反过来包住 A 的，都不是"外部"。
    out.retain(|p| {
        fs::symlink_metadata(p).is_ok()
            && !is_under_any(p, roots)
            && !roots.iter().any(|r| r.starts_with(p))
    });
    Ok(dedup_top_paths(out))
}

/// `~/.cc-switch` 里提到该 agent 的备份文件。只报告，不处理。
fn cc_switch_backups(home: &Path, agent: &str) -> Vec<PathBuf> {
    let dir = home.join(".cc-switch");
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    let _ = walk::walk_files(&dir, &WalkOptions::default(), |p, _| {
        if p.to_string_lossy().contains(agent) {
            out.push(p.to_path_buf());
        }
    });
    out.sort();
    out
}

/// 判定这次卸载是否「真的删干净了」。只有干净才允许清索引。
///
/// 不干净的定义：
/// 1. `leftovers` 非空——删除后仍有路径活着，agent 还在盘上；
/// 2. `keep_paths` 非空——`--keep` 指名保留的内容还在，agent 就没走完。
///
/// 3. **residue 失败**——清单声明的外部残留（shell rc 里的 PATH 行、
///    cc-switch 数据库行）没删掉时，agent 同样还活着：它的痕迹还散在
///    别人家的文件里，从列表里抹掉它就是撒谎。
///
/// `NotFound` 不算不干净：声明的残留本来就不在这台机器上（比如根本没装
/// 过 cc-switch），那是「无事可做」，不是「没做成」。
///
/// 反过来：残留清光才清索引，agent 才能从列表里消失——「没删干净就继续
/// 显示在列表里」正是列表说实话的本分。
fn removal_was_clean(report: &UninstallReport, keep_paths: &[PathBuf]) -> bool {
    report.leftovers.is_empty()
        && keep_paths.is_empty()
        && !report
            .residues
            .iter()
            .any(|r| matches!(r.action, ResidueAction::Failed(_)))
}

/// 清索引：先删该 agent 的全部资源行（连带 turn 与 fts_turn），再删 agent 行。
/// 返回删掉的资源行数。库不存在时跳过——没有索引不该让卸载失败。
///
/// 调用方必须先用 [`removal_was_clean`] 判定卸载真的干净了：残留还在时
/// 清索引会让 agent 从列表里消失而它其实还活着。
fn purge_index(idx_path: &Path, agent: &str) -> Result<usize> {
    if !idx_path.is_file() {
        return Ok(0);
    }
    let idx = Index::open(idx_path)
        .with_context(|| format!("failed to open index for writing: {}", idx_path.display()))?;
    let conn = idx.conn();
    let rows = query::list_resources(
        conn,
        &ResourceFilter {
            agents: vec![agent.to_string()],
            kinds: Vec::new(),
            clean_levels: Vec::new(),
        },
    )?;
    for r in &rows {
        query::delete_resource(conn, r.rid)?;
    }
    query::delete_agent(conn, agent)?;
    Ok(rows.len())
}

fn bullet(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| format!("  - {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}


#[cfg(test)]
mod tests {
    use super::*;
    use duster_index::query::ResourceFilter;

    const SESSION_BODY: &[u8] =
        b"{\"type\":\"message\",\"role\":\"user\",\"text\":\"\xe4\xbd\xa0\xe5\xa5\xbd\"}\n";
    const SKILL_BODY: &[u8] = b"---\nname: demo\ndescription: demo skill\n---\n# demo\n";

    /// 造一个能让内置 codex 清单探测为「已安装」的假 home：
    /// `~/.codex` 存在即命中 `probe.any_of`。清单声明的子路径全部铺齐，
    /// 外加一个**清单没声明**的 `computer-use/`——它正是 uninstall 与 clean
    /// 所有权模型相反的那一半，必须一起被删掉。
    fn fake_home(home: &Path) {
        let codex = home.join(".codex");
        fs::create_dir_all(codex.join("sessions/2026/08/01")).unwrap();
        fs::create_dir_all(codex.join("skills/demo")).unwrap();
        fs::create_dir_all(codex.join("archived_sessions")).unwrap();
        fs::create_dir_all(codex.join("computer-use")).unwrap();
        fs::write(codex.join("config.toml"), "[mcp_servers]\n").unwrap();
        fs::write(codex.join("AGENTS.md"), "# codex memory\n").unwrap();
        fs::write(codex.join("skills/demo/SKILL.md"), SKILL_BODY).unwrap();
        fs::write(
            codex.join("sessions/2026/08/01/rollout-x.jsonl"),
            SESSION_BODY,
        )
        .unwrap();
        fs::write(codex.join("computer-use/blob.bin"), vec![7u8; 4096]).unwrap();
    }

    /// 注入一个只含 codex 的索引，并**在返回前 drop 写句柄**——
    /// `Index::open` 抢单实例排他锁，不放手后面谁都开不了。
    fn fake_index(home: &Path) -> PathBuf {
        let path = home.join(".agent-duster/index.db");
        let idx = Index::open(&path).unwrap();
        duster_index::upsert::upsert_agent(
            idx.conn(),
            &duster_model::AgentInfo {
                id: "codex".to_string(),
                display_name: "Codex".to_string(),
                root: home.join(".codex"),
                version: None,
            },
            0,
        )
        .unwrap();
        for (kind, key, rel) in [
            ("session", "sessions", ".codex/sessions"),
            ("session", "archived_sessions", ".codex/archived_sessions"),
            ("memory", "AGENTS.md", ".codex/AGENTS.md"),
            ("skill", "demo", ".codex/skills/demo"),
        ] {
            duster_index::upsert::upsert_resource(
                idx.conn(),
                &duster_index::upsert::ResourceRow {
                    agent_id: "codex".to_string(),
                    kind: kind.to_string(),
                    scope: "global".to_string(),
                    key: key.to_string(),
                    path: home.join(rel).to_string_lossy().to_string(),
                    size: 0,
                    mtime_ns: 0,
                    hash_content: None,
                    cheap_print: None,
                    clean_level: None,
                    reclaimable: None,
                    install_bytes: None,
                    mapper: None,
                },
            )
            .unwrap();
        }
        drop(idx);
        path
    }

    fn base_opts(home: &Path, index_path: &Path) -> UninstallOptions {
        UninstallOptions {
            index_path: Some(index_path.to_path_buf()),
            home: Some(home.to_path_buf()),
            agent: "codex".to_string(),
            data_only: true,
            confirm: Some("codex".to_string()),
            export_first: true,
            keep: Vec::new(),
            archive: None,
            export_dir: Some(home.join("agent-duster-exports")),
            run_package_manager: false,
            dry_run: true,
        }
    }

    fn agent_rows(index_path: &Path, agent: &str) -> usize {
        let idx = Index::open_readonly(index_path).unwrap();
        query::list_resources(
            idx.conn(),
            &ResourceFilter {
                agents: vec![agent.to_string()],
                kinds: Vec::new(),
                clean_levels: Vec::new(),
            },
        )
        .unwrap()
        .len()
    }

    /// 递归列出目录下的全部常规文件。快照落点带相对目录结构，
    /// 不能只看一层。
    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(root) else {
            return out;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walkdir(&p));
            } else {
                out.push(p);
            }
        }
        out.sort();
        out
    }

    #[test]
    fn 确认串必须逐字相等() {
        let tmp = tempfile::tempdir().unwrap();
        fake_home(tmp.path());
        let index_path = fake_index(tmp.path());
        let mut opts = base_opts(tmp.path(), &index_path);

        opts.confirm = None;
        let err = uninstall(&opts).unwrap_err().to_string();
        assert!(err.contains("confirmation required"), "{err}");
        // 裸 --yes 不是替代品，错误文案必须把这件事说明白。
        assert!(err.contains("--yes"), "{err}");

        opts.confirm = Some("wrong".to_string());
        assert!(
            uninstall(&opts)
                .unwrap_err()
                .to_string()
                .contains("confirmation required")
        );

        // 逐字相等即放行（dry-run，什么都不动）。
        opts.confirm = Some("codex".to_string());
        let report = uninstall(&opts).unwrap();
        assert!(!report.executed);
    }

    /// `--data-only` 从一个恒真的空旗标变成了真开关：它把 shared 与 package
    /// 两栏整个关掉，并且**必须说出来自己关掉了什么**——用户以为卸干净了，
    /// 而别人的配置里还留着一条指向已删程序的声明，那正是「卸载不干净」的
    /// 现代形态。
    #[test]
    fn data_only_关掉共享与包并如实说明() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = demo_home(home, 0);
        let mut opts = demo_opts(home, &index_path);
        opts.data_only = true;

        let report = uninstall(&opts).unwrap();
        assert!(report.shared.is_empty(), "{:?}", report.shared);
        assert!(report.packages.is_empty(), "{:?}", report.packages);
        let note = report
            .warnings
            .iter()
            .find(|w| w.contains("--data-only"))
            .unwrap_or_else(|| panic!("跳过了就必须说出来：{:?}", report.warnings));
        assert!(note.contains("2 shared-file edit(s)"), "{note}");
        assert!(note.contains("1 package hint(s)"), "{note}");
    }

    #[test]
    fn dry_run_一个字节都不动() {
        let tmp = tempfile::tempdir().unwrap();
        fake_home(tmp.path());
        let index_path = fake_index(tmp.path());
        let opts = base_opts(tmp.path(), &index_path);

        let report = uninstall(&opts).unwrap();
        assert!(!report.executed);
        assert_eq!(report.removed_bytes, 0);
        assert!(report.leftovers.is_empty());
        // dry-run 连导出都不做：归档也是副作用。
        assert!(report.archive_path.is_none());
        assert!(!tmp.path().join("agent-duster-exports").exists());

        for rel in [
            ".codex/config.toml",
            ".codex/AGENTS.md",
            ".codex/skills/demo/SKILL.md",
            ".codex/sessions/2026/08/01/rollout-x.jsonl",
            ".codex/computer-use/blob.bin",
        ] {
            assert!(tmp.path().join(rel).exists(), "dry-run 删掉了 {rel}");
        }
        assert_eq!(agent_rows(&index_path, "codex"), 4);
    }

    #[test]
    fn 执行删除包含清单未声明的子目录() {
        let tmp = tempfile::tempdir().unwrap();
        fake_home(tmp.path());
        let index_path = fake_index(tmp.path());
        let mut opts = base_opts(tmp.path(), &index_path);
        opts.dry_run = false;

        let report = uninstall(&opts).unwrap();
        assert!(report.executed);
        assert!(
            report.leftovers.is_empty(),
            "卸载不干净：{:?}",
            report.leftovers
        );
        assert!(report.removed_bytes >= 4096, "{}", report.removed_bytes);

        // 清单声明的与没声明的，一视同仁全没了。
        assert!(!tmp.path().join(".codex/computer-use/blob.bin").exists());
        assert!(!tmp.path().join(".codex").exists());

        // 索引收尾：资源行与 agent 行都不该留下。
        assert_eq!(agent_rows(&index_path, "codex"), 0);
        let idx = Index::open_readonly(&index_path).unwrap();
        assert!(
            !query::agent_ids(idx.conn())
                .unwrap()
                .contains(&"codex".to_string())
        );
    }

    /// 删不干净就不清索引：agent 继续显示在列表里，直到它真的被删干净。
    ///
    /// 现场：把 `computer-use/` 设成只读（unlink 需要目录写权限，普通用户
    /// 下 `remove_dir_all` 必然摘不掉里面的 `blob.bin`），卸载留下残留——
    /// 索引行必须原样保留，并有一句人话解释为什么还列着。
    #[test]
    fn 删不干净时_索引保留_agent_继续可见() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        fake_home(tmp.path());
        let index_path = fake_index(tmp.path());
        let stuck = tmp.path().join(".codex/computer-use");
        let mut perm = fs::metadata(&stuck).unwrap().permissions();
        perm.set_mode(0o555);
        fs::set_permissions(&stuck, perm).unwrap();

        let mut opts = base_opts(tmp.path(), &index_path);
        opts.dry_run = false;
        let report = uninstall(&opts).unwrap();

        // 残留如实报出来——不假装卸载成功。
        assert!(!report.leftovers.is_empty(), "{:?}", report.leftovers);
        // 索引原样保留：agent 还在列表里。
        assert_eq!(agent_rows(&index_path, "codex"), 4);
        let idx = Index::open_readonly(&index_path).unwrap();
        assert!(
            query::agent_ids(idx.conn())
                .unwrap()
                .contains(&"codex".to_string())
        );
        drop(idx);
        // 必须有一句人话解释为什么还列着，而不是叫用户自己去查。
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("is still listed")),
            "warnings: {:?}",
            report.warnings
        );
    }

    /// 删干净了才清索引——与「删不干净时_索引保留_agent_继续可见」互为镜像：
    /// 残留为空时资源行与 agent 行全部清掉，agent 从列表消失，也不需要
    /// 「still listed」警告。
    #[test]
    fn 删干净了才清索引() {
        let tmp = tempfile::tempdir().unwrap();
        fake_home(tmp.path());
        let index_path = fake_index(tmp.path());
        let mut opts = base_opts(tmp.path(), &index_path);
        opts.dry_run = false;

        let report = uninstall(&opts).unwrap();
        assert!(report.leftovers.is_empty(), "{:?}", report.leftovers);
        assert_eq!(agent_rows(&index_path, "codex"), 0);
        let idx = Index::open_readonly(&index_path).unwrap();
        assert!(
            !query::agent_ids(idx.conn())
                .unwrap()
                .contains(&"codex".to_string())
        );
        drop(idx);
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("is still listed")),
            "warnings: {:?}",
            report.warnings
        );
    }

    /// residue 删不掉 ⇒ 索引不清 ⇒ agent 继续显示在列表里。
    ///
    /// 这条是「残留真删」与「列表说实话」两块改动**唯一的咬合点**，而它
    /// 横跨两个函数（residue 引擎填 `residues`、`removal_was_clean` 读它），
    /// 最容易在改动中悄悄断掉:引擎照样跑、列表照样清，只是再也不咬合了,
    /// 全套测试还是绿的。所以直接对 `removal_was_clean` 下断言。
    #[test]
    fn residue_失败也算没删干净() {
        let mk = |action: ResidueAction| ResidueOutcome {
            what: "PATH entry".to_string(),
            path: PathBuf::from("/home/u/.zshrc"),
            why: "installer added it".to_string(),
            action,
        };
        // 只有 residues 这一维在变,其余字段全部取「干净」值:这条测试问的
        // 就是 residue 单独能不能拖住索引。不给 UninstallReport 派生
        // Default——那是为测试便利改生产结构体。
        let report = |residues: Vec<ResidueOutcome>| UninstallReport {
            plan: Plan {
                verb: crate::plan::Verb::Uninstall,
                items: Vec::new(),
                reclaim_bytes: 0,
                install_bytes: 0,
                stale_bytes: 0,
                warnings: Vec::new(),
            },
            checks: Vec::new(),
            executed: true,
            removed_bytes: 0,
            archive_path: None,
            archive_bytes: None,
            leftovers: Vec::new(),
            shared: Vec::new(),
            packages: Vec::new(),
            residues,
            warnings: Vec::new(),
        };

        // 全清光 = 干净，索引该清。
        assert!(removal_was_clean(&report(vec![]), &[]));

        // 声明的残留压根不在这台机器上：无事可做,不是没做成。
        assert!(
            removal_was_clean(&report(vec![mk(ResidueAction::NotFound)]), &[]),
            "NotFound 不该拖住索引"
        );

        // 真的删不掉:痕迹还散在别人家文件里,列表不许抹掉它。
        assert!(
            !removal_was_clean(
                &report(vec![mk(ResidueAction::Failed("permission denied".into()))]),
                &[]
            ),
            "residue 删不掉就必须继续显示在列表里"
        );
    }

    #[test]
    fn 导出包解开后与原会话逐字节一致() {
        let tmp = tempfile::tempdir().unwrap();
        fake_home(tmp.path());
        let index_path = fake_index(tmp.path());
        let mut opts = base_opts(tmp.path(), &index_path);
        opts.dry_run = false;

        let report = uninstall(&opts).unwrap();
        let archive_path = PathBuf::from(report.archive_path.expect("export_first 必须产出归档包"));
        assert!(archive_path.is_file(), "{}", archive_path.display());
        assert_eq!(
            report.archive_bytes,
            Some(fs::metadata(&archive_path).unwrap().len())
        );

        let dest = tmp.path().join("restore");
        archive::extract_to(&archive_path, &dest).unwrap();
        // 条目路径相对 home，解到哪个目录下都能原位还原。
        let restored = dest.join(".codex/sessions/2026/08/01/rollout-x.jsonl");
        assert_eq!(fs::read(&restored).unwrap(), SESSION_BODY);
        assert_eq!(
            fs::read(dest.join(".codex/AGENTS.md")).unwrap(),
            b"# codex memory\n"
        );
    }

    #[test]
    fn 外部硬链被检测并实体化() {
        let tmp = tempfile::tempdir().unwrap();
        fake_home(tmp.path());
        let index_path = fake_index(tmp.path());

        // `duster skill link` 的形态：CAS 里那一份与 agent 目录里的共用 inode。
        let cas = tmp.path().join(".agent-duster/cas");
        fs::create_dir_all(&cas).unwrap();
        let linked = cas.join("demo-SKILL.md");
        fs::hard_link(tmp.path().join(".codex/skills/demo/SKILL.md"), &linked).unwrap();

        let mut opts = base_opts(tmp.path(), &index_path);
        opts.dry_run = false;
        let report = uninstall(&opts).unwrap();

        let cross = report
            .checks
            .iter()
            .find(|c| c.name == "cross-reference")
            .unwrap();
        assert!(cross.passed, "{}", cross.detail);
        assert!(
            cross.detail.contains("materialised") && cross.detail.contains("demo-SKILL.md"),
            "{}",
            cross.detail
        );

        assert!(!tmp.path().join(".codex").exists());
        assert!(linked.is_file());
        assert_eq!(fs::read(&linked).unwrap(), SKILL_BODY);
        // 实体化的定义就是不再共享 inode。
        assert_eq!(fs::symlink_metadata(&linked).unwrap().nlink(), 1);

        // 改写别人的文件之前必须留一份整文件快照：用户确认的是"卸载 codex"，
        // 写坏的却会是 CAS 里那一份。这道保护在确认清单上看不见,
        // 所以只能由代码无条件执行——这条断言是它唯一的守卫。
        let snaps = tmp.path().join(".agent-duster/snapshots");
        let mut found = Vec::new();
        for op in fs::read_dir(&snaps).expect("必须建出快照目录").flatten() {
            for f in walkdir(&op.path()) {
                found.push(f);
            }
        }
        assert_eq!(found.len(), 1, "恰好一份被改写的文件被快照: {found:?}");
        assert_eq!(
            fs::read(&found[0]).unwrap(),
            SKILL_BODY,
            "快照必须是改写前的原样内容"
        );
    }

    #[test]
    fn keep_sessions_把会话留在原地() {
        let tmp = tempfile::tempdir().unwrap();
        fake_home(tmp.path());
        let index_path = fake_index(tmp.path());
        let mut opts = base_opts(tmp.path(), &index_path);
        opts.dry_run = false;
        opts.export_first = false;
        opts.keep = vec!["sessions".to_string(), "memory".to_string()];

        let report = uninstall(&opts).unwrap();
        assert!(report.executed);
        assert!(
            report.leftovers.is_empty(),
            "保留项与其祖先目录不算残留：{:?}",
            report.leftovers
        );
        // 保留的留下，其余（含未声明的 computer-use）照删。
        assert!(
            tmp.path()
                .join(".codex/sessions/2026/08/01/rollout-x.jsonl")
                .is_file()
        );
        assert!(tmp.path().join(".codex/AGENTS.md").is_file());
        assert!(!tmp.path().join(".codex/computer-use").exists());
        assert!(!tmp.path().join(".codex/skills").exists());
        assert!(!tmp.path().join(".codex/config.toml").exists());
    }

    #[test]
    fn 既不导出也不保留即拒绝执行() {
        let tmp = tempfile::tempdir().unwrap();
        fake_home(tmp.path());
        let index_path = fake_index(tmp.path());
        let mut opts = base_opts(tmp.path(), &index_path);
        opts.dry_run = false;
        opts.export_first = false;

        let err = uninstall(&opts).unwrap_err().to_string();
        assert!(err.contains("data-escape"), "{err}");
        // 两条出路都必须写在错误里，否则用户不知道怎么继续。
        assert!(
            err.contains("--export-first") && err.contains("--keep"),
            "{err}"
        );
        // 建议里的命令必须能原样粘回去跑：`memorys` 不是合法的 --keep 值。
        assert!(!err.contains("memorys"), "{err}");
        assert!(err.contains("--keep sessions,memory"), "{err}");
        // 中止即不留痕。
        assert!(tmp.path().join(".codex/computer-use/blob.bin").exists());
        assert_eq!(agent_rows(&index_path, "codex"), 4);
    }

    #[test]
    fn 检查未过的_dry_run_仍然给出完整预览() {
        let tmp = tempfile::tempdir().unwrap();
        fake_home(tmp.path());
        let index_path = fake_index(tmp.path());
        let mut opts = base_opts(tmp.path(), &index_path);
        opts.export_first = false; // data-escape 必然不过

        // 预览不是 Err：卡住的时候恰恰最需要看见清单与"怎么解"。
        let report = uninstall(&opts).unwrap();
        assert!(!report.executed);
        assert!(!report.plan.items.is_empty(), "预览必须带上完整计划");
        let escape = report
            .checks
            .iter()
            .find(|c| c.name == "data-escape")
            .unwrap();
        assert!(!escape.passed);
        assert!(
            escape.detail.contains("--export-first"),
            "{}",
            escape.detail
        );
        assert!(
            escape.detail.contains("--keep sessions,memory"),
            "{}",
            escape.detail
        );
        // 三项检查一项不少，CLI 才能逐条渲染 ✔/✖。
        assert_eq!(report.checks.len(), 3);
        assert!(tmp.path().join(".codex").exists());
    }

    // -----------------------------------------------------------------
    // M2：shared（外科式删键）与 package（只打印）
    // -----------------------------------------------------------------

    /// 一份 `~/.claude.json` 形状的共享文件：两家的 MCP 声明、一条注释、
    /// 一个特定的键序。`demo` 是要删的那一个键，其余每一个字节都必须活下来。
    const CLAUDE_JSON: &str = r#"{
  // MCP servers every agent on this machine reads.
  "mcpServers": {
    "keeper": {
      "command": "keeper-mcp",
      "args": ["--stdio"]
    },
    "demo": {
      "command": "demo-mcp",
      "args": ["--stdio"]
    }
  },
  "theme": "dark"
}
"#;

    /// 同一件事的 TOML 面：codex 的 `config.toml` 形状。
    const CODEX_TOML: &str = r#"# Codex's own settings.
model = "gpt-5"

[mcp_servers.keeper]
command = "keeper-mcp"
args = ["--stdio"]

# Installed by demo-agent.
[mcp_servers.demo]
command = "demo-mcp"
args = ["--stdio"]
"#;

    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, body).unwrap();
        let mut perm = fs::metadata(path).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(path, perm).unwrap();
    }

    /// 卸载脚本会碰的那个文件。它存在 = 包管理器真的被跑过了。
    fn sentinel(home: &Path) -> PathBuf {
        home.join("package-manager-ran")
    }

    /// 造一个带完整 `[uninstall]` 段的假 home。
    ///
    /// `detect_exit` 是探测脚本的退出码：0 = 装着，非 0 = 没装。两个脚本都是
    /// 现造的 `/bin/sh`，**永远不碰真的包管理器**；卸载脚本只做一件事——
    /// 往哨兵文件上一戳。它出现了，就说明"永不代跑"这条规矩被破了。
    fn demo_home(home: &Path, detect_exit: i32) -> PathBuf {
        fs::create_dir_all(home.join(".demo-agent/state")).unwrap();
        fs::write(home.join(".demo-agent/state/blob.bin"), vec![3u8; 2048]).unwrap();
        fs::write(home.join(".claude.json"), CLAUDE_JSON).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(home.join(".codex/config.toml"), CODEX_TOML).unwrap();

        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let detect = bin.join("detect.sh");
        let remove = bin.join("remove.sh");
        write_script(&detect, &format!("#!/bin/sh\nexit {detect_exit}\n"));
        write_script(
            &remove,
            &format!(
                "#!/bin/sh\necho removed\n: > '{}'\n",
                sentinel(home).display()
            ),
        );

        let adapters = home.join(".agent-duster/adapters");
        fs::create_dir_all(&adapters).unwrap();
        fs::write(
            adapters.join("demo-agent.toml"),
            format!(
                r#"
[agent]
id = "demo-agent"
display_name = "Demo Agent"

[probe]
any_of = ["~/.demo-agent"]

[uninstall]
owns = ["~/.demo-agent"]

[[uninstall.shared]]
path = "~/.claude.json"
json_pointer = "/mcpServers/demo"
reason = "MCP entry pointing at this agent"

[[uninstall.shared]]
path = "~/.codex/config.toml"
toml_key = "mcp_servers.demo"
reason = "MCP table pointing at this agent"

[[uninstall.package]]
manager = "npm"
detect = ["{detect}"]
command = ["{remove}", "--global", "demo agent"]
"#,
                detect = detect.display(),
                remove = remove.display(),
            ),
        )
        .unwrap();

        let index_path = home.join(".agent-duster/index.db");
        let idx = Index::open(&index_path).unwrap();
        duster_index::upsert::upsert_agent(
            idx.conn(),
            &duster_model::AgentInfo {
                id: "demo-agent".to_string(),
                display_name: "Demo Agent".to_string(),
                root: home.join(".demo-agent"),
                version: None,
            },
            0,
        )
        .unwrap();
        drop(idx);
        index_path
    }

    /// 三个模式全开、真执行。没有 session/memory 行，data-escape 天然通过，
    /// 所以不需要打包，测试也就不依赖归档路径。
    fn demo_opts(home: &Path, index_path: &Path) -> UninstallOptions {
        UninstallOptions {
            index_path: Some(index_path.to_path_buf()),
            home: Some(home.to_path_buf()),
            agent: "demo-agent".to_string(),
            data_only: false,
            confirm: Some("demo-agent".to_string()),
            export_first: false,
            keep: Vec::new(),
            archive: None,
            export_dir: Some(home.join("agent-duster-exports")),
            run_package_manager: false,
            dry_run: false,
        }
    }

    fn outcome<'a>(report: &'a UninstallReport, key: &str) -> &'a SharedEditOutcome {
        report
            .shared
            .iter()
            .find(|s| s.key == key)
            .unwrap_or_else(|| panic!("报告里没有 {key}：{:?}", report.shared))
    }

    /// 造一个带 `[[uninstall.residue]]` 段的假 home：shell rc 里躺着安装脚本
    /// 追加的 PATH 行（上方还留着它的注释行），cc-switch 的库里躺着供应商
    /// 行，外加一条指向不存在表的声明——「删掉 / 找不到 / 失败」三种结局
    /// 一次测齐。永不碰真实 `~`。
    fn residue_home(home: &Path) -> PathBuf {
        fs::create_dir_all(home.join(".demo-agent")).unwrap();
        // 第二行故意带行尾空白：未命中行必须逐字节活下来，包括它。
        fs::write(
            home.join(".zshrc"),
            "# demo agent shell config\nexport FOO=bar   \n# opencode\nexport PATH=/Users/laibu/.opencode/bin:$PATH\nalias ll='ls -la'\n",
        )
        .unwrap();

        let cc = home.join(".cc-switch");
        fs::create_dir_all(&cc).unwrap();
        {
            let conn = rusqlite::Connection::open(cc.join("cc-switch.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE providers(id TEXT PRIMARY KEY, name TEXT, enabled INTEGER);
                 INSERT INTO providers VALUES ('claude-official', 'Claude Official', 1);
                 INSERT INTO providers VALUES ('codex-official', 'Codex Official', 0);
                 INSERT INTO providers VALUES ('keeper', 'Keeper', 1);",
            )
            .unwrap();
        }

        let adapters = home.join(".agent-duster/adapters");
        fs::create_dir_all(&adapters).unwrap();
        fs::write(
            adapters.join("demo-agent.toml"),
            r#"
[agent]
id = "demo-agent"
display_name = "Demo Agent"

[probe]
any_of = ["~/.demo-agent"]

[uninstall]
owns = ["~/.demo-agent"]

[[uninstall.residue]]
kind = "shell_line"
files = ["~/.zshrc", "~/.zprofile"]
match = "/.opencode/bin"
with_comment_above = true
why = "PATH entry added by the opencode installer"

[[uninstall.residue]]
kind = "sqlite_row"
db = "~/.cc-switch/cc-switch.db"
table = "providers"
column = "id"
equals = "claude-official"
why = "cc-switch keeps one provider row per agent"

[[uninstall.residue]]
kind = "sqlite_row"
db = "~/.cc-switch/cc-switch.db"
table = "no_such_table"
column = "id"
equals = "x"
why = "a table that does not exist must fail, not panic"
"#,
        )
        .unwrap();

        // 索引里只有 agent 行、没有资源行：data-escape 没有 session/memory
        // 可逃，export_first=false 也照常通过（与 demo_home 同一条路）。
        let index_path = home.join(".agent-duster/index.db");
        let idx = Index::open(&index_path).unwrap();
        duster_index::upsert::upsert_agent(
            idx.conn(),
            &duster_model::AgentInfo {
                id: "demo-agent".to_string(),
                display_name: "Demo Agent".to_string(),
                root: home.join(".demo-agent"),
                version: None,
            },
            0,
        )
        .unwrap();
        drop(idx);
        index_path
    }

    /// 取某条残留的结果，按路径后缀找。
    fn residue<'a>(report: &'a UninstallReport, suffix: &str) -> &'a ResidueOutcome {
        report
            .residues
            .iter()
            .find(|r| r.path.to_string_lossy().ends_with(suffix))
            .unwrap_or_else(|| panic!("报告里没有路径以 {suffix} 结尾的残留：{:?}", report.residues))
    }

    /// shell_line：命中行连同上方注释整行删掉，其余行**逐字节不变**
    /// （含行尾空白与最后的换行），快照里是改写前的原文。
    #[test]
    fn residue_shell_line_删命中行留快照其余逐字节不变() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = residue_home(home);

        let report = uninstall(&demo_opts(home, &index_path)).unwrap();
        assert!(report.executed);

        let zsh = residue(&report, ".zshrc");
        assert_eq!(zsh.action, ResidueAction::Removed, "{:?}", zsh);
        assert_eq!(
            zsh.what,
            "shell line containing `/.opencode/bin` (and the comment directly above it, if any)"
        );
        assert_eq!(zsh.why, "PATH entry added by the opencode installer");
        // `~/.zprofile` 不存在：本来就是 NotFound，不是错误。
        assert_eq!(
            residue(&report, ".zprofile").action,
            ResidueAction::NotFound
        );

        // 命中行 + 上方注释都该没了；其余行逐字节不变，连行尾空白都留着。
        assert_eq!(
            fs::read_to_string(home.join(".zshrc")).unwrap(),
            "# demo agent shell config\nexport FOO=bar   \nalias ll='ls -la'\n",
            "未命中行必须逐字节原样，含行尾空白与最后的换行"
        );

        // 快照里必须是改写前的原文，两行都还在。db 快照是二进制，不能当
        // 文本读，这里只挑 `.zshrc` 那份。
        let snaps = home.join(".agent-duster/snapshots");
        let mut found = Vec::new();
        for op in fs::read_dir(&snaps).expect("必须建出快照目录").flatten() {
            found.extend(walkdir(&op.path()));
        }
        let zsh_snap = found
            .iter()
            .find(|p| p.to_string_lossy().ends_with("zshrc"))
            .unwrap_or_else(|| panic!("要有 .zshrc 快照：{found:?}"));
        let body = fs::read_to_string(zsh_snap).unwrap();
        assert!(
            body.contains("# opencode\n") && body.contains("/.opencode/bin"),
            "快照里必须是改写前的原文：{body:?}"
        );
    }

    /// sqlite_row：命中行没了、别的行还在、快照里是原库；表不存在那条
    /// 报 Failed（不是 panic），并有一条 warning 把原因说出来。
    #[test]
    fn residue_sqlite_row_删命中行留快照别的行还在() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = residue_home(home);

        let report = uninstall(&demo_opts(home, &index_path)).unwrap();
        assert!(report.executed);

        let db = residue(&report, "cc-switch.db");
        assert_eq!(db.action, ResidueAction::Removed, "{:?}", db);
        assert_eq!(db.what, "row `claude-official` in `providers`.`id`");
        assert_eq!(db.why, "cc-switch keeps one provider row per agent");
        // 清单声明与库实际形状对不上 → Failed，不是 panic。
        let failed: Vec<&ResidueOutcome> = report
            .residues
            .iter()
            .filter(|r| matches!(r.action, ResidueAction::Failed(_)))
            .collect();
        assert_eq!(failed.len(), 1, "{:?}", report.residues);
        assert!(
            failed[0].what.contains("no_such_table"),
            "{:?}",
            failed[0]
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("could not remove residue")),
            "{:?}",
            report.warnings
        );

        // 命中行没了，别的行还在。
        let conn = rusqlite::Connection::open(home.join(".cc-switch/cc-switch.db")).unwrap();
        let left: Vec<String> = conn
            .prepare("SELECT id FROM providers ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(left, ["codex-official", "keeper"], "别的行必须原样还在");

        // 快照里是原库：被删的那一行还躺在快照里。
        let snaps = home.join(".agent-duster/snapshots");
        let mut found = Vec::new();
        for op in fs::read_dir(&snaps).unwrap().flatten() {
            found.extend(walkdir(&op.path()));
        }
        let db_snap = found
            .iter()
            .find(|p| p.to_string_lossy().ends_with("cc-switch.db"))
            .unwrap_or_else(|| panic!("要有 db 快照：{found:?}"));
        let snap_conn = rusqlite::Connection::open(db_snap).unwrap();
        let n: i64 = snap_conn
            .query_row(
                "SELECT count(*) FROM providers WHERE id = 'claude-official'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "快照里必须还躺着被删掉的那一行");
    }

    /// dry-run：两种 kind 都只报不删——一个字节不改、不建快照、不动 db，
    /// 而且「会删什么」要如实算出来（Planned，不是 Removed）。
    #[test]
    fn residue_dry_run_一个字节不碰也不留快照() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = residue_home(home);
        let zshrc_before = fs::read(home.join(".zshrc")).unwrap();
        let db_before = fs::read(home.join(".cc-switch/cc-switch.db")).unwrap();

        let mut opts = demo_opts(home, &index_path);
        opts.dry_run = true;
        let report = uninstall(&opts).unwrap();

        assert!(!report.executed);
        assert_eq!(
            residue(&report, ".zshrc").action,
            ResidueAction::Planned,
            "预览要说「真跑会删」，而不是假装已经删了"
        );
        assert_eq!(
            residue(&report, "cc-switch.db").action,
            ResidueAction::Planned
        );
        // 表不存在的声明在预览里同样如实报 Failed。
        assert!(
            report
                .residues
                .iter()
                .any(|r| matches!(r.action, ResidueAction::Failed(_))),
            "{:?}",
            report.residues
        );

        // 一个字节没改，也没有任何快照。
        assert_eq!(fs::read(home.join(".zshrc")).unwrap(), zshrc_before);
        assert_eq!(
            fs::read(home.join(".cc-switch/cc-switch.db")).unwrap(),
            db_before
        );
        assert!(
            !home.join(".agent-duster/snapshots").exists(),
            "dry-run 不许建快照"
        );
    }

    /// `--data-only` 连残留一起跳过，并把跳过了几条如实说出来；
    /// 文件和库都原样躺着。
    #[test]
    fn data_only_连残留也一起跳过并如实说明() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = residue_home(home);
        let mut opts = demo_opts(home, &index_path);
        opts.data_only = true;

        let report = uninstall(&opts).unwrap();
        assert!(
            report.residues.is_empty(),
            "data-only 不该碰残留：{:?}",
            report.residues
        );
        let note = report
            .warnings
            .iter()
            .find(|w| w.contains("--data-only"))
            .unwrap_or_else(|| panic!("跳过了就必须说出来：{:?}", report.warnings));
        assert!(note.contains("3 residue(s)"), "{note}");
        // 文件与库都原样。
        assert!(
            fs::read_to_string(home.join(".zshrc"))
                .unwrap()
                .contains("# opencode\n"),
            "PATH 行必须还在"
        );
        let conn = rusqlite::Connection::open(home.join(".cc-switch/cc-switch.db")).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM providers WHERE id = 'claude-official'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "供应商行必须还在");
    }

    /// M2 验收线的前半句：**`uninstall` 对 `shared` 段落只删本 agent 的键，
    /// 同文件内其他键与注释逐字节不变。**
    ///
    /// 断言的是整个文件的完整字符串，不是"包含/不包含"——后者放得过键序被
    /// 重排、缩进被改成两格、行尾换行被抹掉这一整类回归，而那正是保守回写
    /// 唯一要保证的东西。JSON 与 TOML 两条写路径各测一遍：内置清单里只有
    /// TOML 那条有真实用例，JSON 指针分支的覆盖全靠这里。
    #[test]
    fn shared_只删本_agent_的键_同文件其余逐字节不变() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = demo_home(home, 0);

        let report = uninstall(&demo_opts(home, &index_path)).unwrap();
        assert!(report.executed);

        assert_eq!(
            outcome(&report, "/mcpServers/demo").action,
            SharedAction::Removed
        );
        assert_eq!(
            outcome(&report, "mcp_servers.demo").action,
            SharedAction::Removed
        );
        // 清单里那句理由必须原样带到报告里：duster 凭什么动别人的文件，
        // 用户有权看见。
        assert_eq!(
            outcome(&report, "/mcpServers/demo").reason,
            "MCP entry pointing at this agent"
        );

        assert_eq!(
            fs::read_to_string(home.join(".claude.json")).unwrap(),
            r#"{
  // MCP servers every agent on this machine reads.
  "mcpServers": {
    "keeper": {
      "command": "keeper-mcp",
      "args": ["--stdio"]
    }
  },
  "theme": "dark"
}
"#
        );
        assert_eq!(
            fs::read_to_string(home.join(".codex/config.toml")).unwrap(),
            r#"# Codex's own settings.
model = "gpt-5"

[mcp_servers.keeper]
command = "keeper-mcp"
args = ["--stdio"]
"#
        );

        // 改写别人的文件之前必须留一份整文件快照，两个文件各一份。
        let snaps = home.join(".agent-duster/snapshots");
        let mut found = Vec::new();
        for op in fs::read_dir(&snaps).expect("必须建出快照目录").flatten() {
            found.extend(walkdir(&op.path()));
        }
        assert_eq!(found.len(), 2, "两个被改写的文件各一份快照：{found:?}");
        let bodies: Vec<String> = found
            .iter()
            .map(|p| fs::read_to_string(p).unwrap())
            .collect();
        assert!(bodies.contains(&CLAUDE_JSON.to_string()), "{bodies:?}");
        assert!(bodies.contains(&CODEX_TOML.to_string()), "{bodies:?}");
    }

    /// M2 验收线的后半句：**`package` 一栏只打印命令，绝不执行。**
    ///
    /// 卸载脚本被指到一个真跑就会留下哨兵文件的脚本上。哨兵不存在，
    /// 才算这条规矩没被破。
    #[test]
    fn package_一栏只打印命令绝不执行() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = demo_home(home, 0);

        let report = uninstall(&demo_opts(home, &index_path)).unwrap();
        let p = &report.packages[0];
        assert_eq!(p.manager, "npm");
        assert_eq!(p.detected, DetectStatus::Installed);
        assert_eq!(p.action, PackageAction::Printed);
        assert!(p.output.is_none());
        // 命令必须以可粘贴的文本出现，带空格的参数还要带对引号。
        assert!(
            p.command.ends_with("remove.sh --global 'demo agent'"),
            "{}",
            p.command
        );
        assert_eq!(p.argv.last().unwrap(), "demo agent");
        assert!(
            p.detail
                .as_deref()
                .unwrap()
                .contains("never runs a package manager"),
            "{:?}",
            p.detail
        );
        assert!(
            !sentinel(home).exists(),
            "duster 代跑了包管理器——这条铁律被破了"
        );
    }

    /// 显式的例外仍然要是真的：给了 `--run-package-manager` 就得真跑，
    /// 否则那个旗标只是装饰。
    #[test]
    fn run_package_manager_才真跑并带回输出() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = demo_home(home, 0);
        let mut opts = demo_opts(home, &index_path);
        opts.run_package_manager = true;

        let report = uninstall(&opts).unwrap();
        let p = &report.packages[0];
        assert_eq!(p.action, PackageAction::Executed);
        assert_eq!(p.output.as_deref(), Some("removed\n"));
        assert!(sentinel(home).is_file());
    }

    /// 探测说没装就跳过，且**不能**因此把命令藏起来不报——用户还是要知道
    /// duster 看的是哪一条线索。
    #[test]
    fn 探测说没装就跳过() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = demo_home(home, 1);

        let report = uninstall(&demo_opts(home, &index_path)).unwrap();
        let p = &report.packages[0];
        assert_eq!(p.detected, DetectStatus::NotInstalled);
        assert_eq!(p.action, PackageAction::Skipped);
        assert!(p.command.contains("remove.sh"), "{}", p.command);
        assert!(!sentinel(home).exists());
    }

    /// 指纹漂移即拒绝改写：那份文件一个字节都不许动，理由要点名对不上的是
    /// 哪一个基准，而独占树该删还是照删——一个 agent 升级了配置格式，
    /// 不该连累整场卸载。
    #[test]
    fn 指纹漂移即拒绝改写但独占树照删() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = demo_home(home, 0);

        // 只污染 JSON 那一份的基准，TOML 那一份保持"首次见到"。
        {
            let idx = Index::open(&index_path).unwrap();
            meta::set(
                idx.conn(),
                &guard_slot(&home.join(".claude.json")),
                "0123456789abcdef",
            )
            .unwrap();
        }

        let report = uninstall(&demo_opts(home, &index_path)).unwrap();
        assert!(report.executed);

        let refused = outcome(&report, "/mcpServers/demo");
        assert_eq!(refused.action, SharedAction::Refused);
        let detail = refused.detail.as_deref().unwrap();
        assert!(detail.contains("drifted"), "{detail}");
        assert!(
            detail.contains("0123456789abcdef"),
            "对不上的是哪个基准要点名：{detail}"
        );
        assert!(refused.snapshot.is_none(), "没动就不该留快照");

        // 被拒的那一份逐字节原样。
        assert_eq!(
            fs::read_to_string(home.join(".claude.json")).unwrap(),
            CLAUDE_JSON
        );
        // 其余步骤照走：独占树没了，另一处共享键也删掉了。
        assert!(!home.join(".demo-agent").exists());
        assert_eq!(
            outcome(&report, "mcp_servers.demo").action,
            SharedAction::Removed
        );
        // 拒绝必须进 warnings：CLI 靠它把退出码降成"部分成功"。
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("refused to remove")),
            "{:?}",
            report.warnings
        );
    }

    /// 预览把三栏列全，然后一个字节都不动——这是这个动词唯一的防线。
    #[test]
    fn dry_run_列全三栏且不动任何字节() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = demo_home(home, 0);
        let mut opts = demo_opts(home, &index_path);
        opts.dry_run = true;

        let report = uninstall(&opts).unwrap();
        assert!(!report.executed);

        // owns：独占树成项。
        assert!(
            report
                .plan
                .items
                .iter()
                .any(|i| i.path == home.join(".demo-agent")),
            "{:?}",
            report.plan.items
        );
        // shared：两条都在，都带着清单里那句理由，动作是"将要删"。
        assert_eq!(report.shared.len(), 2);
        for s in &report.shared {
            assert_eq!(s.action, SharedAction::Planned);
            assert!(!s.reason.is_empty());
            assert!(s.snapshot.is_none());
        }
        // package：命令列出来了。
        assert_eq!(report.packages.len(), 1);
        assert_eq!(report.packages[0].action, PackageAction::Printed);

        // 盘上一个字节都没变，快照目录也没建。
        assert!(home.join(".demo-agent/state/blob.bin").is_file());
        assert_eq!(
            fs::read_to_string(home.join(".claude.json")).unwrap(),
            CLAUDE_JSON
        );
        assert_eq!(
            fs::read_to_string(home.join(".codex/config.toml")).unwrap(),
            CODEX_TOML
        );
        assert!(!home.join(".agent-duster/snapshots").exists());
        assert!(!sentinel(home).exists());
        // 预览不落闸门基准：一次 dry-run 不该替用户把"第一次见到的形状"定死。
        let idx = Index::open_readonly(&index_path).unwrap();
        assert_eq!(
            meta::get(idx.conn(), &guard_slot(&home.join(".claude.json"))).unwrap(),
            None
        );
    }

    /// 卸载两遍不是错误。第二遍：键本来就不在了，如实报 absent，
    /// 文件依旧逐字节不动。
    #[test]
    fn 卸载两遍不报错且第二遍报告键已不在() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = demo_home(home, 0);

        let first = uninstall(&demo_opts(home, &index_path)).unwrap();
        assert_eq!(
            outcome(&first, "/mcpServers/demo").action,
            SharedAction::Removed
        );
        let after_first = fs::read_to_string(home.join(".claude.json")).unwrap();

        let second = uninstall(&demo_opts(home, &index_path)).unwrap();
        assert!(second.executed);
        for key in ["/mcpServers/demo", "mcp_servers.demo"] {
            let o = outcome(&second, key);
            assert_eq!(o.action, SharedAction::Absent, "{key}: {:?}", o.detail);
            assert!(o.detail.as_deref().unwrap().contains("already removed"));
        }
        assert_eq!(
            fs::read_to_string(home.join(".claude.json")).unwrap(),
            after_first
        );
    }

    /// 共享文件根本不在盘上时报 missing，不是错误也不是拒绝——
    /// 别的 agent 没装，本来就没有那份配置。
    #[test]
    fn 共享文件不存在即报_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let index_path = demo_home(home, 0);
        fs::remove_file(home.join(".claude.json")).unwrap();

        let report = uninstall(&demo_opts(home, &index_path)).unwrap();
        let o = outcome(&report, "/mcpServers/demo");
        assert_eq!(o.action, SharedAction::Missing);
        assert!(o.detail.as_deref().unwrap().contains("not on disk"));
    }
}
