//! 删除前归档：把**即将删除的内容原样打包**成 `tar + zstd`，再永久删除。
//!
//! 定位必须说清楚，否则很容易做成半个回收站：
//! - 归档包**归用户所有**，`tar -xf` 即还原。duster 不做 GC、不做 restore 命令、
//!   不在任何视图里管理它。写完就撒手。
//! - 归档只针对**不可再生**的内容（陈旧 skill、会话、卸载时的用户数据）。
//!   缓存不归档——缓存的撤销毫无意义，而它恰恰是体积最大的一档，
//!   进"回收站"等于承诺清理却一个字节没释放。
//! - 原地改写配置文件不走归档，走 [`crate::snapshot`]。两码事：归档保护的是
//!   "整块内容将消失"，快照保护的是"文件还在但被我改了一处，改坏了就全丢"。
//!
//! 尺寸阈值：预估超过 [`AUTO_ARCHIVE_LIMIT`] 时**不自动归档**，要求调用方
//! 拿到显式的 `--archive` / `--no-archive` 决定，否则拒绝执行。判断在用例层，
//! 本模块只提供 [`estimate_bytes`] 与常量。

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

/// 自动归档的体积上限：200 MB。超过这个数就不该在用户没表态时替他决定——
/// 打包几百 MB 要花时间、要占空间，而用户可能根本不想留。
pub const AUTO_ARCHIVE_LIMIT: u64 = 200 * 1024 * 1024;

/// 归档包默认落点：`~/agent-duster-exports/`。
///
/// 刻意放在 home 下而不是 `~/.agent-duster/` 里：它属于用户，不属于 duster 的
/// 内部状态。用户看得见、能自己搬走、能自己删。
pub fn default_export_dir() -> PathBuf {
    crate::path::expand_tilde("~/agent-duster-exports")
}

/// 一次归档的回执。输出里必须报出 `path` 与 `bytes`——用户要能立刻找到它。
#[derive(Debug, Clone)]
pub struct ArchiveReceipt {
    /// 归档包完整路径。
    pub path: PathBuf,
    /// 归档包实际尺寸（压缩后）。
    pub bytes: u64,
    /// 打包进去的条目数（文件 + 目录）。
    pub entries: usize,
    /// 打包前的原始字节数合计（压缩前）。
    pub source_bytes: u64,
}

/// 归档包文件名：`<op>-<YYYYMMDD-HHMMSS>.tar.zst`。
///
/// `op` 是动词加目标，如 `prune-skill` / `uninstall-qoder`，让用户在
/// 导出目录里一眼看出这包是哪次操作留下的。
pub fn archive_name(op: &str, now: std::time::SystemTime) -> String {
    format!("{op}-{}.tar.zst", crate::zst::stamp(now))
}

