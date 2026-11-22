//! Windows Job Object：服务端进程树随 supervisor 退出
//!
//! supervisor 被外部结束（任务管理器、替换二进制、崩溃）时，`Child` 不会跟着
//! 死：遗留的 JVM 继续持有 `world/session.lock` 与 `logs/latest.log`，而 IPC
//! 端点已经消失，`status` 只能报 stopped；下一次 `start` 拉起的第二个 JVM 必然
//! 撞上目录锁。把服务端进程放进带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job
//! Object 后，最后一个句柄关闭（含进程被强杀后内核回收）时整棵进程树由内核
//! 终止，不再产生孤儿。
//!
//! 句柄挂在 [`Shared`](crate::supervisor) 上保活，drop 即生效。
//!
//! workspace 禁止 `unsafe`，FFI 由 `win32job` 的安全封装承担。非 Windows
//! 平台是零成本占位：孤儿回收交给 init / 进程组。

use std::io;
use std::process::Child;

/// 一个已配置好的 Job Object
///
/// 最后一个句柄关闭时，被分配的进程树一并终止。
#[derive(Debug)]
pub(crate) struct JobObject {
    #[cfg(windows)]
    #[allow(dead_code)]
    job: win32job::Job,
}

impl JobObject {
    /// 建 Job Object 并把 `child` 分配进去
    ///
    /// # Errors
    ///
    /// Windows 上建对象、设置上限或分配进程失败时返回 IO 错误；其他平台不会
    /// 失败。
    #[cfg(windows)]
    pub(crate) fn assign(child: &Child) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle as _;

        use win32job::{ExtendedLimitInfo, Job};

        let mut limits = ExtendedLimitInfo::new();
        limits.limit_kill_on_job_close();
        let job = Job::create_with_limit_info(&limits).map_err(io::Error::other)?;
        job.assign_process(child.as_raw_handle() as isize)
            .map_err(io::Error::other)?;
        Ok(Self { job })
    }

    /// 其他平台无 Job Object 语义，交由进程组 / init 回收
    #[cfg(not(windows))]
    pub(crate) fn assign(_child: &Child) -> io::Result<Self> {
        Ok(Self {})
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    /// 关闭 Job Object 会终止已分配的服务端进程
    ///
    /// 只验证内核语义：把常驻子进程放进 Job 后丢弃句柄，进程必须自行退出。
    #[test]
    fn closing_job_kills_assigned_process() {
        let mut child = Command::new("cmd.exe")
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn helper");
        let job = JobObject::assign(&child).expect("assign job");
        drop(job);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while Instant::now() < deadline {
            if child.try_wait().expect("try_wait").is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            exited,
            "closing the job must terminate the assigned process"
        );
    }
}
