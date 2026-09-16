//! 缓存层。
//!
//! 内存 manifest 是唯一真相源；磁盘 manifest 仅是启动加速快照。
//! 全部写入 tmp+rename 原子替换；读到损坏缓存一律视同 miss 重建
//! （CacheCorrupt 不上抛）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// manifest schema 版本。渲染逻辑变更（模板/shortcode 输出形态等）时递增，
/// 旧版本 manifest 整体不兼容 → 全量重建。
/// v2：notice 由 details 折叠改为 hint 形态。
pub const MANIFEST_VERSION: u32 = 3;

/// 缓存操作错误。`Corrupt` 由调用方降级为 miss，绝不映射 500。
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("缓存 IO 错误 {path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("缓存数据损坏：{0}")]
    Corrupt(String),
}

/// 单页缓存条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageEntry {
    pub url: String,
    pub mtime_ms: u64,
    pub size: u64,
    /// fragment 缓存文件相对 cache 根路径（如 `pages/ab/cdef.html`）
    pub fragment: String,
    pub has_children_shortcode: bool,
}

/// 单目录树缓存条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub mtime_ms: u64,
    pub file: String,
}

/// manifest 磁盘/内存格式。key 为 rel_path / dir 的字符串形态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub pages: HashMap<String, PageEntry>,
    pub trees: HashMap<String, TreeEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            pages: HashMap::new(),
            trees: HashMap::new(),
        }
    }
}

/// 启动增量 diff 结果。仅描述差异，不触发渲染。
#[derive(Debug, Default, PartialEq)]
pub struct StartupDiff {
    /// mtime/size 变化的文件
    pub changed: Vec<String>,
    /// manifest 没有的新文件
    pub added: Vec<String>,
    /// 扫描结果中不存在的旧条目
    pub removed: Vec<String>,
}

/// 查询结果：条目 + fragment 文件是否实际存在（读取侧以文件存在性为准）。
#[derive(Debug, Clone)]
pub struct PageLookup {
    pub entry: PageEntry,
    pub fragment_exists: bool,
}

fn system_time_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// xxh3 hex（64bit）。
fn xxh3_hex(input: &str) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(input.as_bytes()))
}

/// 页面 fragment 缓存相对路径：pages/<前2字符>/<余14字符>.html。
pub fn page_fragment_rel(url: &str) -> String {
    let hex = xxh3_hex(url);
    format!("pages/{}/{}.html", &hex[..2], &hex[2..])
}

/// 树 JSON 缓存相对路径：tree/<hex>.json。
pub fn tree_file_rel(dir: &str) -> String {
    format!("tree/{}.json", xxh3_hex(dir))
}

