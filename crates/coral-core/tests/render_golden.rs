//! golden 快照测试（渲染输出做快照，comrak/syntect 升级时 diff 可见）。
//!
//! 期望文件 `tests/snapshots/golden_input.html` 为手工审查后固化；
//! 升级渲染依赖后 diff 不符合预期时，人工确认新输出并更新快照。

use coral_core::config::ContentConfig;
use coral_core::render::render_page;
use coral_core::scanner::{SiteIndex, scan};
use coral_core::shortcode::UnknownShortcodeCounter;
use std::path::Path;

#[test]
fn test_golden_render_output_stable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/site");
    let cfg = ContentConfig {
        root,
        exclude: vec!["drafts".into(), "excluded".into()],
        draft: false,
    };
    let index = SiteIndex::build(scan(&cfg).unwrap(), false);
    let input = include_str!("snapshots/golden_input.md");

    let mut counter = UnknownShortcodeCounter::default();
    let page = render_page(
        input,
        Path::new("golden.md"),
        &index,
        false,
        false,
        Some("2026-09-11"),
        &coral_core::RenderConfig::default(),
        &mut counter,
    )
    .unwrap();

    // UPDATE_GOLDEN=1 cargo test 重新固化快照（升级 comrak/syntect 后人工审查 diff）
    let snapshot_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/golden_input.html");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&snapshot_path, &page.html).expect("写入 golden 快照失败");
        return;
    }

    let expected: &str = include_str!("snapshots/golden_input.html");
    assert_eq!(
        page.html, expected,
        "渲染输出与 golden 快照不一致（升级 comrak/syntect？用 UPDATE_GOLDEN=1 重固化并审查 diff）"
    );
    assert_eq!(page.date_footer.as_deref(), Some("2026-09-11"));
    // TOC 结构同时快照（标题层级 + 锚点）
    let toc: Vec<String> = page
        .toc
        .iter()
        .map(|t| format!("{} {}", t.level, t.anchor))
        .collect();
    assert_eq!(
        toc,
        vec![
            "1 标题一",
            "2 表格与任务列表",
            "2 提示块",
            "2 标签页",
            "2 代码高亮",
            "3 三级标题",
            "2 未知",
            "2 math-围栏",
        ]
    );
    // 未知 shortcode 计入统计
    assert_eq!(counter.total(), 1);
}
