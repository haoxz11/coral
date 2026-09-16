//! 配置结构与加载：`coral.toml`，CLI 参数可覆盖（覆盖逻辑在 cli 层）。
//!
//! 示例文件见 `deploy/coral.example.toml`。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 顶层配置。字段命名与 `coral.toml` 一一对应（serde 默认，非 snake_case rename）。
///
/// 除 `[content]`（root 无默认值）外，各 section 可省略、逐项取默认值。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub content: ContentConfig,
    #[serde(default)]
    pub tree: TreeConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub render: RenderConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    /// 监听地址；默认 "0.0.0.0"（局域网可访问），仅本机使用可改为 "127.0.0.1"
    #[serde(default = "default_bind")]
    pub bind: String,
    /// 页脚文案；未配置/空回退首页（根 _index）title
    #[serde(default)]
    pub footer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContentConfig {
    /// markdown 根目录，必须存在，否则启动报错退出（校验在 cli 层 fail-fast）
    pub root: PathBuf,
    /// 排除目录（相对路径）
    #[serde(default)]
    pub exclude: Vec<PathBuf>,
    /// false：draft 文档彻底 404；true：正常渲染并进树
    #[serde(default)]
    pub draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TreeConfig {
    /// 首屏层级
    #[serde(default = "default_initial_depth")]
    pub initial_depth: usize,
    /// 展开时预载层级
    #[serde(default = "default_expand_depth")]
    pub expand_depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// 缓存目录（纯派生数据，可随时删除）
    #[serde(default = "default_cache_dir")]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// pretty | json（json 供 K8s 采集）
    #[serde(default = "default_log_format")]
    pub format: LogFormat,
}

/// 搜索配置：默认关闭，仅此一项开关。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    /// false：不建索引、/api/search 返回 disabled 标记空结果、搜索框保持 disabled
    #[serde(default)]
    pub enabled: bool,
}

/// 渲染增强配置（M2-s3）：Mermaid/KaTeX 前端库 CDN 可配（内网指向自建
/// mirror）；行内公式默认关闭（\$ 误伤面由站点所有者判断）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RenderConfig {
    /// mermaid ESM 构建的 URL（默认 jsdelivr）
    #[serde(default = "default_mermaid_cdn")]
    pub mermaid_cdn: String,
    /// katex.min.js 的 URL（默认 jsdelivr）
    #[serde(default = "default_katex_cdn")]
    pub katex_cdn: String,
    /// 行内 $...$ 公式识别：默认 false（价格/shell 变量等单 $ 内容防误伤；
    /// 块级 $$...$$ 无歧义默认启用）
    #[serde(default)]
    pub inline_math: bool,
}

fn default_mermaid_cdn() -> String {
    "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs".to_string()
}
fn default_katex_cdn() -> String {
    "https://cdn.jsdelivr.net/npm/katex@0.16/dist/katex.min.js".to_string()
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            mermaid_cdn: default_mermaid_cdn(),
            katex_cdn: default_katex_cdn(),
            inline_math: false,
        }
    }
}

/// git 内容同步配置：默认关闭；开启时必填项由 `GitConfig::validate`
/// fail-fast 校验（cli 启动调用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    #[serde(default)]
    pub enabled: bool,
    /// SSH 远端（如 git@gitlab.example.com:group/docs.git）；开启时必填
    #[serde(default)]
    pub remote: String,
    /// 跟踪分支（如 main）；开启时必填
    #[serde(default)]
    pub branch: String,
    /// 仓库内关注目录；该目录完整内容镜像到 content.root 根；开启时必填
    #[serde(default)]
    pub watch_dir: String,
    /// SSH 私钥文件路径（K8s 用 Secret 挂载，需 defaultMode: 0400）；开启时必填
    #[serde(default)]
    pub private_key_path: PathBuf,
    /// 可选 SSH hostkey；空 = 首次信任并记录到 cache/git-known-hosts
    #[serde(default)]
    pub host_key: String,
    /// GitLab webhook Secret Token；空 = 仅接受 loopback 来源
    #[serde(default)]
    pub secret_token: String,
    /// 首次 clone 只拉最新快照（depth=1，无历史）。mirror 仅用于读最新树做
    /// 镜像同步，历史无用；关闭后 clone 全量历史（需回滚能力的部署用）。
    /// 只作用于 clone 阶段，存量 mirror 与后续 fetch 不受影响。
    #[serde(default = "default_git_shallow")]
    pub shallow: bool,
}

/// shallow 默认 true（文档镜像场景历史无用；显式 opt-out）。
fn default_git_shallow() -> bool {
    true
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            remote: String::new(),
            branch: String::new(),
            watch_dir: String::new(),
            private_key_path: PathBuf::new(),
            host_key: String::new(),
            secret_token: String::new(),
            shallow: true,
        }
    }
}

impl GitConfig {
    /// 开启态必填项校验（fail-fast）。返回错误信息（可读提示）。
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        for (name, val) in [
            ("remote", &self.remote),
            ("branch", &self.branch),
            ("watch_dir", &self.watch_dir),
        ] {
            if val.trim().is_empty() {
                return Err(format!(
                    "[git] 开启时 {name} 必填（检查 coral.toml [git] 配置）"
                ));
            }
        }
        // 私钥必填/存在性/权限由 GitSync::validate 按 SSH 远端判定（file:// 不需要）
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Pretty,
    Json,
}

