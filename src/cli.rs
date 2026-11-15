//! 命令行入口：`init` / `run` / `status`（M0.md §7.1）。

use std::path::Path;

use clap::{Parser, Subcommand};

use crate::db::Db;
use crate::error::{Error, Result};
use crate::git::Repo;
use crate::supervisor::{db_path, Supervisor, ACCEPTED_REF};

#[derive(Parser, Debug)]
#[command(
    name = "orgy",
    version,
    about = "ORGY：单机多智能体研发流水线（M0 极简原型）"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 在仓库中创建 .orgy/ 目录、状态库与初始 accepted ref
    Init,
    /// 执行一个任务（M0 仅完成 FRESH 判定，其余步骤待实现）
    Run {
        /// 任务 ID，例如 T-1
        task_id: String,
    },
    /// 查看任务状态
    Status,
}

pub fn dispatch(cwd: &Path, cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init => init(cwd),
        Command::Run { task_id } => {
            let sup = Supervisor::open(cwd)?;
            match sup.fresh_check(&task_id)? {
                crate::supervisor::Freshness::Fresh => {
                    println!("任务 {task_id} 已是 FRESH，无需重跑。");
                    Ok(())
                }
                crate::supervisor::Freshness::Stale(reason) => {
                    println!("任务 {task_id} 需要执行，原因：{reason}");
                    sup.run_task(&task_id)
                }
            }
        }
        Command::Status => status(cwd),
    }
}

/// 初始化：校验主工作区干净 → 建目录 → 建库 → 固定 accepted ref。
///
/// 护栏来自 M0.md §2：初始化时必须确认人类主工作区没有未提交修改
/// （`.orgy/` 自身的变化不算），否则任务工作区会丢失人类的本地改动。
fn init(cwd: &Path) -> Result<()> {
    let repo = Repo::discover(cwd)?;
    let root = repo.root().to_path_buf();

    let dirty: Vec<String> = repo
        .status_porcelain()?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter(|line| !is_orgy_path(line))
        .map(|line| line.to_string())
        .collect();
    if !dirty.is_empty() {
        let preview: Vec<&str> = dirty.iter().take(5).map(|s| s.as_str()).collect();
        return Err(Error::Config(format!(
            "主工作区存在未提交修改，请先提交或暂存（共 {} 项）：\n{}",
            dirty.len(),
            preview.join("\n")
        )));
    }

    std::fs::create_dir_all(root.join(".orgy").join("spec").join("tasks"))?;
    std::fs::create_dir_all(root.join(".orgy").join("runtime"))?;

    let db = Db::open(&db_path(&root))?;
    drop(db);

    if repo.has_head()? {
        match repo.read_ref(ACCEPTED_REF)? {
            Some(sha) => println!("refs/orgy/accepted 已存在：{sha}"),
            None => {
                let head = repo.head()?;
                repo.update_ref(ACCEPTED_REF, &head)?;
                println!("已创建 refs/orgy/accepted = {head}");
            }
        }
    } else {
        println!("仓库尚无任何提交；accepted ref 需在首次提交后再创建。");
    }

    println!("初始化完成：.orgy/spec/tasks 与 .orgy/runtime 已就绪。");
    Ok(())
}

fn status(cwd: &Path) -> Result<()> {
    let repo = Repo::discover(cwd)?;
    let path = db_path(repo.root());
    if !path.is_file() {
        return Err(Error::Config(
            "未找到 .orgy/runtime/state.db，请先运行 `orgy init`".into(),
        ));
    }
    let db = Db::open(&path)?;
    let rows = db.list_tasks()?;
    if rows.is_empty() {
        println!("尚无任务记录。");
        return Ok(());
    }
    println!(
        "{:<8} {:<16} {:<10} {:<10} accepted",
        "task", "status", "spec", "plan"
    );
    for row in rows {
        println!(
            "{:<8} {:<16} {:<10} {:<10} {}",
            row.task_id,
            row.status,
            short(&row.spec_digest),
            short(&row.plan_digest),
            row.accepted_commit.as_deref().map(short).unwrap_or("-")
        );
    }
    Ok(())
}

fn short(s: &str) -> &str {
    s.get(..8).unwrap_or(s)
}

/// `git status --porcelain` 的一行是否只涉及 `.orgy/`。
fn is_orgy_path(line: &str) -> bool {
    // 格式形如 " M .orgy/spec/x.yaml" 或 "?? .orgy/runtime/"
    let path = line.get(3..).unwrap_or("").trim();
    path.starts_with(".orgy/") || path == ".orgy"
}
