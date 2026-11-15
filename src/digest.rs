//! 语义投影与 3 个核心指纹（M0.md §4.1）。
//!
//! 三个指纹的分工：
//! - `spec_digest`：任务自身语义 + `inputs` 声明条目的语义投影；
//! - `plan_digest` ：验证计划的语义内容；
//! - `candidate_commit`：由 Git 保证的代码快照标识。
//!
//! 关键约束：`note` / `title` 等说明性字段一律**不进入**投影，替换排版或补充解释
//! 不得让任务失效。为此这里不使用 `serde` 的直接序列化，而是显式构造规范结构，
//! 以便每一处进/出投影的字段都可见、可审计。

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::config::{Entry, Plan, Scalar, Task};
use crate::error::Result;

pub const SPEC_SCHEMA_VERSION: &str = "orgy-spec-v1";
pub const PLAN_SCHEMA_VERSION: &str = "orgy-plan-v1";

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// 计算 `spec_digest`。
///
/// `inputs` 不存在的条目由 `config::Spec::validate` 提前拦下，这里再兜一层，
/// 避免"少算一个条目"这种静默失效。
pub fn spec_digest(task: &Task, entries: &BTreeMap<String, Entry>) -> Result<String> {
    // 集合语义：`inputs` 与 `writes` 的顺序不承载含义，排序去重后再进投影，
    // 这样"调整 YAML 行序"不会造成无谓失效。
    let inputs: BTreeSet<&str> = task.inputs.iter().map(|s| s.as_str()).collect();
    let writes: BTreeSet<&str> = task.writes.iter().map(|s| s.as_str()).collect();

    let mut task_projection: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    task_projection.insert("id", serde_json::json!(task.id));
    task_projection.insert("objective", serde_json::json!(task.objective));
    task_projection.insert(
        "inputs",
        serde_json::json!(inputs.iter().copied().collect::<Vec<_>>()),
    );
    task_projection.insert(
        "writes",
        serde_json::json!(writes.iter().copied().collect::<Vec<_>>()),
    );
    task_projection.insert(
        "verification_plan",
        serde_json::json!(task.verification_plan),
    );

    let mut entry_projection: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    for id in &inputs {
        let entry = entries.get(*id).ok_or_else(|| {
            crate::error::Error::Config(format!("spec_digest 引用了不存在的条目 {id}"))
        })?;
        entry_projection.insert(id, project_entry(entry));
    }

    let mut root: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    root.insert("version", serde_json::json!(SPEC_SCHEMA_VERSION));
    root.insert(
        "task",
        serde_json::Value::Object(to_json_map(task_projection)),
    );
    root.insert(
        "entries",
        serde_json::Value::Object(to_json_map(entry_projection)),
    );

    Ok(sha256_hex(&canonical_bytes(&root)?))
}

/// 条目的语义投影。说明性字段（`note`）在此处被显式丢弃。
fn project_entry(entry: &Entry) -> serde_json::Value {
    match entry {
        Entry::Assumption {
            value_type, value, ..
        } => serde_json::json!({
            "kind": "assumption",
            "value_type": value_type.as_str(),
            "value": project_scalar(value),
        }),
        Entry::Interface {
            sig, invariants, ..
        } => {
            // 不变量是集合，排序后再进投影；顺序本身不承载语义。
            let sorted: BTreeSet<&String> = invariants.iter().collect();
            serde_json::json!({
                "kind": "interface",
                "sig": sig,
                "invariants": sorted.into_iter().collect::<Vec<_>>(),
            })
        }
    }
}

fn project_scalar(scalar: &Scalar) -> serde_json::Value {
    match scalar {
        Scalar::Int(v) => serde_json::json!(v),
        Scalar::Bool(v) => serde_json::json!(v),
        Scalar::Str(v) => serde_json::json!(v),
    }
}

/// 计算 `plan_digest`。步骤顺序承载语义（命令有先后），因此保持声明顺序。
pub fn plan_digest(plan: &Plan) -> Result<String> {
    let resources: BTreeSet<&str> = plan.resources.iter().map(|s| s.as_str()).collect();

    let steps: Vec<serde_json::Value> = plan
        .steps
        .iter()
        .map(|step| {
            serde_json::json!({
                "name": step.name,
                "argv": step.argv,
                "timeout_sec": step.timeout_sec,
                "requires_human_confirm": step.requires_human_confirm,
            })
        })
        .collect();

    let mut root: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    root.insert("version", serde_json::json!(PLAN_SCHEMA_VERSION));
    root.insert(
        "resources",
        serde_json::json!(resources.iter().copied().collect::<Vec<_>>()),
    );
    root.insert("steps", serde_json::json!(steps));

    Ok(sha256_hex(&canonical_bytes(&root)?))
}

