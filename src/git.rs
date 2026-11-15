//! Git 受控封装：工作区安全护栏、`-z` 差异解析、写集匹配。
//!
//! 这里承载 M0 最关键的两条安全边界（M0.md §2 与 §6.2）：
//! 1. 任何破坏性命令必须先确认目标目录位于 `.orgy/runtime/worktrees/` 之下；
//! 2. 写集匹配必须按路径组件进行，禁止字符串前缀（`starts_with`）比较。

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::{Error, Result};

/// 把任意分隔符写法归一为 `/` 分隔的相对路径；不安全时返回 `None`。
///
/// 判定为不安全的情况：空串、绝对路径、含盘符、含 `..`、含空路径段、
/// 首段为 `.orgy`（规范层与运行态对 Worker 只读）。
pub fn normalize_rel_path(raw: &str) -> Option<String> {
    let unified = raw.replace('\\', "/");
    if unified.is_empty() {
        return None;
    }
    if unified.starts_with('/') {
        return None;
    }
    // 盘符形式（C:/... 或 C:...）一律拒绝。
    let bytes = unified.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return None;
    }

    let mut parts: Vec<&str> = Vec::new();
    for seg in unified.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return None,
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return None;
    }
    if parts[0] == ".orgy" {
        return None;
    }
    Some(parts.join("/"))
}

pub fn is_safe_rel_path(raw: &str) -> bool {
    normalize_rel_path(raw).is_some()
}

/// 单条变更记录。`Renamed`/`Copied` 的两个路径分别是源与目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// 类型变化（如普通文件变成符号链接）。M0 一律拒绝。
    TypeChanged,
    Renamed,
    Copied,
    Other(char),
}

/// 一条变更的完整事实：状态、双方路径、以及 Git 记录的文件模式。
///
/// 模式必须保留：判断"是不是符号链接/子模块"不能只看状态字母，
/// `A` 状态的条目同样可能是 mode 120000 的符号链接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawChange {
    pub kind: ChangeKind,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    /// 普通变更为 1 个路径；重命名/复制为 [源, 目标]。
    pub paths: Vec<String>,
}

impl RawChange {
    /// 符号链接（120000）与 Gitlink/子模块（160000）在 M0 中一律视为越界。
    fn forbidden_mode(&self) -> Option<u32> {
        [self.old_mode, self.new_mode]
            .into_iter()
            .flatten()
            .find(|mode| matches!(mode, 0o120000 | 0o160000))
    }
}

/// 写集匹配器：把 `task.writes` 编译成 glob 集合。
pub struct WritesMatcher {
    set: GlobSet,
}

impl WritesMatcher {
    pub fn new(patterns: &[String]) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let normalized = normalize_rel_path(pattern)
                .ok_or_else(|| Error::Config(format!("写集模式不合法：{pattern}")))?;
            let glob = GlobBuilder::new(&normalized)
                // 让 `*`/`?` 不跨越 `/`，路径分隔语义更可预期；
                // `**` 仍然可以跨层匹配。
                .literal_separator(true)
                .build()
                .map_err(|e| Error::Config(format!("写集模式 {pattern} 无法解析：{e}")))?;
            builder.add(glob);
        }
        let set = builder
            .build()
            .map_err(|e| Error::Config(format!("写集模式集合无法编译：{e}")))?;
        Ok(Self { set })
    }

    pub fn is_allowed(&self, raw_path: &str) -> bool {
        match normalize_rel_path(raw_path) {
            Some(p) => self.set.is_match(p),
            None => false,
        }
    }
}

