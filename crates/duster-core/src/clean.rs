//! `duster clean`：回收**程序自己产生、删了会自动重建**的东西（l0 + l1）。
//!
//! # 为什么真删、不归档、不进回收站
//!
//! 缓存的撤销毫无意义，而它恰恰是体积最大的一档（本机 774.6 MB 全在这里）。
//! 进回收站等于「承诺清理却一个字节没释放」。何况 l0 的两种操作——
//! VACUUM 是原地重写、日志截断是原地改——**根本不存在"一个文件可以被移走"**。
//!
//! # 同意模型
//!
//! 无需逐项同意，`--yes` 友好，可以养成习惯。这正是它必须和 `prune` 分开的
//! 原因：一个人 `duster clean --yes` 敲过 50 次之后就不再读输出。
//!
//! # 收尾三桶报告
//!
//! 执行完必须报出另外两桶的体量与去处，否则用户以为 duster 只能回收这点：
//! 已回收 X（本次真实释放）/ 陈旧资源 Y → `duster prune --older-than` /
//! 软件本体 Z → 只统计，整体不要了用 `duster uninstall <agent>`。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_fs::lockprobe::{self, LockStatus};
use duster_model::CleanLevel;

use crate::plan::{Action, Plan, PlanFilter, PlanItem, PlanOptions};

/// clean 的输入。
#[derive(Debug, Clone, Default)]
pub struct CleanOptions {
    pub index_path: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// 只清这几个 agent；空 Vec = 全部。
    pub agents: Vec<String>,
    /// true = 只出计划不执行。**默认 true**，执行是显式行为。
    pub dry_run: bool,
    /// 跳过问句。**不跳过清单打印**。
    pub yes: bool,
}

/// 三桶：这次回收了多少、还有多少陈旧的、多少是碰不得的软件本体。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Buckets {
    /// 本次**真实释放**的字节数（执行后实测，不是计划里的预估）。
    pub reclaimed: u64,
    /// 陈旧资源合计，去处是 `duster prune --older-than`。
    pub stale: u64,
    /// 软件本体合计，去处是 `duster uninstall <agent>`。
    pub install: u64,
}

/// 单项执行结果。
#[derive(Debug, Clone, Serialize)]
pub struct CleanOutcome {
    pub path: String,
    pub action: String,
    /// 执行前占用。
    pub before: u64,
    /// 执行后占用（删除即 0，VACUUM 是重写后的实际大小）。
    pub after: u64,
    /// 实际释放。
    pub freed: u64,
    /// 非致命失败（被锁、权限不足）；有值即该项未执行。
    pub error: Option<String>,
}

/// clean 的完整报告。
#[derive(Debug, Clone, Serialize)]
pub struct CleanReport {
    pub plan: Plan,
    /// false = dry-run，一个字节都没动。
    pub executed: bool,
    pub outcomes: Vec<CleanOutcome>,
    pub buckets: Buckets,
    pub warnings: Vec<String>,
}

/// 生成计划 →（非 dry-run 时）逐项执行 → 汇总三桶。
///
/// 硬要求：
/// - **每一项删除前过锁探测**，命中即跳过该项并记 error，不中断整体；
/// - 半途失败不回滚已完成的项（缓存删一半也是干净的），但必须逐项如实报告；
/// - `reclaimed` 是**执行后实测**的释放量，不是计划预估——l0 的预估是保守值，
///   实际 VACUUM 通常还多挤出一点，报预估等于对不上账。
pub fn clean(opts: &CleanOptions) -> Result<CleanReport> {
    clean_filtered(opts, &PlanFilter::default())
}

