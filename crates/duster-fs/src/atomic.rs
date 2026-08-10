//! 原子写:同目录 temp + rename + fsync 父目录。

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// 原子地将 `bytes` 写入 `path`。
///
/// 保证:
/// - temp 文件创建在目标同一目录(`.tmp-duster-` 前缀),避免跨文件系统 rename;
/// - 写完 flush + `sync_all`,数据落盘后才 rename 到目标;
/// - rename 后 fsync 父目录,确保目录项本身持久化;
/// - 目标已存在时保留其原有权限位,不存在则使用默认权限。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

    // 目标已存在则先记下权限位,persist 后原样恢复。
    let prev_perms = fs::metadata(path).ok().map(|m| m.permissions());

    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-duster-")
        .tempfile_in(&dir)
        .with_context(|| format!("在 {} 创建临时文件失败", dir.display()))?;

    tmp.write_all(bytes)
        .with_context(|| format!("写入临时文件 {} 失败", tmp.path().display()))?;
    tmp.flush().context("flush 临时文件失败")?;
    tmp.as_file().sync_all().context("fsync 临时文件失败")?;

    tmp.persist(path)
        .with_context(|| format!("rename 到 {} 失败", path.display()))?;

    if let Some(perms) = prev_perms {
        fs::set_permissions(path, perms)
            .with_context(|| format!("恢复 {} 权限失败", path.display()))?;
    }

    // fsync 父目录,让 rename 产生的目录项变更真正落盘。
    let dir_fd =
        File::open(&dir).with_context(|| format!("打开父目录 {} 失败", dir.display()))?;
    dir_fd
        .sync_all()
        .with_context(|| format!("fsync 父目录 {} 失败", dir.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 覆盖已有文件且无临时残留() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        fs::write(&target, b"old").unwrap();

        write_atomic(&target, b"new-content").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new-content");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留 .tmp-* 文件");
    }

    #[test]
    fn 新建文件成功() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("fresh.txt");

        write_atomic(&target, b"hello").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"hello");
    }

    #[cfg(unix)]
    #[test]
    fn 覆盖时保留权限位() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.toml");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

        write_atomic(&target, b"new").unwrap();

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "覆盖后应保留 0600 权限");
    }
}
