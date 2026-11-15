//! CLI 适配器：把 `cli_profile` 翻译成一次可执行命令（M0.md §5.2 步骤 3）。
//!
//! M0 只实现一个适配器 `dsh-headless`，对接官方 `dsh --profile headless`：
//! 一次性任务、prompt 走 stdin、stdout 是 NDJSON 事件流、退出码 0 表示正常完成。
//!
//! 之所以用 `node <bin.js>` 而不是 `dsh`：npm 只提供 `.cmd` / `.ps1` shim，
//! Windows 的 `CreateProcess` 不会执行它们，拼 shell 又会破坏参数边界。

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// 目前唯一支持的适配器标识（任务 YAML 里的 `cli_profile`）。
pub const PROFILE_DSH_HEADLESS: &str = "dsh-headless";

/// 组装 argv（不含可执行文件本身）。纯函数，便于测试。
///
/// `git_common_dir` 通过 `--add-dir` 授权：worktree 的 `.git` 是指向公共 Git
/// 目录的文件，不授权的话 Worker 连 `git status` 刷新索引都会被沙箱拒绝。
pub fn build_args(bin: &Path, git_common_dir: &Path) -> Vec<String> {
    vec![
        bin.to_string_lossy().into_owned(),
        "--profile".into(),
        "headless".into(),
        "--json".into(),
        "--permission-mode".into(),
        "workspace-write".into(),
        "--approval-policy".into(),
        "never".into(),
        "--add-dir".into(),
        git_common_dir.to_string_lossy().into_owned(),
        // 唯一的标准输入标记：prompt 从 stdin 读，避开命令行长度上限。
        "-".into(),
    ]
}

/// 解析 DSH 的 JS 入口。
///
/// 优先级：显式环境变量 → npm 全局安装位置。找不到时给出可操作的提示，
/// 而不是等到任务跑到一半才失败。
pub fn resolve_dsh_bin() -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var("ORGY_DSH_BIN") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
        return Err(Error::Config(format!(
            "ORGY_DSH_BIN 指向的文件不存在：{}",
            path.display()
        )));
    }

    let candidates = [
        std::env::var_os("APPDATA").map(|appdata| {
            PathBuf::from(appdata)
                .join("npm")
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js")
        }),
        std::env::var_os("USERPROFILE").map(|home| {
            PathBuf::from(home)
                .join("AppData")
                .join("Roaming")
                .join("npm")
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js")
        }),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(Error::Config(
        "找不到 DSH 的 JS 入口（@deepseek-ai/dsh/lib/bin.js）。\
         请用 ORGY_DSH_BIN 指定绝对路径，或先 `npm i -g @deepseek-ai/dsh`。"
            .into(),
    ))
}

/// 构造一次 Worker 调用的 Command。
pub fn build_command(
    cli_profile: &str,
    worktree: &Path,
    git_common_dir: &Path,
) -> Result<Command> {
    if cli_profile != PROFILE_DSH_HEADLESS {
        return Err(Error::Config(format!(
            "M0 只支持 cli_profile={PROFILE_DSH_HEADLESS}，当前为 {cli_profile}"
        )));
    }
    let bin = resolve_dsh_bin()?;
    let node = std::env::var("ORGY_NODE").unwrap_or_else(|_| "node".to_string());

    let mut cmd = Command::new(&node);
    cmd.args(build_args(&bin, git_common_dir));
    cmd.current_dir(worktree);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 参数顺序与_stdin_标记固定() {
        let args = build_args(
            Path::new(r"C:\npm\node_modules\@deepseek-ai\dsh\lib\bin.js"),
            Path::new(r"C:\repo\.git"),
        );
        assert_eq!(args[0], r"C:\npm\node_modules\@deepseek-ai\dsh\lib\bin.js");
        assert_eq!(&args[1..3], &["--profile", "headless"]);
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"workspace-write".to_string()));
        assert!(args.contains(&"never".to_string()));
        assert_eq!(args[args.len() - 1], "-", "最后一个参数必须是 stdin 标记");
        let idx = args.iter().position(|a| a == "--add-dir").unwrap();
        assert_eq!(args[idx + 1], r"C:\repo\.git");
    }

    #[test]
    fn 未支持的适配器被拒绝() {
        let err = build_command("codex", Path::new("."), Path::new(".")).unwrap_err();
        assert!(err.to_string().contains("dsh-headless"), "实际错误：{err}");
    }
}