/// 同 [`clean`]，但在出计划后套一层 [`PlanFilter`]。
///
/// 存在的唯一理由是交互式 `duster clean`：菜单里的清单是一张勾选列表，
/// 用户勾过的才动。取舍必须记成白名单而不是黑名单——两层之间隔着用户
/// 读清单的时间，重算时新冒出来的项在黑名单下会被执行，而用户从没见过它
/// （为什么，见 [`PlanFilter`] 所在模块的模块头）。用户勾掉的项从计划中
/// 整条剔除：不执行、不出现在对账单里、也不计进承诺的回收量。
///
/// `reclaim_bytes` 由 [`PlanFilter::apply`] 同步重算，否则报告会承诺一个
/// 它不会释放的数字。`stale_bytes` / `install_bytes` 不动：那两桶讲的是
/// 别的动词的地盘，跳过一条待清项不会让它们变小。
pub fn clean_filtered(opts: &CleanOptions, filter: &PlanFilter) -> Result<CleanReport> {
    // 唯一会中止整体的失败：计划都建不出来，后面每一步都失去依据。
    let mut plan = crate::plan::plan_clean(&PlanOptions {
        index_path: opts.index_path.clone(),
        home: opts.home.clone(),
        agents: opts.agents.clone(),
        // clean 看不见 l2，陈旧阈值对它没有意义（`stale_bytes` 只是报个数字）。
        older_than_days: None,
        keep_generations: false,
        now_ms: None,
    })?;
    filter.apply(&mut plan);
    let plan = plan;

    let mut warnings = plan.warnings.clone();

    if opts.dry_run {
        // dry-run **一个字节都没释放**。把预估填进 `reclaimed` 正是这个里程碑
        // 要消灭的那句谎话——预估在 `plan.reclaim_bytes` 里，它自己有名字。
        let buckets = Buckets {
            reclaimed: 0,
            stale: plan.stale_bytes,
            install: plan.install_bytes,
        };
        return Ok(CleanReport {
            plan,
            executed: false,
            outcomes: Vec::new(),
            buckets,
            warnings,
        });
    }

    // `actionable()` 已经滤掉 Keep 与 not cleanable（`install` 行在计划里
    // 只为了让用户看见那几个 GB，绝不进执行器）。先克隆出来，好让 plan
    // 原样进报告。
    let items: Vec<PlanItem> = plan.actionable().cloned().collect();
    let mut outcomes = Vec::with_capacity(items.len());
    let mut reclaimed = 0u64;

    for item in &items {
        // 这里先探一次锁，为的是拿到 `Unknown` 的理由塞进 warnings——
        // 执行器的返回值里没有地方放它。执行器自己还会再探一次：它是公开
        // API，安全性不能寄托在调用方身上，多一次探测换一个不可绕过的保证。
        match lockprobe::probe(&item.path) {
            LockStatus::Locked { reason } => {
                let msg = format!(
                    "{} is locked by another process: {reason}. Quit the agent and try again",
                    item.path.display()
                );
                warnings.push(msg.clone());
                outcomes.push(skipped(item, msg));
                continue;
            }
            LockStatus::Unknown { reason } => warnings.push(format!(
                "{}: lock probe unavailable ({reason}); the item was cleaned without that check",
                item.path.display()
            )),
            LockStatus::Free => {}
        }

        let result = match item.clean_level {
            Some(CleanLevel::L0) => exec_l0(item),
            Some(CleanLevel::L1) => exec_l1(item),
            // l2 归 prune，无级别的行根本不该出现在 clean 的计划里。
            // 真出现了就是计划层的 bug——跳过并说出来，不要闷头删。
            other => Err(anyhow::anyhow!(
                "clean refuses {} : clean_level {:?} is not l0/l1",
                item.path.display(),
                other
            )),
        };

        match result {
            Ok(outcome) => {
                if let Some(err) = &outcome.error {
                    warnings.push(format!("{}: {err}", outcome.path));
                }
                reclaimed += outcome.freed;
                outcomes.push(outcome);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                warnings.push(msg.clone());
                outcomes.push(skipped(item, msg));
            }
        }
    }

    // 三桶。`reclaimed` 是逐项实测 `freed` 的和，与 `plan.reclaim_bytes`
    // （预估）刻意分开：对不上账的时候，两个数摆在一起才看得出差在哪。
    let buckets = Buckets {
        reclaimed,
        stale: plan.stale_bytes,
        install: plan.install_bytes,
    };
    Ok(CleanReport {
        plan,
        executed: true,
        outcomes,
        buckets,
        warnings,
    })
}

