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
    Renamed,
    Copied,
    Other(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChange {
    pub kind: ChangeKind,
    /// 普通变更为 1 个路径；重命名/复制为 [源, 目标]。
    pub paths: Vec<String>,
}

impl PathChange {
    fn single(kind: ChangeKind, path: &str) -> Self {
        Self {
            kind,
            paths: vec![path.to_string()],
        }
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

/// 校验一组变更是否全部落在写集内；返回第一条违规说明。
pub fn validate_changes(changes: &[PathChange], matcher: &WritesMatcher) -> Result<()> {
    for change in changes {
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
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
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

    /// 解析 `git diff --name-status -z`，保留重命名/复制的双方路径。
    pub fn changed_paths(&self, base: &str, target: &str) -> Result<Vec<PathChange>> {
        let out = self.git(&["diff", "--name-status", "-z", base, target])?;
        parse_name_status_z(&out)
    }

    /// 判定 `path..` 指定的路径集在两次提交之间是否完全未变。
    pub fn paths_unchanged_between(
        &self,
        base: &str,
        target: &str,
        paths: &[String],
    ) -> Result<bool> {
        if paths.is_empty() {
            return Ok(true);
        }
        let mut args: Vec<&str> = vec!["diff", "--quiet", base, target, "--"];
        args.extend(paths.iter().map(|s| s.as_str()));
        self.git_status_only(&args)
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

/// 解析 `-z` 输出的 name-status 记录。
///
/// 格式：`STATUS\0path\0`，重命名/复制为 `STATUS\0old\0new\0`，
/// STATUS 可能带相似度后缀（`R100`、`C075`）。
pub fn parse_name_status_z(raw: &str) -> Result<Vec<PathChange>> {
    let mut tokens = raw.split('\0').filter(|t| !t.is_empty());
    let mut changes = Vec::new();
    while let Some(status_token) = tokens.next() {
        let code = status_token
            .chars()
            .next()
            .ok_or_else(|| Error::Other("空的 diff 状态标记".into()))?;
        match code {
            'R' => {
                let from = tokens
                    .next()
                    .ok_or_else(|| Error::Other("重命名记录缺少源路径".into()))?;
                let to = tokens
                    .next()
                    .ok_or_else(|| Error::Other("重命名记录缺少目标路径".into()))?;
                changes.push(PathChange {
                    kind: ChangeKind::Renamed,
                    paths: vec![from.to_string(), to.to_string()],
                });
            }
            'C' => {
                let from = tokens
                    .next()
                    .ok_or_else(|| Error::Other("复制记录缺少源路径".into()))?;
                let to = tokens
                    .next()
                    .ok_or_else(|| Error::Other("复制记录缺少目标路径".into()))?;
                changes.push(PathChange {
                    kind: ChangeKind::Copied,
                    paths: vec![from.to_string(), to.to_string()],
                });
            }
            _ => {
                let path = tokens
                    .next()
                    .ok_or_else(|| Error::Other("变更记录缺少路径".into()))?;
                let kind = match code {
                    'A' => ChangeKind::Added,
                    'M' | 'T' => ChangeKind::Modified,
                    'D' => ChangeKind::Deleted,
                    other => ChangeKind::Other(other),
                };
                changes.push(PathChange::single(kind, path));
            }
        }
    }
    Ok(changes)
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
    fn 解析_z_输出_普通变更() {
        let raw = "M\0src/a.c\0A\0src/b.c\0D\0src/c.c\0";
        let changes = parse_name_status_z(raw).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[2].kind, ChangeKind::Deleted);
    }

    #[test]
    fn 解析_z_输出_重命名包含双方路径() {
        let raw = "R100\0src/old.c\0src/new.c\0";
        let changes = parse_name_status_z(raw).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Renamed);
        assert_eq!(changes[0].paths, vec!["src/old.c", "src/new.c"]);
    }

    #[test]
    fn 重命名逃逸会被拦截() {
        let m = matcher(&["src/new.c"]);
        // 源路径 src/secret.c 不在写集内，即便目标路径合法也必须拒绝。
        let changes = vec![PathChange {
            kind: ChangeKind::Renamed,
            paths: vec!["src/secret.c".into(), "src/new.c".into()],
        }];
        assert!(validate_changes(&changes, &m).is_err());
    }

    #[test]
    fn 多路径全部合规才通过() {
        let m = matcher(&["src/a.c", "src/b.c"]);
        let ok = vec![
            PathChange::single(ChangeKind::Modified, "src/a.c"),
            PathChange::single(ChangeKind::Added, r"src\b.c"),
        ];
        assert!(validate_changes(&ok, &m).is_ok());
        let bad = vec![PathChange::single(ChangeKind::Added, "src/c.c")];
        assert!(validate_changes(&bad, &m).is_err());
    }

    #[test]
    fn 父目录组件可被识别() {
        assert!(has_parent_component("src/../x"));
        assert!(!has_parent_component("src/uart.c"));
    }
}
