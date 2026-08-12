//! 锁探测：目标正被使用就拒绝操作。
//!
//! 覆盖两类占用，判据不同：
//! - **SQLite 库**：文件还在、进程也许早退了，但 WAL 里可能有未落盘的事务，
//!   或者另一个进程正持有写锁。用 `BEGIN IMMEDIATE` 试探（立即回滚），
//!   拿不到写锁就是被占用。VACUUM 本来也要这把锁，探测和执行的判据一致。
//! - **普通文件/目录**：有进程打开着它就不能删（正在追加写的日志、
//!   浏览器 profile 的锁文件）。走 `lsof`；系统上没有 `lsof` 时**降级为放行**
//!   并在结果里说明——探测能力缺失不该变成"什么都不敢做"，但必须让用户知道。
//!
//! 铁律：**一切删除前过锁探测，命中即拒绝**，没有 `--force` 绕过。
//! 想删就先把 agent 关掉——这比让 duster 去猜"这个锁大概没关系"安全得多。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags};

/// SQLite 文件头魔数。非 SQLite 文件直接短路，免得给每个日志文件都开一次连接。
///
/// 与 `duster-index` 的 `sqlite_probe` 是同一份常量，但**故意不共享**：
/// duster-index 在本 crate 之上，向上依赖是分层禁区，宁可重写这 16 字节。
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// 锁探测结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockStatus {
    /// 可以安全操作。
    Free,
    /// 被占用，`reason` 是给用户看的人话（谁占着、怎么判断出来的）。
    Locked { reason: String },
    /// 探测能力缺失（没有 lsof、权限不足）。**按放行处理**，但调用方
    /// 必须把 `reason` 放进 warnings，让用户知道这一项没被真正检查过。
    Unknown { reason: String },
}

impl LockStatus {
    /// 是否允许继续操作（`Free` 与 `Unknown` 放行，`Locked` 拒绝）。
    pub fn allows(&self) -> bool {
        !matches!(self, LockStatus::Locked { .. })
    }
}

/// 锁冲突错误。CLI 据其 Display 文案映射退出码 5（见 duster-cli 的 `exit_code_for`）。
/// 文案里的 `is locked by` 是跨层契约，改动须同步 CLI。
#[derive(Debug, thiserror::Error)]
#[error("{path} is locked by another process: {reason}. Quit the agent and try again")]
pub struct LockedError {
    pub path: String,
    pub reason: String,
}

/// 按路径形态自动选择探测方式：SQLite 库走 [`probe_sqlite`]，其余走 [`probe_processes`]。
/// 路径不存在视为 [`LockStatus::Free`]（没有东西可锁）。
pub fn probe(path: &Path) -> LockStatus {
    let Ok(meta) = std::fs::metadata(path) else {
        // 不存在（或断掉的软链）：没有东西可锁，也没有东西可占。
        return LockStatus::Free;
    };

    if meta.is_dir() {
        // 目录没有 SQLite 语义，只问进程。
        return probe_processes(path);
    }

    // 文件先问 SQLite：`Locked` / `Unknown` 就是结论。
    // `Free` 里混着"根本不是 SQLite 文件"这一种，必须再按普通文件走一遍进程探测。
    match probe_sqlite(path) {
        LockStatus::Free => probe_processes(path),
        conclusive => conclusive,
    }
}

/// SQLite 写锁探测：只读打开判魔数 → 可写打开 → `busy_timeout=0` →
/// `BEGIN IMMEDIATE` → 立即 `ROLLBACK`。
///
/// 拿不到锁返回 [`LockStatus::Locked`]；不是 SQLite 文件返回
/// [`LockStatus::Free`]（交给调用方按普通文件再判一次）。
///
/// **绝不留下副作用**：不建库、不迁移、不 checkpoint、不改 `journal_mode`。
/// 与库的全部交互就是「开一个立即事务，然后回滚」。
pub fn probe_sqlite(path: &Path) -> LockStatus {
    if !has_sqlite_magic(path) {
        // 不是 SQLite 文件不是错误，是"不适用"。
        return LockStatus::Free;
    }

    // 刻意不带 `SQLITE_OPEN_CREATE`（这正是 `Connection::open` 的默认值之一）：
    // 魔数检查与 open 之间文件若被删掉，默认标志会凭空建一个空库——
    // 那是副作用，本模块一个字节都不许写。
    let conn = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        // 权限不足、文件损坏、加密库——都不是"被占用"，是"没测成"。
        Err(e) => return classify_sqlite_error(&e, "opening the database read-write"),
    };

    // 探测要立刻出结论，绝不在这里排队等锁：默认 busy_timeout 会把
    // "被占用"拖成"卡住"，那比拒绝还糟。
    if let Err(e) = conn.busy_timeout(Duration::ZERO) {
        return LockStatus::Unknown {
            reason: format!("could not disable SQLite busy_timeout: {e}"),
        };
    }

    match conn.execute_batch("BEGIN IMMEDIATE") {
        Ok(()) => {
            // 拿到写锁只是为了证明"能拿到"，立刻还回去。
            // 事务里没有改动任何页，ROLLBACK 不会落盘、不会建 journal。
            let _ = conn.execute_batch("ROLLBACK");
            LockStatus::Free
        }
        // VACUUM / 截断本来也要这把写锁，探测判据与执行判据完全一致。
        Err(e) => classify_sqlite_error(&e, "acquiring the SQLite write lock"),
    }
}

