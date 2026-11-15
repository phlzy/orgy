//! ORGY 入口。M0 阶段目标：单任务端到端闭环（docs/milestones/M0.md）。

// 骨架阶段：部分 API 面向 M0 后续步骤（步骤 2–7），暂未全部接入。
#![allow(dead_code)]

mod adapter;
mod cli;
mod config;
mod db;
mod digest;
mod error;
mod git;
mod handoff;
mod process;
mod prompt;
mod supervisor;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("无法获取当前目录：{e}");
            return ExitCode::FAILURE;
        }
    };

    match cli::dispatch(&cwd, cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("错误：{err}");
            ExitCode::FAILURE
        }
    }
}