/// 未执行的一项。`before == after`、`freed == 0`——报告里一眼看得出没动过。
///
/// `before` 取计划里的预估占用：这一项压根没被摸过，为了填个数去 stat 它
/// 反而是在假装测过。
fn skipped(item: &PlanItem, error: String) -> CleanOutcome {
    CleanOutcome {
        path: item.path.display().to_string(),
        action: action_name(item.action).to_string(),
        before: item.bytes,
        after: item.bytes,
        freed: 0,
        error: Some(error),
    }
}

/// 与 `Action` 的 serde 名字（snake_case）保持一致，报告里人机同文。
fn action_name(action: Action) -> &'static str {
    match action {
        Action::Vacuum => "vacuum",
        Action::RemoveSidecar => "remove_sidecar",
        Action::RemoveFile => "remove_file",
        Action::RemoveDir => "remove_dir",
        Action::TruncateFile => "truncate_file",
        Action::CompressFile => "compress_file",
        Action::Keep => "keep",
    }
}

/// 实测占用：目录并行统计整棵树，文件/符号链接取自身大小，不存在为 0。
///
/// 执行前后各取一次，`freed = before - after`。**只用实测值**——预估是
/// 计划的事，报告要能和 `du` 对上。
fn occupied_bytes(path: &Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Err(_) => 0,
        Ok(md) if md.is_dir() => {
            duster_fs::walk::walk_stats(path, &duster_fs::walk::WalkOptions::default())
                .map(|s| s.total_bytes)
                .unwrap_or(0)
        }
        Ok(md) => md.len(),
    }
}

/// 组装一条实测结果。
fn measured(item: &PlanItem, before: u64, after: u64, error: Option<String>) -> CleanOutcome {
    CleanOutcome {
        path: item.path.display().to_string(),
        action: action_name(item.action).to_string(),
        before,
        after,
        freed: before.saturating_sub(after),
        error,
    }
}

/// L0 执行器：SQLite VACUUM、孤儿 WAL/SHM、`.tmp-*` 残留。
///
/// 无损机械操作，但**仍必须过锁探测**。
/// 本机首个验收目标：`~/.codex/logs_2.sqlite` 784 MB → 约 10 MB，
/// 11353 行日志一条不少（VACUUM 前后行数校验，不等即报错并放弃）。
pub fn exec_l0(item: &PlanItem) -> Result<CleanOutcome> {
    let path = item.path.as_path();
    // "机械"不等于"可以在别人正用着的时候做"。命中即跳过，没有 `--force`。
    if let Err(e) = lockprobe::ensure_free(path) {
        return Ok(skipped(item, e.to_string()));
    }

    match item.action {
        Action::Vacuum => match duster_index::maintenance::vacuum(path) {
            Ok(v) => Ok(measured(item, v.before, v.after, None)),
            // 行数对不上、临时空间不够、库中途被占——都只毁掉这一项。
            Err(e) => Ok(skipped(item, format!("{e:#}"))),
        },

        Action::RemoveSidecar if path.is_dir() => {
            // 目录形态的 RemoveSidecar 就是 `.tmp-*` 崩溃残留清理。
            let before: u64 = tmp_residue(path)?.iter().map(|(_, n)| n).sum();
            let freed = remove_tmp_residue(path)?;
            Ok(measured(item, before, before.saturating_sub(freed), None))
        }

        Action::RemoveSidecar => {
            // 传进来的是主库路径；能不能删由 `orphan_sidecars` 判定，
            // 它拿不准就返回空，这里跟着什么都不做。
            let (sidecars, before) = duster_index::maintenance::orphan_sidecars(path)?;
            let mut failed = Vec::new();
            for p in &sidecars {
                if let Err(e) = std::fs::remove_file(p) {
                    failed.push(format!("{}: {e}", p.display()));
                }
            }
            let after: u64 = sidecars
                .iter()
                .filter_map(|p| p.metadata().ok())
                .map(|m| m.len())
                .sum();
            let error = (!failed.is_empty())
                .then(|| format!("failed to remove sidecars: {}", failed.join("; ")));
            Ok(measured(item, before, after, error))
        }

        other => bail!(
            "exec_l0 does not handle action `{}` ({})",
            action_name(other),
            path.display()
        ),
    }
}

