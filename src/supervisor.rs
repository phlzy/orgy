//! Supervisor 主循环（M0.md §5.2）。
//!
//! 已落地：
//! - 步骤 1：`fresh_check`（§4.3 的四条 FRESH 条件）；
//! - 步骤 2：`prepare_attempt`（固定工作区、基线、日志路径与启动指纹）。
//!
//! 未落地：步骤 3–7（启动 CLI、快照对账、staging 验收、防漂移、两步晋升）。
//! `run_task` 会明确返回"尚未实现"，不会静默跳过。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::adapter;
use crate::config::Spec;
use crate::db::Db;
use crate::digest::{plan_digest, spec_digest};
use crate::error::{Error, Result};
use crate::git::{self, RawChange, Repo, WritesMatcher};
use crate::handoff::{Handoff, Status};
use crate::process::{self, Job};
use crate::prompt;

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

/// 一次 Attempt 的判定结论（M0.md §5.3 的 verdict 词表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    ScopeViolation(String),
    ClaimMismatch(String),
    ProtocolAnomaly(String),
    Timeout,
    Blocked(String),
    NeedsClarification(String),
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Passed => "PASSED",
            Verdict::ScopeViolation(_) => "SCOPE_VIOLATION",
            Verdict::ClaimMismatch(_) => "CLAIM_MISMATCH",
            Verdict::ProtocolAnomaly(_) => "PROTOCOL_ANOMALY",
            Verdict::Timeout => "TIMEOUT",
            Verdict::Blocked(_) => "BLOCKED",
            Verdict::NeedsClarification(_) => "NEEDS_CLARIFICATION",
        }
    }

    /// 人类可读的一行说明（CLI 与日志共用）。
    pub fn detail(&self) -> String {
        match self {
            Verdict::Passed => "L1/L2/写集均通过，候选待集成验证".into(),
            Verdict::ScopeViolation(d)
            | Verdict::ClaimMismatch(d)
            | Verdict::ProtocolAnomaly(d)
            | Verdict::Blocked(d)
            | Verdict::NeedsClarification(d) => d.clone(),
            Verdict::Timeout => "超时被强制终止".into(),
        }
    }
}

/// 判定所需的全部事实。做成显式结构体，便于单独测试判定逻辑。
pub struct AttemptFacts<'a> {
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub handoff: Option<&'a Handoff>,
    pub handoff_error: Option<&'a str>,
    pub changed: &'a [RawChange],
    pub matcher: &'a WritesMatcher,
    pub symlink: Option<&'a str>,
}

/// 纯判定函数：把事实映射为 verdict，不做任何 I/O。
///
/// 判定顺序即优先级：超时 → 协议 → 阻塞 → 越界 → 声明不符 → 通过。
pub fn evaluate(facts: &AttemptFacts<'_>) -> Verdict {
    if facts.timed_out {
        return Verdict::Timeout;
    }
    if let Some(err) = facts.handoff_error {
        return Verdict::ProtocolAnomaly(format!("交接单不可用：{err}"));
    }
    let Some(handoff) = facts.handoff else {
        return Verdict::ProtocolAnomaly("没有找到交接单".into());
    };

    // 只有正常退出才接受"阻塞/待澄清"这类非成功声明，避免把崩溃当成交接。
    if facts.exit_code != Some(0) {
        return Verdict::ProtocolAnomaly(format!(
            "进程退出码为 {:?}，但声明了 {}",
            facts.exit_code,
            handoff.status.as_str()
        ));
    }

    match handoff.status {
        Status::Blocked => {
            return Verdict::Blocked(
                handoff
                    .summary
                    .clone()
                    .unwrap_or_else(|| "Worker 声明受阻".into()),
            );
        }
        Status::NeedsClarification => {
            let questions: Vec<String> = handoff
                .questions
                .iter()
                .map(|q| format!("{}: {}", q.id, q.text))
                .collect();
            return Verdict::NeedsClarification(questions.join("; "));
        }
        Status::Completed => {}
    }

    if let Some(path) = facts.symlink {
        return Verdict::ScopeViolation(format!("改动包含符号链接：{path}"));
    }
    if let Err(err) = git::validate_changes(facts.changed, facts.matcher) {
        return Verdict::ScopeViolation(err.to_string());
    }

    // 声明与实物对账：以 Git 差异为准，自报只用于比对。
    let mut actual: Vec<String> = facts
        .changed
        .iter()
        .flat_map(|change| change.paths.iter())
        .filter_map(|path| git::normalize_rel_path(path))
        .collect();
    actual.sort();
    actual.dedup();
    let claimed = handoff.claimed_paths();

    if actual.is_empty() {
        return Verdict::ClaimMismatch("声明 COMPLETED 但工作区没有任何改动".into());
    }
    if actual != claimed {
        return Verdict::ClaimMismatch(format!(
            "自报改动与 Git 差异不一致：自报 {}，实际 {}",
            claimed.join(","),
            actual.join(",")
        ));
    }

    Verdict::Passed
}