/// tmp + rename 原子写。
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    let tmp = path.with_extension("tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CacheError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&tmp, bytes).map_err(|source| CacheError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| CacheError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// 缓存句柄：内存 manifest（真相源）+ cache 根目录。
pub struct CacheStore {
    dir: PathBuf,
    manifest: Mutex<Manifest>,
}

impl CacheStore {
    /// 打开缓存目录（不加载磁盘 manifest，由 [`load_manifest`] 显式触发）。
    pub fn open(dir: &Path) -> CacheStore {
        CacheStore {
            dir: dir.to_path_buf(),
            manifest: Mutex::new(Manifest::default()),
        }
    }

    /// 加载磁盘 manifest。缺失/损坏/版本不符 → 内存空 manifest（全量重建）。
    /// 返回是否成功加载。
    pub fn load_manifest(&self) -> bool {
        let path = self.dir.join("manifest.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return false; // 缺失：正常首次启动
        };
        match serde_json::from_str::<Manifest>(&raw) {
            Ok(m) if m.version == MANIFEST_VERSION => {
                *self.manifest.lock().expect("manifest 锁中毒") = m;
                true
            }
            Ok(m) => {
                warn!(
                    disk_version = m.version,
                    expected = MANIFEST_VERSION,
                    "manifest 版本不符，全量重建"
                );
                false
            }
            Err(e) => {
                warn!(%e, "manifest 损坏，视同不存在全量重建");
                false
            }
        }
    }

    /// 启动增量 diff：扫描结果 vs 内存 manifest，按 mtime_ms+size。
    /// 调用前提：load_manifest 已执行。diff 后将 removed/changed 的 fragment 清理。
    pub fn diff_against_scan(
        &self,
        pages: &HashMap<
            String,
            (
                u64,    /*mtime_ms*/
                u64,    /*size*/
                String, /*url*/
            ),
        >,
        dirs: &HashMap<String, u64 /*mtime_ms*/>,
    ) -> StartupDiff {
        let mut manifest = self.manifest.lock().expect("manifest 锁中毒");
        let mut diff = StartupDiff::default();
        for (rel, (mtime_ms, size, _url)) in pages {
            match manifest.pages.get(rel) {
                Some(entry) if entry.mtime_ms == *mtime_ms && entry.size == *size => {}
                Some(_) => diff.changed.push(rel.clone()),
                None => diff.added.push(rel.clone()),
            }
        }
        for rel in manifest.pages.keys() {
            if !pages.contains_key(rel) {
                diff.removed.push(rel.clone());
            }
        }
        // removed/changed 的 fragment 删除（下次访问按需渲染）
        for rel in diff.changed.iter().chain(diff.removed.iter()) {
            if let Some(entry) = manifest.pages.remove(rel) {
                let _ = std::fs::remove_file(self.dir.join(&entry.fragment));
            }
        }
        // 树条目：目录 mtime 变化或消失时清理（重建在树模块写回时自然发生）
        manifest.trees.retain(|dir, entry| match dirs.get(dir) {
            Some(mtime_ms) => *mtime_ms == entry.mtime_ms,
            None => {
                let _ = std::fs::remove_file(self.dir.join(&entry.file));
                false
            }
        });
        diff
    }

    /// 写页面 fragment + 更新内存条目（原子写）。
    pub fn store_page(
        &self,
        rel_path: &str,
        url: &str,
        mtime: SystemTime,
        size: u64,
        html: &str,
        has_children_shortcode: bool,
    ) -> Result<(), CacheError> {
        let fragment = page_fragment_rel(url);
        write_atomic(&self.dir.join(&fragment), html.as_bytes())?;
        let mut manifest = self.manifest.lock().expect("manifest 锁中毒");
        manifest.pages.insert(
            rel_path.to_string(),
            PageEntry {
                url: url.to_string(),
                mtime_ms: system_time_ms(mtime),
                size,
                fragment,
                has_children_shortcode,
            },
        );
        Ok(())
    }

    /// 写树 JSON + 更新内存条目。
    pub fn store_tree(&self, dir: &str, mtime: SystemTime, json: &str) -> Result<(), CacheError> {
        let file = tree_file_rel(dir);
        write_atomic(&self.dir.join(&file), json.as_bytes())?;
        let mut manifest = self.manifest.lock().expect("manifest 锁中毒");
        manifest.trees.insert(
            dir.to_string(),
            TreeEntry {
                mtime_ms: system_time_ms(mtime),
                file,
            },
        );
        Ok(())
    }

    /// 查页面缓存：条目 + fragment 文件实际存在性（读取侧以文件为准）。
    pub fn lookup_page(&self, rel_path: &str) -> Option<PageLookup> {
        let manifest = self.manifest.lock().expect("manifest 锁中毒");
        let entry = manifest.pages.get(rel_path)?.clone();
        let fragment_exists = self.dir.join(&entry.fragment).is_file();
        Some(PageLookup {
            entry,
            fragment_exists,
        })
    }

    /// 读页面 fragment 内容（调用方已通过 lookup_page 确认存在）。
    pub fn read_page_fragment(&self, rel_path: &str) -> Result<Option<String>, CacheError> {
        let Some(lookup) = self.lookup_page(rel_path) else {
            return Ok(None);
        };
        if !lookup.fragment_exists {
            return Ok(None);
        }
        match std::fs::read_to_string(self.dir.join(&lookup.entry.fragment)) {
            Ok(html) => Ok(Some(html)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(CacheError::Io {
                path: self.dir.join(&lookup.entry.fragment),
                source,
            }),
        }
    }

    /// 查树缓存（含 ETag 语义的原始 JSON；不存在返回 None）。
    pub fn lookup_tree(&self, dir: &str) -> Option<String> {
        let manifest = self.manifest.lock().expect("manifest 锁中毒");
        let entry = manifest.trees.get(dir)?;
        std::fs::read_to_string(self.dir.join(&entry.file)).ok()
    }

    /// 查树条目的记录 mtime_ms（请求侧 stat 目录 mtime 与此比对兜底）。
    pub fn lookup_tree_mtime_ms(&self, dir: &str) -> Option<u64> {
        let manifest = self.manifest.lock().expect("manifest 锁中毒");
        manifest.trees.get(dir).map(|e| e.mtime_ms)
    }

    /// mtime 兜底校验原语：mtime_ms 或 size 任一不等即 stale。
    pub fn is_stale(entry: &PageEntry, mtime: SystemTime, size: u64) -> bool {
        entry.mtime_ms != system_time_ms(mtime) || entry.size != size
    }

    /// 失效页面缓存：删 fragment + 内存条目（文件不存在不算错）。
    pub fn invalidate_page(&self, rel_path: &str) {
        let mut manifest = self.manifest.lock().expect("manifest 锁中毒");
        if let Some(entry) = manifest.pages.remove(rel_path) {
            let _ = std::fs::remove_file(self.dir.join(&entry.fragment));
        }
    }

    /// 失效树缓存。
    pub fn invalidate_tree(&self, dir: &str) {
        let mut manifest = self.manifest.lock().expect("manifest 锁中毒");
        if let Some(entry) = manifest.trees.remove(dir) {
            let _ = std::fs::remove_file(self.dir.join(&entry.file));
        }
    }

    /// 失效全部树缓存（索引整树替换时调用：树 JSON 的数据源是索引，
    /// 索引更新而目录 mtime 未变的窗口内重建会写入旧数据的"新版本"缓存，
    /// 导致树永久陈旧——必须连带清空）。
    pub fn invalidate_all_trees(&self) {
        let mut manifest = self.manifest.lock().expect("manifest 锁中毒");
        for (_, entry) in manifest.trees.drain() {
            let _ = std::fs::remove_file(self.dir.join(&entry.file));
        }
    }

    /// manifest 整体写回磁盘（防抖批次后/每 5s/停机时由 server 层触发）。
    /// 失败 WARN + Err（服务继续，故障矩阵：下次启动走全量重建）。
    pub fn flush(&self) -> Result<(), CacheError> {
        let snapshot = self.manifest.lock().expect("manifest 锁中毒").clone();
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| CacheError::Corrupt(format!("manifest 序列化失败：{e}")))?;
        if let Err(e) = write_atomic(&self.dir.join("manifest.json"), json.as_bytes()) {
            warn!(%e, "manifest 写回失败，服务继续（下次启动全量重建）");
            return Err(e);
        }
        Ok(())
    }

    /// 内存条目数（诊断/测试用）。
    pub fn page_count(&self) -> usize {
        self.manifest.lock().expect("manifest 锁中毒").pages.len()
    }

    pub fn tree_count(&self) -> usize {
        self.manifest.lock().expect("manifest 锁中毒").trees.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_fragment_rel_two_level_sharding() {
        let rel = page_fragment_rel("/guide/intro");
        assert!(rel.starts_with("pages/"), "{rel}");
        let rest = rel.strip_prefix("pages/").unwrap();
        let (first, last) = rest.split_once('/').unwrap();
        assert_eq!(first.len(), 2, "一级目录为 hash 前 2 字符：{rel}");
        assert!(last.ends_with(".html"));
        assert_eq!(
            first.len() + last.len(),
            2 + 14 + 5,
            "hex 64bit=16 字符：{rel}"
        );
        // 确定性
        assert_eq!(rel, page_fragment_rel("/guide/intro"));
    }

    #[test]
    fn test_tree_file_rel_single_level() {
        let rel = tree_file_rel("guide");
        assert!(rel.starts_with("tree/"));
        assert!(rel.ends_with(".json"));
        assert_eq!(rel, tree_file_rel("guide"));
    }

    #[test]
    fn test_manifest_serde_roundtrip() {
        let mut m = Manifest::default();
        m.pages.insert(
            "guide/intro.md".to_string(),
            PageEntry {
                url: "/guide/intro".to_string(),
                mtime_ms: 1756000000000,
                size: 1234,
                fragment: "pages/ab/cdef.html".to_string(),
                has_children_shortcode: false,
            },
        );
        m.trees.insert(
            "guide".to_string(),
            TreeEntry {
                mtime_ms: 1756000000000,
                file: "tree/ab12.json".to_string(),
            },
        );
        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
        // snake_case 字段名（接口规范）
        assert!(json.contains("\"mtime_ms\""));
        assert!(json.contains("\"has_children_shortcode\""));
    }

    #[test]
    fn test_is_stale_detection() {
        let entry = PageEntry {
            url: "/x".into(),
            mtime_ms: 1000,
            size: 10,
            fragment: "pages/ab/cd.html".into(),
            has_children_shortcode: false,
        };
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1000);
        assert!(!CacheStore::is_stale(&entry, t, 10));
        assert!(CacheStore::is_stale(&entry, t, 11), "size 变化");
        let t2 = UNIX_EPOCH + std::time::Duration::from_millis(2000);
        assert!(CacheStore::is_stale(&entry, t2, 10), "mtime 变化");
    }
}
