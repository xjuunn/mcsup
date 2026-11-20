//! Windows Job Object：插件进程树随宿主退出
//!
//! 只 `child.kill()` 杀不掉插件自己拉起的子进程；把插件进程放进带
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object，宿主异常退出或句柄被
//! 关闭时由内核终止整棵进程树。句柄挂在 [`SpawnedPlugin`](crate::process::SpawnedPlugin)
//! 上保活，drop 即生效。
//!
//! workspace 禁止 `unsafe`，FFI 由 `win32job` 的安全封装承担。

use std::io;
use std::os::windows::io::AsRawHandle;
use std::process::Child;

use win32job::{ExtendedLimitInfo, Job};

/// 一个已配置好的 Job Object
///
/// 最后一个句柄关闭时（含进程崩溃后内核回收），被分配的进程树一并终止。
#[derive(Debug)]
pub(crate) struct JobObject(#[allow(dead_code)] Job);

impl JobObject {
    /// 建 Job Object 并把 `child` 分配进去
    ///
    /// # Errors
    ///
    /// 建对象、设置上限或分配进程失败时返回 IO 错误
    pub(crate) fn assign(child: &Child) -> io::Result<Self> {
        let mut limits = ExtendedLimitInfo::new();
        limits.limit_kill_on_job_close();
        let job = Job::create_with_limit_info(&limits).map_err(io::Error::other)?;
        job.assign_process(child.as_raw_handle() as isize)
            .map_err(io::Error::other)?;
        Ok(Self(job))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// 关闭 Job Object 会终止已分配的进程
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
