//! 并行目录遍历与体积统计（jwalk）。
//!
//! 两个入口：
//! - [`walk_stats`]：聚合整棵树的字节数与文件数，并给出一级子目录的聚合体积；
//! - [`walk_files`]：逐文件回调，供上层（指纹、索引）消费。
//!
//! 体积一律按 `symlink_metadata().len()` 累计：树内不跟随符号链接，链接本身
//! 按自身大小计，天然无环。**根路径本身是指向目录的符号链接时例外**：解析到
//! 真实目录再遍历（符号链接安装的 skill 是合法形态），环风险由上层的
//! 路径所有权白名单兜底。

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

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
    /// 全部非目录条目（文件 + 符号链接）的字节数总和。**含 prune 子树**。
    pub total_bytes: u64,
    /// 落在 [`WalkOptions::prune_dirs`] 子树里的字节数，是 `total_bytes`
    /// 的**子集**而非补集。skill 目录里的 `node_modules`/`dist` 属于
    /// install（删了要重装），上层用 `total_bytes - pruned_bytes` 才是
    /// 真·用户内容体积。prune_dirs 为空时恒为 0。
    pub pruned_bytes: u64,
    /// 非目录条目数量。**含 prune 子树**。
    pub file_count: u64,
    /// 整棵树内全部非目录条目 mtime 的最大值（纳秒）；空树为 0。
    ///
    /// skill 的「上次使用」就是这个数，而不是根目录自身的 mtime——
    /// 目录 mtime 只在直接子项增删时变，改一个深层文件它一动不动。
    pub max_mtime_ns: i64,
    /// 一级子目录及其子树聚合体积，按字节数降序（同值按路径升序）。
    pub children: Vec<(PathBuf, u64)>,
}

/// 带每条目「字节数 + mtime 纳秒」的统计遍历器。
type StatsWalk = WalkDirGeneric<((), Option<(u64, i64)>)>;
/// 带每条目元数据的逐文件遍历器。
type FilesWalk = WalkDirGeneric<((), Option<Metadata>)>;

/// 校验根路径并解析:普通目录原样返回;指向目录的符号链接解析为真实路径;
/// 其余(文件、断链)报错。
fn resolve_dir_root(root: &Path) -> anyhow::Result<PathBuf> {
    let meta = std::fs::symlink_metadata(root)
        .with_context(|| format!("failed to read root path: {}", root.display()))?;
    if meta.is_dir() {
        return Ok(root.to_path_buf());
    }
    if meta.is_symlink() {
        let target = std::fs::metadata(root)
            .with_context(|| format!("root path is a broken symlink: {}", root.display()))?;
        if target.is_dir() {
            return root
                .canonicalize()
                .with_context(|| format!("failed to resolve symlink root: {}", root.display()));
        }
    }
    bail!("root path is not a directory: {}", root.display());
}

/// 并行统计 `root` 子树：总字节数、文件数、最大 mtime、一级子目录聚合体积（降序）。
///
/// [`WalkOptions::prune_dirs`] **不改变** `total_bytes` / `file_count` /
/// `children`——「这棵树有多大」要和 `du` 对得上，剪掉就成了另一个数；
/// prune 只额外把命中子树的字节数单独记进 [`DirStats::pruned_bytes`]，
/// 由上层决定怎么拆桶。遍历中单条读取失败（权限不足等）跳过该条目，
/// 不中断整体统计。
pub fn walk_stats(root: &Path, opts: &WalkOptions) -> anyhow::Result<DirStats> {
    let root = &resolve_dir_root(root)?;
    // follow_links 恒 false;prune_dirs 只影响 pruned_bytes 的归属。
    let prune: Vec<OsString> = opts.prune_dirs.iter().map(OsString::from).collect();

    let walker = StatsWalk::new(root)
        .follow_links(false)
        .skip_hidden(false)
        .process_read_dir(|_depth, _dir, _state, children| {
            // 在 jwalk 的读目录线程里并行取 len+mtime,避免消费端串行 stat。
            for child in children.iter_mut().flatten() {
                if !child.file_type.is_dir() {
                    child.client_state = std::fs::symlink_metadata(child.path())
                        .map(|m| (m.len(), mtime_ns_of(&m)))
                        .ok();
                }
            }
        });

    let mut total_bytes = 0u64;
    let mut pruned_bytes = 0u64;
    let mut file_count = 0u64;
    let mut max_mtime_ns = 0i64;
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
        let (len, mtime_ns) = entry.client_state.unwrap_or((0, 0));
        total_bytes += len;
        file_count += 1;
        max_mtime_ns = max_mtime_ns.max(mtime_ns);
        if let Ok(rel) = entry.path().strip_prefix(root) {
            // prune 按**目录**名匹配,所以只看路径的父段,文件自身叫
            // `dist` 不算命中。
            if !prune.is_empty()
                && rel.parent().is_some_and(|dirs| {
                    dirs.components()
                        .any(|c| prune.iter().any(|p| p.as_os_str() == c.as_os_str()))
                })
            {
                pruned_bytes += len;
            }
            if entry.depth >= 2
                && let Some(first) = rel.components().next()
            {
                *by_child.entry(root.join(first.as_os_str())).or_insert(0) += len;
            }
        }
    }

    let mut children: Vec<(PathBuf, u64)> = by_child.into_iter().collect();
    children.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Ok(DirStats {
        total_bytes,
        pruned_bytes,
        file_count,
        max_mtime_ns,
        children,
    })
}