/// 校验一组变更是否全部落在写集内，并拒绝类型层面禁止的变更。
///
/// 三道检查（缺一不可）：
/// 1. 状态为 `TypeChanged` 直接拒绝；
/// 2. Git 模式为符号链接或 Gitlink 直接拒绝——文件名合规不代表内容合规；
/// 3. 路径必须通过归一化与写集匹配。
pub fn validate_changes(changes: &[RawChange], matcher: &WritesMatcher) -> Result<()> {
    for change in changes {
        if change.kind == ChangeKind::TypeChanged {
            return Err(Error::Config(format!(
                "拒绝类型变化（普通文件与符号链接互转会改变仓库语义）：{:?}",
                change.paths
            )));
        }
        if let Some(mode) = change.forbidden_mode() {
            return Err(Error::Config(format!(
                "拒绝符号链接或子模块（mode {mode:o}）：{:?}",
                change.paths
            )));
        }
        for path in &change.paths {
            let Some(normalized) = normalize_rel_path(path) else {
                return Err(Error::Config(format!("变更路径越界或非法：{path}")));
            };
            if !matcher.is_allowed(&normalized) {
                return Err(Error::Config(format!(
                    "变更路径不在写集内：{normalized}（{:?}）",
                    change.kind
                )));
            }
        }
    }
    Ok(())
}

/// 在**工作区**里逐个确认变更路径不是真实的符号链接。
///
/// Git 模式的检查覆盖不了 `core.symlinks=false` 的情形（此时 Git 会把链接
/// 存成普通文件），所以这里再对磁盘状态做一次确认。
pub fn find_worktree_symlink(worktree: &Path, changes: &[RawChange]) -> Result<Option<String>> {
    for change in changes {
        for path in &change.paths {
            let Some(normalized) = normalize_rel_path(path) else {
                continue;
            };
            let absolute = worktree.join(&normalized);
            let Ok(meta) = std::fs::symlink_metadata(&absolute) else {
                continue; // 已删除的路径无需检查
            };
            if meta.file_type().is_symlink() {
                return Ok(Some(normalized));
            }
        }
    }
    Ok(None)
}

pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// 用 `git rev-parse --show-toplevel` 定位仓库根。
    pub fn discover(start: &Path) -> Result<Self> {
        let out = Command::new("git")
            .arg("-C")
            .arg(start)
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !out.status.success() {
            return Err(Error::Git {
                cmd: "git rev-parse --show-toplevel".into(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let mut root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // Git 输出使用 `/`；在 Windows 上统一成原生分隔符，避免日志里出现混合路径。
        if std::path::MAIN_SEPARATOR != '/' {
            root = root.replace('/', std::path::MAIN_SEPARATOR_STR);
        }
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn worktrees_root(&self) -> PathBuf {
        self.root.join(".orgy").join("runtime").join("worktrees")
    }

    /// 执行 git 并返回标准输出；非零退出转成错误。
    fn git(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()?;
        if !out.status.success() {
            return Err(Error::Git {
                cmd: format!("git {}", args.join(" ")),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// 只关心退出码的场景（例如 `merge-base --is-ancestor`）。
    fn git_status_only(&self, args: &[&str]) -> Result<bool> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()?;
        Ok(out.status.success())
    }

    pub fn has_head(&self) -> Result<bool> {
        self.git_status_only(&["rev-parse", "--verify", "--quiet", "HEAD"])
    }

    pub fn head(&self) -> Result<String> {
        Ok(self.git(&["rev-parse", "HEAD"])?.trim().to_string())
    }

    pub fn status_porcelain(&self) -> Result<String> {
        self.git(&["status", "--porcelain"])
    }

    pub fn read_ref(&self, refname: &str) -> Result<Option<String>> {
        if self.git_status_only(&["rev-parse", "--verify", "--quiet", refname])? {
            Ok(Some(self.git(&["rev-parse", refname])?.trim().to_string()))
        } else {
            Ok(None)
        }
    }

    /// 带旧值校验的 ref 更新（Git 侧的 CAS）。
    pub fn update_ref_cas(&self, refname: &str, new: &str, old: &str) -> Result<()> {
        self.git(&["update-ref", refname, new, old])?;
        Ok(())
    }

    pub fn update_ref(&self, refname: &str, new: &str) -> Result<()> {
        self.git(&["update-ref", refname, new])?;
        Ok(())
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        self.git_status_only(&["merge-base", "--is-ancestor", ancestor, descendant])
    }

    /// 解析 `git diff --raw -z`，保留重命名/复制的双方路径**与文件模式**。
    ///
    /// 用 `--raw` 而非 `--name-status`：后者只给状态字母，无法区分
    /// "普通文件被新增"与"符号链接被新增"（两者都显示为 `A`）。
    pub fn changed_entries(&self, base: &str, target: &str) -> Result<Vec<RawChange>> {
        let out = self.git(&["diff", "--raw", "-z", base, target])?;
        parse_raw_z(&out)
    }

    /// 本任务的写集在两次提交之间是否完全未被改动。
    ///
    /// 不在 Git 命令里传 glob：Git 的 pathspec 与 globset 语法不同
    /// （例如 `{a,b}` 在 Git 里不展开、`*` 是否跨 `/` 的默认行为也不同），
    /// 直接把 `task.writes` 当 pathspec 会造成漏检或误报。
    /// 这里统一复用与写集校验完全相同的匹配器，保证语义一致。
    pub fn writes_unchanged_between(
        &self,
        base: &str,
        target: &str,
        matcher: &WritesMatcher,
    ) -> Result<bool> {
        for change in self.changed_entries(base, target)? {
            if change
                .paths
                .iter()
                .any(|path| matcher.is_allowed(path))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 公共 Git 目录（worktree 的 `.git` 文件指向它）。
    ///
    /// Worker 需要它被 `--add-dir` 授权，否则连 `git status` 刷新索引都会被沙箱拒绝。
    pub fn git_common_dir(&self) -> Result<PathBuf> {
        let out = self.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        Ok(PathBuf::from(out.trim()))
    }

    /// 把 ORGY 自己的运行态与出口目录写进 `.git/info/exclude`。
    ///
    /// 用 per-clone 的 exclude 而不是仓库里的 `.gitignore`：不污染用户版本库，
    /// 也让 `git add -A` 不会把 outbox 交付物或运行态卷进快照。
    pub fn ensure_excluded(&self) -> Result<()> {
        const PATTERNS: [&str; 2] = [".orgy/outbox/", ".orgy/runtime/"];
        let info = self.git_common_dir()?.join("info");
        std::fs::create_dir_all(&info)?;
        let path = info.join("exclude");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut changed = false;
        for pattern in PATTERNS {
            if !text.lines().any(|line| line.trim() == pattern) {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(pattern);
                text.push('\n');
                changed = true;
            }
        }
        if changed {
            std::fs::write(&path, text)?;
        }
        Ok(())
    }

    /// 破坏性操作守门人：目标目录的规范化路径必须位于受管工作区之下。
    fn guard_destructive(&self, worktree: &Path) -> Result<()> {
        let worktrees_root = self.worktrees_root();
        let canonical_root = worktrees_root.canonicalize().map_err(|_| {
            Error::UnsafeDestructivePath {
                path: worktree.to_path_buf(),
                root: worktrees_root.clone(),
            }
        })?;
        let canonical_target = worktree
            .canonicalize()
            .map_err(|_| Error::UnsafeDestructivePath {
                path: worktree.to_path_buf(),
                root: worktrees_root.clone(),
            })?;
        if !canonical_target.starts_with(&canonical_root) || canonical_target == canonical_root {
            return Err(Error::UnsafeDestructivePath {
                path: canonical_target,
                root: canonical_root,
            });
        }
        Ok(())
    }

    pub fn reset_hard(&self, worktree: &Path, rev: &str) -> Result<()> {
        self.guard_destructive(worktree)?;
        run_in_dir(worktree, &["reset", "--hard", rev])?;
        Ok(())
    }

    pub fn clean_fdx(&self, worktree: &Path) -> Result<()> {
        self.guard_destructive(worktree)?;
        run_in_dir(worktree, &["clean", "-fdx"])?;
        Ok(())
    }

    /// 打包现场快照：`add -A` + 允许空提交，返回新提交 SHA。
    ///
    /// 显式注入 committer 身份，避免在未配置 `user.name/user.email` 的机器上失败。
    pub fn snapshot_commit(&self, worktree: &Path, message: &str) -> Result<String> {
        self.guard_destructive(worktree)?;
        run_in_dir(worktree, &["add", "-A"])?;
        let out = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args([
                "-c",
                "user.name=ORGY",
                "-c",
                "user.email=orgy@localhost",
                "commit",
                "--allow-empty",
                "--no-verify",
                "-m",
                message,
            ])
            .output()?;
        if !out.status.success() {
            return Err(Error::Git {
                cmd: "git commit --allow-empty".into(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let sha = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["rev-parse", "HEAD"])
            .output()?;
        Ok(String::from_utf8_lossy(&sha.stdout).trim().to_string())
    }

    // ------------------------------------------------------------------
    // 任务工作区（M0 §3.5）
    // ------------------------------------------------------------------

    /// 确保受管工作区根目录存在并返回它。
    pub fn ensure_worktrees_root(&self) -> Result<PathBuf> {
        let root = self.worktrees_root();
        std::fs::create_dir_all(&root)?;
        Ok(root)
    }

    /// 任务 ID 会直接成为工作区目录名，因此必须是单个安全路径组件。
    pub fn validate_task_id(task_id: &str) -> Result<()> {
        let bad = task_id.is_empty()
            || task_id == "."
            || task_id == ".."
            || task_id.contains(['/', '\\', ':', '\0'])
            || task_id.chars().any(char::is_control);
        if bad {
            return Err(Error::Config(format!(
                "任务 ID 不能用作目录名：{task_id:?}"
            )));
        }
        Ok(())
    }

    /// 词法层校验：目标必须是受管工作区根目录的**直接子目录**。
    ///
    /// 用于"即将创建"的路径——此时目录还不存在，无法用 canonicalize 比较。
    fn assert_planned_worktree_path(&self, path: &Path) -> Result<()> {
        let root = self.worktrees_root();
        let unsafe_path = || Error::UnsafeDestructivePath {
            path: path.to_path_buf(),
            root: root.clone(),
        };
        if path.parent() != Some(root.as_path()) {
            return Err(unsafe_path());
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(unsafe_path)?;
        Self::validate_task_id(name).map_err(|_| unsafe_path())?;
        Ok(())
    }

    /// 该路径是否为 Git 已登记的工作区。
    pub fn is_registered_worktree(&self, path: &Path) -> Result<bool> {
        let list = self.git(&["worktree", "list", "--porcelain"])?;
        let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        for line in list.lines() {
            let Some(rest) = line.strip_prefix("worktree ") else {
                continue;
            };
            let entry = PathBuf::from(rest.trim());
            let entry = entry.canonicalize().unwrap_or(entry);
            if entry == target {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 创建或复用任务工作区，并把它重置到基线提交。
    ///
    /// 复用路径是 M0 的常态（同一 Task 的多次 Attempt 共享固定目录），
    /// 因此这里必须把残留内容清干净，否则上一次的可能污染下一次。
    pub fn prepare_worktree(&self, task_id: &str, base_rev: &str) -> Result<PathBuf> {
        Self::validate_task_id(task_id)?;
        let root = self.ensure_worktrees_root()?;
        let path = root.join(task_id);
        self.assert_planned_worktree_path(&path)?;

        if path.exists() {
            if self.is_registered_worktree(&path)? {
                // 护栏在 reset_hard/clean_fdx 内部按 canonical path 再确认一次。
                self.reset_hard(&path, base_rev)?;
                self.clean_fdx(&path)?;
                return Ok(path);
            }
            // 目录存在但未登记：属于上次异常退出的残留，清掉后重建。
            let _ = self.git_status_only(&["worktree", "prune"]);
            std::fs::remove_dir_all(&path)?;
        }

        let path_str = path.to_string_lossy().to_string();
        self.git(&["worktree", "add", "--detach", &path_str, base_rev])?;
        Ok(path)
    }

    /// 移除任务工作区并清理 Git 元数据。
    pub fn remove_worktree(&self, path: &Path) -> Result<()> {
        self.guard_destructive(path)?;
        let path_str = path.to_string_lossy().to_string();
        self.git(&["worktree", "remove", "--force", &path_str])?;
        let _ = self.git_status_only(&["worktree", "prune"]);
        Ok(())
    }
}

fn run_in_dir(dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(Error::Git {
            cmd: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(())
}

/// 解析 `git diff --raw -z` 输出。
///
/// 每条记录的形态是：
/// `:OLD_MODE NEW_MODE OLD_SHA NEW_SHA STATUS\0path\0`
/// 重命名/复制为：`... STATUS\0old\0new\0`，STATUS 可能带相似度后缀（`R100`）。
///
/// 与 `--name-status` 的关键差别是**保留了文件模式**，因此能识别
/// 新增的符号链接（`A` + 120000）与子模块 Gitlink（160000）。
pub fn parse_raw_z(raw: &str) -> Result<Vec<RawChange>> {
    let mut tokens = raw.split('\0').filter(|t| !t.is_empty());
    let mut changes = Vec::new();

    while let Some(meta) = tokens.next() {
        let Some(meta) = meta.strip_prefix(':') else {
            return Err(Error::Other(format!("无法解析的 raw diff 记录：{meta}")));
        };
        let fields: Vec<&str> = meta.split(' ').collect();
        if fields.len() < 5 {
            return Err(Error::Other(format!("raw diff 记录字段不足：{meta}")));
        }
        let old_mode = parse_mode(fields[0]);
        let new_mode = parse_mode(fields[1]);
        let status = fields[4];
        let code = status
            .chars()
            .next()
            .ok_or_else(|| Error::Other("空的 diff 状态标记".into()))?;

        let (kind, paths) = match code {
            'R' | 'C' => {
                let from = tokens
                    .next()
                    .ok_or_else(|| Error::Other("重命名/复制记录缺少源路径".into()))?;
                let to = tokens
                    .next()
                    .ok_or_else(|| Error::Other("重命名/复制记录缺少目标路径".into()))?;
                let kind = if code == 'R' {
                    ChangeKind::Renamed
                } else {
                    ChangeKind::Copied
                };
                (kind, vec![from.to_string(), to.to_string()])
            }
            _ => {
                let path = tokens
                    .next()
                    .ok_or_else(|| Error::Other("变更记录缺少路径".into()))?;
                let kind = match code {
                    'A' => ChangeKind::Added,
                    'M' => ChangeKind::Modified,
                    'D' => ChangeKind::Deleted,
                    'T' => ChangeKind::TypeChanged,
                    other => ChangeKind::Other(other),
                };
                (kind, vec![path.to_string()])
            }
        };

        changes.push(RawChange {
            kind,
            old_mode,
            new_mode,
            paths,
        });
    }
    Ok(changes)
}

/// Git 模式字符串（`100644`、`120000`、`000000`）→ 数值。
fn parse_mode(text: &str) -> Option<u32> {
    u32::from_str_radix(text, 8).ok()
}

/// 供 `config` 复用的路径组件检查（用于拒绝 `a/../b` 一类写法）。
pub fn has_parent_component(raw: &str) -> bool {
    Path::new(&raw.replace('\\', "/"))
        .components()
        .any(|c| matches!(c, Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(patterns: &[&str]) -> WritesMatcher {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        WritesMatcher::new(&owned).unwrap()
    }

    #[test]
    fn 精确路径不会被前缀绕过() {
        let m = matcher(&["src/uart.c"]);
        assert!(m.is_allowed("src/uart.c"));
        assert!(!m.is_allowed("src/uart.c.evil"));
        assert!(!m.is_allowed("src/uart2.c"));
        assert!(!m.is_allowed("src/uart"));
    }

    #[test]
    fn 目录通配只覆盖其子树() {
        let m = matcher(&["src/uart/**"]);
        assert!(m.is_allowed("src/uart/a.c"));
        assert!(m.is_allowed("src/uart/deep/b.c"));
        assert!(!m.is_allowed("src/uartx/c.c"));
        assert!(!m.is_allowed("src/other/d.c"));
    }

    #[test]
    fn 单星号不跨层() {
        let m = matcher(&["src/*.c"]);
        assert!(m.is_allowed("src/a.c"));
        assert!(!m.is_allowed("src/sub/a.c"));
    }

    #[test]
    fn 危险路径一律拒绝() {
        let m = matcher(&["src/**"]);
        for bad in [
            "../escape.c",
            "/etc/passwd",
            "C:/Windows/system32/x.dll",
            ".orgy/spec/tasks/T-1.yaml",
            "src/../../escape.c",
            "",
        ] {
            assert!(!m.is_allowed(bad), "不应放行：{bad}");
        }
    }

    #[test]
    fn 反斜杠写法会被归一() {
        let m = matcher(&["src/uart.c"]);
        assert!(m.is_allowed(r"src\uart.c"));
    }

    #[test]
    fn 解析_raw_输出_普通变更含模式() {
        let raw = concat!(
            ":100644 100644 aaaa bbbb M\0src/a.c\0",
            ":000000 100644 0000 cccc A\0src/b.c\0",
            ":100644 000000 dddd 0000 D\0src/c.c\0",
        );
        let changes = parse_raw_z(raw).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].new_mode, Some(0o100644));
        assert_eq!(changes[2].kind, ChangeKind::Deleted);
        assert_eq!(changes[2].new_mode, Some(0));
    }

    #[test]
    fn 解析_raw_输出_重命名包含双方路径() {
        let raw = ":100644 100644 aaaa bbbb R100\0src/old.c\0src/new.c\0";
        let changes = parse_raw_z(raw).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Renamed);
        assert_eq!(changes[0].paths, vec!["src/old.c", "src/new.c"]);
    }

    #[test]
    fn 重命名逃逸会被拦截() {
        let m = matcher(&["src/new.c"]);
        // 源路径 src/secret.c 不在写集内，即便目标路径合法也必须拒绝。
        let changes = vec![RawChange {
            kind: ChangeKind::Renamed,
            old_mode: Some(0o100644),
            new_mode: Some(0o100644),
            paths: vec!["src/secret.c".into(), "src/new.c".into()],
        }];
        assert!(validate_changes(&changes, &m).is_err());
    }

    #[test]
    fn 多路径全部合规才通过() {
        let m = matcher(&["src/a.c", "src/b.c"]);
        let ok = vec![
            RawChange {
                kind: ChangeKind::Modified,
                old_mode: Some(0o100644),
                new_mode: Some(0o100644),
                paths: vec!["src/a.c".into()],
            },
            RawChange {
                kind: ChangeKind::Added,
                old_mode: Some(0),
                new_mode: Some(0o100644),
                paths: vec![r"src\b.c".into()],
            },
        ];
        assert!(validate_changes(&ok, &m).is_ok());
        let bad = vec![RawChange {
            kind: ChangeKind::Added,
            old_mode: Some(0),
            new_mode: Some(0o100644),
            paths: vec!["src/c.c".into()],
        }];
        assert!(validate_changes(&bad, &m).is_err());
    }

    #[test]
    fn 新增符号链接即使路径合法也被拒绝() {
        let m = matcher(&["src/uart.c"]);
        // 路径在写集内，但 Git 模式是符号链接。
        let changes = vec![RawChange {
            kind: ChangeKind::Added,
            old_mode: Some(0),
            new_mode: Some(0o120000),
            paths: vec!["src/uart.c".into()],
        }];
        let err = validate_changes(&changes, &m).unwrap_err().to_string();
        assert!(err.contains("符号链接"), "实际错误：{err}");
    }

    #[test]
    fn 子模块_gitlink_被拒绝() {
        let m = matcher(&["vendor/lib"]);
        let changes = vec![RawChange {
            kind: ChangeKind::Added,
            old_mode: Some(0),
            new_mode: Some(0o160000),
            paths: vec!["vendor/lib".into()],
        }];
        assert!(validate_changes(&changes, &m).is_err());
    }

    #[test]
    fn 类型变化被拒绝() {
        let m = matcher(&["src/uart.c"]);
        let changes = vec![RawChange {
            kind: ChangeKind::TypeChanged,
            old_mode: Some(0o100644),
            new_mode: Some(0o120000),
            paths: vec!["src/uart.c".into()],
        }];
        assert!(validate_changes(&changes, &m).is_err());
    }

    #[test]
    fn 工作区里的真实符号链接可被发现() {
        let (repo, _dir) = temp_repo("symlink");
        let base = repo.head().unwrap();
        let wt = repo.prepare_worktree("T-1", &base).unwrap();
        let link = wt.join("escape.txt");
        #[cfg(windows)]
        {
            // 普通权限下 Windows 可能不允许创建符号链接；不允许则跳过该项。
            if std::os::windows::fs::symlink_file("base.txt", &link).is_err() {
                return;
            }
        }
        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink("base.txt", &link).unwrap();
        }
        let changes = vec![RawChange {
            kind: ChangeKind::Added,
            old_mode: Some(0),
            new_mode: Some(0o100644),
            paths: vec!["escape.txt".into()],
        }];
        let found = find_worktree_symlink(&wt, &changes).unwrap();
        assert_eq!(found.as_deref(), Some("escape.txt"));
    }

    #[test]
    fn 真实仓库中的符号链接条目被拒绝() {
        let (repo, dir) = temp_repo("symlink-mode");
        let base = repo.head().unwrap();
        let git = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} 失败：{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        // 直接往索引里写 120000 条目：普通权限的 Windows 无法创建真实符号链接，
        // 但我们要验证的正是"Git 模式"这一层判定。
        let blob = git(&["hash-object", "-w", "base.txt"]);
        git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{blob},link.txt"),
        ]);
        git(&["commit", "-q", "-m", "symlink entry"]);

        let changes = repo.changed_entries(&base, &git(&["rev-parse", "HEAD"])).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].new_mode, Some(0o120000), "应解析出符号链接模式");

        let m = matcher(&["link.txt"]);
        let err = validate_changes(&changes, &m).unwrap_err().to_string();
        assert!(err.contains("符号链接"), "实际错误：{err}");
    }

    #[test]
    fn 花括号写集在条件四中仍按匹配器语义生效() {
        // 回归用：把 glob 直接当 Git pathspec 时，`{c,h}` 不会被展开，
        // `git diff --quiet -- 'src/*.{c,h}'` 会匹配不到任何路径并返回 0，
        // 导致"写集其实变了"被误判成 FRESH。
        let (repo, _dir) = temp_repo("brace");
        let base = repo.head().unwrap();
        let wt = repo.prepare_worktree("T-1", &base).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("src/a.c"), "int x;\n").unwrap();
        let snap = repo.snapshot_commit(&wt, "brace").unwrap();

        let m = matcher(&["src/*.{c,h}"]);
        assert!(
            !repo.writes_unchanged_between(&base, &snap, &m).unwrap(),
            "写集内的 src/a.c 被新增，必须判定为已改变"
        );
        // 与写集无关的变更不受影响
        let other = matcher(&["docs/*.md"]);
        assert!(repo.writes_unchanged_between(&base, &snap, &other).unwrap());
    }

    #[test]
    fn 父目录组件可被识别() {
        assert!(has_parent_component("src/../x"));
        assert!(!has_parent_component("src/uart.c"));
    }

    // ------------------------------------------------------------------
    // 任务工作区（需要真实 Git 仓库）
    // ------------------------------------------------------------------

    fn temp_repo(name: &str) -> (Repo, PathBuf) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/git")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} 失败：{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "orgy-test"]);
        git(&["config", "user.email", "orgy-test@example.com"]);
        std::fs::write(dir.join("base.txt"), "hello\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        (Repo::discover(&dir).unwrap(), dir)
    }

    fn worktree_head(path: &Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn 工作区创建在基线提交上() {
        let (repo, _dir) = temp_repo("create");
        let base = repo.head().unwrap();
        let wt = repo.prepare_worktree("T-1", &base).unwrap();
        assert!(wt.is_dir());
        assert!(wt.join(".git").is_file(), "工作区应带有 .git 文件");
        assert_eq!(worktree_head(&wt), base);
        assert!(repo.is_registered_worktree(&wt).unwrap());
    }

    #[test]
    fn 复用工作区会丢弃上次残留() {
        let (repo, _dir) = temp_repo("reuse");
        let base = repo.head().unwrap();
        let wt = repo.prepare_worktree("T-1", &base).unwrap();
        std::fs::write(wt.join("base.txt"), "dirty\n").unwrap();
        std::fs::write(wt.join("leftover.tmp"), "x").unwrap();

        let wt2 = repo.prepare_worktree("T-1", &base).unwrap();
        assert_eq!(wt, wt2, "同一任务必须复用固定路径");
        assert_eq!(
            std::fs::read_to_string(wt.join("base.txt")).unwrap(),
            "hello\n"
        );
        assert!(!wt.join("leftover.tmp").exists(), "未跟踪残留必须被清理");
    }

    #[test]
    fn 工作区可移除() {
        let (repo, _dir) = temp_repo("remove");
        let base = repo.head().unwrap();
        let wt = repo.prepare_worktree("T-1", &base).unwrap();
        repo.remove_worktree(&wt).unwrap();
        assert!(!wt.exists());
        assert!(!repo.is_registered_worktree(&wt).unwrap());
    }

    #[test]
    fn 破坏性操作不能指向仓库根() {
        let (repo, dir) = temp_repo("guard");
        let err = repo.clean_fdx(&dir).unwrap_err();
        assert!(
            matches!(err, crate::error::Error::UnsafeDestructivePath { .. }),
            "实际错误：{err}"
        );
        assert!(repo.reset_hard(&dir, "HEAD").is_err());
    }

    #[test]
    fn 非法任务_id_被拒绝() {
        let (repo, _dir) = temp_repo("badid");
        let base = repo.head().unwrap();
        for bad in ["../escape", "a/b", "a\\b", "", ".", "..", "C:"] {
            assert!(
                repo.prepare_worktree(bad, &base).is_err(),
                "不应接受任务 ID：{bad:?}"
            );
        }
    }

    #[test]
    fn 越界修改在工作区快照上被拦截() {
        let (repo, _dir) = temp_repo("scope");
        let base = repo.head().unwrap();
        let wt = repo.prepare_worktree("T-1", &base).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("src/uart.c"), "int uart_read(void){return 0;}\n").unwrap();

        let matcher = WritesMatcher::new(&["src/uart.c".to_string()]).unwrap();
        let snap = repo.snapshot_commit(&wt, "snapshot").unwrap();
        let changes = repo.changed_entries(&base, &snap).unwrap();
        assert!(validate_changes(&changes, &matcher).is_ok(), "写集内修改应通过");
        // 写集未被后续改动时，条件 4 应判定"未变"
        assert!(repo
            .writes_unchanged_between(&base, &snap, &matcher)
            .is_ok());

        // 前缀绕过：`src/uart.c.evil` 不在写集内。
        std::fs::write(wt.join("src/uart.c.evil"), "x").unwrap();
        let snap2 = repo.snapshot_commit(&wt, "snapshot2").unwrap();
        let changes2 = repo.changed_entries(&snap, &snap2).unwrap();
        assert!(validate_changes(&changes2, &matcher).is_err(), "前缀绕过必须被拦截");
        // 而写集内的 `src/uart.c` 自 snap 起未变 → 条件 4 仍为真
        assert!(repo
            .writes_unchanged_between(&snap, &snap2, &matcher)
            .unwrap());
    }
}