/// L1 执行器：按清单 artifact 声明删缓存 / 裁日志。
///
/// 设计点：
/// - **日志原子截断**：`set_len(0)` 而不是删文件——正被 agent 追加写的文件
///   删掉之后，持有 fd 的进程还在往一个已 unlink 的 inode 写，空间根本没释放；
/// - **活跃文件**：截断前过锁探测；被独占就跳过并说明；
/// - **半途失败**：目录删到一半不回滚（缓存本就可再生），但要如实报告
///   已删多少、剩下什么。
pub fn exec_l1(item: &PlanItem) -> Result<CleanOutcome> {
    let path = item.path.as_path();
    if let Err(e) = lockprobe::ensure_free(path) {
        return Ok(skipped(item, e.to_string()));
    }

    match item.action {
        Action::TruncateFile => {
            let before = occupied_bytes(path);
            // 截断而不是 unlink：agent 还持着 fd 的话，unlink 掉的 inode
            // 依然占着空间——"删了"却一个字节没释放，比不删更糟。
            let r = OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|f| f.set_len(0));
            match r {
                Ok(()) => Ok(measured(item, before, 0, None)),
                Err(e) => Ok(measured(
                    item,
                    before,
                    before,
                    Some(format!("failed to truncate {}: {e}", path.display())),
                )),
            }
        }

        Action::RemoveDir => {
            let before = occupied_bytes(path);
            let err = std::fs::remove_dir_all(path).err();
            // 删到一半不回滚——缓存按定义可再生，回滚反而要先把它复制一份。
            // 但 `after` 必须是重新实测的剩余量，报告里如实写还剩多少。
            let after = occupied_bytes(path);
            Ok(measured(
                item,
                before,
                after,
                err.map(|e| format!("failed to remove {}: {e}", path.display())),
            ))
        }

        Action::RemoveFile => {
            let before = occupied_bytes(path);
            let err = std::fs::remove_file(path).err();
            let after = occupied_bytes(path);
            Ok(measured(
                item,
                before,
                after,
                err.map(|e| format!("failed to remove {}: {e}", path.display())),
            ))
        }

        other => bail!(
            "exec_l1 does not handle action `{}` ({})",
            action_name(other),
            path.display()
        ),
    }
}

/// `.tmp-*` 崩溃残留清理：在 `dir` 一级下删除 `.tmp-duster-*` 与
/// agent 自己的 `.tmp-*` 残留文件，返回释放字节数。
///
/// 只删**一级**、只删文件、只删名字明确是临时前缀的——递归找 tmp 是在
/// 给自己找麻烦，一个叫 `.tmp-important` 的用户文件不该被误伤。
pub fn remove_tmp_residue(dir: &Path) -> Result<u64> {
    let mut freed = 0u64;
    for (path, bytes) in tmp_residue(dir)? {
        // 单个删不掉（权限、正被打开）不算整体失败：剩下的会出现在
        // 调用方实测的 `after` 里，不需要靠一个 Err 把整轮清理拖垮。
        if std::fs::remove_file(&path).is_ok() {
            freed += bytes;
        }
    }
    Ok(freed)
}

