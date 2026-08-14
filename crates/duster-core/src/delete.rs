//! 删除底座（Contract 1）：`duster session rm` / `duster skill rm` /
//! `duster memory rm` / `duster mcp rm` 共用的删除语义。
//!
//! 四个名词的删除各自有各自的定位（会话在 jsonl 里、skill 在目录里、
//! 记忆在文本文件里、MCP 声明在别人家配置文件里），但**删除之前的那道
//! 工序是同一份**：不可再生的内容必须先打包进导出目录，删完再更新索引。
//! 这份底座只放四个名词共用、且签名不许各写各的东西：
//!
//! - [`DeleteOptions`]：一次删除的输入。四组的 CLI 旗标同源
//!   （`--no-archive` / `--dry-run` / `--json`），字段对齐才不会被
//!   某一个入口偷偷多出一个"顺手"的开关。
//! - [`DeleteReport`]：一次删除的完整报告，`--json` 的输出形状。
//! - [`archive_before_delete`]：删除前归档。**唯一的归档入口**——
//!   谁再自己写一遍 tar 逻辑，两套实现迟早走散（uninstall 的归档
//!   实现就长在 `uninstall.rs` 里，见下面的归属说明）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use duster_fs::archive;

/// 一次删除的输入。四个名词共用这一份；字段按各自的入口填。
#[derive(Debug, Clone, Default)]
pub struct DeleteOptions {
    pub index_path: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// 删之前把内容打包进导出目录。**默认 true**；脚本用 `--no-archive`
    /// 显式关——关掉的意思是"我接受内容永久消失"，不该是顺手的事。
    pub archive: bool,
    /// 只报"将删什么"，一个字节都不动。
    pub dry_run: bool,
}

/// 一次删除的完整报告（`--json` 的输出形状，四个名词共用）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeleteReport {
    /// 归档包路径；`--no-archive` 或 dry-run 时为 None。
    pub archived: Option<PathBuf>,
    /// 实际删掉的路径（库型会话删的是源库里的行，这里记的是库文件本身）。
    pub removed: Vec<PathBuf>,
    /// 释放的字节数（被删内容的实测体积和）。
    pub freed_bytes: u64,
    /// 非致命问题（源库被锁、行已被外力删掉等），不中断批量。
    pub warnings: Vec<String>,
}

/// 归档包的落点目录：`<home>/agent-duster-exports`。
///
/// 抽出来是因为有两个用途:真写时 [`archive_before_delete`] 往这里打包,
/// 而干跑要在**不编造包名**的前提下把去处告诉用户(包名带秒级时间戳,
/// 干跑预告一个不会存在的文件名是假信息)。两处各写一遍同一个字面量,
/// 改目录名时必然漏一处。
pub fn exports_dir(home: &Path) -> PathBuf {
    home.join("agent-duster-exports")
}

/// 删之前把这些路径打包进 `<home>/agent-duster-exports/<label>-<ts>.tar.zst`，
/// 返回归档包路径。
///
/// 归档口径**照抄 uninstall 已有的那套**：同一个
/// [`duster_fs::archive::archive_paths`]——同一导出目录、同一命名
/// （`<op>-<YYYYMMDD-HHMMSS>.tar.zst`）、同一 tar 布局（条目路径相对
/// home，`tar -xf` 在 home 下解开即原位还原）。不新起一种格式：
/// 用户导出目录里躺着两种长得不一样的包，等于让他维护两套恢复流程。
///
/// `home` 是各入口 resolve 后的 home（注入的假 home 或真实 home 二选一）。
/// 导出目录跟着 home 走：真实 home 下就是 `~/agent-duster-exports`
/// （与 [`archive::default_export_dir`] 同一个地方），测试注入假 home 时
/// 归档落进假 home，真实 `~/` 一个字节都不碰。
///
/// 打包失败即返回 Err：**先打包成功再删，顺序不能反**——这是归档机制
/// 存在的全部意义。
pub fn archive_before_delete(paths: &[PathBuf], label: &str, home: &Path) -> Result<PathBuf> {
    let out_dir = exports_dir(home);
    let receipt = archive::archive_paths(label, paths, &out_dir, home)
        .with_context(|| format!("failed to archive {label} before deletion"))?;
    Ok(receipt.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// 归档口径必须与 uninstall 的导出同一套：包在 `<home>/agent-duster-exports/`
    /// 下、名字是 `<label>-<ts>.tar.zst`、条目路径相对 home（`tar -xf` 在 home
    /// 下解开即原位还原）。四个名词的 rm 都走这一份，谁也不能新起一种格式。
    #[test]
    fn archive_before_delete_落在导出目录且条目相对home() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let skill = home.join(".claude").join("skills").join("foo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "body").unwrap();

        let archive = archive_before_delete(&[skill.clone()], "skill-rm", home).unwrap();
        assert!(archive.is_file(), "{}", archive.display());
        assert!(
            archive.starts_with(home.join("agent-duster-exports")),
            "归档必须落在 home 的导出目录: {}",
            archive.display()
        );
        let name = archive.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("skill-rm-") && name.ends_with(".tar.zst"),
            "{name}"
        );

        let dest = home.join("restore");
        duster_fs::archive::extract_to(&archive, &dest).unwrap();
        assert!(
            dest.join(".claude/skills/foo/SKILL.md").is_file(),
            "条目路径必须相对 home"
        );
    }
}
