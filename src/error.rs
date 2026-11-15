use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// 全流程统一错误类型。
///
/// 设计原则：M0 阶段"异常一律报错退出交给人"（见 docs/milestones/M0.md §1），
/// 因此这里不为可恢复错误建模复杂的重试语义，只保证每类失败都有可读上下文。
#[derive(Debug, Error)]
pub enum Error {
    #[error("配置错误：{0}")]
    Config(String),

    #[error("git 命令失败：{cmd}\n{stderr}")]
    Git { cmd: String, stderr: String },

    #[error("破坏性操作的路径 {path} 不在受管工作区 {root} 之下，已拒绝执行")]
    UnsafeDestructivePath { path: PathBuf, root: PathBuf },

    #[error("数据库错误：{0}")]
    Db(#[from] rusqlite::Error),

    #[error("I/O 错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("YAML 解析错误：{0}")]
    Yaml(#[from] serde_yml::Error),

    #[error("JSON 序列化错误：{0}")]
    Json(#[from] serde_json::Error),

    #[error("尚未实现（M0 后续步骤）：{0}")]
    NotImplemented(&'static str),

    #[error("{0}")]
    Other(String),
}