/// 元数据 mtime -> 纳秒;取不到(平台不支持/时钟早于纪元)记 0,不中断遍历。
fn mtime_ns_of(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
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
    let root = &resolve_dir_root(root)?;
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

    /// prune 只把字节数分流进 pruned_bytes，聚合口径一个字节都不许少。
    #[test]
    fn walk_stats_prune_只分流不改总量() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let expected = build_tree(root);

        let opts = WalkOptions {
            follow_links: false,
            prune_dirs: vec!["node_modules".into()],
        };
        let stats = walk_stats(root, &opts).unwrap();

        assert_eq!(stats.total_bytes, expected, "总量不得因 prune 缩水");
        assert_eq!(stats.file_count, 6);
        assert_eq!(stats.pruned_bytes, 5000, "node_modules/pkg/big.js");
        // children 同样不受影响:它是「这棵树怎么分布」,不是「能清多少」。
        assert_eq!(
            stats.children,
            vec![(root.join("node_modules"), 5000), (root.join("sub"), 2055)]
        );

        // 没声明 prune 就恒为 0——默认行为与改造前完全一致。
        let bare = walk_stats(root, &WalkOptions::default()).unwrap();
        assert_eq!(bare.pruned_bytes, 0);
    }

    /// prune 按目录名匹配:叫 `dist` 的**文件**不算命中。
    #[test]
    fn walk_stats_prune_只认目录名() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("dist"), 64); // 同名文件
        fs::create_dir_all(root.join("pkg/dist")).unwrap();
        write(&root.join("pkg/dist/bundle.js"), 128);

        let opts = WalkOptions {
            follow_links: false,
            prune_dirs: vec!["dist".into()],
        };
        let stats = walk_stats(root, &opts).unwrap();
        assert_eq!(stats.total_bytes, 192);
        assert_eq!(stats.pruned_bytes, 128, "只有 pkg/dist/ 下的才算");
    }

    /// max_mtime_ns 取树内最新文件,不是根目录自身的 mtime。
    #[test]
    fn walk_stats_max_mtime_取树内最新文件() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("deep/deeper")).unwrap();
        write(&root.join("old.txt"), 10);
        write(&root.join("deep/deeper/new.txt"), 10);

        // 固定绝对时间戳,不依赖当前时钟与文件系统时间精度:
        // old = 2001,new = 2033,而根目录自身的 mtime 是"刚才"(夹在中间)。
        let past = UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        let newest = UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000);
        let touch = |rel: &str, t: std::time::SystemTime| {
            fs::File::options()
                .write(true)
                .open(root.join(rel))
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(t))
                .unwrap();
        };
        touch("old.txt", past);
        touch("deep/deeper/new.txt", newest);

        let stats = walk_stats(root, &WalkOptions::default()).unwrap();
        assert_eq!(stats.max_mtime_ns, 2_000_000_000_000_000_000);

        // 根目录自身的 mtime 与之无关:深层文件的改动不会传导到根。
        let root_mtime = mtime_ns_of(&fs::metadata(root).unwrap());
        assert!(
            stats.max_mtime_ns > root_mtime,
            "深层文件比根目录新,却被根目录 mtime 盖住了"
        );

        // 空树为 0。
        let empty = tmp.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(
            walk_stats(&empty, &WalkOptions::default())
                .unwrap()
                .max_mtime_ns,
            0
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

    #[test]
    fn walk_stats_follows_symlinked_root() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real-skill");
        fs::create_dir_all(&real).unwrap();
        write(&real.join("SKILL.md"), 300);
        let link = tmp.path().join("linked-skill");
        symlink(&real, &link).unwrap();

        // 根是指向目录的符号链接:解析后正常统计(符号链接安装的 skill)。
        let stats = walk_stats(&link, &WalkOptions::default()).unwrap();
        assert_eq!(stats.total_bytes, 300);
        assert_eq!(stats.file_count, 1);

        // 断链依旧报错。
        let broken = tmp.path().join("broken");
        symlink(tmp.path().join("gone"), &broken).unwrap();
        assert!(walk_stats(&broken, &WalkOptions::default()).is_err());
    }
}