/// 预估 `roots` 打包前的原始字节数（目录递归聚合，文件取自身大小）。
///
/// 用于阈值判断。读不到的路径按 0 计并跳过——预估的用途是"要不要问用户"，
/// 不是精确记账，为它中断整个操作不划算。
pub fn estimate_bytes(roots: &[PathBuf]) -> Result<u64> {
    let opts = crate::walk::WalkOptions::default();
    let mut total: u64 = 0;
    for root in roots {
        // 不跟随符号链接，与打包口径保持一致：链接按自身大小计，
        // 否则「预估 2 GB、实际打出 4 KB」这种偏差会直接误导阈值判断。
        let Ok(meta) = fs::symlink_metadata(root) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(stats) = crate::walk::walk_stats(root, &opts) {
                total = total.saturating_add(stats.total_bytes);
            }
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
}

/// 把 `roots` 原样打包到 `out_dir/<archive_name(op)>`。
///
/// 硬要求：
/// - tar 内的条目路径用**相对 home 的路径**（如 `.claude/skills/foo/…`），
///   保证 `tar -xf` 在 home 下解开即还原到原位；home 之外的路径退化为
///   去掉前导 `/` 的绝对路径。
/// - 保留权限位与 mtime；符号链接按链接本身打包，不跟随。
/// - 输出走原子写：同目录 temp + rename，中途失败不留半个包。
/// - **打包失败即中止**：返回 Err，调用方必须放弃删除。这是归档机制存在的
///   全部意义——"先打包成功，再删"，顺序不能反。
/// - `out_dir` 不存在则创建。
pub fn archive_paths(
    op: &str,
    roots: &[PathBuf],
    out_dir: &Path,
    home: &Path,
) -> Result<ArchiveReceipt> {
    if roots.is_empty() {
        bail!("archive requested with no source paths");
    }
    fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create export dir {}", out_dir.display()))?;

    let path = out_dir.join(archive_name(op, SystemTime::now()));
    // 包名带秒级时间戳，撞名基本只可能是同一秒重跑。宁可报错也不覆盖：
    // 归档包是用户的东西，覆盖等于替他删数据。
    if path.symlink_metadata().is_ok() {
        bail!("archive already exists: {}", path.display());
    }

    let source_bytes = estimate_bytes(roots)?;
    let mut entries: usize = 0;

    // 写失败时 temp 文件随 Drop 消失，导出目录里既不留半个包也不留 .tmp-*，
    // 调用方看到 Err 就能安全地放弃删除。
    crate::zst::write_stream_atomic(&path, |w| {
        let enc = zstd::Encoder::new(w, crate::zst::ARCHIVE_LEVEL)
            .context("failed to init zstd encoder")?;
        let mut tar = tar::Builder::new(enc);
        // 符号链接按链接本身打包：跟随会把链接目标的内容复制进来，
        // 既可能撑爆体积，也可能打包到根本不属于本次删除范围的文件。
        tar.follow_symlinks(false);
        for root in roots {
            append_entry(&mut tar, root, home, &mut entries)?;
        }
        let enc = tar.into_inner().context("failed to finalize tar stream")?;
        enc.finish().context("failed to finalize zstd stream")?;
        Ok(())
    })?;

    let bytes = fs::metadata(&path)
        .with_context(|| format!("failed to stat archive {}", path.display()))?
        .len();

    Ok(ArchiveReceipt {
        path,
        bytes,
        entries,
        source_bytes,
    })
}

/// 递归把 `path`（及其子树）追加进 tar，`count` 累计条目数。
///
/// 不用 `tar::Builder::append_dir_all`：它不报条目数，也没法逐条控制包内名字。
/// 目录条目本身也要写进去——只写文件会丢掉空目录和目录的权限位。
fn append_entry<W: Write>(
    tar: &mut tar::Builder<W>,
    path: &Path,
    home: &Path,
    count: &mut usize,
) -> Result<()> {
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    let name = entry_name(path, home);

    // append_path_with_name 在 follow=false 下按 symlink_metadata 分派：
    // 普通文件写内容，目录只写目录项，符号链接写链接目标。权限位与 mtime
    // 由 HeaderMode::Complete（Builder 默认）原样带上。
    tar.append_path_with_name(path, &name)
        .with_context(|| format!("failed to archive {}", path.display()))?;
    *count += 1;

    // is_dir() 对「指向目录的符号链接」为 false，所以不会顺着链接走下去。
    if meta.is_dir() {
        let mut children: Vec<PathBuf> = fs::read_dir(path)
            .with_context(|| format!("failed to read dir {}", path.display()))?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("failed to read dir {}", path.display()))?;
        // 排序遍历：同一份内容每次打出的包字节一致，便于比对与复现。
        children.sort();
        for child in children {
            append_entry(tar, &child, home, count)?;
        }
    }
    Ok(())
}

/// tar 内的条目名：相对 `home` 的路径。
///
/// home 之外（含 `path` 恰为 home 本身）退化为去掉前导 `/` 的绝对路径——
/// tar 条目不允许绝对路径，而保留完整路径至少让用户看得出它原本在哪。
fn entry_name(path: &Path, home: &Path) -> PathBuf {
    if let Ok(rel) = path.strip_prefix(home)
        && !rel.as_os_str().is_empty()
    {
        return rel.to_path_buf();
    }
    PathBuf::from(path.to_string_lossy().trim_start_matches('/').to_string())
}

/// 列出归档包内的条目路径。给 fixtures 断言与 `--dry-run` 复核用。
pub fn list_entries(archive: &Path) -> Result<Vec<String>> {
    let file = File::open(archive)
        .with_context(|| format!("failed to open archive {}", archive.display()))?;
    let dec = zstd::Decoder::new(file)
        .with_context(|| format!("failed to decode archive {}", archive.display()))?;
    let mut ar = tar::Archive::new(dec);

    let mut out = Vec::new();
    for entry in ar.entries().context("failed to read tar entries")? {
        let entry = entry.context("failed to read tar entry")?;
        let p = entry.path().context("tar entry has an unreadable path")?;
        out.push(p.to_string_lossy().into_owned());
    }
    Ok(out)
}

