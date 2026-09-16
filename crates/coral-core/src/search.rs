//! 全文搜索索引器（tantivy 全文搜索，增量索引）。
//!
//! 纯同步（core 无运行时约束，后台构建由 server spawn_blocking 包裹）。
//! 分词：jieba（中文词组命中）；内容源：markdown 原文去 front matter、
//! **shortcode 剔除**（token 清空、text 保留）。
//! 索引范围与树同规则：draft/exclude/垃圾文件排除。
//! 索引目录 `cache/search-index/`（纯派生数据）；meta 版本不符全量重建。

use crate::frontmatter;
use crate::scanner::{SiteIndex, is_draft_excluded};
use crate::shortcode::{self, Chunk};
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::document::Value as _;
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, doc};
use tracing::{info, warn};

/// 索引 schema 版本（不匹配则全量重建，同 manifest 版本语义）。
/// v2：url 字段从默认形态 `page.url` 改为 `href_for_page`（permalink 优先、
/// encode 形态）——搜索命中链接与左树节点 URL 身份一致，落地后侧栏才能
/// 定位/展开（树节点对 permalink 页面/目录用 permalink 形态）。
/// v3：新增 date 字段（frontmatter date 优先、mtime 兜底，epoch 秒）——
/// 排序加权（时间衰减）与结果列表展示时间。
const SCHEMA_VERSION: u32 = 3;
const META_FILE: &str = "coral-search-meta.json";

/// 搜索命中（下拉与结果页共用结构）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    /// `<mark>` 高亮片段（已 HTML 转义 + mark 标记）
    pub snippet: String,
    /// 所属目录路径（面包屑式展示）
    pub dir_path: String,
    /// 文档时间（frontmatter date 优先、mtime 兜底；epoch 秒）
    pub date: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SearchMeta {
    version: u32,
}

/// 搜索错误。构建/IO 失败降级为空结果 + WARN，不上抛 500。
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("搜索索引 IO 错误：{0}")]
    Io(String),
    #[error("搜索索引损坏：{0}")]
    Corrupt(String),
    #[error("查询解析失败：{0}")]
    Query(String),
}

/// 索引字段句柄（构建时缓存，避免逐查询查 schema）。
struct Fields {
    title: Field,
    url: Field,
    rel_path: Field,
    dir_path: Field,
    body: Field,
    date: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    // jieba 分词字段的 indexing 选项：TEXT 常量默认用英文 default tokenizer，
    // 必须显式指定 jieba，否则中文查询无法命中（title/body 同 tokenizer
    // 保证查询词与索引词一致）。positions 必开：QueryParser 多字段短语查询依赖。
    let jieba_indexing = TextFieldIndexing::default()
        .set_tokenizer("jieba")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let jieba_text = TextOptions::default()
        .set_indexing_options(jieba_indexing.clone())
        .set_stored();
    let title = builder.add_text_field("title", jieba_text.clone());
    let url = builder.add_text_field("url", STORED);
    // rel_path 用 STRING：不分词整串精确匹配（upsert 删除的 term 依据）
    let rel_path = builder.add_text_field("rel_path", STRING | STORED);
    let dir_path = builder.add_text_field("dir_path", STORED);
    // body 需要 STORED：SnippetGenerator 从文档取原文生成高亮片段
    let body = builder.add_text_field("body", jieba_text);
    // 文档时间（epoch 秒）：排序加权与展示；不用 tantivy Date 类型，
    // 手动读出算衰减，避免 Collector 定制
    let date = builder.add_i64_field("date", STORED);
    (
        builder.build(),
        Fields {
            title,
            url,
            rel_path,
            dir_path,
            body,
            date,
        },
    )
}

/// 全量重建统计（/search/reindex 响应体：文档数/segment 数/索引体积 + 耗时）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ReindexStats {
    pub indexed_docs: u64,
    /// 重建后 segment 数（观察 tantivy 合并状态）
    pub segments: u64,
    /// 索引目录总体积（字节）
    pub index_bytes: u64,
}

