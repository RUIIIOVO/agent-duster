//! 并行目录遍历与体积统计（jwalk）。
//!
//! 两个入口：
//! - [`walk_stats`]：聚合整棵树的字节数与文件数，并给出一级子目录的聚合体积；
//! - [`walk_files`]：逐文件回调，供上层（指纹、索引）消费。
//!
//! 体积一律按 `symlink_metadata().len()` 累计：不跟随符号链接，链接本身
//! 按自身大小计，天然无环。

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::Metadata;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use jwalk::WalkDirGeneric;

/// 遍历选项。
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// 是否跟随符号链接。恒为 `false`（字段仅为语义显式保留）：
    /// 链接按自身大小计，指向目录的链接不深入。
    pub follow_links: bool,
    /// 不深入的目录名（如 `node_modules`、`.git`）：整棵子树只计入
    /// 聚合体积/数量，不逐文件上报。按目录名精确匹配，只对目录生效。
    pub prune_dirs: Vec<String>,
}

/// 目录体积统计结果。
#[derive(Debug, Clone, Default)]
pub struct DirStats {
    /// 全部非目录条目（文件 + 符号链接）的字节数总和。
    pub total_bytes: u64,
    /// 非目录条目数量。
    pub file_count: u64,
    /// 一级子目录及其子树聚合体积，按字节数降序（同值按路径升序）。
    pub children: Vec<(PathBuf, u64)>,
}

/// 带每条目字节数的统计遍历器。
type StatsWalk = WalkDirGeneric<((), Option<u64>)>;
/// 带每条目元数据的逐文件遍历器。
type FilesWalk = WalkDirGeneric<((), Option<Metadata>)>;

/// 校验根路径存在且为目录。
fn ensure_dir_root(root: &Path) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(root)
        .with_context(|| format!("无法读取根路径: {}", root.display()))?;
    if !meta.is_dir() {
        bail!("根路径不是目录: {}", root.display());
    }
    Ok(())
}

/// 并行统计 `root` 子树：总字节数、文件数、一级子目录聚合体积（降序）。
///
/// prune 目录的子树同样计入聚合结果——聚合体积本就要求走完整棵子树，
/// prune 只影响 [`walk_files`] 的逐文件上报。遍历中单条读取失败
/// （权限不足等）跳过该条目，不中断整体统计。
pub fn walk_stats(root: &Path, opts: &WalkOptions) -> anyhow::Result<DirStats> {
    ensure_dir_root(root)?;
    // follow_links 恒 false、prune 不改变聚合值，opts 在此无额外分支。
    let _ = opts;

    let walker = StatsWalk::new(root)
        .follow_links(false)
        .skip_hidden(false)
        .process_read_dir(|_depth, _dir, _state, children| {
            // 在 jwalk 的读目录线程里并行取 len，避免消费端串行 stat。
            for child in children.iter_mut().flatten() {
                if !child.file_type.is_dir() {
                    child.client_state = std::fs::symlink_metadata(child.path())
                        .map(|m| m.len())
                        .ok();
                }
            }
        });

    let mut total_bytes = 0u64;
    let mut file_count = 0u64;
    let mut by_child: HashMap<PathBuf, u64> = HashMap::new();

    for entry in walker {
        let Ok(entry) = entry else { continue };
        if entry.depth == 0 {
            continue;
        }
        if entry.file_type.is_dir() {
            // 一级子目录即使为空也要出现在 children 里。
            if entry.depth == 1 {
                by_child.entry(entry.path()).or_insert(0);
            }
            continue;
        }
        let len = entry.client_state.unwrap_or(0);
        total_bytes += len;
        file_count += 1;
        if entry.depth >= 2
            && let Ok(rel) = entry.path().strip_prefix(root)
            && let Some(first) = rel.components().next()
        {
            *by_child.entry(root.join(first.as_os_str())).or_insert(0) += len;
        }
    }

    let mut children: Vec<(PathBuf, u64)> = by_child.into_iter().collect();
    children.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Ok(DirStats {
        total_bytes,
        file_count,
        children,
    })
}

