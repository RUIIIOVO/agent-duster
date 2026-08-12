//! 配置文件快照：原地改写一个还装着别的东西的文件之前，先整文件复制一份。
//!
//! **这不是 [`crate::archive`] 的替代品，是另一码事。**
//!
//! 归档保护的是「这块内容将整个消失」——用户在确认清单上看到了要删什么，
//! 归档只是给他一条反悔的路。快照保护的是完全不同的失败模式：
//! `~/.claude.json` 里还有二十条别的 MCP 声明，用户确认的是"删掉 stitch 这一条"，
//! 但一次写坏丢的是**整个文件**。这种损失在确认清单上根本看不见——
//! 用户同意的和可能失去的，压根不是一个东西。所以它不需要用户同意，
//! 也不该出现在确认清单里，它是无条件执行的一道安全网。
//!
//! 落点 `~/.agent-duster/snapshots/<op-id>/`，保留最近 [`KEEP`] 次。
//! 与归档的另一处不同：快照**是 duster 的内部状态**，所以放在 `~/.agent-duster/`
//! 下、有 GC、用户不必知道它存在。

use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

/// 保留的快照批次数。50 次足够覆盖"上周那次改坏了"，又不会无限长。
pub const KEEP: usize = 50;

/// 快照根目录：`~/.agent-duster/snapshots`。
pub fn default_snapshot_root() -> PathBuf {
    crate::path::expand_tilde("~/.agent-duster/snapshots")
}

/// 生成一次操作的 id：`<YYYYMMDD-HHMMSS>-<pid>`。
///
/// 同一次 `duster prune` 里改写的多个文件共用一个 op-id，落在同一个目录下，
/// 用户还原时是"回到那一次操作之前"，不是"回到某个文件的某个版本"。
pub fn new_op_id(now: std::time::SystemTime) -> String {
    format!("{}-{}", crate::zst::stamp(now), std::process::id())
}

/// 一次快照的回执。
#[derive(Debug, Clone)]
pub struct SnapshotReceipt {
    /// 快照文件完整路径。
    pub path: PathBuf,
    /// 原文件路径。
    pub source: PathBuf,
    /// 字节数。
    pub bytes: u64,
}

/// 把 `file` 整文件复制到 `root/<op_id>/`，返回回执。
///
/// 硬要求：
/// - 保留原文件名；同一 op 内不同目录的同名文件（`~/.claude.json` 与
///   `~/.config/x/.claude.json`）不得互相覆盖——用相对 home 的路径展开成
///   子目录结构，而不是拍平成 basename。
/// - 保留权限位（配置文件常是 0600，快照不能放宽）。
/// - 复制走原子写：先写 temp 再 rename，快照本身不能是半个文件。
/// - **复制失败即返回 Err**，调用方必须放弃改写。
pub fn snapshot_file(
    root: &Path,
    op_id: &str,
    file: &Path,
    home: &Path,
) -> Result<SnapshotReceipt> {
    // 先开源文件再建目录：源文件不存在时不能留下一个空的 op 目录，
    // 否则 GC 会把它当成一次真实操作占掉一个保留名额。
    let mut src = File::open(file)
        .with_context(|| format!("failed to open source file {}", file.display()))?;
    let src_meta = src
        .metadata()
        .with_context(|| format!("failed to stat source file {}", file.display()))?;

    let dest = root.join(op_id).join(relative_layout(file, home));
    let dest_dir = dest
        .parent()
        .expect("dest 至少有 root/op_id 两层父目录")
        .to_path_buf();
    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("failed to create snapshot directory {}", dest_dir.display()))?;

    // 同目录 temp + rename，语义与 crate::atomic::write_atomic 一致；
    // 这里不用 write_atomic 是因为它要求先把整个文件读进内存，
    // 而快照的对象是「别人的文件」，大小不由我们决定，只能流式拷。
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-duster-")
        .tempfile_in(&dest_dir)
        .with_context(|| format!("failed to create temp file in {}", dest_dir.display()))?;

    let bytes = io::copy(&mut src, tmp.as_file_mut())
        .with_context(|| format!("failed to copy {}", file.display()))?;

    // 权限位在 rename 之前就设好：配置文件常是 0600，
    // 快照哪怕只在两次系统调用之间放宽过一瞬，也是把秘密摊开过。
    tmp.as_file()
        .set_permissions(src_meta.permissions())
        .with_context(|| format!("failed to apply source permissions to {}", dest.display()))?;
    tmp.as_file()
        .sync_all()
        .context("failed to fsync snapshot temp file")?;

    tmp.persist(&dest)
        .with_context(|| format!("failed to rename snapshot to {}", dest.display()))?;

    // fsync 父目录，让 rename 产生的目录项本身落盘——
    // 快照要防的就是崩溃，目录项没落盘等于没快照。
    let dir_fd = File::open(&dest_dir)
        .with_context(|| format!("failed to open parent directory {}", dest_dir.display()))?;
    dir_fd
        .sync_all()
        .with_context(|| format!("failed to fsync parent directory {}", dest_dir.display()))?;

    Ok(SnapshotReceipt {
        path: dest,
        source: file.to_path_buf(),
        bytes,
    })
}

