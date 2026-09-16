//! 目录树构建。
//!
//! `build_subtree(dir, depth)` 递归生成；首屏 `build_subtree(root, initial_depth)`，
//! 懒加载 `build_subtree(X, expand_depth + 1)`（多一层用于计算各子节点 has_children）。
//! 排序：有 weight 升序在前，无 weight 按标题字符串序在后。

use crate::scanner::{DirMeta, SiteIndex, is_draft_excluded, strip_numeric_prefix};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    /// 目录（分支）
    Branch,
    /// 文档（叶子）
    Leaf,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeNode {
    /// 对外 href（encode 形态；permalink 页面用 permalink 原样）
    pub url: String,
    /// `_index` title > 目录名/文档名（去扩展名）
    pub title: String,
    pub weight: Option<i64>,
    /// 菜单图标（frontmatter `icon`：Iconify 名或图片 URL）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub node_type: NodeType,
    /// 前端据此显示展开箭头
    pub has_children: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeNode>,
}

/// 同级节点排序键：
/// 1) weight 升序，无 weight 整体沉底（Hugo 语义）
/// 2) 文件名数字前缀升序（`1.xxx.md` → 1；无前缀沉底）
/// 3) date 倒序（字符串比较；无 date 沉底）
/// 4) 文件名字符串序
///
/// 必须以元组键整体比较（compare_keys），不可 pairwise 特判
/// "缺失落下一级"——date 级若 pairwise 会破坏传递性：
/// A(a,date2020) < B(b,无date) < C(c,date2021) 但 C < A（date 倒序），
/// Rust sort 检测到非全序直接 panic（真实内容源曾触发）。
#[derive(Debug, Clone)]
pub struct SortKey {
    weight: Option<i64>,
    file_prefix: Option<u64>,
    date: Option<String>,
    file_stem: String,
}

fn file_numeric_prefix(file_stem: &str) -> Option<u64> {
    let num = file_stem.split('.').next()?;
    if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
        num.parse().ok()
    } else {
        None
    }
}

impl SortKey {
    /// date 传入 frontmatter 原始字符串；排序比较用规范化形态
    /// （`YYYY-MM-DD`，见 `frontmatter::normalize_date`）——同日带时间与
    /// 不带时间的文档比较结果一致，非法值按 None 沉底。
    pub fn new(weight: Option<i64>, file_stem: &str, date: Option<&str>) -> Self {
        Self {
            weight,
            file_prefix: file_numeric_prefix(file_stem),
            date: date.and_then(crate::frontmatter::normalize_date),
            file_stem: file_stem.to_string(),
        }
    }
}

pub fn compare_keys(a: &SortKey, b: &SortKey) -> std::cmp::Ordering {
    use std::cmp::Reverse;
    // 统一元组键：缺失项映射到固定沉底位，元组比较天然全序。
    // (无weight沉底, weight升序, 无前缀沉底, 前缀升序, date倒序+无date沉底, 文件名序)
    let ka = (
        a.weight.is_none(),
        a.weight.unwrap_or(0),
        a.file_prefix.is_none(),
        a.file_prefix.unwrap_or(0),
        Reverse(a.date.clone()),
        a.file_stem.clone(),
    );
    let kb = (
        b.weight.is_none(),
        b.weight.unwrap_or(0),
        b.file_prefix.is_none(),
        b.file_prefix.unwrap_or(0),
        Reverse(b.date.clone()),
        b.file_stem.clone(),
    );
    ka.cmp(&kb)
}