/// 按文件头魔数判断是不是 SQLite 库。读不到（权限/不存在/短于 16 字节）视为"不是"。
fn has_sqlite_magic(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 16];
    // 读不满 16 字节就不可能是 SQLite。
    f.read_exact(&mut head).is_ok() && &head == SQLITE_MAGIC
}

/// 把 rusqlite 错误分成"被占用"与"没测成"两类。
///
/// 只有 `SQLITE_BUSY` / `SQLITE_LOCKED` 算 [`LockStatus::Locked`]——
/// 其余（权限、损坏、加密）一律 [`LockStatus::Unknown`]：
/// 把"打不开"冒充成"被锁着"会让用户去关一个根本没开的 agent。
fn classify_sqlite_error(err: &rusqlite::Error, during: &str) -> LockStatus {
    let code = match err {
        rusqlite::Error::SqliteFailure(e, _) => Some(e.code),
        _ => None,
    };
    match code {
        Some(ErrorCode::DatabaseBusy) => LockStatus::Locked {
            reason: format!("SQLITE_BUSY while {during}: another process holds the write lock"),
        },
        Some(ErrorCode::DatabaseLocked) => LockStatus::Locked {
            reason: format!("SQLITE_LOCKED while {during}: the database is locked"),
        },
        _ => LockStatus::Unknown {
            reason: format!("SQLite probe failed while {during}: {err}"),
        },
    }
}

