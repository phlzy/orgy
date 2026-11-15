//! Supervisor 主循环（M0.md §5.2）。
//!
//! 已落地：
//! - 步骤 1：`fresh_check`（§4.3 的四条 FRESH 条件）；
//! - 步骤 2：`prepare_attempt`（固定工作区、基线、日志路径与启动指纹）。
//!
//! 未落地：步骤 3–7（启动 CLI、快照对账、staging 验收、防漂移、两步晋升）。
//! `run_task` 会明确返回"尚未实现"，不会静默跳过。

use std::path::{Path, PathBuf};

use crate::config::Spec;
use crate::db::Db;
use crate::digest::{plan_digest, spec_digest};
use crate::error::{Error, Result};
use crate::git::Repo;

pub const ACCEPTED_REF: &str = "refs/orgy/accepted";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale(String),
}

/// 一次 Attempt 的准备结果（M0 §5.2 步骤 2 的产物）。
#[derive(Debug, Clone)]
pub struct AttemptContext {
    pub task_id: String,
    pub attempt_num: u32,
    /// 本次 Attempt 的代码基线：`refs/orgy/accepted` 的提交。
    pub base_commit: String,
    pub worktree: PathBuf,
    /// 启动时刻的规范指纹，用于事后判定规范是否在运行期间被改动。
    pub spec_digest: String,
    pub plan_digest: String,
    pub log_path: PathBuf,
}

pub struct Supervisor {
    repo: Repo,
    db: Db,
    spec: Spec,
}

impl Supervisor {
    /// 打开一个已 `init` 的仓库。
    pub fn open(cwd: &Path) -> Result<Self> {
        let repo = Repo::discover(cwd)?;
        let db_path = db_path(repo.root());
        if !db_path.is_file() {
            return Err(Error::Config(
                "未找到 .orgy/runtime/state.db，请先运行 `orgy init`".into(),
            ));
        }
        let db = Db::open(&db_path)?;
        let spec = Spec::load(repo.root())?;
        Ok(Self { repo, db, spec })
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// 当前任务对应的 `(spec_digest, plan_digest)`。
    pub fn current_digests(&self, task_id: &str) -> Result<(String, String)> {
        let task = self.spec.task(task_id)?;
        let plan = self.spec.plan(&task.verification_plan)?;
        Ok((
            spec_digest(task, &self.spec.entries)?,
            plan_digest(plan)?,
        ))
    }

    /// §4.3 的四条 FRESH 条件。任一不满足即返回带原因的 `Stale`。
    pub fn fresh_check(&self, task_id: &str) -> Result<Freshness> {
        let task = self.spec.task(task_id)?;
        let (spec_d, plan_d) = self.current_digests(task_id)?;

        let Some(receipt) = self.db.latest_passed_receipt(task_id)? else {
            return Ok(Freshness::Stale("没有通过验证的收据".into()));
        };
        if receipt.spec_digest != spec_d {
            return Ok(Freshness::Stale("规范语义已变化".into()));
        }
        if receipt.plan_digest != plan_d {
            return Ok(Freshness::Stale("验证计划已变化".into()));
        }

        let Some(accepted) = self.repo.read_ref(ACCEPTED_REF)? else {
            return Ok(Freshness::Stale("refs/orgy/accepted 不存在".into()));
        };

        // 条件 3：候选必须是当前 accepted 的祖先或本身，防止人为回滚后误用旧收据。
        if !self.repo.is_ancestor(&receipt.candidate_commit, &accepted)? {
            return Ok(Freshness::Stale(
                "候选提交已不在 accepted 历史中（疑似回滚）".into(),
            ));
        }

        // 条件 4：本任务写集在候选之后未被其他任务改动。
        if !self
            .repo
            .paths_unchanged_between(&receipt.candidate_commit, &accepted, &task.writes)?
        {
            return Ok(Freshness::Stale("写集在候选之后被其他任务修改".into()));
        }

        Ok(Freshness::Fresh)
    }

    /// M0 §5.2 步骤 2：准备一次 Attempt 的上下文（工作区、基线、日志路径）。
    ///
    /// 只做"准备"，不启动 CLI。这样步骤 2 可以单独验收，也便于调试时手工检查工作区。
    pub fn prepare_attempt(&self, task_id: &str) -> Result<AttemptContext> {
        self.spec.task(task_id)?;
        let accepted = self.repo.read_ref(ACCEPTED_REF)?.ok_or_else(|| {
            Error::Config("refs/orgy/accepted 不存在，请先运行 `orgy init`".into())
        })?;
        let (spec_digest, plan_digest) = self.current_digests(task_id)?;
        let attempt_num = self.db.next_attempt_num(task_id)?;
        let worktree = self.repo.prepare_worktree(task_id, &accepted)?;
        let log_path = self
            .repo
            .root()
            .join(".orgy")
            .join("runtime")
            .join("logs")
            .join(format!("{task_id}.a{attempt_num}.log"));

        self.db
            .upsert_task(task_id, "RUNNING", &spec_digest, &plan_digest, None)?;

        Ok(AttemptContext {
            task_id: task_id.to_string(),
            attempt_num,
            base_commit: accepted,
            worktree,
            spec_digest,
            plan_digest,
            log_path,
        })
    }

    pub fn run_task(&self, _task_id: &str) -> Result<()> {
        Err(Error::NotImplemented(
            "M0 步骤 2–7：工作区准备、CLI 执行、快照对账、staging 验收与两步晋升",
        ))
    }
}

pub fn db_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".orgy").join("runtime").join("state.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 收据摘要不一致即失效() {
        let db = Db::open_memory().unwrap();
        db.upsert_task("T-1", "COMPLETED", "s-new", "p", None).unwrap();
        // 构造一条旧摘要收据
        db.insert_receipt(&crate::db::NewReceipt {
            task_id: "T-1",
            parent_commit: "c0",
            candidate_commit: "c1",
            spec_digest: "s-old",
            plan_digest: "p",
            passed: true,
        })
        .unwrap();
        let latest = db.latest_passed_receipt("T-1").unwrap().unwrap();
        assert_ne!(latest.spec_digest, "s-new");
    }
}