/// 把 `file` 折成 op 目录内的相对布局。
///
/// home 内的文件保留相对 home 的完整层级；home 外的退化成绝对路径去掉根前缀。
/// 两条路都保留目录层级而不是拍平 basename——`~/.claude.json` 和
/// `~/.config/x/.claude.json` 在一次操作里同时被改写是常态，拍平就是互相覆盖。
fn relative_layout(file: &Path, home: &Path) -> PathBuf {
    if let Ok(rest) = file.strip_prefix(home)
        && !rest.as_os_str().is_empty()
    {
        return sanitize(rest);
    }
    sanitize(file)
}

/// 丢掉根/盘符/`.`，把 `..` 换成字面量。
///
/// `..` 必须换掉而不是保留：它会让拼出来的落点跑到 op 目录外面去，
/// 一个本该只写 `~/.agent-duster/snapshots/` 的操作绝不能因为入参形状而越界。
fn sanitize(rel: &Path) -> PathBuf {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(PathBuf::from(s)),
            Component::ParentDir => Some(PathBuf::from("__up__")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// 判断目录名是否是 [`new_op_id`] 产出的形状：`YYYYMMDD-HHMMSS-<digits>`。
///
/// 手写字符检查而不是上正则：这一条规则决定的是「敢不敢 `remove_dir_all`」，
/// 应该看一眼就能确认它严不严，不该藏在一个依赖里。
fn is_op_id(name: &str) -> bool {
    let mut parts = name.split('-');
    let (Some(date), Some(time), Some(pid), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    date.len() == 8
        && time.len() == 6
        && !pid.is_empty()
        && [date, time, pid]
            .iter()
            .all(|s| s.bytes().all(|b| b.is_ascii_digit()))
}

/// 只保留最近 `keep` 个 op 目录，其余删除。返回删掉的批次数。
///
/// 按目录名字典序即时序（op-id 前缀是 UTC 时间戳，见 [`new_op_id`]）。
/// 名字不符合 op-id 形状的目录**不删**——那不是 duster 写的，别碰。
pub fn prune_old(root: &Path, keep: usize) -> Result<usize> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        // 从没快照过就没什么可 GC 的，不是错。
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to read snapshot root {}", root.display()));
        }
    };

    let mut ops: Vec<String> = Vec::new();
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read entry under {}", root.display()))?;
        // file_type 不跟随符号链接：指向别处的链接不算 op 目录，
        // 免得 remove_dir_all 顺着链接删到快照根之外。
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_op_id(name) {
            ops.push(name.to_string());
        }
    }

    if ops.len() <= keep {
        return Ok(0);
    }
    ops.sort_unstable();

    let doomed = ops.len() - keep;
    let mut removed = 0usize;
    for name in &ops[..doomed] {
        let dir = root.join(name);
        fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to remove snapshot batch {}", dir.display()))?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 同名不同目录的文件互不覆盖() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = tmp.path().join("snapshots");
        fs::create_dir_all(home.join(".config/x")).unwrap();

        let a = home.join(".claude.json");
        let b = home.join(".config/x/.claude.json");
        fs::write(&a, b"top-level").unwrap();
        fs::write(&b, b"nested").unwrap();

        let ra = snapshot_file(&root, "20260811-101010-1", &a, &home).unwrap();
        let rb = snapshot_file(&root, "20260811-101010-1", &b, &home).unwrap();

        assert_ne!(ra.path, rb.path, "同一 op 内两份快照不应落在同一路径");
        assert_eq!(fs::read(&ra.path).unwrap(), b"top-level");
        assert_eq!(fs::read(&rb.path).unwrap(), b"nested");
        assert_eq!(ra.bytes, 9);
        assert_eq!(rb.source, b);
        assert_eq!(
            rb.path,
            root.join("20260811-101010-1/.config/x/.claude.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn 快照保留源文件权限位() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let src = home.join("secret.json");
        fs::write(&src, b"{}").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o600)).unwrap();

        let receipt = snapshot_file(
            &tmp.path().join("snapshots"),
            "20260811-101010-1",
            &src,
            &home,
        )
        .unwrap();

        let mode = fs::metadata(&receipt.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "快照不能比源文件更宽松");
    }

    #[test]
    fn 源文件不存在时报错且不建目录() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let root = tmp.path().join("snapshots");

        let err = snapshot_file(&root, "20260811-101010-1", &home.join("nope.json"), &home);

        assert!(err.is_err());
        assert!(
            !root.join("20260811-101010-1").exists(),
            "失败的快照不应留下空 op 目录"
        );
    }

    #[test]
    fn 只保留最新的若干批次且不碰陌生目录() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("snapshots");
        fs::create_dir_all(&root).unwrap();

        for i in 0..60u32 {
            fs::create_dir_all(root.join(format!("20260811-{:06}-1", i))).unwrap();
        }
        fs::create_dir_all(root.join("not-an-op")).unwrap();

        assert_eq!(prune_old(&root, 50).unwrap(), 10);

        let mut left: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();

        assert!(left.contains(&"not-an-op".to_string()), "陌生目录不该被删");
        let ops: Vec<&String> = left.iter().filter(|n| is_op_id(n)).collect();
        assert_eq!(ops.len(), 50);
        assert_eq!(ops[0], "20260811-000010-1", "应删掉最旧的 10 个");
        assert_eq!(ops[49], "20260811-000059-1");

        // 已经不超额时是 no-op。
        assert_eq!(prune_old(&root, 50).unwrap(), 0);
    }

    #[test]
    fn 根目录不存在不是错误() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(prune_old(&tmp.path().join("never-written"), 50).unwrap(), 0);
    }

    #[test]
    fn home之外的文件退化为绝对路径布局() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let outside = tmp.path().join("etc");
        fs::create_dir_all(&outside).unwrap();
        let src = outside.join("hosts");
        fs::write(&src, b"127.0.0.1").unwrap();

        let root = tmp.path().join("snapshots");
        let receipt = snapshot_file(&root, "20260811-101010-1", &src, &home).unwrap();

        // 去掉根之后层级完整保留，落点仍在 op 目录内。
        assert!(receipt.path.starts_with(root.join("20260811-101010-1")));
        assert!(receipt.path.ends_with("etc/hosts"));
        assert_eq!(fs::read(&receipt.path).unwrap(), b"127.0.0.1");
    }

    #[test]
    fn op_id形状校验() {
        assert!(is_op_id(&new_op_id(std::time::SystemTime::now())));
        assert!(is_op_id("20260811-101010-1"));
        assert!(is_op_id("20260811-101010-99999"));
        assert!(!is_op_id("not-an-op"));
        assert!(!is_op_id("20260811-101010"));
        assert!(!is_op_id("20260811-101010-1-extra"));
        assert!(!is_op_id("2026081-101010-1"));
        assert!(!is_op_id("20260811-10101a-1"));
        assert!(!is_op_id("20260811-101010-"));
    }
}
