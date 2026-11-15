//! SQLite 运行态（M0.md §4.2）：3 张表 + WAL + 外键。
//!
//! M0 只把"任务/尝试/收据"三类事实落库，不建 operations 意图对账表，
//! 跨存储一致性问题在 §5.1 的启动对账里单独处理。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    task_id          TEXT PRIMARY KEY,
    status           TEXT NOT NULL,
    spec_digest      TEXT NOT NULL,
    plan_digest      TEXT NOT NULL,
    accepted_commit  TEXT,
    updated_at       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attempts (
    attempt_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id          TEXT NOT NULL REFERENCES tasks(task_id),
    attempt_num      INTEGER NOT NULL,
    base_commit      TEXT NOT NULL,
    start_spec_digest TEXT NOT NULL,
    start_plan_digest TEXT NOT NULL,
    snapshot_commit  TEXT,
    exit_code        INTEGER,
    l2_status        TEXT,
    verdict          TEXT NOT NULL,
    error_detail     TEXT,
    log_path         TEXT NOT NULL,
    created_at       INTEGER NOT NULL,
    UNIQUE(task_id, attempt_num)
);

CREATE TABLE IF NOT EXISTS receipts (
    receipt_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id          TEXT NOT NULL REFERENCES tasks(task_id),
    parent_commit    TEXT NOT NULL,
    candidate_commit TEXT NOT NULL,
    spec_digest      TEXT NOT NULL,
    plan_digest      TEXT NOT NULL,
    passed           INTEGER NOT NULL,
    created_at       INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_receipts_task_time
    ON receipts(task_id, created_at DESC);
"#;

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub task_id: String,
    pub status: String,
    pub spec_digest: String,
    pub plan_digest: String,
    pub accepted_commit: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct ReceiptRow {
    pub receipt_id: i64,
    pub task_id: String,
    pub parent_commit: String,
    pub candidate_commit: String,
    pub spec_digest: String,
    pub plan_digest: String,
    pub passed: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct NewReceipt<'a> {
    pub task_id: &'a str,
    pub parent_commit: &'a str,
    pub candidate_commit: &'a str,
    pub spec_digest: &'a str,
    pub plan_digest: &'a str,
    pub passed: bool,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::configure(&conn)?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// WAL + FULL 同步 + 外键。外键是**连接级**设置，必须在每个连接上开启。
    fn configure(conn: &Connection) -> Result<()> {
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL;", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(crate::error::Error::Other(format!(
                "无法启用 WAL（当前模式：{mode}）"
            )));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA)?;
        Ok(())
    }

    pub fn upsert_task(
        &self,
        task_id: &str,
        status: &str,
        spec_digest: &str,
        plan_digest: &str,
        accepted_commit: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO tasks (task_id, status, spec_digest, plan_digest, accepted_commit, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(task_id) DO UPDATE SET
                status = excluded.status,
                spec_digest = excluded.spec_digest,
                plan_digest = excluded.plan_digest,
                accepted_commit = COALESCE(excluded.accepted_commit, tasks.accepted_commit),
                updated_at = excluded.updated_at
            "#,
            params![
                task_id,
                status,
                spec_digest,
                plan_digest,
                accepted_commit,
                now()
            ],
        )?;
        Ok(())
    }

    pub fn set_status(&self, task_id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET status = ?2, updated_at = ?3 WHERE task_id = ?1",
            params![task_id, status, now()],
        )?;
        Ok(())
    }

    pub fn get_task(&self, task_id: &str) -> Result<Option<TaskRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT task_id, status, spec_digest, plan_digest, accepted_commit, updated_at
                 FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| {
                    Ok(TaskRow {
                        task_id: r.get(0)?,
                        status: r.get(1)?,
                        spec_digest: r.get(2)?,
                        plan_digest: r.get(3)?,
                        accepted_commit: r.get(4)?,
                        updated_at: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_tasks(&self) -> Result<Vec<TaskRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, status, spec_digest, plan_digest, accepted_commit, updated_at
             FROM tasks ORDER BY task_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(TaskRow {
                task_id: r.get(0)?,
                status: r.get(1)?,
                spec_digest: r.get(2)?,
                plan_digest: r.get(3)?,
                accepted_commit: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_attempt(
        &self,
        task_id: &str,
        attempt_num: u32,
        base_commit: &str,
        start_spec_digest: &str,
        start_plan_digest: &str,
        verdict: &str,
        log_path: &str,
    ) -> Result<i64> {
        self.conn.execute(
            r#"
            INSERT INTO attempts (
                task_id, attempt_num, base_commit, start_spec_digest,
                start_plan_digest, verdict, log_path, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                task_id,
                attempt_num,
                base_commit,
                start_spec_digest,
                start_plan_digest,
                verdict,
                log_path,
                now()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn insert_receipt(&self, receipt: &NewReceipt<'_>) -> Result<i64> {
        self.conn.execute(
            r#"
            INSERT INTO receipts (task_id, parent_commit, candidate_commit,
                                  spec_digest, plan_digest, passed, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                receipt.task_id,
                receipt.parent_commit,
                receipt.candidate_commit,
                receipt.spec_digest,
                receipt.plan_digest,
                i64::from(receipt.passed),
                now()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 该任务下一个尝试序号（从 1 开始，不复用已用过的序号）。
    pub fn next_attempt_num(&self, task_id: &str) -> Result<u32> {
        let max: Option<i64> = self.conn.query_row(
            "SELECT MAX(attempt_num) FROM attempts WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        )?;
        Ok(max.unwrap_or(0) as u32 + 1)
    }

    pub fn latest_passed_receipt(&self, task_id: &str) -> Result<Option<ReceiptRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT receipt_id, task_id, parent_commit, candidate_commit,
                        spec_digest, plan_digest, passed, created_at
                 FROM receipts
                 WHERE task_id = ?1 AND passed = 1
                 ORDER BY receipt_id DESC LIMIT 1",
                params![task_id],
                |r| {
                    Ok(ReceiptRow {
                        receipt_id: r.get(0)?,
                        task_id: r.get(1)?,
                        parent_commit: r.get(2)?,
                        candidate_commit: r.get(3)?,
                        spec_digest: r.get(4)?,
                        plan_digest: r.get(5)?,
                        passed: r.get::<_, i64>(6)? == 1,
                        created_at: r.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// 最近一条收据（无论通过与否），供 §5.1 的跨存储对账使用。
    pub fn latest_receipt(&self) -> Result<Option<ReceiptRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT receipt_id, task_id, parent_commit, candidate_commit,
                        spec_digest, plan_digest, passed, created_at
                 FROM receipts ORDER BY receipt_id DESC LIMIT 1",
                [],
                |r| {
                    Ok(ReceiptRow {
                        receipt_id: r.get(0)?,
                        task_id: r.get(1)?,
                        parent_commit: r.get(2)?,
                        candidate_commit: r.get(3)?,
                        spec_digest: r.get(4)?,
                        plan_digest: r.get(5)?,
                        passed: r.get::<_, i64>(6)? == 1,
                        created_at: r.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 建表与外键生效() {
        let db = Db::open_memory().unwrap();
        db.upsert_task("T-1", "PENDING", "s", "p", None).unwrap();
        assert_eq!(db.get_task("T-1").unwrap().unwrap().status, "PENDING");
        // 孤儿 attempt 必须被外键拒绝
        let err = db
            .insert_attempt("T-9", 1, "c0", "s", "p", "PASSED", "log")
            .unwrap_err();
        assert!(matches!(err, crate::error::Error::Db(_)));
    }

    #[test]
    fn 尝试序号唯一() {
        let db = Db::open_memory().unwrap();
        db.upsert_task("T-1", "PENDING", "s", "p", None).unwrap();
        db.insert_attempt("T-1", 1, "c0", "s", "p", "PASSED", "log")
            .unwrap();
        assert!(db
            .insert_attempt("T-1", 1, "c0", "s", "p", "PASSED", "log")
            .is_err());
    }

    #[test]
    fn 只取最新通过收据() {
        let db = Db::open_memory().unwrap();
        db.upsert_task("T-1", "PENDING", "s", "p", None).unwrap();
        db.insert_receipt(&NewReceipt {
            task_id: "T-1",
            parent_commit: "c0",
            candidate_commit: "c1",
            spec_digest: "s1",
            plan_digest: "p1",
            passed: true,
        })
        .unwrap();
        db.insert_receipt(&NewReceipt {
            task_id: "T-1",
            parent_commit: "c1",
            candidate_commit: "c2",
            spec_digest: "s1",
            plan_digest: "p1",
            passed: false,
        })
        .unwrap();
        let latest = db.latest_passed_receipt("T-1").unwrap().unwrap();
        assert_eq!(latest.candidate_commit, "c1");
    }
}