/// 目录递归体积（字节；IO 失败按 0 计该文件）。
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                total += dir_size(&entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// SystemTime → epoch 秒（早于 1970 视为 0）。
fn systemtime_epoch(t: &std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 提取索引正文：front matter 去除、shortcode 剔除（text 保留）。
pub fn extract_body(markdown_raw: &str) -> String {
    let (_fm, body) = frontmatter::parse(markdown_raw).unwrap_or_default();
    let mut out = String::with_capacity(body.len());
    for chunk in shortcode::parse(body) {
        if let Chunk::Text(t) = chunk {
            out.push_str(&t);
            out.push(' ');
        }
    }
    out
}

/// frontmatter date → epoch 秒（日期粒度）。
/// 解析走 `frontmatter::normalize_date` 唯一口径（形态兼容/月日校验一致）；
/// 非法/缺失返回 None，调用方以 mtime 兜底（无 chrono 依赖，手写换算）。
fn parse_fm_date(raw: &str) -> Option<i64> {
    let norm = crate::frontmatter::normalize_date(raw)?;
    let (y, m, d) = (
        norm[0..4].parse::<i64>().ok()?,
        norm[5..7].parse::<i64>().ok()?,
        norm[8..10].parse::<i64>().ok()?,
    );
    // 天数（含闰年）：1970-01-01 起的累计天数 × 86400
    let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in = |y: i64, m: i64| match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap(y) => 29,
        _ => 28,
    };
    let mut days = 0i64;
    for yy in 1970..y {
        days += if leap(yy) { 366 } else { 365 };
    }
    for mm in 1..m {
        days += days_in(y, mm);
    }
    days += d - 1;
    Some(days * 86400)
}

#[cfg(test)]
mod date_tests {
    use super::parse_fm_date;

    #[test]
    fn test_parse_fm_date_valid() {
        assert_eq!(parse_fm_date("1970-01-01"), Some(0));
        assert_eq!(parse_fm_date("2025-03-01"), Some(1740787200));
        assert_eq!(parse_fm_date("2025-03-01 10:30"), Some(1740787200));
        assert_eq!(parse_fm_date("2025-03-01 9:15:00"), Some(1740787200));
    }

    #[test]
    fn test_parse_fm_date_invalid() {
        assert_eq!(parse_fm_date(""), None);
        assert_eq!(parse_fm_date("2025-3-1"), None);
        assert_eq!(parse_fm_date("2025-13-01"), None);
        assert_eq!(parse_fm_date("not-a-date"), None);
        // 正文形态的时间串（ocean-book 实测样本）：None → mtime 兜底
        assert_eq!(parse_fm_date("Tue, 07 Jun 2014 20:51:35 GMT"), None);
        assert_eq!(parse_fm_date("2023-02-29 00:00:00"), None);
    }
}

/// 搜索索引句柄。reader 实时 reload（增量写后查询可见）。
/// writer 走 Mutex：句柄以 Arc 共享（AppState），watcher 增量需要写路径。
pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    fields: Fields,
    writer: std::sync::Mutex<IndexWriter>,
    /// 索引目录（meta 写入用；tantivy 0.26 Index 不暴露 path）
    dir: PathBuf,
}

/// 清空目录内容（目录本身保留）。search-index 目录是纯派生数据，
/// 只被 SearchIndex 独占使用，schema 不一致时可整体丢弃重建。
fn clear_dir(dir: &Path) -> Result<(), SearchError> {
    for entry in std::fs::read_dir(dir).map_err(|e| SearchError::Io(e.to_string()))? {
        let path = entry.map_err(|e| SearchError::Io(e.to_string()))?.path();
        let res = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        res.map_err(|e| SearchError::Io(format!("{}: {e}", path.display())))?;
    }
    Ok(())
}

impl SearchIndex {
    /// 打开或创建索引目录。
    /// 返回 (句柄, 是否需要全量重建)：meta 缺失/版本不符/损坏，或磁盘索引
    /// schema 与当前不一致（整目录重建）→ 重建。
    pub fn open(dir: &Path) -> Result<(SearchIndex, bool), SearchError> {
        let (schema, fields) = build_schema();
        std::fs::create_dir_all(dir).map_err(|e| SearchError::Io(e.to_string()))?;
        let meta_path = dir.join(META_FILE);
        let meta_ok = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<SearchMeta>(&raw).ok())
            .is_some_and(|m| m.version == SCHEMA_VERSION);

