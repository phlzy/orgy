//! `.orgy/spec/` 下三个 YAML 文件的解析与跨字段校验。
//!
//! 对应 M0.md §3：M0 直接读纯 YAML，不做 Markdown 围栏解析。
//! 校验失败一律报错退出（M0 不实现"宽松容错 + 警告"这一层）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::git::is_safe_rel_path;

// ---------------------------------------------------------------------------
// 条目：假设与接口
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueType {
    Integer,
    String,
    Boolean,
}

impl ValueType {
    fn matches(self, scalar: &Scalar) -> bool {
        matches!(
            (self, scalar),
            (ValueType::Integer, Scalar::Int(_))
                | (ValueType::String, Scalar::Str(_))
                | (ValueType::Boolean, Scalar::Bool(_))
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ValueType::Integer => "integer",
            ValueType::String => "string",
            ValueType::Boolean => "boolean",
        }
    }
}

/// 标量值。
///
/// 变体顺序即 serde untagged 的尝试顺序，必须保持 整数 → 布尔 → 字符串，
/// 否则 YAML 里的 `115200` 会被吞成字符串，破坏 M0.md §4.1 的类型严格性要求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Int(i64),
    Bool(bool),
    Str(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    Assumption {
        value_type: ValueType,
        value: Scalar,
        /// 仅作说明，不参与 spec_digest。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Interface {
        sig: String,
        #[serde(default)]
        invariants: Vec<String>,
        /// 仅作说明，不参与 spec_digest。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

impl Entry {
    pub fn note(&self) -> Option<&str> {
        match self {
            Entry::Assumption { note, .. } | Entry::Interface { note, .. } => note.as_deref(),
        }
    }

    /// 条目 ID 的前缀必须与 kind 一致：`A-*` 是假设，`IF-*` 是接口。
    fn expected_prefix(&self) -> &'static str {
        match self {
            Entry::Assumption { .. } => "A-",
            Entry::Interface { .. } => "IF-",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractsFile {
    pub entries: BTreeMap<String, Entry>,
}

// ---------------------------------------------------------------------------
// 任务定义
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    /// 仅供展示，不参与 spec_digest。
    #[serde(default)]
    pub title: Option<String>,
    pub objective: String,
    #[serde(default = "default_cli_profile")]
    pub cli_profile: String,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_timeout_sec")]
    pub timeout_sec: u64,
    /// 只有列在这里的条目才进入 spec_digest（M0.md §1「增量失效」）。
    #[serde(default)]
    pub inputs: Vec<String>,
    /// 允许修改的仓库相对路径，支持 `src/uart/**` 这类组件级 glob。
    #[serde(default)]
    pub writes: Vec<String>,
    pub verification_plan: String,
    /// 仅作说明，不参与 spec_digest。
    #[serde(default)]
    pub note: Option<String>,
}

fn default_cli_profile() -> String {
    "codex".to_string()
}

fn default_max_attempts() -> u32 {
    3
}

fn default_timeout_sec() -> u64 {
    600
}

// ---------------------------------------------------------------------------
// 验证计划
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationFile {
    pub plans: BTreeMap<String, Plan>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// 本机独占资源，如 `hw:board-1`。
    #[serde(default)]
    pub resources: Vec<String>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    /// 参数数组，禁止拼 shell 字符串（M0.md §6.3）。
    pub argv: Vec<String>,
    #[serde(default = "default_step_timeout_sec")]
    pub timeout_sec: u64,
    #[serde(default)]
    pub requires_human_confirm: bool,
}

fn default_step_timeout_sec() -> u64 {
    120
}

// ---------------------------------------------------------------------------
// 聚合视图
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Spec {
    pub entries: BTreeMap<String, Entry>,
    pub plans: BTreeMap<String, Plan>,
    pub tasks: BTreeMap<String, Task>,
}

impl Spec {
    /// 从仓库根目录加载 `.orgy/spec/`。
    pub fn load(repo_root: &Path) -> Result<Self> {
        let spec_dir = repo_root.join(".orgy").join("spec");
        let contracts_path = spec_dir.join("contracts.yaml");
        let verification_path = spec_dir.join("verification.yaml");
        let tasks_dir = spec_dir.join("tasks");

        if !contracts_path.is_file() {
            return Err(Error::Config(format!(
                "缺少 {}，请先编写契约与假设文件",
                contracts_path.display()
            )));
        }
        if !verification_path.is_file() {
            return Err(Error::Config(format!(
                "缺少 {}，请先编写验证计划",
                verification_path.display()
            )));
        }

        let contracts: ContractsFile = read_yaml(&contracts_path)?;
        let verification: VerificationFile = read_yaml(&verification_path)?;

        let mut tasks = BTreeMap::new();
        if tasks_dir.is_dir() {
            let mut files: Vec<_> = std::fs::read_dir(&tasks_dir)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("yaml"))
                .collect();
            files.sort();
            for path in files {
                let task: Task = read_yaml(&path)?;
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                if stem != task.id {
                    return Err(Error::Config(format!(
                        "{} 内的 id={} 与文件名不一致",
                        path.display(),
                        task.id
                    )));
                }
                if tasks.insert(task.id.clone(), task).is_some() {
                    return Err(Error::Config(format!("任务 ID 重复：{stem}")));
                }
            }
        }

        let spec = Spec {
            entries: contracts.entries,
            plans: verification.plans,
            tasks,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn task(&self, id: &str) -> Result<&Task> {
        self.tasks
            .get(id)
            .ok_or_else(|| Error::Config(format!("任务 {id} 不存在")))
    }

    pub fn plan(&self, name: &str) -> Result<&Plan> {
        self.plans
            .get(name)
            .ok_or_else(|| Error::Config(format!("验证计划 {name} 不存在")))
    }

    fn validate(&self) -> Result<()> {
        for (id, entry) in &self.entries {
            if !id.starts_with(entry.expected_prefix()) {
                return Err(Error::Config(format!(
                    "条目 {id} 的 ID 前缀与 kind 不匹配（期望以 {} 开头）",
                    entry.expected_prefix()
                )));
            }
            match entry {
                Entry::Assumption {
                    value_type, value, ..
                } => {
                    if !value_type.matches(value) {
                        return Err(Error::Config(format!(
                            "假设 {id} 的 value 与 value_type={} 不符（M0 不做隐式类型转换）",
                            value_type.as_str()
                        )));
                    }
                }
                Entry::Interface { sig, .. } => {
                    if sig.trim().is_empty() {
                        return Err(Error::Config(format!("接口 {id} 的 sig 不能为空")));
                    }
                }
            }
        }

        for (name, plan) in &self.plans {
            if plan.steps.is_empty() {
                return Err(Error::Config(format!("验证计划 {name} 没有任何检查步骤")));
            }
            for step in &plan.steps {
                if step.name.trim().is_empty() {
                    return Err(Error::Config(format!("验证计划 {name} 存在无名步骤")));
                }
                if step.argv.is_empty() {
                    return Err(Error::Config(format!(
                        "验证计划 {name} 的步骤 {} 缺少 argv",
                        step.name
                    )));
                }
                if step.timeout_sec == 0 {
                    return Err(Error::Config(format!(
                        "验证计划 {name} 的步骤 {} 超时必须大于 0",
                        step.name
                    )));
                }
            }
        }

        for (id, task) in &self.tasks {
            if !id.starts_with("T-") {
                return Err(Error::Config(format!("任务 ID {id} 必须以 T- 开头")));
            }
            if task.max_attempts == 0 {
                return Err(Error::Config(format!("任务 {id} 的 max_attempts 必须大于 0")));
            }
            if task.timeout_sec == 0 {
                return Err(Error::Config(format!("任务 {id} 的 timeout_sec 必须大于 0")));
            }
            if task.objective.trim().is_empty() {
                return Err(Error::Config(format!("任务 {id} 缺少 objective")));
            }
            for input in &task.inputs {
                if !self.entries.contains_key(input) {
                    return Err(Error::Config(format!(
                        "任务 {id} 的 inputs 引用了不存在的条目 {input}"
                    )));
                }
            }
            if task.writes.is_empty() {
                return Err(Error::Config(format!("任务 {id} 的 writes 不能为空")));
            }
            for pattern in &task.writes {
                validate_write_pattern(pattern)
                    .map_err(|e| Error::Config(format!("任务 {id} 的 writes 中 {pattern} 不合法：{e}")))?;
            }
            if !self.plans.contains_key(&task.verification_plan) {
                return Err(Error::Config(format!(
                    "任务 {id} 引用了不存在的验证计划 {}",
                    task.verification_plan
                )));
            }
        }

        Ok(())
    }
}

/// writes 里的模式本身也必须是安全的相对路径（可含 glob）。
///
/// 注意：这里只做"是否安全"的语法校验，真正的匹配在 `git::WritesMatcher`。
fn validate_write_pattern(pattern: &str) -> Result<()> {
    if pattern.trim().is_empty() {
        return Err(Error::Other("空模式".into()));
    }
    // glob 通配符不是合法路径字符，校验前先剥离，只检查路径骨架。
    let skeleton: String = pattern
        .chars()
        .filter(|c| !matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | ','))
        .collect();
    if skeleton.contains("//") {
        return Err(Error::Other("包含空的路径段".into()));
    }
    if is_safe_rel_path(&skeleton) {
        Ok(())
    } else {
        Err(Error::Other("必须是仓库内相对路径，且不得越界或指向 .orgy/".into()))
    }
}

fn read_yaml<T: serde::de::DeserializeOwned + 'static>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)?;
    serde_yml::from_str(&text).map_err(Error::Yaml)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_entries(yaml: &str) -> Result<BTreeMap<String, Entry>> {
        let file: ContractsFile = serde_yml::from_str(yaml)?;
        Ok(file.entries)
    }

    #[test]
    fn 整数与字符串保持区分() {
        let entries = parse_entries(
            r#"
entries:
  A-1: { kind: assumption, value_type: integer, value: 115200 }
  A-2: { kind: assumption, value_type: string, value: "115200" }
"#,
        )
        .unwrap();
        assert_eq!(
            entries.get("A-1").map(|e| matches!(e, Entry::Assumption { value: Scalar::Int(115200), .. })),
            Some(true)
        );
        assert_eq!(
            entries.get("A-2").map(|e| matches!(e, Entry::Assumption { value: Scalar::Str(_), .. })),
            Some(true)
        );
    }

    #[test]
    fn 类型不匹配会被拒绝() {
        let entries = parse_entries(
            r#"
entries:
  A-1: { kind: assumption, value_type: integer, value: "115200" }
"#,
        )
        .unwrap();
        let spec = Spec {
            entries,
            plans: BTreeMap::new(),
            tasks: BTreeMap::new(),
        };
        let err = spec.validate().unwrap_err().to_string();
        assert!(err.contains("value_type=integer"), "实际错误：{err}");
    }

    #[test]
    fn 条目前缀必须与_kind_一致() {
        let entries = parse_entries(
            r#"
entries:
  IF-9: { kind: assumption, value_type: integer, value: 1 }
"#,
        )
        .unwrap();
        let spec = Spec {
            entries,
            plans: BTreeMap::new(),
            tasks: BTreeMap::new(),
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn writes_越界模式被拒绝() {
        assert!(validate_write_pattern("src/uart/**").is_ok());
        assert!(validate_write_pattern("../escape.c").is_err());
        assert!(validate_write_pattern("/abs/path.c").is_err());
        assert!(validate_write_pattern(".orgy/spec/x.yaml").is_err());
        assert!(validate_write_pattern("src//double.c").is_err());
    }
}
