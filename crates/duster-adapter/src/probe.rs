//! Probe：agent 安装探测（any_of/all_of/binary/version_cmd）。
//!
//! 输入是清单 `probe` 段的结构化描述（[`ProbeSpec`]），输出是探测结论
//! （[`ProbeOutcome`]）。为与 manifest 模块解耦并行开发，本模块自带
//! `ProbeSpec` 定义，manifest 侧解析后复用/转换到这里，不反向依赖。
//!
//! 探测是纯只读操作：只看文件系统与 `PATH`，不执行任何外部命令。

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 清单 `probe` 段：描述「怎样算装了这个 agent」。
///
/// 三组信号取**或**关系，任一命中即视为已安装：
/// - `any_of`：路径列表，任一存在即命中；
/// - `all_of`：路径列表，**全部**存在才命中（空列表不参与判定）;
/// - `binary`：可执行文件名，按 `which` 语义在 `PATH` 中找到即命中。
///
/// 路径中的 `~` 相对 `probe` 的 `home` 参数展开（而非真实用户目录），
/// 便于测试注入假 home。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProbeSpec {
    /// 任一存在即算安装的路径列表。
    pub any_of: Vec<String>,
    /// 全部存在才算安装的路径列表。
    pub all_of: Vec<String>,
    /// 在 `PATH` 中查找的可执行文件名。
    pub binary: Option<String>,
    /// 获取版本号的命令行（argv 形式）。
    ///
    /// M0 不执行：运行外部命令属于运行时行为，留给 M2 的 doctor。
    pub version_cmd: Option<Vec<String>>,
}

/// 探测结论。
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// 是否判定为已安装。
    pub installed: bool,
    /// `any_of`/`all_of` 中第一个存在的**目录**（按声明顺序）。
    pub root: Option<PathBuf>,
    /// 版本号。M0 恒为 `None`：`version_cmd` 需要执行外部命令，
    /// 属运行时行为，留 M2 的 doctor 实现。
    pub version: Option<String>,
}

/// 依据 `spec` 探测安装状态。
///
/// `home` 用于展开路径中的 `~` 前缀（`~` 或 `~/...`），测试可传入
/// 临时目录当假 home；其余路径原样使用。
pub fn probe(spec: &ProbeSpec, home: &Path) -> ProbeOutcome {
    let any_paths: Vec<PathBuf> = spec.any_of.iter().map(|p| expand_tilde(p, home)).collect();
    let all_paths: Vec<PathBuf> = spec.all_of.iter().map(|p| expand_tilde(p, home)).collect();

    let any_hit = any_paths.iter().any(|p| p.exists());
    let all_hit = !all_paths.is_empty() && all_paths.iter().all(|p| p.exists());
    let binary_hit = spec
        .binary
        .as_deref()
        .is_some_and(|name| find_in_path(name, env::var_os("PATH").as_deref()).is_some());

    // root：any_of/all_of 中第一个存在的目录（普通文件不算 root）。
    let root = any_paths
        .iter()
        .chain(all_paths.iter())
        .find(|p| p.is_dir())
        .cloned();

    ProbeOutcome {
        installed: any_hit || all_hit || binary_hit,
        root,
        // M0 不执行 version_cmd（外部命令属运行时，留 M2 doctor），版本恒缺省。
        version: None,
    }
}

/// 把路径开头的 `~` 展开为 `home`。
///
/// 只处理 `~` 与 `~/...` 两种形式；`~user` 之类不支持，原样返回。
fn expand_tilde(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

/// `which` 语义的 PATH 查找：遍历 `path_var` 的每个目录，返回第一个
/// 存在且（Unix 下）带可执行位的候选。自实现以避免引入依赖；
/// `path_var` 作为参数传入，便于测试注入而不污染进程环境。
fn find_in_path(binary: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    for dir in env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(binary);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// 判断路径是否为可执行的普通文件。
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Windows 下无可执行位概念，存在即可。
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// any_of：假 home 下任一路径存在即 installed，root 指向该目录。
    #[test]
    fn probe_any_of_hits_on_first_existing_dir() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();

        let spec = ProbeSpec {
            any_of: vec!["~/.missing".into(), "~/.claude".into()],
            ..Default::default()
        };
        let out = probe(&spec, home.path());
        assert!(out.installed);
        assert_eq!(out.root, Some(home.path().join(".claude")));
        assert_eq!(out.version, None, "M0 不执行 version_cmd");
    }

    /// all_of：缺一个就不算安装；补齐后判定通过。
    #[test]
    fn probe_all_of_requires_every_path() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".codex")).unwrap();

        let spec = ProbeSpec {
            all_of: vec!["~/.codex".into(), "~/.codex/config.toml".into()],
            ..Default::default()
        };
        assert!(!probe(&spec, home.path()).installed, "缺 config.toml 时不算安装");

        fs::write(home.path().join(".codex/config.toml"), "").unwrap();
        let out = probe(&spec, home.path());
        assert!(out.installed);
        assert_eq!(out.root, Some(home.path().join(".codex")), "root 取第一个存在的目录");
    }

    /// 什么都不存在（空 spec 或路径全缺失）-> not installed、无 root。
    #[test]
    fn probe_nothing_found_means_not_installed() {
        let home = tempfile::tempdir().unwrap();
        let spec = ProbeSpec {
            any_of: vec!["~/.nope".into()],
            all_of: vec!["~/.also-nope".into()],
            ..Default::default()
        };
        let out = probe(&spec, home.path());
        assert!(!out.installed);
        assert_eq!(out.root, None);

        let empty = probe(&ProbeSpec::default(), home.path());
        assert!(!empty.installed, "空 spec 不算安装");
    }

    /// binary：which 语义在注入的 PATH 中找到可执行文件。
    #[test]
    fn find_in_path_locates_executable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("fake-agent");
        fs::write(&exe, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let empty = tempfile::tempdir().unwrap();
        let path_var = env::join_paths([empty.path(), dir.path()]).unwrap();

        assert_eq!(find_in_path("fake-agent", Some(&path_var)), Some(exe));
        assert_eq!(find_in_path("absent-agent", Some(&path_var)), None);
        assert_eq!(find_in_path("fake-agent", None), None, "无 PATH 时找不到");
    }

    /// binary 缺失/不可执行时不误报 installed（正向命中已由
    /// `find_in_path_locates_executable` 覆盖，避免依赖进程真实 PATH）。
    #[cfg(unix)]
    #[test]
    fn probe_binary_absent_or_not_executable_is_not_installed() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let spec = ProbeSpec {
            binary: Some("duster-definitely-not-a-real-binary-42".into()),
            ..Default::default()
        };
        assert!(!probe(&spec, home.path()).installed);

        // 非可执行文件不被 which 语义接受。
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        fs::write(&plain, "data").unwrap();
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
        let path_var = env::join_paths([dir.path()]).unwrap();
        assert_eq!(find_in_path("plain", Some(&path_var)), None);
    }
}