/// `BTreeMap<&str, Value>` → `serde_json::Map`（键需要拥有所有权）。
fn to_json_map(map: BTreeMap<&str, serde_json::Value>) -> serde_json::Map<String, serde_json::Value> {
    map.into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// 规范化序列化：`BTreeMap` 保证键按 UTF-8 字典序输出，且不含多余空白。
fn canonical_bytes(value: &BTreeMap<&str, serde_json::Value>) -> Result<Vec<u8>> {
    Ok(serde_json::to_string(value)?.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ValueType;

    fn task() -> Task {
        Task {
            id: "T-1".into(),
            title: Some("标题".into()),
            objective: "实现环形缓冲区".into(),
            cli_profile: "codex".into(),
            max_attempts: 3,
            timeout_sec: 600,
            inputs: vec!["A-1".into(), "IF-1".into()],
            writes: vec!["src/uart.c".into(), "include/uart.h".into()],
            verification_plan: "uart-unit-test".into(),
            note: Some("说明".into()),
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
                invariants: vec!["非阻塞".into(), "返回字节数".into()],
                note: Some("驱动接口".into()),
            },
        );
        map
    }

    #[test]
    fn 修改说明字段不改变指纹() {
        let t = task();
        let mut e = entries();
        let before = spec_digest(&t, &e).unwrap();

        if let Some(Entry::Assumption { note, .. }) = e.get_mut("A-1") {
            *note = Some("换一段说明".into());
        }
        let mut t2 = t.clone();
        t2.note = Some("另一段说明".into());
        t2.title = Some("改过的标题".into());

        assert_eq!(before, spec_digest(&t2, &e).unwrap());
    }

    #[test]
    fn 修改语义值会改变指纹() {
        let t = task();
        let mut e = entries();
        let before = spec_digest(&t, &e).unwrap();
        if let Some(Entry::Assumption { value, .. }) = e.get_mut("A-1") {
            *value = Scalar::Int(921600);
        }
        assert_ne!(before, spec_digest(&t, &e).unwrap());
    }

    #[test]
    fn 整数与字符串值不互通() {
        let t = task();
        let mut e = entries();
        let int_digest = spec_digest(&t, &e).unwrap();
        if let Some(Entry::Assumption {
            value, value_type, ..
        }) = e.get_mut("A-1")
        {
            *value = Scalar::Str("115200".into());
            *value_type = ValueType::String;
        }
        assert_ne!(int_digest, spec_digest(&t, &e).unwrap());
    }

    #[test]
    fn 集合顺序不影响指纹() {
        let mut t = task();
        let mut e = entries();
        let before = spec_digest(&t, &e).unwrap();

        t.inputs.reverse();
        t.writes.reverse();
        if let Some(Entry::Interface { invariants, .. }) = e.get_mut("IF-1") {
            invariants.reverse();
        }
        assert_eq!(before, spec_digest(&t, &e).unwrap());
    }

    #[test]
    fn 未声明的条目不影响指纹() {
        let t = task();
        let mut e = entries();
        let before = spec_digest(&t, &e).unwrap();
        e.insert(
            "A-9".into(),
            Entry::Assumption {
                value_type: ValueType::Integer,
                value: Scalar::Int(1),
                note: None,
            },
        );
        assert_eq!(before, spec_digest(&t, &e).unwrap());
    }

    #[test]
    fn 同一输入产生稳定十六进制() {
        let t = task();
        let d = spec_digest(&t, &entries()).unwrap();
        assert_eq!(d.len(), 64);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(d, spec_digest(&t, &entries()).unwrap());
    }

    #[test]
    fn 计划步骤顺序承载语义() {
        use crate::config::Step;
        let mut plan = Plan {
            resources: vec!["port:8080".into(), "hw:board-1".into()],
            steps: vec![
                Step {
                    name: "build".into(),
                    argv: vec!["cargo".into(), "build".into()],
                    timeout_sec: 120,
                    requires_human_confirm: false,
                },
                Step {
                    name: "test".into(),
                    argv: vec!["cargo".into(), "test".into()],
                    timeout_sec: 120,
                    requires_human_confirm: false,
                },
            ],
        };
        let before = plan_digest(&plan).unwrap();
        plan.steps.reverse();
        assert_ne!(before, plan_digest(&plan).unwrap());
    }
}
