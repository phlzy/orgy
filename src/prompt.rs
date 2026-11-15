//! Prompt 组装（M0.md §5.2 步骤 3 的空白处）。
//!
//! 原则：只给 Worker 完成**这一个切片**所需要的东西——目标、可写范围、声明过的
//! 契约条目、以及交付协议。不给全量规范、不给仓库树、不给历史尝试的完整日志。

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::config::{Entry, Scalar, Task};
use crate::supervisor::AttemptContext;

/// 组装一次 Attempt 的提示词。
pub fn build(
    task: &Task,
    entries: &BTreeMap<String, Entry>,
    ctx: &AttemptContext,
    handoff_path: &Path,
) -> String {
    let mut out = String::with_capacity(2048);

    let _ = writeln!(out, "# 任务\n");
    let _ = writeln!(out, "{}\n", task.objective);
    let _ = writeln!(
        out,
        "任务 ID：{}；这是第 {} 次尝试。\n",
        ctx.task_id, ctx.attempt_num
    );

    let _ = writeln!(out, "# 可写范围\n");
    let _ = writeln!(
        out,
        "只能新增、修改或删除下列路径。其它任何改动（包括 `.orgy/` 下的文件，\
         交付文件除外）都会被判为越界并丢弃本次成果：\n"
    );
    for pattern in &task.writes {
        let _ = writeln!(out, "- `{pattern}`");
    }
    let _ = writeln!(out);

    if !task.inputs.is_empty() {
        let _ = writeln!(out, "# 输入契约\n");
        for id in &task.inputs {
            if let Some(entry) = entries.get(id) {
                let _ = writeln!(out, "{}\n", render_entry(id, entry));
            }
        }
    }

    let _ = writeln!(out, "# 交付\n");
    let _ = writeln!(
        out,
        "无论成功还是受阻，结束前都必须把下面这个 JSON 写入：\n\n`{}`\n",
        handoff_path.display()
    );
    let _ = writeln!(out, "```json\n{}\n```\n", handoff_schema(&ctx.task_id, ctx.attempt_num));

    let _ = writeln!(out, "# 规则\n");
    let _ = writeln!(out, "- `status` 只能取 `COMPLETED` / `BLOCKED` / `NEEDS_CLARIFICATION`。");
    let _ = writeln!(
        out,
        "- `COMPLETED` 时 `questions` 必须是空数组；`NEEDS_CLARIFICATION` 时必须有内容。"
    );
    let _ = writeln!(
        out,
        "- `files_changed` 必须与实际改动完全一致（漏报或虚报都会被判为声明不符）。"
    );
    let _ = writeln!(out, "- 不要执行 `git commit`、`git push` 或任何网络发布动作。");
    let _ = writeln!(
        out,
        "- 不要修改 `.orgy/` 下除上述交付文件以外的任何内容，也不要伪造交付文件。"
    );
    let _ = writeln!(
        out,
        "- 需求不清时优先按已有假设推进；只有真正阻塞时才用 `NEEDS_CLARIFICATION`，\
         并把问题一次性列全。"
    );

    out
}

fn render_entry(id: &str, entry: &Entry) -> String {
    match entry {
        Entry::Assumption {
            value_type,
            value,
            note,
        } => {
            let mut text = format!(
                "- `{id}`（假设）：`{}` = `{}`",
                value_type.as_str(),
                render_scalar(value)
            );
            if let Some(note) = note {
                let _ = write!(text, "；说明：{note}");
            }
            text
        }
        Entry::Interface {
            sig,
            invariants,
            note,
        } => {
            let mut text = format!("- `{id}`（接口）：`{sig}`");
            for inv in invariants {
                let _ = write!(text, "\n  - 约束：{inv}");
            }
            if let Some(note) = note {
                let _ = write!(text, "\n  - 说明：{note}");
            }
            text
        }
    }
}

fn render_scalar(scalar: &Scalar) -> String {
    match scalar {
        Scalar::Int(v) => v.to_string(),
        Scalar::Bool(v) => v.to_string(),
        Scalar::Str(v) => format!("\"{v}\""),
    }
}

fn handoff_schema(task_id: &str, attempt: u32) -> String {
    format!(
        r#"{{
  "task_id": "{task_id}",
  "attempt": {attempt},
  "status": "COMPLETED | BLOCKED | NEEDS_CLARIFICATION",
  "summary": "不超过 400 字符",
  "files_changed": ["相对路径"],
  "questions": [{{"id": "Q1", "text": "问题", "proposed_default": "建议默认值"}}]
}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ValueType;
    use std::path::PathBuf;

    fn task() -> Task {
        Task {
            id: "T-1".into(),
            title: None,
            objective: "实现 UART 环形缓冲区读取".into(),
            cli_profile: "dsh-headless".into(),
            max_attempts: 3,
            timeout_sec: 600,
            inputs: vec!["A-1".into(), "IF-1".into()],
            writes: vec!["src/uart.c".into(), "include/uart.h".into()],
            verification_plan: "uart-unit-test".into(),
            note: None,
        }
    }

    fn entries() -> BTreeMap<String, Entry> {
        let mut map = BTreeMap::new();
        map.insert(
            "A-1".into(),
            Entry::Assumption {
                value_type: ValueType::Integer,
                value: Scalar::Int(115200),
                note: Some("波特率".into()),
            },
        );
        map.insert(
            "IF-1".into(),
            Entry::Interface {
                sig: "int uart_read(uint8_t*, size_t)".into(),
                invariants: vec!["非阻塞".into()],
                note: None,
            },
        );
        map
    }

    fn ctx() -> AttemptContext {
        AttemptContext {
            task_id: "T-1".into(),
            attempt_num: 1,
            base_commit: "abc".into(),
            worktree: PathBuf::from(r"C:\repo\.orgy\runtime\worktrees\T-1"),
            spec_digest: "s".into(),
            plan_digest: "p".into(),
            log_path: PathBuf::from(r"C:\repo\.orgy\runtime\logs\T-1.a1.log"),
        }
    }

    #[test]
    fn 包含目标_范围_契约与交付路径() {
        let handoff = Path::new(r"C:\repo\.orgy\runtime\worktrees\T-1\.orgy\outbox\T-1.a1.handoff.json");
        let text = build(&task(), &entries(), &ctx(), handoff);
        assert!(text.contains("实现 UART 环形缓冲区读取"));
        assert!(text.contains("src/uart.c"));
        assert!(text.contains("115200"));
        assert!(text.contains("非阻塞"));
        assert!(text.contains(handoff.to_string_lossy().as_ref()));
        assert!(text.contains("COMPLETED"));
    }

    #[test]
    fn 不包含未声明的条目() {
        let mut map = entries();
        map.insert(
            "A-9".into(),
            Entry::Assumption {
                value_type: ValueType::Integer,
                value: Scalar::Int(42),
                note: Some("无关假设".into()),
            },
        );
        let text = build(&task(), &map, &ctx(), Path::new("h.json"));
        assert!(!text.contains("无关假设"), "未声明的条目不得进入 prompt");
    }

    #[test]
    fn 长度有界() {
        let text = build(&task(), &entries(), &ctx(), Path::new("h.json"));
        assert!(text.len() < 6000, "prompt 过长：{} 字节", text.len());
    }
}