/// 构建指定目录下 depth 层的子树。
///
/// `depth = 0` 返回空 children（仅用于父层 has_children 计算，不直接对外）；
/// `depth >= 1` 时返回该目录的子节点，每个子节点再向下展开 depth - 1 层。
/// draft 页面在 `draft_enabled = false` 时排除。
pub fn build_subtree(
    index: &SiteIndex,
    dir: &Path,
    depth: usize,
    draft_enabled: bool,
) -> Vec<TreeNode> {
    let Some(dm) = index.dirs.get(dir) else {
        return Vec::new();
    };
    if depth == 0 {
        return Vec::new();
    }
    let mut nodes: Vec<(TreeNode, SortKey)> = Vec::new();

    for child_dir in &dm.child_dirs {
        let Some(child_dm) = index.dirs.get(child_dir) else {
            continue;
        };
        // draft 排除的 _index.md：目录仍保留在树中（命名结构），但标题回退目录名
        let branch_excluded = child_dm
            .branch_page
            .as_ref()
            .and_then(|bp| index.pages.get(bp))
            .is_some_and(|p| is_draft_excluded(&p.fm, draft_enabled));
        let branch_stem = child_dm
            .rel_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        nodes.push((
            TreeNode {
                url: index.href_for_dir(child_dir),
                title: if branch_excluded {
                    strip_numeric_prefix(&branch_stem).to_string()
                } else {
                    child_dm.title.clone()
                },
                weight: if branch_excluded {
                    None
                } else {
                    child_dm.weight
                },
                icon: if branch_excluded {
                    None
                } else {
                    child_dm.icon.clone()
                },
                node_type: NodeType::Branch,
                has_children: dir_has_visible_children(index, child_dm, draft_enabled),
                children: build_subtree(index, child_dir, depth - 1, draft_enabled),
            },
            SortKey::new(
                if branch_excluded {
                    None
                } else {
                    child_dm.weight
                },
                &branch_stem,
                if branch_excluded {
                    None
                } else {
                    child_dm.date.as_deref()
                },
            ),
        ));
    }

    for page_rel in &dm.child_pages {
        let Some(page) = index.pages.get(page_rel) else {
            continue;
        };
        if is_draft_excluded(&page.fm, draft_enabled) {
            continue;
        }
        let leaf_stem = page_rel
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        nodes.push((
            TreeNode {
                url: index.href_for_page(page_rel),
                title: page
                    .fm
                    .title
                    .clone()
                    .unwrap_or_else(|| strip_numeric_prefix(&leaf_stem).to_string()),
                weight: page.fm.weight,
                icon: page.fm.icon.clone(),
                node_type: NodeType::Leaf,
                has_children: false,
                children: Vec::new(),
            },
            SortKey::new(page.fm.weight, &leaf_stem, page.fm.date.as_deref()),
        ));
    }

    nodes.sort_by(|a, b| compare_keys(&a.1, &b.1));
    nodes.into_iter().map(|(n, _)| n).collect()
}

