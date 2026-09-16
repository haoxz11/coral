//! coral 领域层。
//!
//! 纯同步代码，不依赖 tokio/axum 等运行时（AGENTS.md 分层准则）。
//! 模块：config（配置）、frontmatter、scanner、tree、render、
//! shortcode、highlight、cache。

pub mod cache;
pub mod config;
pub mod frontmatter;
pub mod git_sync;
pub mod highlight;
pub mod math;
pub mod render;
pub mod scanner;
pub mod search;
pub mod shortcode;
pub mod tree;
pub mod url;

pub use cache::{CacheError, CacheStore, Manifest, PageEntry, StartupDiff, TreeEntry};
pub use git_sync::{GitSync, GitSyncError, SyncStats};
pub use render::{RenderError, RenderedPage, TocEntry, render_page};
pub use search::{SearchError, SearchHit, SearchIndex};

pub use config::{Config, ConfigError, GitConfig, LogFormat, RenderConfig, SearchConfig};
pub use frontmatter::{FmError, FmFormat, FrontMatter, parse as parse_frontmatter};
pub use scanner::{DirMeta, PageMeta, ScanError, ScanResult, SiteIndex, scan};
pub use tree::{NodeType, TreeNode, build_subtree};
