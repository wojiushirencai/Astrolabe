//! 项目记忆：跨会话的知识留存（Serena memories 对应物）。
//! 存 `<root>/.astrolabe/memories/<name>.md`；`.astrolabe` 已有自忽略 `.gitignore`，
//! 记忆不会污染 git status。
//!
//! 使用心智对齐 Serena memories：
//! - 记忆按**名字**寻址：read / write / delete 都只给 `name`，不给路径；
//!   名字要有意义（如 `build-quirks`、`testing-conventions`），因为模型是
//!   从 list 的名字 + 首行摘要推断哪条记忆与当前任务相关的。
//! - 由模型在会话中**自主读写**：学到值得跨会话保留的项目知识（构建怪癖、
//!   常见坑、架构决定）就写一条；下次会话开场 list/read 即可取回。
//! - 正文是 markdown，第一个非空行惯例上是标题或一句话摘要（list 用它做预览）。
//! - CLAUDE.md / AGENTS.md 是**人**写给 agent 的指令文件，不算记忆；
//!   记忆是 agent 自己沉淀的项目知识，两者互补。
//!
//! 全部函数 panic-free：IO 失败映射到 `Err` / `None` / 空表，绝不半写
//! （写盘走同目录 `.tmp` + `rename` 原子替换）。

use std::path::{Path, PathBuf};

/// 记忆名最大长度（字符数）
const MAX_NAME_LEN: usize = 64;

/// 摘要最大长度（字符数）：正文第一个非空行截到此长度
const SUMMARY_MAX_CHARS: usize = 80;

/// 单条记忆正文最大字节数（UTF-8）。超限返回清晰 Err，避免撑爆 MCP 响应。
const MAX_CONTENT_BYTES: usize = 1024 * 1024;

/// 记忆存储目录：`<root>/.astrolabe/memories`
fn memories_dir(root: &Path) -> PathBuf {
    root.join(".astrolabe").join("memories")
}

/// 校验并归一记忆名：先 trim，再要求非空、仅 `[A-Za-z0-9_-]`、长度 ≤ 64。
///
/// 白名单天然拒绝路径分隔符（`/`、`\`）、`..`、`.`、空格与中文等，
/// 保证名字不可能逸出 memories 目录。非法返回 `Err(原因)`。
fn validate_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("memory name is empty".to_string());
    }
    if name
        .chars()
        .any(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
    {
        return Err(format!(
            "memory name '{name}' contains characters outside [A-Za-z0-9_-] \
(path separators, '..', dots and non-ASCII are not allowed)"
        ));
    }
    let len = name.chars().count();
    if len > MAX_NAME_LEN {
        return Err(format!(
            "memory name is too long: {len} chars (max {MAX_NAME_LEN})"
        ));
    }
    Ok(name.to_string())
}

/// 单条记忆文件路径：`<dir>/<name>.md`
fn memory_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.md"))
}

/// 提取正文摘要：第一个非空行 trim 后截 80 字符（按字符截，UTF-8 安全）。
fn summarize(content: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return trimmed.chars().take(SUMMARY_MAX_CHARS).collect();
        }
    }
    String::new()
}