/// 一次 Attempt 的执行结果。
#[derive(Debug)]
pub struct AttemptOutcome {
    pub verdict: Verdict,
    pub exit_code: Option<i32>,
    pub snapshot_commit: Option<String>,
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
        //
        // 这里必须复用写集匹配器，而不是把 `task.writes` 当 Git pathspec 传下去：
        // Git 的 pathspec 语法与 globset 不同（`{a,b}` 不展开、`*` 的跨目录行为
        // 也不同），直接传会导致漏检或误报。
        let matcher = WritesMatcher::new(&task.writes)?;
        if !self.repo.writes_unchanged_between(
            &receipt.candidate_commit,
            &accepted,
            &matcher,
        )? {
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

    /// M0 §5.2 步骤 3–4：启动一次 Worker、采集快照并给出 verdict。
    ///
    /// 只做到"L1/L2/写集"这一层：L3（staging 独立验收）与晋升尚未实现，
    /// 所以 `PASSED` 只代表"候选可以被送去验收"，不代表任务已完成。
    pub fn run_attempt(&self, ctx: &AttemptContext) -> Result<AttemptOutcome> {
        let task = self.spec.task(&ctx.task_id)?;
        let matcher = WritesMatcher::new(&task.writes)?;

        // 让 `git add -A` 不要把 outbox 与运行态卷进快照。
        self.repo.ensure_excluded()?;

        let outbox_dir = ctx.worktree.join(".orgy").join("outbox");
        std::fs::create_dir_all(&outbox_dir)?;
        let handoff_path = outbox_dir.join(format!(
            "{}.a{}.handoff.json",
            ctx.task_id, ctx.attempt_num
        ));
        // 上一轮残留的交接单会污染本次判定。
        let _ = std::fs::remove_file(&handoff_path);

        let git_common_dir = self.repo.git_common_dir()?;
        let prompt_text = prompt::build(task, &self.spec.entries, ctx, &handoff_path);
        let cmd = adapter::build_command(&task.cli_profile, &ctx.worktree, &git_common_dir)?;

        let job = Job::new()?;
        let run = process::run_captured(
            cmd,
            &job,
            &ctx.log_path,
            Duration::from_secs(task.timeout_sec),
            prompt_text.as_bytes(),
        )?;

        // 交付物先取出并归档，再打快照：交接单不进代码提交。
        let (handoff, handoff_error) = match std::fs::read_to_string(&handoff_path) {
            Ok(text) => {
                let archive_dir = self.repo.root().join(".orgy").join("runtime").join("handoffs");
                std::fs::create_dir_all(&archive_dir)?;
                let _ = std::fs::write(
                    archive_dir.join(format!("{}.a{}.json", ctx.task_id, ctx.attempt_num)),
                    &text,
                );
                match Handoff::parse(&text, &ctx.task_id, ctx.attempt_num) {
                    Ok(h) => (Some(h), None),
                    Err(e) => (None, Some(e.to_string())),
                }
            }
            Err(_) => (None, Some("Worker 没有写出交接单".to_string())),
        };
        let _ = std::fs::remove_file(&handoff_path);

        let snapshot = self
            .repo
            .snapshot_commit(&ctx.worktree, &format!("orgy attempt {}.a{}", ctx.task_id, ctx.attempt_num))?;
        let changed = self.repo.changed_entries(&ctx.base_commit, &snapshot)?;
        let symlink = git::find_worktree_symlink(&ctx.worktree, &changed)?;

        let verdict = evaluate(&AttemptFacts {
            timed_out: run.timed_out,
            exit_code: run.exit_code,
            handoff: handoff.as_ref(),
            handoff_error: handoff_error.as_deref(),
            changed: &changed,
            matcher: &matcher,
            symlink: symlink.as_deref(),
        });

        self.db.insert_attempt(
            &ctx.task_id,
            ctx.attempt_num,
            &ctx.base_commit,
            &ctx.spec_digest,
            &ctx.plan_digest,
            verdict.as_str(),
            &ctx.log_path.to_string_lossy(),
        )?;

        Ok(AttemptOutcome {
            verdict,
            exit_code: run.exit_code,
            snapshot_commit: Some(snapshot),
            log_path: ctx.log_path.clone(),
        })
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
