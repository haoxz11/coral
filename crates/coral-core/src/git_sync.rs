//! git 内容同步。
//!
//! 单仓单分支；`watch_dir` 镜像到 content.root 根（含删除）；
//! 原子应用 = 写 content.root **同卷**临时文件 + rename（禁止跨卷 rename，
//! K8s 下 cache 与 content 是不同卷）；失败不动 content.root（旧内容服务）。
//! 纯同步（core 无运行时约束，调用侧 spawn_blocking）。
//! gix 0.x churn 隔离：git 交互全部收敛在本模块。

use crate::config::GitConfig;
use gix::objs::tree::EntryKind;
use gix::progress::Discard;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// 同步统计（精确变更清单供刷新链使用；elapsed 由调用方计时）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct SyncStats {
    /// 新增或修改并已应用的文件（content.root 相对路径）
    pub changed: Vec<String>,
    /// 已从 content.root 删除的文件
    pub removed: Vec<String>,
}

/// 同步错误。
#[derive(Debug, thiserror::Error)]
pub enum GitSyncError {
    #[error("git 操作失败：{0}")]
    Git(String),
    #[error("IO 错误 {path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("配置错误：{0}")]
    Config(String),
}

/// git 同步器。镜像仓库在 `mirror_dir`（调用方传 cache/git-mirror）。
pub struct GitSync {
    cfg: GitConfig,
    mirror_dir: PathBuf,
    content_root: PathBuf,
}

impl GitSync {
    pub fn new(cfg: GitConfig, mirror_dir: PathBuf, content_root: PathBuf) -> Self {
        Self {
            cfg,
            mirror_dir,
            content_root,
        }
    }

    /// known_hosts 存 cache（Pod 无 HOME；派生数据语义，删缓存重新信任）。
    fn known_hosts_path(&self) -> PathBuf {
        self.mirror_dir
            .parent()
            .unwrap_or(Path::new("."))
            .join("git-known-hosts")
    }

    /// 镜像仓库是否已 clone（观测用；与 ensure_cloned 的判定同口径）。
    pub fn mirror_is_cloned(&self) -> bool {
        self.mirror_dir.join(".git").is_dir()
    }

