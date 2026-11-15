//! 结构化交接单（`.orgy/outbox/<task>.a<n>.handoff.json`）的解析与自身一致性校验。
//!
//! 交接单是 Worker 的**声明**，不是事实：文件是否真的改了、改得是否合规，
//! 仍由 Supervisor 用 Git 差异与验证器判定（M0.md §5.3）。

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Completed,
    Blocked,
    NeedsClarification,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Completed => "COMPLETED",
            Status::Blocked => "BLOCKED",
            Status::NeedsClarification => "NEEDS_CLARIFICATION",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub proposed_default: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    pub task_id: String,
    pub attempt: u32,
    pub status: Status,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub questions: Vec<Question>,
}

impl Handoff {
    /// 解析并校验交接单的**身份**与**跨字段一致性**。
    ///
    /// 这里只做"自洽性"检查：身份对不上、状态与字段矛盾、声明与请求不符，
    /// 都属于协议异常，直接拒绝；不在这里判断改动是否合规（那是写集校验的事）。
    pub fn parse(text: &str, task_id: &str, attempt: u32) -> Result<Self> {
        let handoff: Handoff = serde_json::from_str(text)
            .map_err(|e| Error::Config(format!("交接单不是合法 JSON：{e}")))?;

        if handoff.task_id != task_id {
            return Err(Error::Config(format!(
                "交接单的 task_id 是 {}，期望 {task_id}",
                handoff.task_id
            )));
        }
        if handoff.attempt != attempt {
            return Err(Error::Config(format!(
                "交接单的 attempt 是 {}，期望 {attempt}",
                handoff.attempt
            )));
        }

        match handoff.status {
            Status::Completed => {
                if !handoff.questions.is_empty() {
                    return Err(Error::Config(
                        "status=COMPLETED 时 questions 必须为空".into(),
                    ));
                }
            }
            Status::NeedsClarification => {
                if handoff.questions.is_empty() {
                    return Err(Error::Config(
                        "status=NEEDS_CLARIFICATION 时 questions 不能为空".into(),
                    ));
                }
            }
            Status::Blocked => {}
        }

        Ok(handoff)
    }

    /// 交付物自报的改动集合（已归一化、去重、排序）。
    pub fn claimed_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .files_changed
            .iter()
            .filter_map(|p| crate::git::normalize_rel_path(p))
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(status: &str, questions: &str) -> String {
        format!(
            r#"{{"task_id":"T-1","attempt":1,"status":"{status}","files_changed":["src/uart.c"],"questions":{questions}}}"#
        )
    }

    #[test]
    fn 合法交接单被接受() {
        let h = Handoff::parse(&json("COMPLETED", "[]"), "T-1", 1).unwrap();
        assert_eq!(h.status, Status::Completed);
        assert_eq!(h.claimed_paths(), vec!["src/uart.c"]);
    }

    #[test]
    fn 身份不符被拒绝() {
        let text = json("COMPLETED", "[]");
        assert!(Handoff::parse(&text, "T-2", 1).is_err());
        assert!(Handoff::parse(&text, "T-1", 2).is_err());
    }

    #[test]
    fn 完成时不得带问题() {
        let text = json("COMPLETED", r#"[{"id":"Q1","text":"?"}]"#);
        assert!(Handoff::parse(&text, "T-1", 1).is_err());
    }

    #[test]
    fn 需要澄清时必须有问题() {
        let text = json("NEEDS_CLARIFICATION", "[]");
        assert!(Handoff::parse(&text, "T-1", 1).is_err());
    }

    #[test]
    fn 非法状态值被拒绝() {
        let text = json("DONE", "[]");
        assert!(Handoff::parse(&text, "T-1", 1).is_err());
    }

    #[test]
    fn 未知字段被拒绝() {
        let text = r#"{"task_id":"T-1","attempt":1,"status":"COMPLETED","verified":true}"#;
        assert!(Handoff::parse(text, "T-1", 1).is_err());
    }
}
