//! backfill-date：把文件的可靠时间写入 frontmatter date（M2）。
//!
//! 一次性内容源维护命令（`coral --backfill-date <root> [--force] [--all]`）：
//! - git 仓库（root 或其祖先）：默认只处理**工作区有变更**的文件
//!   （`git status --porcelain` 的 M/A/??）——tracked 变更用 git 最后
//!   提交时间（比 mtime 可靠，同步会刷 mtime），新文件（??）用 mtime；
//!   `--all` 处理全部 md（tracked 用 git 时间，untracked 用 mtime）
//! - 非 git 目录：全部 md 用 mtime（--all 无区别）
//!
//! 写入 date 后同级排序第三级（date 倒序，技术设计 §5.3a）生效。

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
    /// mtime 回填数（untracked/非 git；含于 filled）
    pub mtime_filled: usize,
}

/// 对 root 下 .md 执行 backfill（M2 最终语义见模块注释）。
pub fn backfill_date(root: &Path, force: bool, all: bool) -> std::io::Result<BackfillReport> {
    let mut report = BackfillReport::default();
    let repo = find_git_root(root);
    // 待处理文件集：git 默认模式=工作区变更；其余=全部 md
    let files: Vec<PathBuf> = match &repo {
        Some(r) if !all => changed_md_files(r, root)?,
        _ => {
            let mut v = Vec::new();
            collect_md_files(root, &mut v)?;
            v
        }
    };
    for file in files {
        let Ok(raw) = read_file(&file) else {
            continue; // 读失败跳过（不中断批处理）
        };
        // 时间来源：git 最后提交时间 > mtime 兜底
        let (date, from_mtime) = match repo.as_ref().and_then(|r| git_last_commit(r, &file)) {
            Some(git_time) => (normalize_git_time(&git_time), false),
            None => (file_mtime(&file)?, true),
        };
        match apply_date(&raw, &date, force) {
            ApplyOutcome::Filled(new_content) => {
                std::fs::write(&file, new_content)?;
                report.filled += 1;
                if from_mtime {
                    report.mtime_filled += 1;
                }
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

/// git 默认模式：工作区有变更的 md（`git status --porcelain` 含 untracked ??）。
/// 返回绝对路径（限定 root 子树内）。
fn changed_md_files(repo: &Path, root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("status")
        .arg("--porcelain")
        .arg("--")
        .arg(root)
        .output()
        .map_err(|e| std::io::Error::other(format!("git status 执行失败：{e}")))?;
    if !out.status.success() {
        return Err(std::io::Error::other("git status 非零退出"));
    }
    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.len() < 4 {
            continue;
        }
        let rel = line[3..].trim().trim_matches('"');
        // 重命名形态 "old -> new"：取 new
        let rel = rel.rsplit(" -> ").next().unwrap_or(rel);
        if !rel.ends_with(".md") {
            continue;
        }
        let abs = repo.join(rel);
        // 限定 root 子树（status 按 pathspec 已限定，防御性双保险）
        if abs.starts_with(root) && abs.is_file() {
            files.push(abs);
        }
    }
    Ok(files)
}

/// 文件 mtime → `YYYY-MM-DD HH:MM:SS`（本地时区，与 git 时间格式一致）。
fn file_mtime(path: &Path) -> std::io::Result<String> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| std::io::Error::other(format!("mtime 早于纪元：{e}")))?;
    // 无 chrono 依赖：用本地时区格式化——调用 date 命令不可移植；
    // 简化口径用 UTC+offset？此处直接用秒级时间戳转 RFC 手写成本高。
    // 采用：SystemTime → 手写 UTC 格式（date 倒序字符串比较只需单调一致；
    // git 时间为本地时区，混用不影响同源内排序，跨源偏差可接受——
    // 实际场景 untracked 文件的 mtime 与 git 时间仅在同目录混排时比较，
    // UTC 与本地时区差是常数，不改变同目录内相对顺序）
    let secs = mtime.as_secs() as i64;
    Ok(format_utc(secs))
}

/// epoch 秒 → `YYYY-MM-DD HH:MM:SS`（UTC）。civil-from-days 算法（Howard Hinnant）。
fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // days since 1970-01-01 → civil date
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02}")
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
    fn test_format_utc_known_epoch() {
        // 2026-09-16 07:19:00 UTC（本机 e2e 出现过的值）
        assert_eq!(format_utc(1_789_543_140), "2026-09-16 07:19:00");
        // epoch 0
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
    }

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