    /// SSH 远端的 `core.sshCommand` 值：`ssh -i <key> -o UserKnownHostsFile=...
    /// -o StrictHostKeyChecking=...`。非 SSH 远端返回 None。
    ///
    /// gix SSH 传输调用系统 ssh 命令——私钥/hostkey 参数必须经
    /// `core.sshCommand` 注入（distroless 无 ssh 的根因配套）。
    /// hostkey 策略：配置了 host_key → 严格校验；未配置 → accept-new
    ///（首次信任并记录，内网 GitLab 常规做法）。
    fn ssh_command_value(&self) -> Result<Option<String>, GitSyncError> {
        let is_ssh = self.cfg.remote.starts_with("git@") || self.cfg.remote.starts_with("ssh://");
        if !is_ssh {
            return Ok(None);
        }
        let known_hosts = self.known_hosts_path();
        let strict = if self.cfg.host_key.trim().is_empty() {
            "accept-new"
        } else {
            if let Some(parent) = known_hosts.parent() {
                std::fs::create_dir_all(parent).map_err(|e| GitSyncError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
            std::fs::write(&known_hosts, format!("{}\n", self.cfg.host_key.trim())).map_err(
                |e| GitSyncError::Io {
                    path: known_hosts.clone(),
                    source: e,
                },
            )?;
            "yes"
        };
        Ok(Some(format!(
            // LogLevel=ERROR：压掉 accept-new 首次信任主机的
            // "Warning: Permanently added ..."（ssh stderr 直混容器输出，
            // 看似错误实为正常首次信任；真实错误是 ERROR 级不受影响）
            "ssh -i {} -o UserKnownHostsFile={} -o StrictHostKeyChecking={} -o LogLevel=ERROR",
            self.cfg.private_key_path.display(),
            known_hosts.display(),
            strict,
        )))
    }

    /// 把 key/value 追加写入 mirror 仓库 .git/config（已存在同名 key 跳过）。
    /// 直接 append 文本——gix config API 的 append+commit 未可靠落盘
    ///（实测 API override 路径只在内存生效）。
    fn append_mirror_config(
        &self,
        section: &str,
        entries: &[(&str, String)],
    ) -> Result<(), GitSyncError> {
        let config_path = self.mirror_dir.join(".git").join("config");
        let mut existing = std::fs::read_to_string(&config_path).map_err(|e| GitSyncError::Io {
            path: config_path.clone(),
            source: e,
        })?;
        let (missing_keys, missing_vals): (Vec<_>, Vec<_>) = entries
            .iter()
            .filter(|(k, _)| !existing.contains(k))
            .cloned()
            .unzip();
        if missing_keys.is_empty() {
            return Ok(());
        }
        if !existing.ends_with('\n') {
            existing.push('\n');
        }
        existing.push_str(&format!("[{section}]\n"));
        for (k, v) in missing_keys.iter().zip(missing_vals) {
            existing.push_str(&format!("\t{k} = {v}\n"));
        }
        std::fs::write(&config_path, existing).map_err(|e| GitSyncError::Io {
            path: config_path,
            source: e,
        })?;
        Ok(())
    }

    /// 把 sshCommand 写入 mirror 仓库的 .git/config（供后续 fetch）。
    fn write_ssh_command_to_mirror(&self) -> Result<(), GitSyncError> {
        if let Some(ssh_command) = self.ssh_command_value()? {
            self.append_mirror_config("core", &[("sshCommand", ssh_command)])?;
        }
        Ok(())
    }

    /// 把 committer 身份写入 mirror 仓库的 .git/config。
    ///
    /// fetch 更新 refs/remotes/origin/<branch> 时 gix 要写 reflog，
    /// reflog 需要 committer（user.name/email）。Pod 无 HOME 无全局
    /// git 配置，缺身份时 ref 事务失败（"Failed to update references
    /// to their new position..."）。占位身份即可——只进 reflog，无语义。
    fn write_committer_to_mirror(&self) -> Result<(), GitSyncError> {
        self.append_mirror_config(
            "user",
            &[
                ("name", "coral".to_string()),
                ("email", "coral@localhost".to_string()),
            ],
        )
    }

    /// 启动 fail-fast 校验（cli 调用）：必填 + content.root 可写探针 + 私钥权限。
    /// 私钥仅在 SSH 远端（git@host:...）时必填——file:// 等本地协议不需要。
    pub fn validate(&self) -> Result<(), String> {
        self.cfg.validate()?;
        let is_ssh = self.cfg.remote.starts_with("git@") || self.cfg.remote.starts_with("ssh://");
        // content.root 可写探针（git-sync 形态下卷必须可写）
        let probe = self.content_root.join(".coral-write-probe");
        std::fs::write(&probe, b"probe")
            .map_err(|e| {
                format!(
                    "[git] 开启时 content.root 必须可写（git-sync 会同步文件进来）：{}：{e}（K8s 检查卷 readOnly 配置）",
                    self.content_root.display()
                )
            })?;
        let _ = std::fs::remove_file(&probe);
        // 私钥校验：SSH 远端必填 + 权限 0400/0600
        if is_ssh {
            if self.cfg.private_key_path.as_os_str().is_empty() {
                return Err("[git] SSH 远端必须配置 private_key_path".to_string());
            }
            if !self.cfg.private_key_path.is_file() {
                return Err(format!(
                    "[git] 私钥文件不存在：{}（K8s 检查 Secret 挂载）",
                    self.cfg.private_key_path.display()
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&self.cfg.private_key_path)
                    .map_err(|e| format!("[git] 读取私钥元数据失败：{e}"))?
                    .permissions()
                    .mode();
                if mode & 0o077 != 0 {
                    return Err(format!(
                        "[git] 私钥权限过宽（{:o}，需 0400/0600）：{}（K8s Secret 需 defaultMode: 0400）",
                        mode & 0o777,
                        self.cfg.private_key_path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    /// 确保镜像仓库存在：无则 clone（fetch 到最新分支）。
    /// 返回是否执行了 clone（首次）。
    pub fn ensure_cloned(&self) -> Result<bool, GitSyncError> {
        let start = std::time::Instant::now();
        let dot_git = self.mirror_dir.join(".git");
        if dot_git.is_dir() {
            return Ok(false);
        }
        let _ = std::fs::remove_dir_all(&self.mirror_dir);
        std::fs::create_dir_all(self.mirror_dir.parent().unwrap_or(Path::new("."))).map_err(
            |e| GitSyncError::Io {
                path: self.mirror_dir.clone(),
                source: e,
            },
        )?;
        // sshCommand 必须在 clone 阶段就生效（根因：clone 后再写 config 时
        // gix 已用裸 ssh 连接——StrictHostKeyChecking=ask 非交互环境退出 255）。
        // 经 open::Options::config_overrides API 预注入
        let open_opts = match self.ssh_command_value()? {
            Some(ssh_command) => gix::open::Options::default()
                .config_overrides([format!("core.sshCommand={ssh_command}")]),
            None => gix::open::Options::default(),
        };
        let mut fetch = gix::clone::PrepareFetch::new(
            self.cfg.remote.as_str(),
            &self.mirror_dir,
            gix::create::Kind::WithWorktree,
            gix::create::Options::default(),
            open_opts,
        )
        .map_err(|e| GitSyncError::Git(format!("prepare_clone: {e}")))?;
        // 浅克隆：只拉最新快照（文档镜像只读最新树；默认 true，见 GitConfig）
        if self.cfg.shallow {
            fetch = fetch.with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(
                std::num::NonZeroU32::MIN,
            ));
        }
        // 单分支 refspec：只拉配置分支
        let refspec = format!(
            "+refs/heads/{b}:refs/remotes/origin/{b}",
            b = self.cfg.branch
        );
        fetch = fetch.configure_connection(|_c| Ok(())).with_fetch_options(
            gix::remote::ref_map::Options {
                extra_refspecs: vec![
                    gix::refspec::parse(
                        gix::bstr::BStr::new(&refspec),
                        gix::refspec::parse::Operation::Fetch,
                    )
                    .map_err(|e| GitSyncError::Config(format!("refspec: {e}")))?
                    .to_owned(),
                ],
                ..Default::default()
            },
        );
        let interrupt = std::sync::atomic::AtomicBool::new(false);
        let (_repo, _outcome) = fetch
            .fetch_only(Discard, &interrupt)
            .map_err(|e| GitSyncError::Git(format!("clone: {e}")))?;
        // 持久化到 mirror config：后续 fetch（gix::open 默认读 repo config）
        // sshCommand（SSH fetch 用）+ committer 身份（reflog 写入需要）
        self.write_ssh_command_to_mirror()?;
        self.write_committer_to_mirror()?;
        info!(
            branch = %self.cfg.branch,
            shallow = self.cfg.shallow,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "git 镜像仓库已 clone"
        );
        Ok(true)
    }

    /// 拉取并镜像同步：fetch → 读 watch_dir 文件树 → 与 content.root diff
    /// → 原子应用（同卷临时+rename；删除直接删）→ 返回精确变更清单。
    pub fn sync(&self) -> Result<SyncStats, GitSyncError> {
        // 1) fetch 最新
        let repo = gix::open(&self.mirror_dir)
            .map_err(|e| GitSyncError::Git(format!("open mirror: {e}")))?;
        // 存量 mirror 升级兜底：无 sshCommand / committer 配置时补写
        // （重开拿所有权；committer 缺失时 ref 更新报 Failed to update
        // references——2026-09-15 线上，Pod 无全局 git 配置）
        let repo = if repo
            .config_snapshot()
            .string_by("core", None, "sshCommand")
            .is_none()
            || repo
                .config_snapshot()
                .string_by("user", None, "name")
                .is_none()
        {
            let owned = gix::open(&self.mirror_dir)
                .map_err(|e| GitSyncError::Git(format!("reopen mirror: {e}")))?;
            self.write_ssh_command_to_mirror()?;
            self.write_committer_to_mirror()?;
            drop(owned);
            gix::open(&self.mirror_dir)
                .map_err(|e| GitSyncError::Git(format!("reopen mirror: {e}")))?
        } else {
            repo
        };
        let mut remote = repo
            .remote_at(self.cfg.remote.as_str())
            .map_err(|e| GitSyncError::Git(format!("remote_at: {e}")))?;
        let refspec = format!(
            "+refs/heads/{b}:refs/remotes/origin/{b}",
            b = self.cfg.branch
        );
        remote
            .replace_refspecs(
                [gix::bstr::BStr::new(&refspec)],
                gix::remote::Direction::Fetch,
            )
            .map_err(|e| GitSyncError::Git(format!("replace_refspecs: {e}")))?;
        let interrupt = std::sync::atomic::AtomicBool::new(false);
        let connection = remote
            .connect(gix::remote::Direction::Fetch)
            .map_err(|e| GitSyncError::Git(format!("connect: {e}")))?;
        let prepare = connection
            .prepare_fetch(Discard, Default::default())
            .map_err(|e| GitSyncError::Git(format!("prepare_fetch: {e}")))?;
        let _outcome = prepare
            .receive(Discard, &interrupt)
            .map_err(|e| GitSyncError::Git(format!("fetch: {e}")))?;

        // 2) 读 origin/<branch> 的 watch_dir 树 → {rel_path: blob id}
        let repo = gix::open(&self.mirror_dir)
            .map_err(|e| GitSyncError::Git(format!("reopen mirror: {e}")))?;
        let commit_id = repo
            .rev_parse_single(format!("refs/remotes/origin/{}", self.cfg.branch).as_str())
            .map_err(|e| GitSyncError::Git(format!("rev_parse origin/{}: {e}", self.cfg.branch)))?;
        let commit = commit_id
            .object()
            .map_err(|e| GitSyncError::Git(format!("read commit: {e}")))?;
        let tree_id = commit
            .try_to_commit_ref()
            .map_err(|e| GitSyncError::Git(format!("decode commit: {e}")))?
            .tree();
        let root_tree = repo
            .find_tree(tree_id)
            .map_err(|e| GitSyncError::Git(format!("read tree: {e}")))?;
        // watch_dir 树（根 watch_dir 用根树）
        let mut files: BTreeMap<String, gix::ObjectId> = BTreeMap::new();
        let watch_dir = self.cfg.watch_dir.trim().trim_matches('/');
        let base_tree = if watch_dir.is_empty() {
            root_tree
        } else {
            let entry = root_tree
                .lookup_entry_by_path(watch_dir)
                .map_err(|e| GitSyncError::Git(format!("lookup {watch_dir}: {e}")))?;
            let Some(entry) = entry else {
                return Err(GitSyncError::Config(format!(
                    "仓库中不存在关注目录：{watch_dir}"
                )));
            };
            let tree_id = entry.id().detach();
            // 校验该条目确为 tree（EntryKind::Tree 已保证；find_tree 失败给出可读错）
            repo.find_tree(tree_id)
                .map_err(|e| GitSyncError::Git(format!("read watch_dir tree: {e}")))?
        };
        collect_tree_files(&repo, &base_tree, "", &mut files)?;
        // 大小写冲突检测（APFS 不敏感 vs Linux 敏感）
        detect_case_conflicts(&files);

        // 3) 与 content.root 现状 diff → 原子应用
        let mut stats = SyncStats::default();
        for (rel, id) in &files {
            let target = self.content_root.join(rel);
            let needs_write = match file_matches(&target, id) {
                Some(true) => false,
                Some(false) => true,
                None => true, // 不存在
            };
            if !needs_write {
                continue;
            }
            let blob = repo
                .find_object(*id)
                .map_err(|e| GitSyncError::Git(format!("read blob {rel}: {e}")))?;
            let data = &blob.data;
            // 同卷临时文件 + rename（禁止跨卷 rename）
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| GitSyncError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
            let tmp = target.with_extension("coral-tmp");
            std::fs::write(&tmp, data).map_err(|e| GitSyncError::Io {
                path: tmp.clone(),
                source: e,
            })?;
            std::fs::rename(&tmp, &target).map_err(|e| GitSyncError::Io {
                path: target.clone(),
                source: e,
            })?;
            stats.changed.push(rel.clone());
        }
        // 删除：content.root 中存在但仓库树中不存在的 .md（只管我们域内的常规文件；
        // 缓存/探针等隐藏文件不受影响）
        remove_stale(&self.content_root, "", &files, &mut stats)?;
        if !stats.changed.is_empty() || !stats.removed.is_empty() {
            info!(
                changed = stats.changed.len(),
                removed = stats.removed.len(),
                "git 同步完成"
            );
        }
        Ok(stats)
    }
}

/// 递归收集树内全部 blob 文件（rel_path 相对 watch_dir 根）。
fn collect_tree_files(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    out: &mut BTreeMap<String, gix::ObjectId>,
) -> Result<(), GitSyncError> {
    for entry in tree.iter() {
        let entry = entry.map_err(|e| GitSyncError::Git(format!("tree entry: {e}")))?;
        let name = String::from_utf8_lossy(entry.filename()).into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        match entry.mode().kind() {
            EntryKind::Tree => {
                let sub_id = entry.id();
                let sub_tree = repo
                    .find_tree(sub_id)
                    .map_err(|e| GitSyncError::Git(format!("read subtree {rel}: {e}")))?;
                collect_tree_files(repo, &sub_tree, &rel, out)?;
            }
            EntryKind::Blob | EntryKind::BlobExecutable => {
                out.insert(rel, entry.id().detach());
            }
            // symlink 等：跳过（文档场景不需要；记录 DEBUG）
            _ => {
                tracing::debug!(path = %rel, "跳过非常规文件（symlink/子模块）");
            }
        }
    }
    Ok(())
}

/// 大小写冲突检测：仅大小写不同的路径对 WARN。
fn detect_case_conflicts(files: &BTreeMap<String, gix::ObjectId>) {
    let mut groups: std::collections::BTreeMap<String, Vec<&String>> = Default::default();
    for k in files.keys() {
        groups.entry(k.to_lowercase()).or_default().push(k);
    }
    for (lower, keys) in groups {
        if keys.len() > 1 {
            warn!(
                lower = %lower,
                files = ?keys,
                "检测到仅大小写不同的文件（APFS 不敏感/Linux 敏感，按字典序后者覆盖前者）；建议仓库内消除该冲突"
            );
        }
    }
}

/// 目标文件是否已与 blob 一致（内容比较；None = 文件不存在）。
fn file_matches(target: &Path, id: &gix::ObjectId) -> Option<bool> {
    let data = std::fs::read(target).ok()?;
    // git blob oid = sha1("blob <len>\0<content>")，非裸内容哈希
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    hasher.update(format!("blob {}\0", data.len()).as_bytes());
    hasher.update(&data);
    let oid = hasher.try_finalize().ok()?;
    Some(oid == *id)
}

/// 递归删除 content.root 中仓库已不存在的文件（镜像语义；跳过隐藏/缓存文件）。
fn remove_stale(
    dir: &Path,
    prefix: &str,
    files: &BTreeMap<String, gix::ObjectId>,
    stats: &mut SyncStats,
) -> Result<(), GitSyncError> {
    let entries = std::fs::read_dir(dir).map_err(|e| GitSyncError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // 隐藏文件（缓存/探针）不归镜像管
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        if path.is_dir() {
            remove_stale(&path, &rel, files, stats)?;
            // 空目录清理（仓库树没有目录概念，空目录即 stale）
            if std::fs::read_dir(&path)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(&path);
            }
        } else if !files.contains_key(&rel) {
            std::fs::remove_file(&path).map_err(|e| GitSyncError::Io {
                path: path.clone(),
                source: e,
            })?;
            stats.removed.push(rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// 构造 file:// 远端测试仓库（系统 git 创建——gix 只用于我们自己的拉取路径）。
    fn make_remote_repo(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-b", "main", "--quiet"])
                .current_dir(dir)
                .status()
                .expect("git init")
                .success()
        );
        for (rel, content) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "--quiet",
                    "-m",
                    "init"
                ])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
        dir.to_path_buf()
    }

    fn git_push(dir: &Path, msg: &str) {
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "--quiet",
                    "-m",
                    msg
                ])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    }

    fn sync_for(remote: &Path, branch: &str, watch_dir: &str) -> (tempfile::TempDir, GitSync) {
        let tmp = tempfile::tempdir().unwrap();
        let content = tmp.path().join("content");
        std::fs::create_dir_all(&content).unwrap();
        let cfg = GitConfig {
            enabled: true,
            remote: format!("file://{}", remote.display()),
            branch: branch.to_string(),
            watch_dir: watch_dir.to_string(),
            ..GitConfig::default()
        };
        let sync = GitSync::new(cfg, tmp.path().join("mirror"), content.clone());
        (tmp, sync)
    }

    #[test]
    fn test_first_clone_and_sync() {
        let remote_tmp = tempfile::tempdir().unwrap();
        let remote = make_remote_repo(
            remote_tmp.path(),
            &[
                ("docs/_index.md", "---\ntitle: 首页\n---\n首页"),
                ("docs/guide/intro.md", "入门内容"),
                ("README.md", "仓库根文件，不在 watch_dir 不应同步"),
            ],
        );
        let (tmp, sync) = sync_for(&remote, "main", "docs");
        assert!(sync.ensure_cloned().unwrap(), "首次应 clone");
        let stats = sync.sync().unwrap();
        assert_eq!(stats.changed.len(), 2, "{stats:?}");
        assert!(stats.removed.is_empty());
        let content = tmp.path().join("content");
        assert!(content.join("_index.md").is_file());
        assert!(content.join("guide/intro.md").is_file());
        assert!(!content.join("README.md").exists(), "watch_dir 外不同步");
    }

    /// 增量 push 后 sync + 存量 mirror 缺 committer 身份的自愈（回归：
    /// 2026-09-15 线上，Pod 无全局 git 配置，ref 更新写 reflog 时报
    /// "fetch: Failed to update references to their new position..."）。
    ///
    /// 注：运行环境可能有全局 user.name 兜底（开发机），故采用
    /// "clone 后手动剥掉 user 段"模拟存量 mirror，验证 sync 侧补写。
    #[test]
    fn test_incremental_push_sync() {
        let remote_tmp = tempfile::tempdir().unwrap();
        let remote = make_remote_repo(
            remote_tmp.path(),
            &[("docs/a.md", "v1"), ("docs/b.md", "保留")],
        );
        let (tmp, sync) = sync_for(&remote, "main", "docs");
        sync.ensure_cloned().unwrap();
        sync.sync().unwrap();

        // 增量 push：改 a、删 b、增 c
        std::fs::write(remote.join("docs/a.md"), "v2 内容变了").unwrap();
        std::fs::remove_file(remote.join("docs/b.md")).unwrap();
        std::fs::write(remote.join("docs/c.md"), "新增").unwrap();
        git_push(&remote, "update");

        let stats = sync.sync().unwrap();
        assert_eq!(stats.changed.len(), 2, "a 修改 + c 新增：{stats:?}");
        assert_eq!(stats.removed, vec!["b.md".to_string()], "{stats:?}");
        let content = tmp.path().join("content");
        assert_eq!(
            std::fs::read_to_string(content.join("a.md")).unwrap(),
            "v2 内容变了"
        );
        assert!(!content.join("b.md").exists(), "删除应同步");
        assert!(content.join("c.md").is_file());

        // 存量 mirror 升级自愈：剥掉 user 段（模拟无 committer 身份的旧
        // mirror），push 新提交后再 sync——sync 侧应补写身份并成功 fetch
        let config_path = tmp.path().join("mirror/.git/config");
        let raw = std::fs::read_to_string(&config_path).unwrap();
        let stripped = raw
            .lines()
            .filter(|l| !l.contains("name =") && !l.contains("email =") && !l.contains("[user]"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&config_path, stripped + "\n").unwrap();
        std::fs::write(remote.join("docs/d.md"), "再增").unwrap();
        git_push(&remote, "again");

        let stats = sync.sync().unwrap();
        assert_eq!(stats.changed, vec!["d.md".to_string()], "{stats:?}");
        // 身份已补写
        let healed = std::fs::read_to_string(&config_path).unwrap();
        assert!(healed.contains("name = coral"), "{healed}");
    }

    #[test]
    fn test_shallow_clone_creates_shallow_boundary() {
        let remote_tmp = tempfile::tempdir().unwrap();
        let remote = make_remote_repo(remote_tmp.path(), &[("docs/x.md", "内容")]);
        // 默认（shallow=true）：mirror 带 shallow 边界文件
        let (tmp, sync) = sync_for(&remote, "main", "docs");
        assert!(sync.ensure_cloned().unwrap());
        assert!(
            tmp.path().join("mirror/.git/shallow").is_file(),
            "shallow 默认开启，clone 后应有 shallow 边界文件"
        );
        // 显式关闭：无 shallow 边界
        let (tmp2, sync2) = {
            let mut cfg = GitConfig {
                enabled: true,
                remote: format!("file://{}", remote.display()),
                branch: "main".to_string(),
                watch_dir: "docs".to_string(),
                ..GitConfig::default()
            };
            cfg.shallow = false;
            let tmp2 = tempfile::tempdir().unwrap();
            let content = tmp2.path().join("content");
            std::fs::create_dir_all(&content).unwrap();
            let sync2 = GitSync::new(cfg, tmp2.path().join("mirror"), content);
            (tmp2, sync2)
        };
        assert!(sync2.ensure_cloned().unwrap());
        assert!(
            !tmp2.path().join("mirror/.git/shallow").is_file(),
            "shallow=false 不应产生 shallow 边界"
        );
    }

    #[test]
    fn test_idempotent_second_sync() {
        let remote_tmp = tempfile::tempdir().unwrap();
        let remote = make_remote_repo(remote_tmp.path(), &[("docs/x.md", "内容")]);
        let (_tmp, sync) = sync_for(&remote, "main", "docs");
        sync.ensure_cloned().unwrap();
        sync.sync().unwrap();
        // 无变化再同步：零变更
        let stats = sync.sync().unwrap();
        assert!(
            stats.changed.is_empty() && stats.removed.is_empty(),
            "{stats:?}"
        );
    }

    #[test]
    fn test_case_conflict_warn() {
        // 仓库中仅大小写不同的两个文件（Linux 合法；gix 读树都能看到）
        let remote_tmp = tempfile::tempdir().unwrap();
        let remote = make_remote_repo(
            remote_tmp.path(),
            &[("docs/Foo.md", "大写"), ("docs/foo.md", "小写")],
        );
        let (tmp, sync) = sync_for(&remote, "main", "docs");
        sync.ensure_cloned().unwrap();
        // 同步能完成（不 panic），大小写敏感平台上两个都在
        let stats = sync.sync().unwrap();
        let content = tmp.path().join("content");
        if cfg!(target_os = "macos") {
            // APFS 大小写不敏感：两路径同一物理文件，后者覆盖前者（已知限制）
            assert_eq!(stats.changed.len(), 1, "{stats:?}");
            assert_eq!(
                std::fs::read_to_string(content.join("foo.md")).unwrap(),
                "小写"
            );
        } else {
            // Linux 大小写敏感：两个文件独立存在
            assert_eq!(stats.changed.len(), 2, "{stats:?}");
            assert!(content.join("Foo.md").is_file() && content.join("foo.md").is_file());
        }
    }

    #[test]
    fn test_missing_watch_dir_is_config_error() {
        let remote_tmp = tempfile::tempdir().unwrap();
        let remote = make_remote_repo(remote_tmp.path(), &[("other/a.md", "x")]);
        let (_tmp, sync) = sync_for(&remote, "main", "docs");
        sync.ensure_cloned().unwrap();
        let err = sync.sync().unwrap_err();
        assert!(err.to_string().contains("关注目录"), "{err}");
    }

    #[test]
    fn test_validate_file_remote_no_key_needed() {
        let remote_tmp = tempfile::tempdir().unwrap();
        let remote = make_remote_repo(remote_tmp.path(), &[("docs/a.md", "x")]);
        let (_tmp, sync) = sync_for(&remote, "main", "docs");
        // file:// 远端：无私钥也通过（私钥仅 SSH 必填）；content 可写已探针
        assert!(sync.validate().is_ok(), "file:// 不需要私钥");
    }

    #[test]
    fn test_validate_ssh_key_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let content = tmp.path().join("content");
        std::fs::create_dir_all(&content).unwrap();
        let key = tmp.path().join("id_ed25519");
        std::fs::write(&key, "k").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mk_cfg = |remote: &str| GitConfig {
                enabled: true,
                remote: remote.to_string(),
                branch: "main".into(),
                watch_dir: "docs".into(),
                private_key_path: key.clone(),
                ..GitConfig::default()
            };
            // 0600 → 通过
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
            let sync = GitSync::new(
                mk_cfg("git@gitlab.example.com:g/docs.git"),
                tmp.path().join("m"),
                content.clone(),
            );
            assert!(sync.validate().is_ok());
            // 0644 → 权限过宽
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
            let sync = GitSync::new(
                mk_cfg("git@gitlab.example.com:g/docs.git"),
                tmp.path().join("m"),
                content.clone(),
            );
            let err = sync.validate().unwrap_err();
            assert!(err.contains("权限过宽"), "{err}");
        }
    }

    #[test]
    fn test_validate_ssh_key_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let content = tmp.path().join("content");
        std::fs::create_dir_all(&content).unwrap();
        let cfg = GitConfig {
            enabled: true,
            remote: "git@gitlab.example.com:g/docs.git".into(),
            branch: "main".into(),
            watch_dir: "docs".into(),
            private_key_path: tmp.path().join("nonexistent"),
            ..GitConfig::default()
        };
        let sync = GitSync::new(cfg, tmp.path().join("m"), content);
        let err = sync.validate().unwrap_err();
        assert!(err.contains("不存在"), "{err}");
    }
}

#[cfg(test)]
mod ssh_tests {
    use super::*;
    use std::process::Command;

    /// 真 SSH 路径联调（#[ignore]：需要本机 sshd + 测试密钥，手动跑）：
    /// ```
    /// cargo test -p coral-core --lib ssh_tests -- --ignored --nocapture
    /// ```
    /// 验证 core.sshCommand 注入（-i 私钥 + accept-new hostkey）后
    /// gix 经系统 ssh 完成 clone+fetch——即 K8s Pod 内实际执行路径。
    #[test]
    #[ignore]
    fn ssh_clone_via_local_sshd() {
        let tmp = tempfile::tempdir().unwrap();
        // 测试密钥对（无口令）
        let key = tmp.path().join("id_ed25519");
        assert!(
            Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-N", "", "-q", "-f"])
                .arg(&key)
                .status()
                .unwrap()
                .success()
        );
        // 远端仓库（file:// 形态 + ssh URL 到本机）
        let remote = tmp.path().join("repo");
        std::fs::create_dir_all(remote.join("docs")).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-b", "main", "--quiet"])
                .current_dir(&remote)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(remote.join("docs/a.md"), "ssh 内容").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(&remote)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "--quiet",
                    "-m",
                    "init"
                ])
                .current_dir(&remote)
                .status()
                .unwrap()
                .success()
        );

        let content = tmp.path().join("content");
        std::fs::create_dir_all(&content).unwrap();
        let cfg = GitConfig {
            enabled: true,
            // ssh:// 协议到本机 sshd 的 file 仓库路径
            remote: format!("ssh://{}{}", whoami(), remote.display()),
            branch: "main".into(),
            watch_dir: "docs".into(),
            private_key_path: key.clone(),
            ..GitConfig::default()
        };
        let sync = GitSync::new(cfg, tmp.path().join("mirror"), content.clone());
        let cr = sync.ensure_cloned();
        assert!(cr.is_ok(), "clone 应经系统 ssh 成功：{cr:?}");
        let stats = sync.sync().unwrap();
        assert_eq!(stats.changed.len(), 1);
        assert!(content.join("a.md").is_file());

        // 复现线上 webhook 场景：push 新提交 → 增量 sync（应成功）→
        // 无变化再 sync（GitLab 测试消息路径）
        std::fs::write(remote.join("docs/b.md"), "增量").unwrap();
        for args in [
            vec!["add", "."],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--quiet",
                "-m",
                "inc",
            ],
        ] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(&remote)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let s2 = sync.sync();
        assert!(s2.is_ok(), "ssh 增量 fetch 失败：{s2:?}");
        assert_eq!(s2.as_ref().unwrap().changed.len(), 1, "{s2:?}");
        let s3 = sync.sync();
        assert!(s3.is_ok(), "ssh 无变化 fetch 失败：{s3:?}");
    }

    fn whoami() -> String {
        std::env::var("USER").unwrap_or_else(|_| "root".into())
    }
}