/// 把归档包解开到 `dest`。duster 自己不提供 restore 命令，
/// 这个函数只服务于**往返一致性校验**（fixtures 断言与 prune 的自检）。
/// 刻意**不**长成 restore 命令：归档包归用户所有，还原就是 `tar -xf`。
pub fn extract_to(archive: &Path, dest: &Path) -> Result<usize> {
    fs::create_dir_all(dest)
        .with_context(|| format!("failed to create dest dir {}", dest.display()))?;

    let file = File::open(archive)
        .with_context(|| format!("failed to open archive {}", archive.display()))?;
    let dec = zstd::Decoder::new(file)
        .with_context(|| format!("failed to decode archive {}", archive.display()))?;
    let mut ar = tar::Archive::new(dec);
    // 校验的是「打出去的和原来的一样」，所以权限位与 mtime 都要还原。
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    ar.set_overwrite(true);
    ar.unpack(dest)
        .with_context(|| format!("failed to extract {}", archive.display()))?;

    // unpack 不报条目数，只能再解一遍数。这个函数只服务于往返校验，
    // 多一趟解压换实现简单是划算的。
    Ok(list_entries(archive)?.len())
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::time::{Duration, UNIX_EPOCH};

    /// 造一棵 `~/.claude/skills/foo/` 小树：两层目录 + 一个相对符号链接。
    fn make_tree(home: &Path) -> PathBuf {
        let root = home.join(".claude/skills/foo");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("SKILL.md"), b"# foo\n").unwrap();
        fs::write(root.join("sub/a.txt"), vec![7u8; 4096]).unwrap();
        fs::set_permissions(root.join("SKILL.md"), fs::Permissions::from_mode(0o640)).unwrap();
        symlink("SKILL.md", root.join("link")).unwrap();
        root
    }

    #[test]
    fn 包名等于操作名加时间戳() {
        let t = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        assert_eq!(
            archive_name("prune-skill", t),
            "prune-skill-20210101-000000.tar.zst"
        );
    }

    #[test]
    fn 体积预估等于真实总和且忽略缺失路径() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("d");
        fs::create_dir_all(sub.join("inner")).unwrap();
        fs::write(sub.join("a.bin"), vec![0u8; 1000]).unwrap();
        fs::write(sub.join("inner/b.bin"), vec![0u8; 2345]).unwrap();
        let loose = dir.path().join("loose.bin");
        fs::write(&loose, vec![0u8; 77]).unwrap();

        let roots = vec![sub.clone(), loose.clone(), dir.path().join("nope")];
        assert_eq!(estimate_bytes(&roots).unwrap(), 1000 + 2345 + 77);

        // 只给一个缺失路径也不该报错，按 0 计。
        assert_eq!(estimate_bytes(&[dir.path().join("nope")]).unwrap(), 0);
    }

    #[test]
    fn 归档条目为home相对路径且往返一致() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let root = make_tree(&home);
        let out_dir = tmp.path().join("exports");

        let src_bytes = estimate_bytes(std::slice::from_ref(&root)).unwrap();
        let receipt =
            archive_paths("prune-skill", std::slice::from_ref(&root), &out_dir, &home).unwrap();

        assert!(receipt.path.starts_with(&out_dir));
        assert!(receipt.path.to_string_lossy().ends_with(".tar.zst"));
        assert_eq!(receipt.bytes, fs::metadata(&receipt.path).unwrap().len());
        assert!(receipt.bytes > 0);
        assert_eq!(receipt.source_bytes, src_bytes);
        assert_eq!(
            receipt.entries, 5,
            "foo + SKILL.md + link + sub + sub/a.txt"
        );
        // 原子写：导出目录里只有成品，没有 .tmp-* 残渣。
        let names: Vec<String> = fs::read_dir(&out_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");

        let mut listed = list_entries(&receipt.path).unwrap();
        listed.sort();
        assert_eq!(
            listed,
            vec![
                ".claude/skills/foo".to_string(),
                ".claude/skills/foo/SKILL.md".to_string(),
                ".claude/skills/foo/link".to_string(),
                ".claude/skills/foo/sub".to_string(),
                ".claude/skills/foo/sub/a.txt".to_string(),
            ]
        );

        let dest = tmp.path().join("restored");
        assert_eq!(extract_to(&receipt.path, &dest).unwrap(), 5);

        let back = dest.join(".claude/skills/foo");
        assert_eq!(fs::read(back.join("SKILL.md")).unwrap(), b"# foo\n");
        assert_eq!(fs::read(back.join("sub/a.txt")).unwrap(), vec![7u8; 4096]);
        // 权限位随包带出。
        assert_eq!(
            fs::metadata(back.join("SKILL.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        // 符号链接仍是链接，且指向原来的目标——没有被跟随成一份副本。
        let link = back.join("link");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("SKILL.md"));
    }

    #[test]
    fn home之外的路径退化为去前导斜杠的绝对路径() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let outside = tmp.path().join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("x.txt"), b"x").unwrap();

        let receipt = archive_paths(
            "uninstall-x",
            &[outside.join("x.txt")],
            &tmp.path().join("e"),
            &home,
        )
        .unwrap();
        let listed = list_entries(&receipt.path).unwrap();
        let want = outside
            .join("x.txt")
            .to_string_lossy()
            .trim_start_matches('/')
            .to_string();
        assert_eq!(listed, vec![want]);
    }

    #[test]
    fn 源路径不可读时报错且不留半个包() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let out_dir = tmp.path().join("exports");

        let err = archive_paths("prune-skill", &[home.join("ghost")], &out_dir, &home).unwrap_err();
        assert!(err.to_string().contains("failed to stat"), "{err}");
        // 目录被建出来了，但里面不能有任何东西——成品和临时文件都不留。
        assert_eq!(fs::read_dir(&out_dir).unwrap().count(), 0);

        assert!(archive_paths("prune-skill", &[], &out_dir, &home).is_err());
    }
}