/// 目录是否有任何可见子项（有子目录或未 draft 排除的子文档；用于 has_children）。
fn dir_has_visible_children(index: &SiteIndex, dm: &DirMeta, draft_enabled: bool) -> bool {
    let has_visible_dir = dm.child_dirs.iter().any(|d| index.dirs.contains_key(d));
    let has_visible_page = dm.child_pages.iter().any(|p| {
        index
            .pages
            .get(p)
            .is_none_or(|pg| !is_draft_excluded(&pg.fm, draft_enabled))
    });
    has_visible_dir || has_visible_page
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sort_key_four_level_chain() {
        let k = |w: Option<i64>, stem: &str, d: Option<&str>| SortKey::new(w, stem, d);
        // 1) 有 weight 升序在前；无 weight 沉底（无论对方 weight 多大）
        assert_eq!(
            compare_keys(&k(Some(1), "a", None), &k(Some(2), "b", None)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_keys(&k(None, "a", None), &k(Some(2), "b", None)),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_keys(&k(None, "1.a", None), &k(Some(2), "z", None)),
            std::cmp::Ordering::Greater
        );
        // 2) weight 相等 → 数字前缀升序；无前缀排有前缀之后
        assert_eq!(
            compare_keys(&k(None, "2.b", None), &k(None, "10.a", None)),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_keys(&k(None, "b", None), &k(None, "1.a", None)),
            std::cmp::Ordering::Greater
        );
        // 3) 前缀相同/都无 → date 倒序（新在前）
        assert_eq!(
            compare_keys(
                &k(None, "a", Some("2026-09-02")),
                &k(None, "b", Some("2026-09-01"))
            ),
            std::cmp::Ordering::Less
        );
        // 4) date 相同/都无 → 文件名序
        assert_eq!(
            compare_keys(&k(None, "a.md", None), &k(None, "b.md", None)),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn test_sort_key_total_order_regression() {
        // 修复前该组合 pairwise 比较破坏传递性，Rust sort 直接 panic
        // （真实内容源触发）：
        // A(a,2020) < B(b,无date)（文件名序）、B < C(c,2021)（文件名序）、
        // 但 C < A（date 倒序）——矛盾。
        let a = SortKey::new(None, "a", Some("2020-01-01"));
        let b = SortKey::new(None, "b", None);
        let c = SortKey::new(None, "c", Some("2021-01-01"));
        let mut v = [a.clone(), b.clone(), c.clone()];
        v.sort_by(compare_keys);
        // 全序检查：sort 不 panic 即通过；再验证传递性
        let ab = compare_keys(&a, &b);
        let bc = compare_keys(&b, &c);
        let ac = compare_keys(&a, &c);
        // 传递性：a<b 且 b<c ⇒ a<c
        if ab == std::cmp::Ordering::Less && bc == std::cmp::Ordering::Less {
            assert_eq!(ac, std::cmp::Ordering::Less);
        }
        // 结果确定性：c（有新 date）应排在无 date 的 b 之前
        let titles: Vec<&str> = v.iter().map(|k| k.file_stem.as_str()).collect();
        assert_eq!(titles, vec!["c", "a", "b"]); // c/a 有 date 倒序在前，b 无 date 沉底
    }

    #[test]
    fn test_sort_key_same_day_time_suffix_consistent() {
        // 同一天：带时间与不带时间的文档 date 排序等价（规范化口径，
        // ocean-book 实测两种写法并存；原始字符串比较会因前缀关系反直觉）
        let plain = SortKey::new(None, "a", Some("2025-03-01"));
        let timed = SortKey::new(None, "b", Some("2025-03-01 10:30:09"));
        assert_eq!(
            compare_keys(&plain, &timed),
            std::cmp::Ordering::Less,
            "同日不同写法：date 相等时按文件名序（a < b），而非时间后缀反超"
        );
        let timed2 = SortKey::new(None, "a", Some("2025-03-01 9:15"));
        assert_eq!(
            compare_keys(&plain, &timed2),
            std::cmp::Ordering::Equal,
            "同文件名同日不同写法应完全等价"
        );
    }

    #[test]
    fn test_sort_key_invalid_date_sinks() {
        // 非法 date（正文时间串形态）按无 date 沉底，不乱插
        let bad = SortKey::new(None, "b", Some("Tue, 07 Jun 2014 20:51:35 GMT"));
        let good = SortKey::new(None, "c", Some("2025-01-01"));
        let none = SortKey::new(None, "d", None);
        assert_eq!(compare_keys(&good, &bad), std::cmp::Ordering::Less);
        // 非法 date 规范化为 None：date 级比较相等（沉底），
        // 落到下一级（文件名序 b < d）
        assert_eq!(compare_keys(&bad, &none), std::cmp::Ordering::Less);
        // 同文件名时非法与无 date 完全等价
        let bad_same = SortKey::new(None, "d", Some("Tue, 07 Jun 2014 20:51:35 GMT"));
        assert_eq!(compare_keys(&bad_same, &none), std::cmp::Ordering::Equal);
    }

    #[test]
    fn test_file_numeric_prefix() {
        assert_eq!(file_numeric_prefix("1.入门"), Some(1));
        assert_eq!(file_numeric_prefix("10.deploy"), Some(10));
        assert_eq!(file_numeric_prefix("guide"), None);
        assert_eq!(file_numeric_prefix("1x.bad"), None);
    }
}
