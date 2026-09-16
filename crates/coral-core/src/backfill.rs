//! backfill-date：git 最后提交时间写入 frontmatter date（M2）。
//!
//! 一次性内容源维护命令（`coral --backfill-date <root> [--force]`）：
//! git 时间比 mtime 可靠（同步/rsync 会刷 mtime）；写入 date 后
//! 同级排序第三级（date 倒序，技术设计 §5.3a）直接生效。

use std::path::{Path, PathBuf};

/// 执行结果统计。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BackfillReport {
    /// 补齐 date（原无值）
    pub filled: usize,
    /// 已有 date 跳过
    pub skipped_has_date: usize,
    /// --force 替换
    pub replaced: usize,
    /// git 无提交历史（untracked）
    pub no_history: usize,
}

/// 对 root 下全部 .md 执行 backfill。
///
/// - git 历史查 `git log -1 --format=%ci -- <file>`（在 git 仓库根执行；
///   仓库根 = root 或其祖先，找不到 git 目录则全部计 no_history）
/// - date 缺失 → 在 frontmatter 首行后插入 `date: ...`；无 frontmatter
///   块则创建（仅 date）
/// - 已有 date 默认跳过；force 才替换
/// - git 输出格式 `YYYY-MM-DD HH:MM:SS +0800` → 截时区保留秒
pub fn backfill_date(root: &Path, force: bool) -> std::io::Result<BackfillReport> {
    let mut report = BackfillReport::default();
    let repo = find_git_root(root);
    let mut files = Vec::new();
    collect_md_files(root, &mut files)?;
    for file in files {
        let raw = match read_file(&file) {
            Ok(r) => r,
            Err(_) => continue, // 读失败跳过（不中断批处理）
        };
        let Some(git_time) = repo.as_ref().and_then(|r| git_last_commit(r, &file)) else {
            report.no_history += 1;
            continue;
        };
        let date = normalize_git_time(&git_time);
        match apply_date(&raw, &date, force) {
            ApplyOutcome::Filled(new_content) => {
                std::fs::write(&file, new_content)?;
                report.filled += 1;
            }
            ApplyOutcome::Replaced(new_content) => {
                std::fs::write(&file, new_content)?;
                report.replaced += 1;
            }
            ApplyOutcome::Skipped => report.skipped_has_date += 1,
        }
    }
    Ok(report)
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(p) = cur {
        if p.join(".git").exists() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

fn collect_md_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_md_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    Ok(())
}

fn read_file(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

fn git_last_commit(repo: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(repo).ok()?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("log")
        .arg("-1")
        .arg("--format=%ci")
        .arg("--")
        .arg(rel)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `2026-09-01 13:44:06 +0800` → `2026-09-01 13:44:06`。
fn normalize_git_time(raw: &str) -> String {
    raw.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

enum ApplyOutcome {
    Filled(String),
    Replaced(String),
    Skipped,
}

fn apply_date(raw: &str, date: &str, force: bool) -> ApplyOutcome {
    // frontmatter 块内是否已有 date 字段（浅判 key 行，避免全量 YAML 解析）
    if raw.starts_with("---\n") || raw.starts_with("---\r\n") {
        let body = &raw[raw.find('\n').map(|i| i + 1).unwrap_or(4)..];
        if let Some(end) = find_block_end(body) {
            let (fm, rest) = body.split_at(end);
            if fm.lines().any(|l| l.trim_start().starts_with("date:")) {
                if !force {
                    return ApplyOutcome::Skipped;
                }
                let new_fm: String = fm
                    .lines()
                    .map(|l| {
                        if l.trim_start().starts_with("date:") {
                            format!("date: {date}")
                        } else {
                            l.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return ApplyOutcome::Replaced(format!("---\n{new_fm}\n{rest}"));
            }
            // 无 date：插入到 frontmatter 末行（闭合 --- 之前）
            let trimmed = fm.trim_end_matches(['\n', '\r']);
            return ApplyOutcome::Filled(format!("---\n{trimmed}\ndate: {date}\n{rest}"));
        }
    }
    // 无 frontmatter 块：创建
    ApplyOutcome::Filled(format!("---\ndate: {date}\n---\n\n{raw}"))
}

fn find_block_end(body: &str) -> Option<usize> {
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_git_time_strips_tz() {
        assert_eq!(
            normalize_git_time("2026-09-01 13:44:06 +0800"),
            "2026-09-01 13:44:06"
        );
    }

    #[test]
    fn test_apply_date_fills_into_existing_fm() {
        let raw = "---\ntitle: A\n---\nbody";
        match apply_date(raw, "2026-09-01 13:00:00", false) {
            ApplyOutcome::Filled(s) => {
                assert!(s.contains("title: A"), "{s}");
                assert!(s.contains("\ndate: 2026-09-01 13:00:00\n---"), "{s}");
            }
            _ => panic!("should fill"),
        }
    }

    #[test]
    fn test_apply_date_skips_existing_without_force() {
        let raw = "---\ntitle: A\ndate: 2020-01-01\n---\nbody";
        assert!(matches!(
            apply_date(raw, "2026-09-01", false),
            ApplyOutcome::Skipped
        ));
    }

    #[test]
    fn test_apply_date_replaces_with_force() {
        let raw = "---\ntitle: A\ndate: 2020-01-01\n---\nbody";
        match apply_date(raw, "2026-09-01 13:00:00", true) {
            ApplyOutcome::Replaced(s) => {
                assert!(s.contains("date: 2026-09-01 13:00:00"), "{s}");
                assert!(!s.contains("2020-01-01"), "{s}");
            }
            _ => panic!("should replace"),
        }
    }

    #[test]
    fn test_apply_date_replace_keeps_closing_delim_newline() {
        // 回归：替换后闭合 --- 前必须保留换行（曾出现 "date: ...---" 粘连）
        let raw = "---\ntitle: A\ndate: 2020-01-01\n---\nbody";
        match apply_date(raw, "2026-09-01 13:00:00", true) {
            ApplyOutcome::Replaced(s) => {
                assert!(s.contains("\ndate: 2026-09-01 13:00:00\n---\nbody"), "{s}");
            }
            _ => panic!("should replace"),
        }
    }

    #[test]
    fn test_apply_date_creates_fm_when_missing() {
        let raw = "# body only";
        match apply_date(raw, "2026-09-01 13:00:00", false) {
            ApplyOutcome::Filled(s) => {
                assert!(
                    s.starts_with("---\ndate: 2026-09-01 13:00:00\n---\n"),
                    "{s}"
                );
            }
            _ => panic!("should create"),
        }
    }
}
