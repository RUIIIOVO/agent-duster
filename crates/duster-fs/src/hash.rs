//! BLAKE3 指纹:bytes hash(流式)+ cheap print(size, mtime_ns, inode)。
//!
//! - [`hash_file`][]:流式计算单文件内容哈希,大文件不整读内存。
//! - [`CheapPrint`][]:廉价指纹(size + mtime_ns + inode),用于索引层快速判断
//!   文件是否可能变化;编码为定长 24 字节 BLOB。
//! - [`hash_tree`][]:目录语义哈希,按相对路径字典序 feed(路径, 内容哈希),
//!   与 mtime/遍历顺序无关,用于 skill 目录去重。

use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Context;

/// 流式计算文件内容的 BLAKE3 哈希。
///
/// 内部由 blake3 以固定大小缓冲区分块读取,不会将整个文件读入内存。
pub fn hash_file(path: &Path) -> anyhow::Result<blake3::Hash> {
    let file =
        File::open(path).with_context(|| format!("failed to open file: {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_reader(file)
        .with_context(|| format!("failed to read file: {}", path.display()))?;
    Ok(hasher.finalize())
}

/// 廉价指纹:不读文件内容,仅凭元数据判断"文件是否可能变了"。
///
/// 三元组任一变化都视为需要重新计算内容哈希;三者都相同则认为文件未变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheapPrint {
    /// 文件字节数。
    pub size: u64,
    /// 修改时间,纳秒精度(Unix epoch 起)。
    pub mtime_ns: i64,
    /// inode 号,识别"同路径被替换成另一个文件"。
    pub ino: u64,
}

impl CheapPrint {
    /// 定长小端编码:`size(8) | mtime_ns(8) | ino(8)`,供索引层作 BLOB 存储。
    pub fn to_bytes(&self) -> [u8; 24] {
        let mut buf = [0u8; 24];
        buf[..8].copy_from_slice(&self.size.to_le_bytes());
        buf[8..16].copy_from_slice(&self.mtime_ns.to_le_bytes());
        buf[16..].copy_from_slice(&self.ino.to_le_bytes());
        buf
    }

    /// [`Self::to_bytes`] 的逆操作。
    pub fn from_bytes(bytes: &[u8; 24]) -> Self {
        Self {
            size: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            mtime_ns: i64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            ino: u64::from_le_bytes(bytes[16..].try_into().unwrap()),
        }
    }
}

/// 读取文件的廉价指纹(Unix 专用,依赖 `MetadataExt`)。
pub fn cheap_print(path: &Path) -> anyhow::Result<CheapPrint> {
    let meta = fs::metadata(path)
        .with_context(|| format!("failed to read metadata: {}", path.display()))?;
    Ok(CheapPrint {
        size: meta.size(),
        mtime_ns: meta
            .mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(meta.mtime_nsec()),
        ino: meta.ino(),
    })
}

/// 目录语义哈希:内容相同则哈希相同,与 mtime、遍历顺序、绝对路径无关。
///
/// 规则:
/// - 只统计常规文件,符号链接与其他类型跳过;
/// - 目录名命中 `prune`(如 `node_modules`、`.git`)则整棵子树跳过;
/// - 按相对路径(`/` 分隔)字典序,逐个 feed
///   `len(rel_path) LE u64 | rel_path bytes | 文件内容 BLAKE3(32B)`,
///   长度前缀避免路径与内容拼接产生歧义。
pub fn hash_tree(root: &Path, prune: &[String]) -> anyhow::Result<blake3::Hash> {
    // 收集所有 (相对路径, 绝对路径),再统一排序,保证与遍历顺序无关。
    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    collect_files(root, root, prune, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    for (rel, abs) in &files {
        hasher.update(&(rel.len() as u64).to_le_bytes());
        hasher.update(rel.as_bytes());
        hasher.update(hash_file(abs)?.as_bytes());
    }
    Ok(hasher.finalize())
}

/// 递归收集 `dir` 下的常规文件,填入 `(相对 root 的路径, 绝对路径)`。
fn collect_files(
    root: &Path,
    dir: &Path,
    prune: &[String],
    out: &mut Vec<(String, std::path::PathBuf)>,
) -> anyhow::Result<()> {
    let entries = fs::read_dir(dir)
        .with_context(|| format!("failed to read directory: {}", dir.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read directory entry: {}", dir.display()))?;
        let path = entry.path();
        // 用 symlink_metadata 拿到的类型判断,符号链接不跟随。
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let name = entry.file_name();
            if prune.iter().any(|p| p.as_str() == name.to_string_lossy()) {
                continue;
            }
            collect_files(root, &path, prune, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .expect("child path must be prefixed by root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
        }
        // 符号链接、socket 等一律跳过。
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 同内容不同文件名 -> hash_file 相等;内容不同 -> 不等。
    #[test]
    fn hash_file_depends_only_on_content() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let c = dir.path().join("c.txt");
        fs::write(&a, b"hello duster").unwrap();
        fs::write(&b, b"hello duster").unwrap();
        fs::write(&c, b"something else").unwrap();

        assert_eq!(hash_file(&a).unwrap(), hash_file(&b).unwrap());
        assert_ne!(hash_file(&a).unwrap(), hash_file(&c).unwrap());
    }

    /// 树内容不变仅 mtime 变 -> hash_tree 不变;内容变 -> 变。
    #[test]
    fn hash_tree_ignores_mtime_but_tracks_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("top.md"), b"top").unwrap();
        fs::write(dir.path().join("sub/inner.md"), b"inner").unwrap();

        let before = hash_tree(dir.path(), &[]).unwrap();

        // 重写相同内容,mtime 前移但内容不变。
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(dir.path().join("top.md"), b"top").unwrap();
        assert_eq!(before, hash_tree(dir.path(), &[]).unwrap());

        // 内容变化 -> 哈希变化。
        fs::write(dir.path().join("top.md"), b"TOP").unwrap();
        assert_ne!(before, hash_tree(dir.path(), &[]).unwrap());
    }

    /// prune 目录内的文件变动不影响 hash_tree。
    #[test]
    fn hash_tree_prunes_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        fs::write(dir.path().join("main.rs"), b"fn main() {}").unwrap();
        fs::write(dir.path().join("node_modules/pkg/index.js"), b"v1").unwrap();

        let prune = vec!["node_modules".to_string()];
        let before = hash_tree(dir.path(), &prune).unwrap();

        fs::write(dir.path().join("node_modules/pkg/index.js"), b"v2").unwrap();
        assert_eq!(before, hash_tree(dir.path(), &prune).unwrap());

        // 不 prune 时能感知到差异,证明 prune 确实生效而非碰巧相等。
        assert_ne!(hash_tree(dir.path(), &[]).unwrap(), before);
    }

    /// 文件重命名(路径参与哈希)-> hash_tree 变化。
    #[test]
    fn hash_tree_tracks_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"same").unwrap();
        let before = hash_tree(dir.path(), &[]).unwrap();

        fs::rename(dir.path().join("a.txt"), dir.path().join("b.txt")).unwrap();
        assert_ne!(before, hash_tree(dir.path(), &[]).unwrap());
    }

    /// CheapPrint 定长编解码 roundtrip,含负 mtime_ns 边界。
    #[test]
    fn cheap_print_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"12345").unwrap();

        let print = cheap_print(&file).unwrap();
        assert_eq!(print.size, 5);
        assert_eq!(print, CheapPrint::from_bytes(&print.to_bytes()));

        // 人工构造的极端值也必须 roundtrip。
        let extreme = CheapPrint {
            size: u64::MAX,
            mtime_ns: i64::MIN,
            ino: 0,
        };
        assert_eq!(extreme, CheapPrint::from_bytes(&extreme.to_bytes()));
    }
}
