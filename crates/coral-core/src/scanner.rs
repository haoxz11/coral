//! 目录扫描与站点索引。
//!
//! scan() 产出原料（PageMeta/DirMeta），SiteIndex::build 做路由裁决、
//! permalink 覆盖与 children 依赖反向索引；draft 过滤发生在 build/tree 层。

use crate::config::ContentConfig;
use crate::frontmatter::{self, FrontMatter};
use crate::tree::{self, SortKey};
use crate::url;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};
use tracing::{debug, info, warn};

/// 单个 markdown 文档的扫描元数据。
#[derive(Debug, Clone)]
pub struct PageMeta {
    pub rel_path: PathBuf,
    /// 默认 URL（decode 形态，与路由表 key 同构）；permalink 覆盖在 SiteIndex 层
    pub url: String,
    pub fm: FrontMatter,
    /// front matter 解析失败的错误描述；页面仍入索引（该页 500 而非 404）
    pub fm_parse_error: Option<String>,
    pub mtime: SystemTime,
    pub size: u64,
    /// 目录分支页（`_index.md`/`index.md`/`readme.md` 回退链被选中的那个；URL 为目录自身）
    pub is_branch: bool,
    /// `{{% children %}}` 粗判（等价正则 `\{\{[<%]\s*children\b`），渲染期才做真正的状态机解析
    pub has_children_shortcode: bool,
}

/// 顶栏一级菜单项。
#[derive(Debug, Clone)]
pub struct TopSection {
    pub title: String,
    /// encode 形态 URL（与树 href 同构）
    pub url: String,
    /// 目录（有 _index 首页）还是一级文档
    pub is_dir: bool,
    /// _index/frontmatter 的 icon（顶栏渲染）
    pub icon: Option<String>,
    sort: SortKey,
}

/// 目录元数据（树数据源）。
#[derive(Debug, Clone)]
pub struct DirMeta {
    pub rel_path: PathBuf,
    /// 该目录分支页（`_index.md`/`index.md`/`readme.md` 回退链）的 rel_path；无则树节点标题回退目录名
    pub branch_page: Option<PathBuf>,
    pub title: String,
    pub weight: Option<i64>,
    /// `_index.md` frontmatter 的 icon（菜单图标）
    pub icon: Option<String>,
    /// `_index.md` frontmatter 的 date（树排序第 3 级，字符串比较）
    pub date: Option<String>,
    pub mtime: SystemTime,
    pub child_dirs: Vec<PathBuf>,
    /// 直接子文档（不含 `_index.md`）
    pub child_pages: Vec<PathBuf>,
}

/// 扫描结果：SiteIndex 的原料，未做路由裁决与 draft 过滤。
#[derive(Debug)]
pub struct ScanResult {
    /// canonicalize 后的 content 根（请求侧安全校验复用同一基准）
    pub root: PathBuf,
    pub pages: HashMap<PathBuf, PageMeta>,
    pub dirs: HashMap<PathBuf, DirMeta>,
}

/// 站点索引。
///
/// 路由表 key 为 decode 形态 URL（与 rel_path 同构）；任何变更由上层整树替换（写时复制）。
#[derive(Debug)]
pub struct SiteIndex {
    /// decode 形态 URL -> rel_path（目录分支页 + 普通文档）
    pub routes: HashMap<String, PathBuf>,
    /// permalink 表：匹配时先查；原 URL 同时保留在 routes
    pub permalinks: HashMap<String, PathBuf>,
    pub pages: HashMap<PathBuf, PageMeta>,
    pub dirs: HashMap<PathBuf, DirMeta>,
    /// 目录 -> 依赖该目录子树的 children shortcode 页面（含祖先链注册）
    pub children_deps: HashMap<PathBuf, Vec<PathBuf>>,
}