/// 列出记忆：返回 (name, 首行摘要) 列表，按名字典序。目录不存在 → 空。
pub fn list_memories(root: &Path) -> Vec<(String, String)> {
    let dir = memories_dir(root);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // 目录不存在或不可读：当作没有记忆
        Err(_) => return Vec::new(),
    };

    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_md = path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md");
        if !is_md {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // 只列名字合法的记忆（历史遗留的怪文件名不参与寻址）
        if validate_name(stem).is_err() {
            continue;
        }
        let summary = std::fs::read_to_string(&path)
            .map(|content| summarize(&content))
            .unwrap_or_default();
        result.push((stem.to_string(), summary));
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

/// 读单个记忆全文。不存在（或名字非法、IO 失败）→ None。
pub fn read_memory(root: &Path, name: &str) -> Option<String> {
    let name = validate_name(name).ok()?;
    std::fs::read_to_string(memory_path(&memories_dir(root), &name)).ok()
}

/// 写/覆写记忆（name 合法性：非空、只允许 [A-Za-z0-9_-]、长度 ≤ 64，
/// 非法 → Err(原因字符串)；正文超过 [`MAX_CONTENT_BYTES`] → Err；
/// 自动创建目录；原子写：先写 .tmp 再 rename；Windows 上目的已存在时先删再 rename）。
pub fn write_memory(root: &Path, name: &str, content: &str) -> Result<(), String> {
    let name = validate_name(name)?;
    let content_len = content.len();
    if content_len > MAX_CONTENT_BYTES {
        return Err(format!(
            "memory content is too large: {content_len} bytes (max {MAX_CONTENT_BYTES})"
        ));
    }
    let dir = memories_dir(root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create memories dir {}: {e}", dir.display()))?;

    // 原子写：同目录先写 .tmp 再 rename，崩溃/中断最多留下一个 .tmp，
    // 不会把既有记忆截成半写状态（.tmp 后缀也不会被 list 误收）。
    // Windows 上 rename 不能覆盖已存在目标，失败时先 remove 再 retry。
    let final_path = memory_path(&dir, &name);
    let tmp_path = dir.join(format!("{name}.md.tmp"));
    if let Err(e) = std::fs::write(&tmp_path, content) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("failed to write memory '{name}': {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        // Best-effort Windows-safe overwrite: remove destination then rename.
        match std::fs::remove_file(&final_path)
            .and_then(|_| std::fs::rename(&tmp_path, &final_path))
        {
            Ok(()) => {}
            Err(_) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!("failed to finalize memory '{name}': {e}"));
            }
        }
    }
    Ok(())
}

