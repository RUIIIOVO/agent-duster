//! `~` 路径展开与归一化。

use std::path::{Path, PathBuf};

/// 展开开头的 `~`:仅处理单独 `~` 与 `~/...` 两种形式。
///
/// `~user` 形式不做解析,原样返回;拿不到 home 目录时也原样返回。
pub fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

/// 反向操作:若 `p` 以 home 目录为前缀,则替换回 `~` 显示。
pub fn display_tilde(p: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if p == home {
            return "~".to_string();
        }
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 展开与显示往返一致() {
        let Some(home) = dirs::home_dir() else {
            return; // 无 home 环境下无契约可验证
        };

        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));

        assert_eq!(display_tilde(&home), "~");
        assert_eq!(display_tilde(&home.join("foo/bar")), "~/foo/bar");

        // roundtrip
        let orig = "~/some/dir";
        assert_eq!(display_tilde(&expand_tilde(orig)), orig);
    }

    #[test]
    fn 非波浪线路径原样返回() {
        assert_eq!(expand_tilde("/etc/hosts"), PathBuf::from("/etc/hosts"));
        assert_eq!(expand_tilde("~user/x"), PathBuf::from("~user/x"));
        assert_eq!(display_tilde(Path::new("/etc/hosts")), "/etc/hosts");
    }
}
