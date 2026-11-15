//! 子进程执行边界：Windows Job Object + 双管道排空（M0.md §6.1）。
//!
//! 本模块当前实现了两件事：
//! 1. `Job`：kill-on-close 的作业对象封装，父进程退出时整棵进程树被回收；
//! 2. `run_captured`：stdin 关闭 + stdout/stderr 双线程并发排空 + 超时连根强杀。
//!
//! 仍未实现（M0 步骤 3 的剩余部分）：用 `CreateProcessW` + `EXTENDED_STARTUPINFO_PRESENT` +
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 显式限定可继承句柄，并以 `CREATE_SUSPENDED`
//! 启动后先入组再 `ResumeThread`。当前 `std::process::Command` 版本存在两个已知窗口：
//!
//! - 子进程可能在 `assign` 之前就派生出孙进程（入组前窗口）；
//! - 句柄继承依赖进程级可继承性，多 Worker 并发时理论上可能互相继承管道写端。
//!
//! 这两点在 M1 引入并发之前必须替换为显式句柄白名单。

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// 单个 Attempt 的日志上限；超出后停止写入但继续排空管道。
const LOG_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
const READ_CHUNK: usize = 8 * 1024;

#[derive(Debug)]
pub struct RunOutcome {
    /// 正常退出时的退出码；被超时强杀或异常终止时为 `None`。
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub log_path: PathBuf,
}

/// Windows 作业对象。`Drop` 时关闭句柄，进而触发 kill-on-close。
#[cfg(windows)]
pub struct Job {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for Job {}

#[cfg(windows)]
impl Job {
    pub fn new() -> Result<Self> {
        use std::mem::size_of;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // SAFETY: 传入空属性与空名字，返回的句柄由本结构体独占并负责关闭。
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(Error::Other(format!(
                "CreateJobObjectW 失败：{}",
                std::io::Error::last_os_error()
            )));
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: info 为已初始化的结构体，长度与实际类型一致。
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(Error::Other(format!(
                "SetInformationJobObject(KILL_ON_JOB_CLOSE) 失败：{err}"
            )));
        }

        Ok(Self { handle })
    }