/// 删除记忆。不存在（或名字非法、IO 失败）→ false。
pub fn delete_memory(root: &Path, name: &str) -> bool {
    let Ok(name) = validate_name(name) else {
        return false;
    };
    std::fs::remove_file(memory_path(&memories_dir(root), &name)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// 唯一临时目录夹具（tag + 进程 id + 纳秒时间戳防并发撞名）。
    /// 调用方负责在测试末尾 `remove_dir_all` 清理。
    fn temp_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "astrolabe_memory_{tag}_{}_{}",
            std::process::id(),
            nanos
        ))
    }

    /// 目录下是否存在 .tmp 残留
    fn has_tmp_residue(root: &Path) -> bool {
        std::fs::read_dir(memories_dir(root))
            .map(|entries| {
                entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().contains(".tmp"))
            })
            .unwrap_or(false)
    }

    #[test]
    fn test_0_validate_name_unit() {
        // 合法：字母数字下划线连字符、前后空白被 trim、边界长度 64
        assert_eq!(validate_name("build-quirks").unwrap(), "build-quirks");
        assert_eq!(validate_name("Abc_123-x").unwrap(), "Abc_123-x");
        assert_eq!(validate_name("  spaced  ").unwrap(), "spaced");
        assert_eq!(validate_name(&"a".repeat(64)).unwrap(), "a".repeat(64));

        // 非法：空、纯空白、路径分隔符、.. 、点、空格、中文、超长
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("a b").is_err());
        assert!(validate_name("中文记忆").is_err());
        assert!(validate_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn test_1_write_read_roundtrip_and_list_summary() {
        let root = temp_root("roundtrip");

        // write → read 往返（名字带前后空白也会归一）
        let content = "# 构建怪癖\n\n集成测试需要 --features full。\n";
        assert!(write_memory(&root, " build-quirks ", content).is_ok());
        assert_eq!(read_memory(&root, "build-quirks").as_deref(), Some(content));
        assert_eq!(
            read_memory(&root, "  build-quirks  ").as_deref(),
            Some(content)
        );

        // list 含名字与首行摘要（第一个非空行，跳过前导空行）
        let listed = list_memories(&root);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "build-quirks");
        assert_eq!(listed[0].1, "# 构建怪癖");

        // 摘要截 80 字符：ASCII 长行与中文长行都不 panic
        let long_ascii = "x".repeat(100);
        let long_cjk = "构".repeat(100);
        assert!(write_memory(&root, "ascii", &format!("\n\n{long_ascii}\ntail")).is_ok());
        assert!(write_memory(&root, "cjk", &long_cjk).is_ok());
        let by_name: Vec<(String, String)> = list_memories(&root)
            .into_iter()
            .filter(|(n, _)| n == "ascii" || n == "cjk")
            .collect();
        assert_eq!(by_name.len(), 2);
        assert_eq!(by_name[0], ("ascii".to_string(), "x".repeat(80)));
        assert_eq!(by_name[1], ("cjk".to_string(), "构".repeat(80)));

        // 空内容 → 摘要为空串
        assert!(write_memory(&root, "empty", "").is_ok());
        assert!(list_memories(&root)
            .iter()
            .any(|(n, s)| n == "empty" && s.is_empty()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_2_invalid_names_rejected() {
        let root = temp_root("invalid");

        for bad in ["", "   ", "a/b", "..", "a\\b", "中文", "a.b"] {
            assert!(write_memory(&root, bad, "x").is_err(), "应拒绝: {bad:?}");
        }
        let too_long = "a".repeat(65);
        assert!(write_memory(&root, &too_long, "x").is_err());

        // 拒绝时不应留下任何文件/目录副作用之外的状态：list 为空
        assert!(list_memories(&root).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_3_delete_existing_and_missing_read_missing() {
        let root = temp_root("delete");

        assert!(write_memory(&root, "gone", "bye").is_ok());

        // 删除存在的记忆 → true，删除后读不到
        assert!(delete_memory(&root, "gone"));
        assert_eq!(read_memory(&root, "gone"), None);

        // 删除不存在的 → false；读不存在的 → None
        assert!(!delete_memory(&root, "never-existed"));
        assert_eq!(read_memory(&root, "never-existed"), None);

        // 非法名字同样安全返回 false / None
        assert!(!delete_memory(&root, "a/b"));
        assert_eq!(read_memory(&root, ".."), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_4_missing_dir_list_empty_and_write_creates_dir() {
        let root = temp_root("missing_dir");

        // 目录不存在 → list 为空（而非报错）
        assert!(!memories_dir(&root).exists());
        assert!(list_memories(&root).is_empty());

        // write 自动创建 .astrolabe/memories 目录层级
        assert!(write_memory(&root, "first", "# hello").is_ok());
        let file = memories_dir(&root).join("first.md");
        assert!(file.is_file());
        assert_eq!(read_memory(&root, "first").as_deref(), Some("# hello"));
        assert_eq!(list_memories(&root).len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_5_no_tmp_residue_after_write() {
        let root = temp_root("atomic");

        for (i, name) in ["alpha", "beta", "gamma"].iter().enumerate() {
            assert!(write_memory(&root, name, &format!("content {i}")).is_ok());
        }
        // 多次写入后 memories 目录里不应有任何 .tmp 残留
        assert!(!has_tmp_residue(&root));
        // 且三个文件内容完好
        assert_eq!(read_memory(&root, "beta").as_deref(), Some("content 1"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_6_overwrite_same_name() {
        let root = temp_root("overwrite");

        assert!(write_memory(&root, "note", "# v1\nold body").is_ok());
        assert_eq!(
            read_memory(&root, "note").as_deref(),
            Some("# v1\nold body")
        );

        // 覆写同名记忆：内容整体替换，不留旧版本
        assert!(write_memory(&root, "note", "# v2\nnew body").is_ok());
        assert_eq!(
            read_memory(&root, "note").as_deref(),
            Some("# v2\nnew body")
        );

        // list 仍只有一条，摘要换成新首行
        let listed = list_memories(&root);
        assert_eq!(listed, vec![("note".to_string(), "# v2".to_string())]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_7_list_sorted_and_ignores_foreign_files() {
        let root = temp_root("sorting");

        for name in ["zeta", "alpha", "mid_01"] {
            assert!(write_memory(&root, name, &format!("summary of {name}")).is_ok());
        }
        let listed = list_memories(&root);
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid_01", "zeta"]);

        // 杂散文件不进 list：非 .md 后缀、.tmp、以及名字非法的 .md
        let dir = memories_dir(&root);
        assert!(std::fs::write(dir.join("notes.txt"), "not a memory").is_ok());
        assert!(std::fs::write(dir.join("orphan.md.tmp"), "half").is_ok());
        assert!(std::fs::write(dir.join("怪 名字.md"), "bad name").is_ok());
        assert_eq!(list_memories(&root).len(), 3);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_8_content_size_limit_rejected() {
        let root = temp_root("size_limit");
        let too_big = "x".repeat(MAX_CONTENT_BYTES + 1);
        let err = write_memory(&root, "huge", &too_big).unwrap_err();
        assert!(
            err.contains("too large") && err.contains(&MAX_CONTENT_BYTES.to_string()),
            "{err}"
        );
        assert!(list_memories(&root).is_empty());
        // Boundary: exactly max is allowed
        let exact = "y".repeat(MAX_CONTENT_BYTES);
        assert!(write_memory(&root, "exact", &exact).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }
}
