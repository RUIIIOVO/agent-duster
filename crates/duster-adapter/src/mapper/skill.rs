//! `skill/frontmatter-md`：解析 SKILL.md YAML frontmatter -> SkillMeta。
//!
//! 容错原则：宁可信息降级，不可扫描中断。frontmatter 缺失或损坏都不报错，
//! 只有 SKILL.md 本身不存在/不可读才算失败（调用方已确认该目录是 skill 时才会调）。

use std::path::Path;

use anyhow::Context;
use duster_model::SkillMeta;

/// 解析 `<skill_root>/SKILL.md`，产出 [`SkillMeta`]。
///
/// - `name`：frontmatter `name` 优先，缺省用目录名。
/// - `description`：frontmatter `description`，可缺。
/// - `extra`：其余 frontmatter 字段序列化为 JSON 原样带走；无剩余字段则为 `None`。
/// - `tree_hash`：恒为 `None`，由上层按需计算（职责分离）。
///
/// 容错：无 frontmatter -> 只有目录名，正常返回；YAML 语法错 -> 降级为无
/// frontmatter 处理，并在 `extra` 记 `{"_parse_error": ...}`。
/// SKILL.md 不存在或不可读 -> 报错。
pub fn parse_skill_md(skill_root: &Path) -> anyhow::Result<SkillMeta> {
    let md_path = skill_root.join("SKILL.md");
    let content = std::fs::read_to_string(&md_path)
        .with_context(|| format!("failed to read {}", md_path.display()))?;

    // 目录名兜底：根路径没有 file_name 时退化为完整路径展示。
    let dir_name = skill_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| skill_root.display().to_string());

    let mut meta = SkillMeta {
        name: dir_name,
        description: None,
        root: skill_root.to_path_buf(),
        tree_hash: None,
        extra: None,
    };

    let Some(raw) = extract_frontmatter(&content) else {
        return Ok(meta); // 无 frontmatter：目录名兜底，正常返回。
    };

    match serde_yaml::from_str::<serde_yaml::Value>(raw) {
        Ok(serde_yaml::Value::Mapping(mut map)) => {
            if let Some(name) = take_string(&mut map, "name") {
                meta.name = name;
            }
            meta.description = take_string(&mut map, "description");
            if !map.is_empty() {
                match serde_json::to_string(&map) {
                    Ok(json) => meta.extra = Some(json),
                    // 非字符串键等 JSON 表达不了的结构：降级记错，不中断。
                    Err(e) => meta.extra = Some(parse_error_json(&e.to_string())),
                }
            }
        }
        // frontmatter 不是映射（纯标量/序列）：无字段可取，记错降级。
        Ok(_) => meta.extra = Some(parse_error_json("frontmatter is not a key-value mapping")),
        Err(e) => meta.extra = Some(parse_error_json(&e.to_string())),
    }

    Ok(meta)
}