    /// 把已创建的子进程放入作业。
    ///
    /// 注意：若当前进程本身已在某个作业中（例如某些终端），Windows 8+ 允许作业嵌套，
    /// 但更早的系统会返回 `ERROR_ACCESS_DENIED`。
    pub fn assign(&self, child: &Child) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        // SAFETY: child 尚未退出，句柄在本调用期间有效；作业句柄由 self 持有。
        let ok = unsafe {
            AssignProcessToJobObject(
                self.handle,
                child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            )
        };
        if ok == 0 {
            return Err(Error::Other(format!(
                "AssignProcessToJobObject 失败：{}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// 终止作业内全部进程。
    pub fn terminate(&self, exit_code: u32) -> Result<()> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: 句柄由 self 持有且未被关闭。
        let ok = unsafe { TerminateJobObject(self.handle, exit_code) };
        if ok == 0 {
            return Err(Error::Other(format!(
                "TerminateJobObject 失败：{}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: 句柄只在此处关闭一次；关闭即触发 kill-on-close。
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(not(windows))]
pub struct Job;

#[cfg(not(windows))]
impl Job {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    pub fn assign(&self, _child: &Child) -> Result<()> {
        Err(Error::NotImplemented(
            "非 Windows 平台的进程收容边界（M0 只实现 Windows）",
        ))
    }

    pub fn terminate(&self, _exit_code: u32) -> Result<()> {
        Err(Error::NotImplemented(
            "非 Windows 平台的进程收容边界（M0 只实现 Windows）",
        ))
    }
}

/// 启动子进程、并发排空双管道、按超时回收整棵进程树。
///
/// 返回后保证：日志文件已落盘、子进程已被回收、作业内进程已被终止或自然退出。
pub fn run_captured(
    mut cmd: Command,
    job: &Job,
    log_path: &Path,
    timeout: Duration,
) -> Result<RunOutcome> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    job.assign(&child)?;

    // 立即关闭写端：CLI 读到 EOF 即知道没有更多输入，不会等待交互。
    drop(child.stdin.take());

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Other("未能取得子进程 stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Other("未能取得子进程 stderr".into()))?;

    let sink = Arc::new(Mutex::new(BufWriter::new(File::create(log_path)?)));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;

    std::thread::scope(|scope| {
        let out_sink = Arc::clone(&sink);
        let err_sink = Arc::clone(&sink);
        scope.spawn(move || drain(stdout, out_sink));
        scope.spawn(move || drain(stderr, err_sink));

        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        // 连根强杀整棵进程树；随后必须 wait 以免留下僵尸句柄。
                        let _ = job.terminate(1);
                        let _ = child.wait();
                        timed_out = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = job.terminate(1);
                    let _ = child.wait();
                    timed_out = false;
                    // 记录错误后继续向下，保证线程收敛。
                    eprintln!("等待子进程失败：{e}");
                    break;
                }
            }
        }
    });

    if let Ok(mut guard) = sink.lock() {
        let _ = guard.flush();
    }

    let exit_code = if timed_out {
        None
    } else {
        Some(child.try_wait().ok().flatten().and_then(|s| s.code()).unwrap_or(-1))
    };

    Ok(RunOutcome {
        exit_code,
        timed_out,
        log_path: log_path.to_path_buf(),
    })
}

/// 持续读取管道直到 EOF；超过上限后停止写盘但继续读取，避免管道写满阻塞子进程。
fn drain(mut reader: impl Read, sink: Arc<Mutex<BufWriter<File>>>) {
    let mut buf = [0u8; READ_CHUNK];
    let mut written: u64 = 0;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if written < LOG_LIMIT_BYTES {
                    if let Ok(mut guard) = sink.lock() {
                        let _ = guard.write_all(&buf[..n]);
                        if guard.flush().is_err() {
                            break;
                        }
                    }
                    written += n as u64;
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_cmd(text: &str) -> Command {
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "echo", text]);
        cmd
    }

    /// 每个测试用独立目录：并行执行时共享同一路径的 `create_dir_all`
    /// 在 Windows 上会竞争失败（ERROR_ACCESS_DENIED）。
    ///
    /// 目录放在 `target/test-tmp` 而不是系统临时目录：单元测试拿不到
    /// `CARGO_TARGET_TMPDIR`，且部分受限环境下对系统临时目录的写入会被拒绝。
    fn case_dir(name: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn 正常退出并采集日志() {
        let job = Job::new().unwrap();
        let log = case_dir("ok").join("echo.log");
        let outcome = run_captured(echo_cmd("hello-orgy"), &job, &log, Duration::from_secs(10)).unwrap();
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, Some(0));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("hello-orgy"), "日志内容：{text}");
    }

    #[test]
    fn 非零退出码被保留() {
        let job = Job::new().unwrap();
        let log = case_dir("exit3").join("exit3.log");
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "exit", "3"]);
        let outcome = run_captured(cmd, &job, &log, Duration::from_secs(10)).unwrap();
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, Some(3));
    }

    #[test]
    fn 超时会强杀整棵进程树() {
        let job = Job::new().unwrap();
        let log = case_dir("timeout").join("timeout.log");
        let mut cmd = Command::new("cmd");
        // 父进程 + 孙进程同时存活，用于验证作业回收。
        cmd.args(["/c", "ping -n 30 127.0.0.1 > nul & ping -n 30 127.0.0.1 > nul"]);
        let started = Instant::now();
        let outcome = run_captured(cmd, &job, &log, Duration::from_secs(1)).unwrap();
        assert!(outcome.timed_out, "应判定为超时");
        assert_eq!(outcome.exit_code, None);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "强杀后应立即返回，实际耗时 {:?}",
            started.elapsed()
        );
    }
}