/// 进程占用探测：`lsof -t -- <path>`。
///
/// - 有 PID 输出 → [`LockStatus::Locked`]，reason 里带上进程名与 pid；
/// - 无输出 → [`LockStatus::Free`]；
/// - `lsof` 不存在或执行失败 → [`LockStatus::Unknown`]。
///
/// 目录**不用** `lsof +D`：那会递归 stat 整棵树，1 GB 的 skill 目录要跑十几秒，
/// 而清理前每一项都要探一次，累计代价不可接受。改为探目录本身
/// （被谁当 cwd / mmap 持有），外加一级子目录里的锁文件——
/// 浏览器 profile、SQLite 周边工具都把锁扔在这一层。
pub fn probe_processes(path: &Path) -> LockStatus {
    // 探测能力缺失只记第一条：后面的目标失败也是同一个原因，重复没有信息量。
    let mut lost: Option<String> = None;

    for target in probe_targets(path) {
        match lsof_pids(&target) {
            Ok(pids) if !pids.is_empty() => {
                let who = pids
                    .iter()
                    .map(|pid| format!("pid {pid} ({})", process_name(*pid)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let reason = if target == path {
                    format!("{who} still has it open (per lsof)")
                } else {
                    format!(
                        "{who} still has the lock file {} open (per lsof)",
                        target.display()
                    )
                };
                return LockStatus::Locked { reason };
            }
            Ok(_) => {}
            Err(e) => {
                let reason = if e.kind() == std::io::ErrorKind::NotFound {
                    "lsof was not found on PATH, so open file handles were not checked".to_string()
                } else {
                    format!("could not run lsof on {}: {e}", target.display())
                };
                lost.get_or_insert(reason);
            }
        }
    }

    match lost {
        // 探测失败按放行处理，但必须把话说清楚——调用方要把它放进 warnings。
        Some(reason) => LockStatus::Unknown { reason },
        None => LockStatus::Free,
    }
}

/// 要喂给 `lsof` 的路径清单：路径本身，目录再加上一级子目录里的锁文件。
fn probe_targets(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    if !path.is_dir() {
        return out;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return out;
    };
    let mut locks: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(is_lock_file_name))
        .map(|e| e.path())
        .collect();
    // 排序只为让 reason 文案可复现：同一个目录两次探测该给同一句话。
    locks.sort();
    out.append(&mut locks);
    out
}

/// 常见的"这里有人在用"标记文件名。
fn is_lock_file_name(name: &str) -> bool {
    name.ends_with(".lock") || matches!(name, "LOCK" | "lockfile" | "SingletonLock")
}

/// 跑一次 `lsof -t -- <target>`，返回占用它的 pid。
///
/// 一枪一个，不轮询不重试：探测慢过头还不如没有。
/// lsof 找不到占用时退出码是 1 且 stdout 为空——那不是错误，是"没人用"，
/// 所以只看 stdout，不看退出码。
fn lsof_pids(target: &Path) -> std::io::Result<Vec<u32>> {
    // `--` 终止选项解析：路径可能以 `-` 开头。
    let out = Command::new("lsof")
        .arg("-t")
        .arg("--")
        .arg(target)
        .output()?;
    let mut pids: Vec<u32> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        // duster 自己打开着不算被占用：我们清楚自己在干什么，
        // 而且 POSIX 下删掉自己打开的文件本来就合法。
        .filter(|pid| *pid != std::process::id())
        .collect();
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

/// 查进程名，尽力而为——查不到就叫 `unknown`，绝不因此让整个探测失败。
fn process_name(pid: u32) -> String {
    let Ok(out) = Command::new("ps")
        .arg("-o")
        .arg("comm=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
    else {
        return "unknown".to_string();
    };
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return "unknown".to_string();
    }
    // ps 给的常是完整路径，取最后一段更像用户认得的那个名字。
    Path::new(&raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(raw.as_str())
        .to_string()
}

/// [`probe`] 的断言版：`Locked` 返回 [`LockedError`]，其余返回该状态
/// （`Unknown` 也要回给调用方，好进 warnings）。
pub fn ensure_free(path: &Path) -> anyhow::Result<LockStatus> {
    let st = probe(path);
    if let LockStatus::Locked { reason } = &st {
        return Err(LockedError {
            path: path.display().to_string(),
            reason: reason.clone(),
        }
        .into());
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小的真库：有魔数、有 user_version、有一张表。
    fn make_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("PRAGMA user_version = 7; CREATE TABLE t(x);")
            .unwrap();
    }

    /// 只读读回 user_version，用于验证探测没动过库。
    fn user_version(path: &Path) -> i64 {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    /// 判据必须随锁的存在与否翻转：空闲 → 放行，持锁 → 拒绝，松手 → 再放行。
    /// 只要有一头不翻转，锁探测就是个摆设。
    #[test]
    fn sqlite_free_then_locked_then_free_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("logs.sqlite");
        make_db(&db);

        assert_eq!(probe_sqlite(&db), LockStatus::Free, "没人用的库要放行");

        let holder = Connection::open(&db).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
        match probe_sqlite(&db) {
            LockStatus::Locked { reason } => assert!(
                reason.contains("SQLITE_BUSY") || reason.contains("SQLITE_LOCKED"),
                "reason 必须点名 SQLite 错误码，实际是：{reason}"
            ),
            other => panic!("独占事务期间必须判为 Locked，实际 {other:?}"),
        }

        holder.execute_batch("ROLLBACK").unwrap();
        drop(holder);
        assert_eq!(probe_sqlite(&db), LockStatus::Free, "锁释放后要恢复放行");
    }

    /// 非 SQLite 文件是"不适用"，不是错误也不是占用——
    /// 报成 Locked 会让每个日志文件都删不掉。
    #[test]
    fn non_sqlite_file_probes_free() {
        let dir = tempfile::tempdir().unwrap();

        let text = dir.path().join("app.log");
        std::fs::write(&text, b"just some plain text, nothing to see here\n").unwrap();
        assert_eq!(probe_sqlite(&text), LockStatus::Free);

        // 比魔数还短：read_exact 读不满 16 字节，同样按"不是库"处理。
        let tiny = dir.path().join("tiny");
        std::fs::write(&tiny, b"hi").unwrap();
        assert_eq!(probe_sqlite(&tiny), LockStatus::Free);
    }

    /// 路径不存在就没有东西可锁，绝不能因此报错或拒绝。
    #[test]
    fn missing_path_probes_free() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(probe(&dir.path().join("nope")), LockStatus::Free);
    }

    /// `is locked by` 是跨层契约：CLI 靠它把错误映射成退出码 5。
    /// 这条断言一旦失败，锁冲突会被当成普通错误报成退出码 1。
    #[test]
    fn ensure_free_error_carries_cli_contract_phrase() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("busy.sqlite");
        make_db(&db);

        let holder = Connection::open(&db).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let err = ensure_free(&db).expect_err("持锁期间必须报错");
        let msg = err.to_string();
        assert!(msg.contains("is locked by"), "错误文案丢了跨层契约：{msg}");

        holder.execute_batch("ROLLBACK").unwrap();
    }

    /// 探测别人家的库不许留下任何痕迹：字节数、user_version、
    /// 以及目录里的文件数量（不许多出 WAL / journal 残留）都要一模一样。
    #[test]
    fn probe_leaves_database_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("intact.sqlite");
        make_db(&db);

        let before_len = std::fs::metadata(&db).unwrap().len();
        let before_uv = user_version(&db);
        let before_files = std::fs::read_dir(dir.path()).unwrap().count();

        assert_eq!(probe_sqlite(&db), LockStatus::Free);

        assert_eq!(
            std::fs::metadata(&db).unwrap().len(),
            before_len,
            "探测改变了文件长度"
        );
        assert_eq!(user_version(&db), before_uv, "探测改动了 user_version");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            before_files,
            "探测留下了 WAL / journal 之类的边角文件"
        );
    }

    /// 没人占用的临时路径不该被判为 Locked。系统上没有 lsof 时降级为
    /// Unknown，但 Unknown 也必须放行——探测能力缺失不等于什么都不敢做。
    #[test]
    fn unused_paths_are_never_locked() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("idle.txt");
        std::fs::write(&f, b"idle").unwrap();

        assert!(probe_processes(&f).allows(), "空闲文件不该被判占用");
        assert!(probe_processes(dir.path()).allows(), "空闲目录不该被判占用");
    }

    /// 目录探测的目标清单：目录本身 + 一级锁文件，且不下钻子目录。
    /// 走 `lsof +D` 递归整棵树在 1 GB 的 skill 目录上根本跑不动。
    #[test]
    fn directory_targets_cover_lock_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SingletonLock"), b"").unwrap();
        std::fs::write(dir.path().join("index.lock"), b"").unwrap();
        std::fs::write(dir.path().join("notes.md"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("LOCK"), b"").unwrap();

        let targets = probe_targets(dir.path());
        let names: Vec<String> = targets
            .iter()
            .skip(1)
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(targets[0], dir.path(), "第一个目标必须是目录本身");
        assert_eq!(names, vec!["SingletonLock", "index.lock"]);
    }

    /// 真的有外部进程开着文件就必须拒绝，并且 reason 里要报出 pid 与进程名——
    /// 用户看到"谁占着"才知道该关掉什么。这是本模块唯一的实战判据。
    ///
    /// 用 `sh -c 'exec sleep … < file'` 让子进程把 fd 0 挂在目标文件上，
    /// 比在本进程里开文件更真实（本进程的 pid 会被 [`lsof_pids`] 主动滤掉）。
    #[test]
    fn external_process_holding_a_file_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("held.log");
        std::fs::write(&f, b"held open by someone else").unwrap();

        // 机器上没有 lsof 时探测能力本就缺失，这条断言无从谈起，直接跳过。
        if matches!(probe_processes(&f), LockStatus::Unknown { .. }) {
            return;
        }

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("exec sleep 30 < {}", f.display()))
            .spawn()
            .expect("sh 与 sleep 是 POSIX 保底命令");

        // 等子进程真正 exec 到 sleep 并把 fd 挂上；一次探到就走，不空转。
        let mut status = LockStatus::Free;
        for _ in 0..50 {
            status = probe_processes(&f);
            if !status.allows() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let _ = child.kill();
        let _ = child.wait();

        match status {
            LockStatus::Locked { reason } => {
                assert!(reason.contains("pid "), "reason 必须点名 pid：{reason}");
                assert!(reason.contains("sleep"), "reason 必须点名进程：{reason}");
            }
            other => panic!("外部进程开着文件时必须判为 Locked，实际 {other:?}"),
        }
    }
}