/// `dir` 一级下的临时残留文件及其字节数。
///
/// 判据只有一条：**文件**（不含目录、符号链接）且文件名以 `.tmp-` 开头。
/// 递归下去找 `tmp` 是在给自己找麻烦——深处一个叫 `.tmp-important` 的
/// 用户文件不该被误伤，一个叫 `.tmp-dir` 的目录也不是残留。
/// 目录不存在返回空：没扫过就没有残留，不是错。
fn tmp_residue(dir: &Path) -> Result<Vec<(PathBuf, u64)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::Error::from(e))
                .context(format!("failed to read directory {}", dir.display()));
        }
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(".tmp-") {
            continue;
        }
        // `file_type()` 走 lstat：指向别处的符号链接同样不删。
        match entry.file_type() {
            Ok(ft) if ft.is_file() => {}
            _ => continue,
        }
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push((entry.path(), bytes));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    use duster_index::db::Index;
    use duster_index::upsert::{self, ResourceRow};

    /// 只要它还在磁盘上任何角落出现，就说明有人偷偷留了一份副本。
    const MARKER: &[u8] = b"duster-clean-marker-4f21c0";

    fn plan_item(path: &Path, action: Action, level: CleanLevel, bytes: u64) -> PlanItem {
        PlanItem {
            agent_id: "fixture".into(),
            kind: "artifact".into(),
            clean_level: Some(level),
            path: path.to_path_buf(),
            key: "fixture".into(),
            rid: -1,
            what: "fixture".into(),
            why: "fixture".into(),
            impact: "fixture".into(),
            archived: false,
            action,
            bytes,
            cleanable: true,
            generation: false,
            last_used_ms: None,
        }
    }

    /// 最小索引：一个 agent + 一行 l1 artifact（缓存目录）。返回 (缓存目录, 真实字节数)。
    fn seed(home: &Path) -> (PathBuf, u64) {
        let cache = home.join(".fixture").join("cache");
        std::fs::create_dir_all(cache.join("deep")).unwrap();
        std::fs::write(cache.join("a.bin"), vec![b'A'; 1000]).unwrap();
        std::fs::write(cache.join("deep").join("b.bin"), vec![b'B'; 2345]).unwrap();
        std::fs::write(cache.join("marker.bin"), MARKER).unwrap();
        let bytes = 1000 + 2345 + MARKER.len() as u64;
        assert_eq!(occupied_bytes(&cache), bytes);

        let db = home.join(".agent-duster").join("index.db");
        let idx = Index::open(&db).unwrap();
        upsert::upsert_agent(
            idx.conn(),
            &duster_model::AgentInfo {
                id: "fixture".into(),
                display_name: "Fixture".into(),
                root: home.join(".fixture"),
                version: None,
            },
            0,
        )
        .unwrap();
        upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: "fixture".into(),
                kind: "artifact".into(),
                scope: "global".into(),
                key: "cache".into(),
                path: cache.display().to_string(),
                size: bytes,
                mtime_ns: 0,
                hash_content: None,
                cheap_print: None,
                clean_level: Some("l1".into()),
                reclaimable: Some(bytes),
                install_bytes: None,
            },
        )
        .unwrap();
        // Index 持有单实例写锁，必须先放手再让 plan_clean 去读。
        drop(idx);

        (cache, bytes)
    }

    /// 整棵树里有没有哪个文件装着 `MARKER`。
    fn holds_marker(root: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(root) else {
            return false;
        };
        for e in entries.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => {
                    if holds_marker(&p) {
                        return true;
                    }
                }
                Ok(t) if t.is_file() => {
                    if let Ok(bytes) = std::fs::read(&p)
                        && bytes.windows(MARKER.len()).any(|w| w == MARKER)
                    {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// 只删一级、只删文件、只删 `.tmp-` 前缀。
    #[test]
    fn remove_tmp_residue_只碰一级的临时文件() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join(".tmp-duster-x"), vec![b'a'; 100]).unwrap();
        std::fs::write(root.join(".tmp-y"), vec![b'b'; 50]).unwrap();
        std::fs::create_dir(root.join(".tmp-dir")).unwrap();
        std::fs::write(root.join(".tmp-dir").join("inner"), vec![b'c'; 7]).unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join(".tmp-y"), vec![b'd'; 30]).unwrap();
        std::fs::write(root.join("mytmp-z"), vec![b'e'; 11]).unwrap();
        std::fs::write(root.join(".tmp-important.keep"), vec![b'f'; 3]).unwrap();

        let freed = remove_tmp_residue(root).unwrap();
        assert_eq!(
            freed,
            100 + 50 + 3,
            "一级下的 `.tmp-` 文件，一个不多一个不少"
        );

        assert!(!root.join(".tmp-duster-x").exists());
        assert!(!root.join(".tmp-y").exists());
        assert!(root.join(".tmp-dir").is_dir(), "同名目录不是残留");
        assert!(root.join(".tmp-dir").join("inner").is_file());
        assert!(root.join("sub").join(".tmp-y").is_file(), "不递归");
        assert!(root.join("mytmp-z").is_file(), "名字里带 tmp 不等于是残留");
    }

    /// 截断，不 unlink：文件还在，还是同一个 inode。
    #[test]
    fn exec_l1_截断日志且保留同一个_inode() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("agent.log");
        std::fs::write(&log, vec![b'x'; 4096]).unwrap();
        let ino_before = std::fs::metadata(&log).unwrap().ino();

        let out = exec_l1(&plan_item(&log, Action::TruncateFile, CleanLevel::L1, 4096)).unwrap();
        assert!(out.error.is_none(), "{out:#?}");
        assert_eq!((out.before, out.after, out.freed), (4096, 0, 4096));

        let md = std::fs::metadata(&log).expect("日志必须原地留着");
        assert_eq!(md.len(), 0);
        assert_eq!(md.ino(), ino_before, "同一个 inode——不是删了再建一个");
    }

    /// 目录形态的 `RemoveSidecar` 走残留清理，实测量必须对得上。
    #[test]
    fn exec_l0_目录形态清理_tmp_残留() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".tmp-duster-a"), vec![b'a'; 64]).unwrap();
        std::fs::write(root.join("keep.json"), vec![b'k'; 9]).unwrap();

        let out = exec_l0(&plan_item(root, Action::RemoveSidecar, CleanLevel::L0, 64)).unwrap();
        assert!(out.error.is_none(), "{out:#?}");
        assert_eq!((out.before, out.after, out.freed), (64, 0, 64));
        assert!(root.join("keep.json").is_file());
    }

    /// 主库形态的 `RemoveSidecar`：空 WAL 是孤儿，可以收；主库分毫不动。
    #[test]
    fn exec_l0_只收空的孤儿_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("logs.sqlite");
        // 借 duster 自己的建库路径造一个真库，免得手搓文件头。
        drop(Index::open(&db).unwrap());
        let wal = dir.path().join("logs.sqlite-wal");
        std::fs::write(&wal, b"").unwrap();

        let out = exec_l0(&plan_item(&db, Action::RemoveSidecar, CleanLevel::L0, 0)).unwrap();
        assert!(out.error.is_none(), "{out:#?}");
        assert!(!wal.exists(), "空 WAL 该被收走");
        assert!(db.is_file(), "主库不是旁路文件，绝不能碰");
    }

    /// 缓存目录整棵删掉，`before/after` 是实测值。
    #[test]
    fn exec_l1_删整棵缓存目录() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(cache.join("x")).unwrap();
        std::fs::write(cache.join("x").join("f"), vec![b'z'; 777]).unwrap();

        let out = exec_l1(&plan_item(&cache, Action::RemoveDir, CleanLevel::L1, 777)).unwrap();
        assert!(out.error.is_none(), "{out:#?}");
        assert_eq!((out.before, out.after, out.freed), (777, 0, 777));
        assert!(!cache.exists());
    }

    /// dry-run 一个字节都不动，`reclaimed` 必须是 0，预估只许待在计划里。
    #[test]
    fn clean_dry_run_不动磁盘且回收量为零() {
        let home = tempfile::tempdir().unwrap();
        let (cache, bytes) = seed(home.path());

        let report = clean(&CleanOptions {
            home: Some(home.path().to_path_buf()),
            dry_run: true,
            ..Default::default()
        })
        .unwrap();

        assert!(!report.executed);
        assert!(report.outcomes.is_empty());
        assert_eq!(report.buckets.reclaimed, 0, "dry-run 什么也没释放");
        assert_eq!(report.plan.reclaim_bytes, bytes, "预估待在它自己的字段里");
        assert_eq!(occupied_bytes(&cache), bytes, "磁盘上原封不动");
        assert!(cache.join("marker.bin").is_file());
    }

    /// 执行：`reclaimed` 等于实测释放量，原件真的没了，磁盘上也没有第二份。
    #[test]
    fn clean_执行后回收量属实且不留任何副本() {
        let home = tempfile::tempdir().unwrap();
        let (cache, bytes) = seed(home.path());
        assert!(holds_marker(home.path()), "清理前标记当然在");

        let report = clean(&CleanOptions {
            home: Some(home.path().to_path_buf()),
            dry_run: false,
            yes: true,
            ..Default::default()
        })
        .unwrap();

        assert!(report.executed);
        let failed: Vec<_> = report
            .outcomes
            .iter()
            .filter(|o| o.error.is_some())
            .collect();
        assert!(failed.is_empty(), "不该有失败项：{failed:#?}");
        assert_eq!(report.buckets.reclaimed, bytes, "报的必须是实测释放量");
        assert_eq!(
            report.outcomes.iter().map(|o| o.freed).sum::<u64>(),
            report.buckets.reclaimed,
            "三桶的第一个数就是逐项实测之和"
        );

        assert!(!cache.exists(), "clean 是真删，不是搬家");
        assert!(report.plan.archive_roots().is_empty(), "缓存永不入归档包");
        assert!(
            !holds_marker(home.path()),
            "磁盘上不许还留着一份——没有回收站，这一条就是 clean 的定义"
        );
    }

    /// 再补一行 l1 artifact 缓存目录（`seed` 已经建过 agent 行）。
    fn seed_more(home: &Path, key: &str, fill: u8, n: usize) -> (PathBuf, u64) {
        let dir = home.join(".fixture").join(key);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.bin"), vec![fill; n]).unwrap();

        let idx = Index::open(&home.join(".agent-duster").join("index.db")).unwrap();
        upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: "fixture".into(),
                kind: "artifact".into(),
                scope: "global".into(),
                key: key.into(),
                path: dir.display().to_string(),
                size: n as u64,
                mtime_ns: 0,
                hash_content: None,
                cheap_print: None,
                clean_level: Some("l1".into()),
                reclaimable: Some(n as u64),
                install_bytes: None,
            },
        )
        .unwrap();
        drop(idx);
        (dir, n as u64)
    }

    /// 交互式勾选里取消掉的那一条：整条剔除——不执行、不出现在对账单里、
    /// 也不计进承诺的回收量；同一批里没取消的照常清掉。
    ///
    /// 菜单的部分勾选全压在这条上：报一个它不会释放的数字，或者顺手把用户
    /// 明确留下的那条也删了，两者都是这个命令最不能犯的错。
    #[test]
    fn clean_filtered_跳过的项分毫不动_其余照清() {
        let home = tempfile::tempdir().unwrap();
        let (cache, cache_bytes) = seed(home.path());
        let (kept, kept_bytes) = seed_more(home.path(), "keep-cache", b'K', 4096);

        let report = clean_filtered(
            &CleanOptions {
                home: Some(home.path().to_path_buf()),
                dry_run: false,
                yes: true,
                ..Default::default()
            },
            &PlanFilter::skipping(vec![kept.clone()]),
        )
        .unwrap();

        assert!(report.executed);
        assert_eq!(
            report.plan.reclaim_bytes, cache_bytes,
            "承诺的回收量必须把跳过的那条减掉"
        );
        assert_eq!(report.buckets.reclaimed, cache_bytes, "实测释放量同理");
        assert!(!cache.exists(), "没被跳过的那条照常清掉");
        assert_eq!(
            occupied_bytes(&kept),
            kept_bytes,
            "跳过的那条一个字节都不许动"
        );
        let kept_str = kept.display().to_string();
        assert!(
            report.outcomes.iter().all(|o| o.path != kept_str),
            "跳过的那条连对账单都不该出现：{:#?}",
            report.outcomes
        );
        assert!(
            report.plan.items.iter().all(|i| i.path != kept),
            "计划里也不该留着它——否则 --json 的读者会以为它被动过"
        );
    }

    /// 白名单挡住计划外的新项：菜单上只有用户勾过的那几条会动，计划重算时
    /// 新冒出来的项自动落在名单外。这是 allow 与 skip 的本质区别——skip
    /// 只会让名单变窄，挡不住「用户从没见过」的新项。
    #[test]
    fn clean_filtered_白名单挡住计划外的新项() {
        let home = tempfile::tempdir().unwrap();
        let (cache, cache_bytes) = seed(home.path());
        let (other, other_bytes) = seed_more(home.path(), "other-cache", b'O', 8192);

        let report = clean_filtered(
            &CleanOptions {
                home: Some(home.path().to_path_buf()),
                dry_run: false,
                yes: true,
                ..Default::default()
            },
            &PlanFilter::allow_only(vec![(cache.clone(), Action::RemoveDir)]),
        )
        .unwrap();

        assert!(report.executed);
        assert_eq!(
            report.plan.reclaim_bytes, cache_bytes,
            "承诺的回收量只算勾过的那条"
        );
        assert_eq!(report.buckets.reclaimed, cache_bytes, "实测释放量同理");
        assert!(!cache.exists(), "勾过的那条照常清掉");
        assert_eq!(occupied_bytes(&other), other_bytes, "没勾的那条分毫不动");
        let other_str = other.display().to_string();
        assert!(
            report.outcomes.iter().all(|o| o.path != other_str),
            "没勾的那条连对账单都不该出现：{:#?}",
            report.outcomes
        );
        assert!(
            report.plan.items.iter().all(|i| i.path != other),
            "计划里也不该留着它——否则 --json 的读者会以为它被动过"
        );
    }

    /// allow 里放一个计划里根本不存在的路径：不 panic、不误伤——结果
    /// 等同于 allow 只含交集。幽灵路径只可能缩小名单，不可能扩大。
    #[test]
    fn clean_filtered_allow里的幽灵路径不panic不误伤() {
        let home = tempfile::tempdir().unwrap();
        let (cache, cache_bytes) = seed(home.path());
        let (other, other_bytes) = seed_more(home.path(), "other-cache", b'O', 8192);
        let phantom = home.path().join(".fixture").join("never-existed");

        let report = clean_filtered(
            &CleanOptions {
                home: Some(home.path().to_path_buf()),
                dry_run: false,
                yes: true,
                ..Default::default()
            },
            &PlanFilter::allow_only(vec![
                (cache.clone(), Action::RemoveDir),
                (phantom, Action::RemoveDir),
            ]),
        )
        .unwrap();

        assert_eq!(
            report.plan.reclaim_bytes, cache_bytes,
            "幽灵路径不该改变回收量"
        );
        assert_eq!(report.buckets.reclaimed, cache_bytes);
        assert!(!cache.exists(), "勾过的那条照常清掉");
        assert_eq!(
            occupied_bytes(&other),
            other_bytes,
            "幽灵路径不许误伤别的项"
        );
    }
}