/// 遍历 `dir` 的一级子目录，含 SKILL.md 的都算 skill。
///
/// `dir` 不存在视为空（很多 agent 根本没装过 skill），结果按 name 排序保证稳定。
///
/// **悬空软链也会产出**：条目本身是软链但目标 resolve 失败时，没有 SKILL.md
/// 可读（frontmatter 无从谈起），按目录项名产出一条字段全空的占位元数据。
/// 为什么索引一个"目标已不存在"的 skill——它是 claude 启动时当真会去加载
/// 却失败的一项，用户必须能在 `skill list` 里看见它（status 体检已删，
/// 那里是唯一出口）；"坏没坏"由 `skill_ops::list` 当场看文件系统判定，
/// 索引里只负责保证这一行存在。
pub fn discover_skills(dir: &Path) -> anyhow::Result<Vec<SkillMeta>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("failed to traverse {}", dir.display())),
    };

    let mut skills = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to traverse {}", dir.display()))?;
        let path = entry.path();
        // 真目录与有效软链走同一条判定：is_dir()/is_file() 跟随软链，
        // 「目标目录里有 SKILL.md」才算 skill。行为与旧版一字不差
        // （docx/pdf 那些有效链继续正常报 linked）。
        if path.is_dir() && path.join("SKILL.md").is_file() {
            skills.push(parse_skill_md(&path)?);
            continue;
        }
        // 悬空软链：is_dir() 跟随失败，上面的判定恒 false，旧版被静默丢弃。
        // file_type() 是 symlink_metadata 语义（不跟随）；metadata() 跟随，
        // 报错即目标 resolve 不了——正是「claude 加载它必然失败」的那一种。
        // 其余条目（无 SKILL.md 的真目录、普通文件、有效软链指向文件等）
        // 照旧跳过。
        if entry.file_type().is_ok_and(|ft| ft.is_symlink())
            && std::fs::metadata(&path).is_err()
        {
            skills.push(SkillMeta {
                // 读不到 SKILL.md，没有 frontmatter 可解，目录项名是唯一
                // 诚实的名字；其余字段给空值（description 空 / None）。
                name: entry.file_name().to_string_lossy().into_owned(),
                description: None,
                root: path,
                tree_hash: None,
                extra: None,
            });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// 提取 `---` 围栏内的 frontmatter 文本。无围栏或未闭合返回 `None`。
fn extract_frontmatter(content: &str) -> Option<&str> {
    // 首行必须恰好是 `---`（容忍 \r\n 与 BOM）。
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = content
        .strip_prefix("---")
        .and_then(|r| r.strip_prefix("\r\n").or_else(|| r.strip_prefix('\n')))?;
    // 找闭合围栏行：`\n---` 后跟行尾或文件尾。
    let mut search_from = 0;
    // 闭合围栏也可能是第一行（空 frontmatter）。
    if let Some(inner) = fence_at_line_start(rest, 0) {
        return Some(inner);
    }
    while let Some(pos) = rest[search_from..].find("\n---") {
        let at = search_from + pos + 1;
        if let Some(inner) = fence_at_line_start(rest, at) {
            return Some(inner);
        }
        search_from = at + 3;
    }
    None
}

/// 若 `rest[at..]` 以 `---` 开头且该行仅含围栏，返回 `rest[..at]` 作为 frontmatter。
fn fence_at_line_start(rest: &str, at: usize) -> Option<&str> {
    let after = rest[at..].strip_prefix("---")?;
    let line_end_ok = after.is_empty()
        || after.starts_with('\n')
        || after.starts_with("\r\n")
        || after.starts_with('\r');
    line_end_ok.then(|| &rest[..at])
}

/// 从 YAML 映射中取出字符串字段（取走即删除，剩余字段进 extra）。
fn take_string(map: &mut serde_yaml::Mapping, key: &str) -> Option<String> {
    match map.remove(serde_yaml::Value::String(key.to_string()))? {
        serde_yaml::Value::String(s) => Some(s),
        // 非字符串值（如 name: 123）：YAML 标量统一转字符串展示。
        other => serde_yaml::to_string(&other)
            .ok()
            .map(|s| s.trim_end().to_string()),
    }
}

/// 构造 `{"_parse_error": ...}` 的 JSON 文本。
fn parse_error_json(msg: &str) -> String {
    serde_json::json!({ "_parse_error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在临时目录下建一个含 SKILL.md 的 skill 目录。
    fn make_skill(root: &Path, dir: &str, md: &str) -> std::path::PathBuf {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), md).unwrap();
        path
    }

    #[test]
    fn parse_standard_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_skill(
            tmp.path(),
            "pdf-tools",
            "---\nname: pdf\ndescription: 处理 PDF\nversion: 2\ntags:\n  - doc\n---\n\n正文。\n",
        );
        let meta = parse_skill_md(&root).unwrap();
        assert_eq!(meta.name, "pdf");
        assert_eq!(meta.description.as_deref(), Some("处理 PDF"));
        assert_eq!(meta.root, root);
        assert!(meta.tree_hash.is_none());
        // 其余字段进 extra，JSON 可回读。
        let extra: serde_json::Value =
            serde_json::from_str(meta.extra.as_deref().unwrap()).unwrap();
        assert_eq!(extra["version"], 2);
        assert_eq!(extra["tags"][0], "doc");
    }

    #[test]
    fn parse_without_frontmatter_falls_back_to_dir_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_skill(tmp.path(), "my-skill", "# 标题\n\n没有 frontmatter。\n");
        let meta = parse_skill_md(&root).unwrap();
        assert_eq!(meta.name, "my-skill");
        assert!(meta.description.is_none());
        assert!(meta.extra.is_none());
    }

    #[test]
    fn parse_broken_yaml_degrades_with_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_skill(
            tmp.path(),
            "broken",
            "---\nname: [未闭合\ndescription: x: y: z\n---\n正文\n",
        );
        let meta = parse_skill_md(&root).unwrap();
        // 降级为无 frontmatter：目录名兜底 + extra 记录错误。
        assert_eq!(meta.name, "broken");
        assert!(meta.description.is_none());
        let extra: serde_json::Value =
            serde_json::from_str(meta.extra.as_deref().unwrap()).unwrap();
        assert!(extra["_parse_error"].is_string());
    }

    #[test]
    fn parse_missing_skill_md_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("empty");
        std::fs::create_dir_all(&root).unwrap();
        assert!(parse_skill_md(&root).is_err());
    }

    #[test]
    fn discover_skips_dirs_without_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        make_skill(tmp.path(), "beta", "---\nname: beta\n---\n");
        make_skill(tmp.path(), "alpha", "无 frontmatter\n");
        // 无 SKILL.md 的目录和散落文件都要跳过。
        std::fs::create_dir_all(tmp.path().join("not-a-skill")).unwrap();
        std::fs::write(tmp.path().join("README.md"), "x").unwrap();

        let skills = discover_skills(tmp.path()).unwrap();
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"]); // 按 name 排序稳定。

        // 目录不存在视为空。
        assert!(
            discover_skills(&tmp.path().join("nope"))
                .unwrap()
                .is_empty()
        );
    }

    /// 真目录 skill、指向有效目录的软链 skill、悬空软链 skill 三者都要返回。
    ///
    /// 悬空软链没有 SKILL.md 可读(目标都没了),按**目录项名**产出占位
    /// 元数据——`skill list` 靠这一行把状态判成 broken,没有这一行用户
    /// 永远看不见它。
    #[test]
    fn discover_keeps_dangling_symlinks_with_dir_name() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();

        // 1. 真目录 skill。
        make_skill(&skills, "a-real", "---\nname: a-real\n---\n");
        // 2. 指向有效目录的软链 skill(目标在 skills 之外)。
        let target = tmp.path().join("target-skill");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("SKILL.md"), "---\nname: b-linked\n---\n").unwrap();
        symlink(&target, skills.join("b-linked")).unwrap();
        // 3. 悬空软链:目标从未存在。
        symlink(tmp.path().join("gone"), skills.join("z-broken")).unwrap();

        let found = discover_skills(&skills).unwrap();
        let names: Vec<_> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a-real", "b-linked", "z-broken"]);

        // 真目录与有效软链照旧解析出 frontmatter。
        assert_eq!(found[0].root, skills.join("a-real"));
        assert_eq!(found[1].description, None);
        // 悬空那个:名字 = 目录项名,root = 软链自身,其余字段诚实为空。
        let broken = &found[2];
        assert_eq!(broken.name, "z-broken");
        assert_eq!(broken.root, skills.join("z-broken"));
        assert!(broken.description.is_none());
        assert!(broken.tree_hash.is_none());
        assert!(broken.extra.is_none());
    }
}