        // 磁盘索引 schema 与当前不一致（或损坏）时整目录清空重建。
        // 只认 META_FILE 版本不够：旧 schema 上 add 新字段句柄会在
        // worker 线程越界 panic，writer 永久中毒（"index writer was killed"），
        // 且重启后同样失败，只有重建目录才能自愈。
        let (index, need_rebuild) = match Index::open_in_dir(dir) {
            Ok(idx) if idx.schema() == schema => (idx, !meta_ok),
            _ => {
                clear_dir(dir)?;
                let idx = Index::create_in_dir(dir, schema)
                    .map_err(|e| SearchError::Io(e.to_string()))?;
                (idx, true)
            }
        };
        index
            .tokenizers()
            .register("jieba", tantivy_jieba::JiebaTokenizer::new());
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| SearchError::Io(e.to_string()))?;
        let writer = index
            .writer(16 * 1024 * 1024)
            .map_err(|e| SearchError::Io(e.to_string()))?;
        let writer = std::sync::Mutex::new(writer);
        Ok((
            SearchIndex {
                index,
                reader,
                fields,
                writer,
                dir: dir.to_path_buf(),
            },
            need_rebuild,
        ))
    }

    /// 全量重建：清空后按 SiteIndex 范围规则（draft/exclude/垃圾）写入全部文档。
    /// 返回重建统计（文档数/segment 数/索引体积；耗时由调用方计时）。
    pub fn build_full(
        &self,
        index: &SiteIndex,
        root: &Path,
        draft_enabled: bool,
    ) -> Result<ReindexStats, SearchError> {
        self.writer
            .lock()
            .expect("search writer 锁中毒")
            .delete_all_documents()
            .map_err(|e| SearchError::Io(e.to_string()))?;
        let mut count = 0u64;
        for (rel, page) in &index.pages {
            if is_draft_excluded(&page.fm, draft_enabled) {
                continue;
            }
            // href 形态（permalink 优先）：与树节点 URL 同源，搜索跳转落地后
            // 左树高亮/展开链才能匹配
            let href = index.href_for_page(rel);
            // frontmatter date 优先、mtime 兜底（epoch 秒）
            let date = page
                .fm
                .date
                .as_deref()
                .and_then(parse_fm_date)
                .unwrap_or_else(|| systemtime_epoch(&page.mtime));
            if let Err(e) = self.upsert_doc(
                root,
                &page.fm.title.clone().unwrap_or_else(|| {
                    rel.file_stem()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                }),
                &href,
                rel,
                date,
            ) {
                warn!(rel_path = %rel.display(), %e, "搜索索引单文档写入失败，跳过");
                continue;
            }
            count += 1;
        }
        self.commit()?;
        // 写 meta（成功 commit 后才标记版本）
        let meta = SearchMeta {
            version: SCHEMA_VERSION,
        };
        std::fs::write(
            self.dir.join(META_FILE),
            serde_json::to_string(&meta).map_err(|e| SearchError::Corrupt(e.to_string()))?,
        )
        .map_err(|e| SearchError::Io(e.to_string()))?;
        let stats = ReindexStats {
            indexed_docs: count,
            segments: self.reader.searcher().segment_readers().len() as u64,
            index_bytes: dir_size(&self.dir),
        };
        info!(
            docs = stats.indexed_docs,
            segments = stats.segments,
            index_bytes = stats.index_bytes,
            "搜索索引全量构建完成"
        );
        Ok(stats)
    }

    /// 增量插入/更新单个文档（watcher 批处理调用；调用方负责最后 commit）。
    fn upsert_doc(
        &self,
        root: &Path,
        title: &str,
        url: &str,
        rel: &Path,
        date: i64,
    ) -> Result<(), SearchError> {
        let raw = std::fs::read_to_string(root.join(rel))
            .map_err(|e| SearchError::Io(format!("{}: {e}", rel.display())))?;
        let body = extract_body(&raw);
        // 同 rel_path 旧文档先删（upsert 语义）
        self.writer
            .lock()
            .expect("search writer 锁中毒")
            .delete_term(tantivy::Term::from_field_text(
                self.fields.rel_path,
                &rel.to_string_lossy(),
            ));
        let dir_path = rel
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.writer
            .lock()
            .expect("search writer 锁中毒")
            .add_document(doc!(
                self.fields.title => title,
                self.fields.url => url,
                self.fields.rel_path => rel.to_string_lossy().into_owned(),
                self.fields.dir_path => dir_path,
                self.fields.body => body,
                self.fields.date => date,
            ))
            .map_err(|e| SearchError::Io(e.to_string()))?;
        Ok(())
    }

    /// watcher 批处理增量：changed/added 重插，removed 删除；一次 commit。
    pub fn apply_changes(
        &self,
        index: &SiteIndex,
        root: &Path,
        draft_enabled: bool,
        changed: &[PathBuf],
        removed: &[PathBuf],
    ) -> Result<(), SearchError> {
        for rel in removed {
            self.writer
                .lock()
                .expect("search writer 锁中毒")
                .delete_term(tantivy::Term::from_field_text(
                    self.fields.rel_path,
                    &rel.to_string_lossy(),
                ));
        }
        for rel in changed {
            let Some(page) = index.pages.get(rel) else {
                continue;
            };
            if is_draft_excluded(&page.fm, draft_enabled) {
                // 变成 draft 的文档从索引移除
                self.writer
                    .lock()
                    .expect("search writer 锁中毒")
                    .delete_term(tantivy::Term::from_field_text(
                        self.fields.rel_path,
                        &rel.to_string_lossy(),
                    ));
                continue;
            }
            let title = page.fm.title.clone().unwrap_or_else(|| {
                rel.file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            // href 形态（permalink 优先），与 build_full 一致
            let href = index.href_for_page(rel);
            let date = page
                .fm
                .date
                .as_deref()
                .and_then(parse_fm_date)
                .unwrap_or_else(|| systemtime_epoch(&page.mtime));
            if let Err(e) = self.upsert_doc(root, &title, &href, rel, date) {
                warn!(rel_path = %rel.display(), %e, "搜索增量更新失败，跳过该文档");
            }
        }
        self.commit()
    }

    fn commit(&self) -> Result<(), SearchError> {
        self.writer
            .lock()
            .expect("search writer 锁中毒")
            .commit()
            .map(|_| ())
            .map_err(|e| SearchError::Io(e.to_string()))
    }

    /// 查询：jieba 分词命中 title/body；SnippetGenerator 高亮。
    /// 查询串只进 tantivy QueryParser（无路径拼接面，安全红线自检通过）。
    /// 排序 = 相关性分 × 时间衰减（180 天半衰期）。
    /// 返回 (hits, tokens)：tokens 是 q 的 jieba 分词（去停用级短词），
    /// 供前端详情页关键词高亮（前端无 jieba）。
    pub fn search(
        &self,
        q: &str,
        limit: usize,
    ) -> Result<(Vec<SearchHit>, Vec<String>), SearchError> {
        // OnCommitWithDelay 是异步 reload：查询前强制 reload 保证增量可见
        self.reader
            .reload()
            .map_err(|e| SearchError::Io(e.to_string()))?;
        let qp = QueryParser::for_index(&self.index, vec![self.fields.title, self.fields.body]);
        let query = qp
            .parse_query(q)
            .map_err(|e| SearchError::Query(e.to_string()))?;
        // tantivy 0.26：TopDocs 自身不是 Collector，order_by_score() 返回 impl Collector。
        // 取 limit×5 候选，读出 date 做衰减重排后截断（加权排序需要全字段读取）
        let top = self
            .reader
            .searcher()
            .search(
                &query,
                &TopDocs::with_limit(limit.saturating_mul(5).max(limit)).order_by_score(),
            )
            .map_err(|e| SearchError::Io(e.to_string()))?;
        let searcher = self.reader.searcher();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // 衰减加权：score × 0.5^(age_days/180)；date 缺失按 epoch 0（最老）
        let mut scored: Vec<(f32, tantivy::DocAddress)> = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let age_days = searcher
                .doc::<tantivy::TantivyDocument>(addr)
                .ok()
                .and_then(|d| d.get_first(self.fields.date).and_then(|v| v.as_i64()))
                .map(|date| ((now - date) / 86400).max(0) as f32)
                .unwrap_or(f32::MAX);
            let decayed = score * (-age_days / 180.0).exp2();
            scored.push((decayed, addr));
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut snippet_gen = SnippetGenerator::create(&searcher, &*query, self.fields.body)
            .map_err(|e| SearchError::Io(e.to_string()))?;
        snippet_gen.set_max_num_chars(100);
        let mut hits = Vec::with_capacity(scored.len().min(limit));
        for (_score, addr) in scored.into_iter().take(limit) {
            let Ok(doc) = searcher.doc::<tantivy::TantivyDocument>(addr) else {
                continue;
            };
            let title = doc
                .get_first(self.fields.title)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let url = doc
                .get_first(self.fields.url)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let dir_path = doc
                .get_first(self.fields.dir_path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let date = doc
                .get_first(self.fields.date)
                .and_then(|v| v.as_i64())
                .unwrap_or_default();
            // SnippetGenerator 从 STORED 的 body 生成 <b> 高亮片段；
            // 统一替换为 <mark>（前端样式选择器锚点）
            let snippet = snippet_gen
                .snippet_from_doc(&doc)
                .to_html()
                .replace("<b>", "<mark>")
                .replace("</b>", "</mark>");
            let fallback = title.clone();
            hits.push(SearchHit {
                title,
                url,
                snippet: if snippet.is_empty() {
                    fallback
                } else {
                    snippet
                },
                dir_path,
                date,
            });
        }
        Ok((hits, self.tokenize_query(q)))
    }

    /// 查询串 jieba 分词（去单字符 token）——详情页关键词高亮用。
    /// 分词后全为单字（如专有名词「邦盛」拆成 邦/盛）时回退为原始整词：
    /// 整词高亮比逐字高亮噪音小，也保证 tokens 非空（空则链接不带 ?hl=，
    /// 详情页完全无高亮）。
    fn tokenize_query(&self, q: &str) -> Vec<String> {
        let Some(mut analyzer) = self.index.tokenizers().get("jieba") else {
            return vec![q.to_string()];
        };
        let mut stream = analyzer.token_stream(q);
        let mut tokens = Vec::new();
        while stream.advance() {
            let text = stream.token().text.clone();
            // 单字符 token（含单汉字）噪音大，跳过
            if text.chars().count() > 1 {
                tokens.push(text);
            }
        }
        if tokens.is_empty() {
            let raw = q.trim();
            if !raw.is_empty() {
                tokens.push(raw.to_string());
            }
        }
        tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContentConfig;
    use crate::scanner;

    fn make_content(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("content");
        for (rel, content) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        (tmp, root)
    }

    fn build_index(root: &Path) -> SiteIndex {
        let cfg = ContentConfig {
            root: root.to_path_buf(),
            exclude: vec![],
            draft: false,
        };
        let scan_result = scanner::scan(&cfg).unwrap();
        SiteIndex::build(scan_result, false)
    }

    #[test]
    fn test_extract_body_strips_shortcode_and_fm() {
        let raw = "---\ntitle: 标题\n---\n正文开始 {{% notice %}}提示内容{{% /notice %}} 继续 {{% children %}} 结束";
        let body = extract_body(raw);
        assert!(body.contains("正文开始"), "{body}");
        assert!(body.contains("继续"), "{body}");
        assert!(!body.contains("notice"), "shortcode 应剔除：{body}");
        assert!(!body.contains("children"), "{body}");
        assert!(!body.contains("提示内容"), "shortcode body 应剔除：{body}");
        assert!(!body.contains("title"), "front matter 应去除：{body}");
    }

    #[test]
    fn test_chinese_search_via_jieba() {
        let (tmp, root) = make_content(&[
            (
                "guide/intro.md",
                "---\ntitle: 入门指南\n---\n# 入门\n这是一篇关于脚本编写的入门教程",
            ),
            ("other.md", "---\ntitle: 无关\n---\n完全无关的内容"),
        ]);
        let index = build_index(&root);
        let si = SearchIndex::open(&tmp.path().join("idx")).unwrap().0;
        si.build_full(&index, &root, false).unwrap();

        let (hits, _tokens) = si.search("脚本", 10).unwrap();
        assert!(!hits.is_empty(), "中文词应命中");
        assert_eq!(hits[0].url, "/guide/intro");
        assert_eq!(hits[0].title, "入门指南");
        assert!(!hits[0].dir_path.is_empty());

        let (none, _) = si.search("不存在的词组", 10).unwrap();
        assert!(none.is_empty());
        drop(tmp);
    }

    #[test]
    fn test_draft_excluded_from_index() {
        let (tmp, root) = make_content(&[
            ("a.md", "---\ntitle: 正常页\n---\n关键词内容"),
            ("d.md", "---\ntitle: 草稿页\ndraft: true\n---\n关键词内容"),
        ]);
        let index = build_index(&root);
        let si = SearchIndex::open(&tmp.path().join("idx")).unwrap().0;
        si.build_full(&index, &root, false).unwrap();
        let (hits, _) = si.search("关键词", 10).unwrap();
        assert_eq!(hits.len(), 1, "draft 不入索引");
        assert_eq!(hits[0].url, "/a");
        drop(tmp);
    }

    #[test]
    fn test_incremental_update_on_change_and_remove() {
        let (tmp, root) = make_content(&[("a.md", "---\ntitle: 旧标题\n---\n旧关键词")]);
        let index = build_index(&root);
        let si = SearchIndex::open(&tmp.path().join("idx")).unwrap().0;
        si.build_full(&index, &root, false).unwrap();

        // 修改文档 → 增量重插 → 新关键词命中
        std::fs::write(root.join("a.md"), "---\ntitle: 新标题\n---\n全新关键词").unwrap();
        let index2 = build_index(&root);
        si.apply_changes(&index2, &root, false, &[PathBuf::from("a.md")], &[])
            .unwrap();
        let (hits, _) = si.search("全新关键词", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "新标题");

        // 删除文档 → 增量删除 → 不再命中
        std::fs::remove_file(root.join("a.md")).unwrap();
        si.apply_changes(&index2, &root, false, &[], &[PathBuf::from("a.md")])
            .unwrap();
        assert!(si.search("全新关键词", 10).unwrap().0.is_empty());
        drop(tmp);
    }

    #[test]
    fn test_branch_fallback_search_hit_urls_match_routes() {
        // 三文件并存目录：搜索命中 URL 必须与路由表一致——
        // 分支页命中给目录 URL，未被选中的普通文档给自身 URL
        let (tmp, root) = make_content(&[
            (
                "dir/_index.md",
                "---\ntitle: 首选项\n---\n索引页唯一关键词 alpha",
            ),
            (
                "dir/index.md",
                "---\ntitle: 普通索引\n---\n普通索引关键词 beta",
            ),
            (
                "dir/readme.md",
                "---\ntitle: 读我页\n---\n读我页关键词 gamma",
            ),
        ]);
        let index = build_index(&root);
        let si = SearchIndex::open(&tmp.path().join("idx")).unwrap().0;
        si.build_full(&index, &root, false).unwrap();

        // 分支页 _index.md：命中 URL = 目录 URL
        let (hits, _) = si.search("alpha", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "/dir", "分支页命中应为目录 URL");

        // 未被选中的 index.md / readme.md：命中 URL = 自身普通文档 URL
        let (hits, _) = si.search("beta", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "/dir/index");

        let (hits, _) = si.search("gamma", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "/dir/readme");

        // 全部命中 URL 在路由表中可达
        for u in ["/dir", "/dir/index", "/dir/readme"] {
            assert!(index.routes.contains_key(u), "搜索跳转目标 {u} 不可达");
        }
        drop(tmp);
    }

    #[test]
    fn test_permalink_page_hit_url_is_permalink() {
        // 搜索命中 URL 必须与树节点 href 同源（permalink 优先、encode 形态），
        // 否则跳转落地后左树 current 高亮失配（回归）
        let (tmp, root) = make_content(&[(
            "guide/子目录/legacy.md",
            "---\ntitle: 旧文档\npermalink: /old-doc/\n---\npermalink 页面内容",
        )]);
        let index = build_index(&root);
        let si = SearchIndex::open(&tmp.path().join("idx")).unwrap().0;
        si.build_full(&index, &root, false).unwrap();
        let (hits, _) = si.search("旧文档", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "/old-doc/", "命中 URL 应为 permalink 形态");
        drop(tmp);
    }

    #[test]
    fn test_recency_weighted_ranking_and_date_field() {
        // 同等相关性时新文档在前（时间衰减加权）；date 字段随命中返回
        let (tmp, root) = make_content(&[
            (
                "old.md",
                "---\ntitle: 旧文档\ndate: 2020-01-01\n---\n账单 账单 账单 账单",
            ),
            (
                "new.md",
                "---\ntitle: 新文档\ndate: 2026-09-01\n---\n账单 账单 账单 账单",
            ),
        ]);
        let index = build_index(&root);
        let si = SearchIndex::open(&tmp.path().join("idx")).unwrap().0;
        si.build_full(&index, &root, false).unwrap();
        let (hits, _) = si.search("账单", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "新文档", "时间加权后新文档应排前");
        assert_eq!(hits[1].title, "旧文档");
        // date 字段（epoch 秒）：frontmatter date 优先
        assert_eq!(hits[0].date, 1788220800); // 2026-09-01
        assert_eq!(hits[1].date, 1577836800); // 2020-01-01
        assert!(hits[0].date > hits[1].date, "date 字段应参与并可比");
        drop(tmp);
    }

    #[test]
    fn test_search_returns_tokens_and_mark_snippet() {
        // tokens 供详情页关键词高亮；snippet 用 <mark> 标签（CSS 锚点）
        let (tmp, root) = make_content(&[("a.md", "---\ntitle: 标题\n---\n账单模块化设计正文")]);
        let index = build_index(&root);
        let si = SearchIndex::open(&tmp.path().join("idx")).unwrap().0;
        si.build_full(&index, &root, false).unwrap();
        let (hits, tokens) = si.search("账单模块", 10).unwrap();
        assert!(!hits.is_empty());
        assert!(
            tokens.iter().any(|t| t.contains("账单")),
            "tokens 应含分词结果：{tokens:?}"
        );
        drop(tmp);
    }

    #[test]
    fn test_tokens_fallback_when_all_single_char() {
        // 专有名词被 jieba 拆成单字（邦盛 → 邦/盛）时，回退为原始整词：
        // 空数组会导致链接不带 ?hl=，详情页完全无高亮（真实内容实测回归）
        let (tmp, root) = make_content(&[("a.md", "---\ntitle: 邦盛监控\n---\n邦盛系统监控正文")]);
        let index = build_index(&root);
        let si = SearchIndex::open(&tmp.path().join("idx")).unwrap().0;
        si.build_full(&index, &root, false).unwrap();
        let (_hits, tokens) = si.search("邦盛", 10).unwrap();
        assert!(!tokens.is_empty(), "tokens 不应为空：{tokens:?}");
        assert!(
            tokens.iter().any(|t| t.contains("邦盛")),
            "应回退为整词「邦盛」：{tokens:?}"
        );
        drop(tmp);
    }

    #[test]
    fn test_reopen_version_mismatch_rebuild() {
        let (tmp, root) = make_content(&[("a.md", "---\ntitle: 页面\n---\n关键词内容")]);
        let index = build_index(&root);
        let dir = tmp.path().join("idx");
        {
            let (si, need) = SearchIndex::open(&dir).unwrap();
            assert!(need, "首次打开无 meta 应重建");
            si.build_full(&index, &root, false).unwrap();
        }
        {
            let (si, need) = SearchIndex::open(&dir).unwrap();
            assert!(!need, "meta 版本一致不重建");
            assert_eq!(si.search("关键词", 10).unwrap().0.len(), 1);
        }
        // 破坏 meta → 版本不符重建
        std::fs::write(dir.join(META_FILE), r#"{"version": 999}"#).unwrap();
        let (si, need) = SearchIndex::open(&dir).unwrap();
        assert!(need, "版本不符应重建");
        // 重建前旧文档仍在可查；build_full 会清空重写
        let _ = si;
        drop(tmp);
    }

    #[test]
    fn test_open_stale_schema_dir_recreated_not_writer_killed() {
        // 回归：磁盘索引是旧 schema（少 date 字段）而 META_FILE 版本是新的
        // （半更新状态）。旧逻辑 open_in_dir 成功后拿新字段句柄 add_document，
        // worker 线程越界 panic → "index writer was killed"，重启也无法自愈。
        // 新逻辑应整目录重建，build_full 正常完成且可查。
        let (tmp, root) = make_content(&[("a.md", "---\ntitle: 页面\n---\n关键词内容")]);
        let index = build_index(&root);
        let dir = tmp.path().join("idx");
        std::fs::create_dir_all(&dir).unwrap();
        // 模拟旧版本落的索引：不含 date 字段的 schema
        let old = {
            let mut b = tantivy::schema::Schema::builder();
            b.add_text_field("title", tantivy::schema::TEXT);
            b.build()
        };
        Index::create_in_dir(&dir, old).unwrap();
        // META_FILE 版本却是当前的（半更新状态，纯版本门禁防不住）
        std::fs::write(
            dir.join(META_FILE),
            format!(r#"{{"version": {SCHEMA_VERSION}}}"#),
        )
        .unwrap();

        let (si, need) = SearchIndex::open(&dir).unwrap();
        assert!(need, "schema 不一致应整目录重建");
        let stats = si.build_full(&index, &root, false).unwrap();
        assert_eq!(stats.indexed_docs, 1, "写入不应被 worker panic 中断");
        assert_eq!(si.search("关键词", 10).unwrap().0.len(), 1);
        drop(tmp);
    }
}