/// 并行遍历 `root` 子树，对每个非目录条目（文件 + 符号链接）调用
/// `f(路径, symlink_metadata)`。
///
/// [`WalkOptions::prune_dirs`] 命中的目录不深入，其子树不产生任何回调；
/// 指向目录的符号链接同样不深入。单条读取失败跳过。
pub fn walk_files(
    root: &Path,
    opts: &WalkOptions,
    mut f: impl FnMut(&Path, &Metadata),
) -> anyhow::Result<()> {
    ensure_dir_root(root)?;
    let prune: Vec<OsString> = opts.prune_dirs.iter().map(OsString::from).collect();

    let walker = FilesWalk::new(root)
        .follow_links(false)
        .skip_hidden(false)
        .process_read_dir(move |_depth, _dir, _state, children| {
            for child in children.iter_mut().flatten() {
                if child.file_type.is_dir() {
                    if prune.contains(&child.file_name) {
                        // 命中 prune：不深入该目录。
                        child.read_children_path = None;
                    }
                } else {
                    child.client_state = std::fs::symlink_metadata(child.path()).ok();
                }
            }
        });

    for entry in walker {
        let Ok(mut entry) = entry else { continue };
        if entry.file_type.is_dir() {
            continue;
        }
        let Some(meta) = entry.client_state.take() else {
            continue;
        };
        f(&entry.path(), &meta);
    }
    Ok(())
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn write(path: &Path, bytes: usize) {
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    /// 构造嵌套树（含 symlink 与 node_modules），返回精确的预期总字节数。
    ///
    /// ```text
    /// root/
    ///   a.txt              100 B
    ///   link -> a.txt      （符号链接，按自身大小计）
    ///   dirlink -> sub     （指向目录的符号链接，不得深入）
    ///   sub/
    ///     b.bin            2048 B
    ///     deep/c           7 B
    ///   node_modules/pkg/
    ///     big.js           5000 B
    /// ```
    fn build_tree(root: &Path) -> u64 {
        fs::create_dir_all(root.join("sub/deep")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        write(&root.join("a.txt"), 100);
        write(&root.join("sub/b.bin"), 2048);
        write(&root.join("sub/deep/c"), 7);
        write(&root.join("node_modules/pkg/big.js"), 5000);
        symlink(root.join("a.txt"), root.join("link")).unwrap();
        symlink(root.join("sub"), root.join("dirlink")).unwrap();
        let links = fs::symlink_metadata(root.join("link")).unwrap().len()
            + fs::symlink_metadata(root.join("dirlink")).unwrap().len();
        100 + 2048 + 7 + 5000 + links
    }

    #[test]
    fn walk_stats_exact_totals_and_children_desc() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let expected = build_tree(root);

        let stats = walk_stats(root, &WalkOptions::default()).unwrap();

        // 精确等于构造值 ⇒ dirlink 未被跟随（跟随会重复计入 sub 的 2055 字节）。
        assert_eq!(stats.total_bytes, expected);
        assert_eq!(stats.file_count, 6); // 4 个文件 + 2 个符号链接
        assert_eq!(
            stats.children,
            vec![(root.join("node_modules"), 5000), (root.join("sub"), 2055)]
        );
    }

    #[test]
    fn walk_files_prunes_subtree_and_skips_symlinked_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        build_tree(root);

        let opts = WalkOptions {
            follow_links: false,
            prune_dirs: vec!["node_modules".into()],
        };
        let mut seen: Vec<(String, u64)> = Vec::new();
        walk_files(root, &opts, |path, meta| {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            seen.push((rel, meta.len()));
        })
        .unwrap();
        seen.sort();

        // prune 生效：node_modules 子树无任何回调。
        assert!(seen.iter().all(|(rel, _)| !rel.contains("node_modules")));
        // 指向目录的符号链接不深入：只上报链接自身，不上报 dirlink/ 下的文件。
        assert!(seen.iter().all(|(rel, _)| !rel.starts_with("dirlink/")));
        let rels: Vec<&str> = seen.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(
            rels,
            ["a.txt", "dirlink", "link", "sub/b.bin", "sub/deep/c"]
        );
        // 回调携带的是 symlink_metadata:普通文件长度精确。
        assert!(
            seen.iter()
                .any(|(rel, len)| rel == "sub/b.bin" && *len == 2048)
        );
    }

    #[test]
    fn walk_stats_rejects_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-dir");
        assert!(walk_stats(&missing, &WalkOptions::default()).is_err());
    }
}