fn default_port() -> u16 {
    3000
}
fn default_bind() -> String {
    "0.0.0.0".to_string()
}
fn default_initial_depth() -> usize {
    2
}
fn default_expand_depth() -> usize {
    1
}
fn default_cache_dir() -> PathBuf {
    PathBuf::from("./cache")
}
fn default_log_format() -> LogFormat {
    LogFormat::Pretty
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            bind: default_bind(),
            footer: None,
        }
    }
}

impl Default for TreeConfig {
    fn default() -> Self {
        Self {
            initial_depth: default_initial_depth(),
            expand_depth: default_expand_depth(),
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dir: default_cache_dir(),
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: default_log_format(),
        }
    }
}

/// 配置加载错误：字段级可读上下文。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("配置文件读取失败 {path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("配置解析失败 {path}：{message}")]
    Parse { path: PathBuf, message: String },
}

impl Config {
    /// 从 `coral.toml` 加载配置。
    ///
    /// 未知键由 `deny_unknown_fields` 直接报错——配置面保持最小、
    /// 拼写错误不被静默吞掉（AGENTS.md 禁止静默吞错）。
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("coral.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_defaults_match_requirement() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "[content]\nroot = \"/tmp/content\"\n");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.server.port, 3000);
        assert_eq!(cfg.server.bind, "0.0.0.0");
        assert_eq!(cfg.content.root, PathBuf::from("/tmp/content"));
        assert!(cfg.content.exclude.is_empty());
        assert!(!cfg.content.draft);
        assert_eq!(cfg.tree.initial_depth, 2);
        assert_eq!(cfg.tree.expand_depth, 1);
        assert_eq!(cfg.cache.dir, PathBuf::from("./cache"));
        assert_eq!(cfg.log.format, LogFormat::Pretty);
    }

    #[test]
    fn test_full_example_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"
[server]
port = 8080
bind = "0.0.0.0"

[content]
root = "/srv/content"
exclude = ["drafts", "archive"]
draft = true

[tree]
initial_depth = 3
expand_depth = 2

[cache]
dir = "/var/cache/coral"

[log]
format = "json"
"#,
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.server.bind, "0.0.0.0");
        assert_eq!(cfg.content.root, PathBuf::from("/srv/content"));
        assert_eq!(
            cfg.content.exclude,
            vec![PathBuf::from("drafts"), PathBuf::from("archive")]
        );
        assert!(cfg.content.draft);
        assert_eq!(cfg.tree.initial_depth, 3);
        assert_eq!(cfg.tree.expand_depth, 2);
        assert_eq!(cfg.cache.dir, PathBuf::from("/var/cache/coral"));
        assert_eq!(cfg.log.format, LogFormat::Json);
    }

    #[test]
    fn test_missing_content_root_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "[content]\nexclude = []\n");
        let err = Config::load(&path).unwrap_err();
        assert!(
            err.to_string().contains("root"),
            "错误信息应指出缺失字段：{err}"
        );
    }

    #[test]
    fn test_unknown_key_is_error_not_silent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            "[content]\nroot = \"/tmp/content\"\n[server]\nprot = 3000\n",
        );
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn test_unreadable_file_is_io_error() {
        let err = Config::load(Path::new("/nonexistent/coral.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn test_invalid_toml_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "[content\nroot = ");
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn test_git_config_default_disabled() {
        let cfg: Config = toml::from_str("[content]\nroot = \"/tmp/c\"\n").unwrap();
        assert!(!cfg.git.enabled);
        assert!(cfg.git.validate().is_ok(), "关闭态不校验必填");
    }

    #[test]
    fn test_git_config_enabled_missing_required() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id_ed25519");
        std::fs::write(&key, "key").unwrap();
        // enabled 但缺 remote/branch/watch_dir
        let cfg: Config = toml::from_str(&format!(
            "[content]\nroot = \"/tmp/c\"\n[git]\nenabled = true\nprivate_key_path = \"{}\"\n",
            key.display()
        ))
        .unwrap();
        let err = cfg.git.validate().unwrap_err();
        assert!(err.contains("remote"), "{err}");
    }

    #[test]
    fn test_git_config_ssh_key_checked_in_git_sync() {
        // 私钥校验已收敛到 GitSync::validate（按 SSH 远端判定），config 只查基础必填
        let cfg: Config = toml::from_str(
            "[content]\nroot = \"/tmp/c\"\n[git]\nenabled = true\nremote = \"git@x:y.git\"\nbranch = \"main\"\nwatch_dir = \"docs\"\nprivate_key_path = \"/nonexistent/key\"\n",
        )
        .unwrap();
        assert!(
            cfg.git.validate().is_ok(),
            "基础必填齐全；私钥存在性由 GitSync 按 SSH 判定"
        );
    }

    #[test]
    fn test_git_config_full_parse() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id_ed25519");
        std::fs::write(&key, "key").unwrap();
        let cfg: Config = toml::from_str(&format!(
            "[content]\nroot = \"/tmp/c\"\n[git]\nenabled = true\nremote = \"git@gitlab.example.com:g/docs.git\"\nbranch = \"main\"\nwatch_dir = \"docs\"\nprivate_key_path = \"{}\"\nhost_key = \"ssh-ed25519 AAAA\"\nsecret_token = \"s3cr3t\"\n",
            key.display()
        ))
        .unwrap();
        assert!(cfg.git.validate().is_ok());
        assert_eq!(cfg.git.branch, "main");
        assert_eq!(cfg.git.secret_token, "s3cr3t");
    }
}