/// 扫描错误。单个文件/子目录的失败不整体上抛（WARN + 跳过），
/// 只有 content 根自身的 IO 失败才是 ScanError。
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("content 根目录不可读 {path}：{source}")]
    RootUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("扫描 IO 错误 {path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// draft 页面在当前配置下是否被排除（`content.draft = false` 时不渲染、不进树）。
pub fn is_draft_excluded(fm: &FrontMatter, draft_enabled: bool) -> bool {
    fm.draft && !draft_enabled
}

/// 垃圾文件/目录判定：`.` 开头、`~` 结尾、`#...#` 包裹。
pub fn is_junk(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with('~')
        || (name.len() >= 2 && name.starts_with('#') && name.ends_with('#'))
}

/// 排除目录判定：rel_path 等于排除项或位于其下。
pub fn is_excluded(rel_path: &Path, exclude: &[PathBuf]) -> bool {
    exclude
        .iter()
        .any(|e| rel_path == e || rel_path.starts_with(e))
}

/// children shortcode 粗判（等价正则 `\{\{[<%]\s*children\b`）。
/// 不引入 regex 依赖，手工扫描 `{{<` / `{{%` 定界。
pub fn has_children_shortcode(body: &str) -> bool {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' && matches!(bytes[i + 2], b'<' | b'%') {
            let mut j = i + 3;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if body[j..].starts_with("children") {
                let after = j + "children".len();
                if after >= bytes.len()
                    || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_')
                {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// 分支页候选优先级（文件名小写比较，不区分大小写）。
const BRANCH_CANDIDATES: [&str; 3] = ["_index.md", "index.md", "readme.md"];

fn file_name_lower(rel_path: &Path) -> String {
    rel_path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// 文件名小写为 `_index.md`（任意大小写变体）。
/// `_index` 类不注册普通文档 URL、不进树子节点，与 index/readme 分支页（双可达）不同。
fn is_index_class(rel_path: &Path) -> bool {
    file_name_lower(rel_path) == "_index.md"
}

/// 从目录的 md 文件清单中选分支页：`_index.md` > `index.md` > `readme.md`
/// （文件名小写比较）。同级多候选（大小写敏感 FS 上 index.md 与 INDEX.md
/// 并存）取文件名字节序最小者保证确定性，WARN 提示歧义；无候选返回 None。
fn select_branch_file(dir: &Path, md_files: &[PathBuf]) -> Option<PathBuf> {
    for candidate in BRANCH_CANDIDATES {
        let mut hits: Vec<&PathBuf> = md_files
            .iter()
            .filter(|f| file_name_lower(f) == candidate)
            .collect();
        if hits.is_empty() {
            continue;
        }
        hits.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        if let Some(loser) = hits.get(1) {
            warn!(
                dir = %dir.display(),
                selected = %hits[0].display(),
                loser = %loser.display(),
                "分支页同级多候选，取文件名序最小者"
            );
        }
        return Some(hits[0].clone());
    }
    None
}

/// 剥离文件名开头的数字前缀（`1.入门` → `入门`；无前缀原样）。
/// 排序用数字前缀，但 fallback 标题显示时去掉。
pub fn strip_numeric_prefix(stem: &str) -> &str {
    let Some((num, rest)) = stem.split_once('.') else {
        return stem;
    };
    if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
        rest
    } else {
        stem
    }
}

fn fallback_title(rel_path: &Path) -> String {
    rel_path
        .file_name()
        .map(|n| strip_numeric_prefix(&n.to_string_lossy()).to_string())
        .unwrap_or_default()
}

struct RawDir {
    mtime: SystemTime,
    child_dirs: Vec<PathBuf>,
    md_files: Vec<PathBuf>,
}

/// 扫描 content 根。根不存在/不可读返回 `ScanError::RootUnreadable`
/// （调用方启动 fail-fast）。
pub fn scan(content: &ContentConfig) -> Result<ScanResult, ScanError> {
    let start = Instant::now();
    let canonical_root =
        content
            .root
            .canonicalize()
            .map_err(|source| ScanError::RootUnreadable {
                path: content.root.clone(),
                source,
            })?;
    let mut pages: HashMap<PathBuf, PageMeta> = HashMap::new();
    let mut raw_dirs: HashMap<PathBuf, RawDir> = HashMap::new();
    // 根到当前目录的 canonical 路径链（回溯式），拦截 in-root symlink 构成的目录环
    let mut canon_chain: HashSet<PathBuf> = HashSet::new();
    canon_chain.insert(canonical_root.clone());
    walk(
        &canonical_root,
        Path::new(""),
        &canonical_root,
        &content.exclude,
        &mut pages,
        &mut raw_dirs,
        &mut canon_chain,
    )?;

    let mut dirs: HashMap<PathBuf, DirMeta> = HashMap::new();
    for (rel, raw) in raw_dirs {
        let branch_page = select_branch_file(&rel, &raw.md_files);
        let branch_fm = branch_page
            .as_ref()
            .and_then(|bp| pages.get(bp))
            .map(|p| p.fm.clone());
        let title = branch_fm
            .as_ref()
            .and_then(|f| f.title.clone())
            .unwrap_or_else(|| fallback_title(&rel));
        dirs.insert(
            rel.clone(),
            DirMeta {
                rel_path: rel,
                // 树子节点只吸收被选中的分支页；同目录其余候选（大小写变体等）保留为普通文档
                child_pages: raw
                    .md_files
                    .iter()
                    .filter(|f| branch_page.as_ref() != Some(*f))
                    .cloned()
                    .collect(),
                branch_page,
                title,
                weight: branch_fm.as_ref().and_then(|f| f.weight),
                icon: branch_fm.as_ref().and_then(|f| f.icon.clone()),
                date: branch_fm.as_ref().and_then(|f| f.date.clone()),
                mtime: raw.mtime,
                child_dirs: raw.child_dirs,
            },
        );
    }
    info!(
        files = pages.len(),
        dirs = dirs.len(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "扫描完成"
    );
    Ok(ScanResult {
        root: canonical_root,
        pages,
        dirs,
    })
}

fn walk(
    root: &Path,
    rel: &Path,
    canonical_root: &Path,
    exclude: &[PathBuf],
    pages: &mut HashMap<PathBuf, PageMeta>,
    raw_dirs: &mut HashMap<PathBuf, RawDir>,
    canon_chain: &mut HashSet<PathBuf>,
) -> Result<(), ScanError> {
    let abs = root.join(rel);
    let mtime = std::fs::metadata(&abs)
        .and_then(|m| m.modified())
        .map_err(|source| ScanError::Io {
            path: abs.clone(),
            source,
        })?;
    let mut raw = RawDir {
        mtime,
        child_dirs: Vec::new(),
        md_files: Vec::new(),
    };
    for entry in std::fs::read_dir(&abs).map_err(|source| ScanError::Io {
        path: abs.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| ScanError::Io {
            path: abs.clone(),
            source,
        })?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if is_junk(&name_str) {
            continue;
        }
        let child_rel = rel.join(&name);
        if is_excluded(&child_rel, exclude) {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(source) => {
                warn!(rel_path = %child_rel.display(), %source, "读取文件类型失败，跳过");
                continue;
            }
        };
        // symlink：根内跟随、逃逸拒绝；目录环由 canon_chain 拦截
        let (is_dir, canon_child) = if ft.is_symlink() {
            match std::fs::canonicalize(entry.path()) {
                Ok(target) if target.starts_with(canonical_root) => {
                    match std::fs::metadata(entry.path()) {
                        Ok(m) => (m.is_dir(), Some(target)),
                        Err(source) => {
                            warn!(rel_path = %child_rel.display(), %source, "读取 symlink 落点失败，跳过");
                            continue;
                        }
                    }
                }
                Ok(_) => {
                    warn!(rel_path = %child_rel.display(), "symlink 逃逸 content 根，拒绝");
                    continue;
                }
                Err(source) => {
                    warn!(rel_path = %child_rel.display(), %source, "symlink 无法解析，跳过");
                    continue;
                }
            }
        } else {
            (ft.is_dir(), None)
        };

        if is_dir {
            let canon = match canon_child {
                Some(c) => c,
                None => match std::fs::canonicalize(entry.path()) {
                    Ok(c) => c,
                    Err(source) => {
                        warn!(rel_path = %child_rel.display(), %source, "目录 canonicalize 失败，跳过");
                        continue;
                    }
                },
            };
            if canon_chain.contains(&canon) {
                warn!(rel_path = %child_rel.display(), "symlink 目录环，跳过");
                continue;
            }
            raw.child_dirs.push(child_rel.clone());
            canon_chain.insert(canon.clone());
            walk(
                root,
                &child_rel,
                canonical_root,
                exclude,
                pages,
                raw_dirs,
                canon_chain,
            )?;
            canon_chain.remove(&canon);
        } else if name_str.ends_with(".md") {
            raw.md_files.push(child_rel.clone());
        }
        // 非 md 文件：静态资源，按原路径按需服务，扫描不索引
    }
    // 分支页是目录级裁决（回退链 _index > index > readme，需同级完整清单），
    // 先收齐 md_files 再统一读 PageMeta
    let branch = select_branch_file(rel, &raw.md_files);
    for child_rel in &raw.md_files {
        let is_branch = Some(child_rel) == branch.as_ref();
        match read_page(root, child_rel, is_branch) {
            Ok(pm) => {
                if !pm.fm.unconsumed.is_empty() {
                    debug!(
                        rel_path = %child_rel.display(),
                        fields = ?pm.fm.unconsumed,
                        "未消费 front matter 字段"
                    );
                }
                if let Some(err) = &pm.fm_parse_error {
                    warn!(
                        rel_path = %child_rel.display(),
                        error = %err,
                        "front matter 解析失败，按默认值索引"
                    );
                }
                pages.insert(child_rel.clone(), pm);
            }
            Err(source) => {
                warn!(rel_path = %child_rel.display(), %source, "读取文档失败，跳过");
            }
        }
    }
    raw_dirs.insert(rel.to_path_buf(), raw);
    Ok(())
}

fn read_page(root: &Path, rel: &Path, is_branch: bool) -> Result<PageMeta, std::io::Error> {
    let abs = root.join(rel);
    let raw = std::fs::read_to_string(&abs)?;
    let meta = std::fs::metadata(&abs)?;
    // 解析失败不中断：默认值 + 错误随 PageMeta 携带（该页 500 而非 404）
    let (fm, fm_parse_error, body) = match frontmatter::parse(&raw) {
        Ok((fm, body)) => (fm, None, body),
        Err(e) => (FrontMatter::default(), Some(e.to_string()), raw.as_str()),
    };
    let url = if is_branch {
        url::dir_url(rel.parent().unwrap_or(Path::new("")))
    } else {
        url::page_url(rel)
    };
    Ok(PageMeta {
        rel_path: rel.to_path_buf(),
        url,
        fm,
        fm_parse_error,
        mtime: meta.modified()?,
        size: meta.len(),
        is_branch,
        has_children_shortcode: has_children_shortcode(body),
    })
}

impl SiteIndex {
    /// 深拷贝（增量更新：锁外 merge 前取快照）。
    pub fn clone_shallow(&self) -> SiteIndex {
        SiteIndex {
            routes: self.routes.clone(),
            permalinks: self.permalinks.clone(),
            pages: self.pages.clone(),
            dirs: self.dirs.clone(),
            children_deps: self.children_deps.clone(),
        }
    }

    /// 增量合并（git 同步小变更不全量重扫的优化）。
    ///
    /// 局部重扫受影响目录（变更文件的父目录链 + 根，子树只到直接子项——
    /// 增删文件只影响所在目录的 child 列表与该文件的条目），把新 PageMeta
    /// 合并进当前索引副本；路由/children_deps 按既有 build 规则整体重算
    /// （纯内存操作，微秒级，避免在增量路径重新实现裁决规则造成漂移）。
    ///
    /// `changed`/`removed` 为 content.root 相对路径清单。
    pub fn merge_changes(
        &self,
        content: &ContentConfig,
        root: &Path,
        draft_enabled: bool,
        changed: &[String],
        removed: &[String],
    ) -> SiteIndex {
        // 1) 受影响目录集合：变更/删除文件的父目录链（含根""）
        let mut affected: HashSet<PathBuf> = HashSet::new();
        affected.insert(PathBuf::new());
        for rel in changed.iter().chain(removed.iter()) {
            let mut dir = Path::new(rel).parent();
            while let Some(d) = dir {
                affected.insert(d.to_path_buf());
                dir = d.parent();
            }
        }

        // 2) 局部重扫受影响目录（只读直接子项；read_page 复用全量逻辑）
        let mut pages = self.pages.clone();
        let mut raw_dirs: HashMap<PathBuf, RawDir> = HashMap::new();
        let mut touched_pages: HashSet<PathBuf> = HashSet::new();
        for dir in &affected {
            let abs = root.join(dir);
            let Ok(entries) = std::fs::read_dir(&abs) else {
                continue; // 目录已不存在（整目录删除）——后续按 removed 清理
            };
            let mtime = std::fs::metadata(&abs)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let mut rd = RawDir {
                mtime,
                child_dirs: Vec::new(),
                md_files: Vec::new(),
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if is_junk(&name_str) || is_excluded(&dir.join(&name), &content.exclude) {
                    continue;
                }
                let child_rel = dir.join(&name);
                let Ok(ft) = entry.file_type() else { continue };
                let is_dir = if ft.is_symlink() {
                    match std::fs::canonicalize(entry.path()) {
                        Ok(c) if c.starts_with(root) => std::fs::metadata(entry.path())
                            .map(|m| m.is_dir())
                            .unwrap_or(false),
                        _ => continue,
                    }
                } else {
                    ft.is_dir()
                };
                if is_dir {
                    rd.child_dirs.push(child_rel);
                } else if name_str.ends_with(".md") {
                    rd.md_files.push(child_rel);
                }
            }
            // 与全量 walk 同序：先收齐 md_files 做目录级分支裁决，再统一读 PageMeta
            let branch = select_branch_file(dir, &rd.md_files);
            for child_rel in &rd.md_files {
                let is_branch = Some(child_rel) == branch.as_ref();
                if let Ok(pm) = read_page(root, child_rel, is_branch) {
                    pages.insert(child_rel.clone(), pm);
                    touched_pages.insert(child_rel.clone());
                }
            }
            raw_dirs.insert(dir.clone(), rd);
        }

        // 3) removed 清理：pages 条目 + 所在目录 child 列表
        for rel in removed {
            let p = PathBuf::from(rel);
            pages.remove(&p);
            if let Some(rd) = p.parent().and_then(|dir| raw_dirs.get_mut(dir)) {
                rd.md_files.retain(|f| f != &p);
            }
        }

        // 4) DirMeta 局部更新（受影响目录整组替换；未受影响目录保留旧值）
        let mut dirs = self.dirs.clone();
        for (rel, raw) in &raw_dirs {
            let branch_page = select_branch_file(rel, &raw.md_files);
            let branch_fm = branch_page
                .as_ref()
                .and_then(|bp| pages.get(bp))
                .map(|p| p.fm.clone());
            dirs.insert(
                rel.clone(),
                DirMeta {
                    rel_path: rel.clone(),
                    title: branch_fm
                        .as_ref()
                        .and_then(|f| f.title.clone())
                        .unwrap_or_else(|| fallback_title(rel)),
                    weight: branch_fm.as_ref().and_then(|f| f.weight),
                    icon: branch_fm.as_ref().and_then(|f| f.icon.clone()),
                    date: branch_fm.as_ref().and_then(|f| f.date.clone()),
                    mtime: raw.mtime,
                    // 只吸收被选中的分支页，与全量 scan 同规则
                    child_pages: raw
                        .md_files
                        .iter()
                        .filter(|f| branch_page.as_ref() != Some(*f))
                        .cloned()
                        .collect(),
                    branch_page,
                    child_dirs: raw.child_dirs.clone(),
                },
            );
        }
        // 受影响目录已不存在（整目录删除，git diff 只报文件）：移除条目
        // 并从父目录 child_dirs 清理
        let affected_vec: Vec<PathBuf> = affected.iter().cloned().collect();
        for rel in &affected_vec {
            if !rel.as_os_str().is_empty() && !root.join(rel).is_dir() {
                dirs.remove(rel);
                if let Some(d) = rel.parent().and_then(|parent| dirs.get_mut(parent)) {
                    d.child_dirs.retain(|c| c != rel);
                }
            }
        }

        // 5) 路由/children_deps 按既有规则重算（纯内存；见 build 的裁决注释）
        let scan = ScanResult {
            root: root.to_path_buf(),
            pages,
            dirs,
        };
        SiteIndex::build(scan, draft_enabled)
    }

    /// 由扫描结果构建索引。
    ///
    /// 裁决顺序：目录 URL 占位 → 普通文档 → permalink 覆盖；
    /// 最后构建 children 依赖反向索引。所有遍历按 rel_path 排序保证确定性。
    pub fn build(scan: ScanResult, draft_enabled: bool) -> SiteIndex {
        let ScanResult {
            root: _,
            mut pages,
            dirs,
        } = scan;

        // 1) 目录占位：全部目录的 URL 进入命名空间占位集（目录优先于同名文档）；
        //    仅"有可服务 _index.md"的目录进路由表
        let dir_urls: HashSet<String> = dirs.keys().map(|d| url::dir_url(d)).collect();
        let mut routes: HashMap<String, PathBuf> = HashMap::new();
        let mut sorted_dirs: Vec<&PathBuf> = dirs.keys().collect();
        sorted_dirs.sort();
        for d in sorted_dirs {
            let branch_page = match &dirs[d].branch_page {
                Some(bp) => bp.clone(),
                None => continue,
            };
            let Some(page) = pages.get(&branch_page) else {
                continue;
            };
            if is_draft_excluded(&page.fm, draft_enabled) {
                continue;
            }
            routes.insert(url::dir_url(d), branch_page);
        }

        let mut all_pages: Vec<&PageMeta> = pages.values().collect();
        all_pages.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

        // 2) 普通文档：URL 被目录/既有路由占用 → 追加 /index；仍冲突保留先注册者。
        //    裁决出的最终 URL 记入 url_overrides，随后回写 PageMeta
        //    （树的 href 与路由表必须一致，否则树链接指向不可服务的目录占位 URL）
        let mut url_overrides: HashMap<PathBuf, String> = HashMap::new();
        let mut permalinks: HashMap<String, PathBuf> = HashMap::new();
        let mut children_deps: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for page in &all_pages {
            if is_draft_excluded(&page.fm, draft_enabled) {
                continue;
            }
            // 3) permalink 覆盖映射（与 2 同轮处理避免重复借用）：原 URL 不变。
            //    冲突仲裁：date 老者优先保留（拷贝文件忘改 permalink 的
            //    场景，老文档不被新文档顶掉）；date 缺失/相等时先注册者保留（路径序，
            //    确定性）。落败者 permalink 不生效（href_for_page 按仲裁表裁决）。
            //    date 比较用规范化形态（YYYY-MM-DD）——同日带时间与不带时间
            //    不会因字符串前缀关系误判先后（normalize_date 唯一口径）。
            if let Some(permalink) = page.fm.permalink.clone() {
                match permalinks.get(&permalink) {
                    Some(old_owner) => {
                        let norm = |d: Option<&str>| d.and_then(crate::frontmatter::normalize_date);
                        let old_date = pages
                            .get(old_owner)
                            .and_then(|p| p.fm.date.as_deref())
                            .and_then(crate::frontmatter::normalize_date);
                        let new_date = norm(page.fm.date.as_deref());
                        let (winner, loser, reason) = match (&old_date, &new_date) {
                            (Some(od), Some(nd)) if nd < od => {
                                (page.rel_path.clone(), old_owner.clone(), "date 更老")
                            }
                            (Some(_), Some(_)) => {
                                (old_owner.clone(), page.rel_path.clone(), "date 更老")
                            }
                            _ => (
                                old_owner.clone(),
                                page.rel_path.clone(),
                                "先注册（date 缺失或相等）",
                            ),
                        };
                        if winner.as_path() != old_owner.as_path() {
                            permalinks.insert(permalink.clone(), winner.clone());
                        }
                        warn!(
                            url = %permalink,
                            winner = %winner.display(),
                            winner_date = ?pages.get(&winner).and_then(|p| p.fm.date.as_deref()),
                            loser = %loser.display(),
                            loser_date = ?pages.get(&loser).and_then(|p| p.fm.date.as_deref()),
                            "permalink 冲突：{reason}者保留，落败者 permalink 不生效"
                        );
                    }
                    None => {
                        permalinks.insert(permalink.clone(), page.rel_path.clone());
                        if routes.contains_key(&permalink) {
                            warn!(
                                url = %permalink,
                                "permalink 与常规路由同 URL，匹配时 permalink 优先"
                            );
                        }
                    }
                }
            }
            // 分支页的 URL 即目录自身（第 1 步已注册），不进普通文档裁决；
            // index/readme 分支页的普通文档 URL 双可达在循环后单独注册
            if page.is_branch {
                continue;
            }
            let mut u = page.url.clone();
            if dir_urls.contains(&u) || routes.contains_key(&u) {
                warn!(
                    rel_path = %page.rel_path.display(),
                    url = %u,
                    "URL 被目录/既有路由占用，改用 /index 后缀"
                );
                u = format!("{u}/index");
            }
            if let Some(existing) = routes.get(&u) {
                warn!(
                    rel_path = %page.rel_path.display(),
                    url = %u,
                    existing = %existing.display(),
                    "URL 仍冲突，保留先注册页面"
                );
                continue;
            }
            if u != page.url {
                url_overrides.insert(page.rel_path.clone(), u.clone());
            }
            routes.insert(u, page.rel_path.clone());
        }
        drop(all_pages);
        for (rel, u) in url_overrides {
            if let Some(page) = pages.get_mut(&rel) {
                page.url = u;
            }
        }

        // 2b) index/readme 分支页双可达：目录 URL（/dir）之外保留普通文档 URL
        //     （/dir/index、/dir/readme）。被目录占位（如存在同名子目录）或既有
        //     路由占用时不覆盖——双可达让位于第 2 步的既有裁决，WARN 提示。
        //     排序遍历保证确定性。_index 类分支页不适用（无普通文档 URL 约定）。
        let mut sorted_branch: Vec<&PathBuf> = pages
            .iter()
            .filter(|(rel, p)| {
                p.is_branch && !is_index_class(rel) && !is_draft_excluded(&p.fm, draft_enabled)
            })
            .map(|(rel, _)| rel)
            .collect();
        sorted_branch.sort();
        for rel in sorted_branch {
            // 普通文档 URL 用小写化文件名：README.md 的双可达入口是
            // /dir/readme（大小写变体归一到同一 URL，与选取规则的小写口径一致）
            let lower_name = file_name_lower(rel);
            let lower_rel = rel.with_file_name(&lower_name);
            let plain = url::page_url(&lower_rel);
            if dir_urls.contains(&plain) || routes.contains_key(&plain) {
                warn!(
                    rel_path = %rel.display(),
                    url = %plain,
                    "分支页普通文档 URL 被占用，双可达让位"
                );
                continue;
            }
            routes.insert(plain, rel.clone());
        }

        // 4) children 依赖反向索引：注册到所在目录及全部祖先
        // icon 裸名（无前缀）无法被 iconify-icon 解析，渲染为空白：
        // 扫描期 WARN 提示作者写全名（不猜默认前缀）
        for page in pages.values() {
            if let Some(icon) = page
                .fm
                .icon
                .as_deref()
                .filter(|i| !i.contains(':') && !i.contains('/'))
            {
                warn!(
                    rel_path = %page.rel_path.display(),
                    %icon,
                    "icon 缺少图标集前缀（应为 前缀:名称，如 mdi:home），渲染为空白"
                );
            }
        }

        for page in pages.values() {
            if !page.has_children_shortcode || is_draft_excluded(&page.fm, draft_enabled) {
                continue;
            }
            let mut dir = page.rel_path.parent();
            while let Some(d) = dir {
                children_deps
                    .entry(d.to_path_buf())
                    .or_default()
                    .push(page.rel_path.clone());
                if d.parent().is_none() {
                    break;
                }
                dir = d.parent();
            }
        }

        SiteIndex {
            routes,
            permalinks,
            pages,
            dirs,
            children_deps,
        }
    }

    /// 页面的对外 href（encode 形态；permalink 原样）。
    /// 注意用 `page.url`（路由裁决后的最终 URL），而非从 rel_path 重算。
    /// permalink 以仲裁表为准（冲突落败者的配置不生效，回退默认 URL）。
    pub fn href_for_page(&self, rel_path: &Path) -> String {
        if let Some(page) = self.pages.get(rel_path) {
            let won = page.fm.permalink.as_ref().filter(|pl| {
                self.permalinks.get(pl.as_str()).map(PathBuf::as_path) == Some(rel_path)
            });
            if let Some(permalink) = won {
                return permalink.clone();
            }
            return url::encode_url(&page.url);
        }
        url::encode_url(&url::page_url(rel_path))
    }

    /// 目录的对外 href（encode 形态；分支页 permalink 原样；仲裁表裁决同上）。
    pub fn href_for_dir(&self, dir: &Path) -> String {
        if let Some(permalink) = self
            .dirs
            .get(dir)
            .and_then(|dm| dm.branch_page.as_ref())
            .and_then(|bp| {
                let pl = self.pages.get(bp)?.fm.permalink.as_deref()?;
                (self.permalinks.get(pl).map(PathBuf::as_path) == Some(bp.as_path())).then_some(pl)
            })
        {
            return permalink.to_string();
        }
        url::encode_url(&url::dir_url(dir))
    }

    /// 根下一级节点列表（顶栏一级菜单）。
    /// 复用树排序（四级链）；目录取 _index title，文档取 title/fallback。
    pub fn top_sections(&self) -> Vec<TopSection> {
        let root = self
            .dirs
            .get(Path::new(""))
            .expect("根目录必在 dirs 表（scan 保证）");
        let mut sections: Vec<TopSection> = Vec::new();
        for d in &root.child_dirs {
            if let Some(dm) = self.dirs.get(d) {
                sections.push(TopSection {
                    title: dm.title.clone(),
                    url: self.href_for_dir(d),
                    is_dir: true,
                    icon: dm.icon.clone(),
                    sort: SortKey::new(
                        dm.weight,
                        &dm.rel_path.to_string_lossy(),
                        dm.date.as_deref(),
                    ),
                });
            }
        }
        for p in &root.child_pages {
            if let Some(page) = self.pages.get(p) {
                if page.is_branch {
                    continue; // _index.md 不在顶栏（根首页由站点标题承担）
                }
                sections.push(TopSection {
                    title: page.fm.title.clone().unwrap_or_else(|| {
                        p.file_stem()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    }),
                    url: self.href_for_page(p),
                    is_dir: false,
                    icon: page.fm.icon.clone(),
                    sort: SortKey::new(
                        page.fm.weight,
                        &p.file_stem()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        page.fm.date.as_deref(),
                    ),
                });
            }
        }
        sections.sort_by(|a, b| tree::compare_keys(&a.sort, &b.sort));
        sections
    }

    /// 当前页 rel_path 所属的一级目录 rel_path（首段）；一级文档/根页面返回 None。
    pub fn top_section_of(&self, rel_path: &Path) -> Option<PathBuf> {
        let first = rel_path.components().next()?;
        let dir = PathBuf::from(first.as_os_str());
        self.dirs.contains_key(&dir).then_some(dir)
    }

    /// 当前页的全部祖先目录 href 链（含根，encode 形态）。
    /// 供侧栏展开判定：目录带 permalink 时其 URL 与子页面 URL 无前缀关系，
    /// 前端不能按路径前缀推断祖先。
    pub fn ancestor_dir_hrefs(&self, rel_path: &Path) -> Vec<String> {
        let mut hrefs = Vec::new();
        let mut dir = rel_path.parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            if self.dirs.contains_key(&d) {
                hrefs.push(self.href_for_dir(&d));
            }
            if d.parent().is_none() {
                break;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        hrefs
    }

    /// URL（decode 形态）反查目录 rel_path（懒加载 API 用）。
    /// 支持三种形态：branch 页 permalink（含仲裁表裁决）、目录默认 URL、
    /// 目录 URL 的 encode 形态（逐段解码后匹配）。
    pub fn dir_for_url(&self, decoded_url: &str) -> Option<PathBuf> {
        let normalized = decoded_url.trim_end_matches('/');
        if normalized.is_empty() {
            return Some(PathBuf::new());
        }
        // branch 页 permalink：仲裁表裁决后，值是 _index.md 的 rel_path，
        // 其 parent 即目录；也覆盖无冲突但配了 permalink 的目录
        if let Some(page_rel) = self
            .permalinks
            .get(normalized)
            .or_else(|| self.permalinks.get(&format!("{normalized}/")))
        {
            let page = self.pages.get(page_rel)?;
            if page.is_branch {
                return page_rel.parent().map(Path::to_path_buf);
            }
            return None; // 文档 permalink：不是目录，children 无意义
        }
        // 目录默认 URL：decode 形态直接对应 rel_path
        let rel = PathBuf::from(normalized.trim_start_matches('/'));
        if self.dirs.contains_key(&rel) {
            return Some(rel);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_junk_rules() {
        assert!(is_junk(".git"));
        assert!(is_junk(".DS_Store"));
        assert!(is_junk("foo.md~"));
        assert!(is_junk("#foo.md#"));
        assert!(!is_junk("foo.md"));
        assert!(!is_junk("normal-dir"));
        // 单个 # 不构成 #...# 包裹
        assert!(!is_junk("#"));
    }

    #[test]
    fn test_is_excluded_prefix_match() {
        let exclude = vec![PathBuf::from("drafts"), PathBuf::from("a/b")];
        assert!(is_excluded(Path::new("drafts"), &exclude));
        assert!(is_excluded(Path::new("drafts/x.md"), &exclude));
        assert!(is_excluded(Path::new("a/b"), &exclude));
        assert!(is_excluded(Path::new("a/b/c.md"), &exclude));
        assert!(!is_excluded(Path::new("a"), &exclude));
        assert!(!is_excluded(Path::new("a/c.md"), &exclude));
        assert!(!is_excluded(Path::new("drafts2"), &exclude));
    }

    #[test]
    fn test_has_children_shortcode_variants() {
        assert!(has_children_shortcode("前文 {{% children %}} 后文"));
        assert!(has_children_shortcode("{{< children >}}"));
        assert!(has_children_shortcode("{{%children%}}"));
        assert!(has_children_shortcode("{{% children sort=\"weight\" %}}"));
        assert!(has_children_shortcode("{{<\nchildren depth=2 >}}"));
        // 词边界：child / childrenx 不算
        assert!(!has_children_shortcode("{{% child %}}"));
        assert!(!has_children_shortcode("{{% childrenx %}}"));
        assert!(!has_children_shortcode("正文提到 children 一词"));
        assert!(!has_children_shortcode(
            "{{% notice %}}children{{% /notice %}}"
        ));
    }

    fn mds(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from(format!("dir/{n}")))
            .collect()
    }

    #[test]
    fn test_select_branch_file_priority_chain() {
        assert_eq!(
            select_branch_file(Path::new("dir"), &mds(&["readme.md", "index.md"])),
            Some(PathBuf::from("dir/index.md"))
        );
        assert_eq!(
            select_branch_file(Path::new("dir"), &mds(&["readme.md"])),
            Some(PathBuf::from("dir/readme.md"))
        );
        // _index 最高优先
        assert_eq!(
            select_branch_file(
                Path::new("dir"),
                &mds(&["readme.md", "index.md", "_index.md"])
            ),
            Some(PathBuf::from("dir/_index.md"))
        );
        assert_eq!(
            select_branch_file(Path::new("dir"), &mds(&["foo.md"])),
            None
        );
        assert_eq!(select_branch_file(Path::new("dir"), &[]), None);
    }

    #[test]
    fn test_select_branch_file_case_insensitive() {
        assert_eq!(
            select_branch_file(Path::new("dir"), &mds(&["README.md"])),
            Some(PathBuf::from("dir/README.md"))
        );
        assert_eq!(
            select_branch_file(Path::new("dir"), &mds(&["Index.MD"])),
            Some(PathBuf::from("dir/Index.MD"))
        );
        assert_eq!(
            select_branch_file(Path::new("dir"), &mds(&["_INDEX.md"])),
            Some(PathBuf::from("dir/_INDEX.md"))
        );
    }

    #[test]
    fn test_select_branch_file_same_level_tie_breaks_by_byte_order() {
        // 大小写敏感 FS 上 index.md 与 INDEX.md 并存：文件名字节序最小者胜（确定性）
        assert_eq!(
            select_branch_file(Path::new("dir"), &mds(&["INDEX.md", "index.md"])),
            Some(PathBuf::from("dir/INDEX.md"))
        );
        // 变体让位后不阻断下一候选：INDEX.md 落选 index 级，但 readme 级仍独立裁决
        assert_eq!(
            select_branch_file(Path::new("dir"), &mds(&["INDEX.md", "readme.md"])),
            Some(PathBuf::from("dir/INDEX.md"))
        );
    }

    #[test]
    fn test_is_index_class_variants() {
        assert!(is_index_class(Path::new("a/_index.md")));
        assert!(is_index_class(Path::new("a/_Index.MD")));
        assert!(!is_index_class(Path::new("a/index.md")));
        assert!(!is_index_class(Path::new("a/readme.md")));
    }
}
