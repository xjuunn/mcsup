//! 插件进程与日志采集
//!
//! 进程只通过 [`SpawnPlan`] 描述，测试可以只覆盖命令拼装而不用真的拉起进程。
//! 日志走 stdout / stderr（`[禁止]` 写进协议端点），逐行采集、限速、截断、
//! 前缀与已知令牌打码后写入 `logs/plugins/<id>.log`。

use domain::plugin::manifest::EntrySpec;
use port::plugin::{HostError, SecretValue};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 单行日志最大字节数
pub const MAX_LOG_LINE_BYTES: usize = 8 * 1024;

/// 每插件日志限速（行/秒）
pub const LOG_LINES_PER_SEC: u32 = 200;

/// 进打码表的密钥最小长度
///
/// 太短的值（`1`、`on`）会在日志里命中无关文本，整行替换成 `***` 后日志不可
/// 读，问题反而更难排查；这类短值单独泄漏也通常不足以直接利用。
pub const MIN_REDACT_LEN: usize = 8;

/// 打码表上限
///
/// 异常配置（几百条密钥）会把整份日志变成 `***`，上限保证打码仍然只作用于
/// 少数敏感片段。
pub const MAX_REDACT_VALUES: usize = 64;

/// 拉起参数
#[derive(Debug, Clone)]
pub struct SpawnPlan {
    /// 可执行文件绝对路径
    pub program: PathBuf,
    /// 工作目录（产物根）
    pub cwd: PathBuf,
    /// 参数
    pub args: Vec<String>,
    /// 环境变量
    pub env: Vec<(String, String)>,
    /// 日志文件
    pub log_file: PathBuf,
    /// 日志前缀（插件 id）
    pub prefix: String,
    /// 采集时打码的已知敏感值（一次性令牌）
    pub redact: Vec<String>,
}

impl SpawnPlan {
    /// 由安装记录构造
    ///
    /// `endpoint`、`token`、`scope` 由宿主生成；令牌只经环境变量传递。
    ///
    /// # Errors
    ///
    /// 入口文件不存在、不是普通文件或没有可执行位时返回 [`HostError`]
    // 启动参数就是这份清单；分组反而增加一层无意义的中间结构
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        artifact_dir: &Path,
        entry: &EntrySpec,
        log_file: PathBuf,
        prefix: String,
        endpoint: &str,
        token: &str,
        plugin_id: &str,
        scope: domain::plugin::PluginScope,
        server: Option<&str>,
    ) -> Result<Self, HostError> {
        let platform = platform_key();
        let cmd = entry
            .platforms
            .get(&platform)
            .map_or(entry.cmd.as_str(), String::as_str);
        let program = artifact_dir.join(cmd);
        check_executable(&program)?;
        let mut args = vec!["--endpoint".to_owned(), endpoint.to_owned()];
        if let Some(server) = server {
            args.push("--server".to_owned());
            args.push(server.to_owned());
        }
        args.push("--log-level".to_owned());
        args.push("info".to_owned());
        Ok(Self {
            program,
            cwd: artifact_dir.to_path_buf(),
            args,
            env: vec![
                ("MCSUP_PLUGIN_TOKEN".to_owned(), token.to_owned()),
                ("MCSUP_PLUGIN_ID".to_owned(), plugin_id.to_owned()),
                ("MCSUP_PLUGIN_SCOPE".to_owned(), scope.as_str().to_owned()),
            ],
            log_file,
            prefix,
            redact: vec![token.to_owned()],
        })
    }
}

impl SpawnPlan {
    /// 把已知密钥值并入日志打码表
    ///
    /// 日志是文本，非 UTF-8 的密钥值不会以原样出现，直接跳过；重复值与过短的
    /// 值也跳过，总数受限。
    pub fn extend_redact(&mut self, values: impl IntoIterator<Item = SecretValue>) {
        for value in values {
            if self.redact.len() >= MAX_REDACT_VALUES {
                break;
            }
            let Ok(text) = std::str::from_utf8(value.expose()) else {
                continue;
            };
            if text.len() < MIN_REDACT_LEN || self.redact.iter().any(|known| known == text) {
                continue;
            }
            self.redact.push(text.to_owned());
        }
    }
}

/// 平台标识：`darwin-arm64` / `linux-x64` / `win32-x64`
#[must_use]
pub fn platform_key() -> String {
    domain::plugin::platform_key()
}

/// 脚本型入口：Windows 上必须经 `cmd.exe /C` 启动，`CreateProcess` 不能直接
/// 执行 `.cmd` / `.bat`。
#[cfg(windows)]
fn is_script(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
}

/// 展开为实际执行命令：`(程序, 参数)`
///
/// 非 Windows 平台原样返回；Windows 上脚本入口改走 `COMSPEC /C <script>`，
/// 解释器缺失时回退到 `System32\cmd.exe`。
fn command_line(program: &Path, args: &[String]) -> (PathBuf, Vec<String>) {
    #[cfg(windows)]
    {
        if is_script(program) {
            let comspec = std::env::var_os("COMSPEC").map_or_else(
                || PathBuf::from(r"C:\Windows\System32\cmd.exe"),
                PathBuf::from,
            );
            let mut expanded = Vec::with_capacity(args.len() + 2);
            expanded.push("/C".to_owned());
            expanded.push(program.to_string_lossy().into_owned());
            expanded.extend(args.iter().cloned());
            return (comspec, expanded);
        }
    }
    (program.to_path_buf(), args.to_vec())
}

fn check_executable(path: &Path) -> Result<(), HostError> {
    let metadata = std::fs::metadata(path).map_err(|err| {
        HostError::not_found(format!(
            "plugin entry {} is not accessible: {err}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(HostError::invalid_input(format!(
            "plugin entry {} is not a file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(HostError::invalid_input(format!(
                "plugin entry {} is not executable",
                path.display()
            )));
        }
    }
    Ok(())
}

/// 已拉起的插件进程
pub struct SpawnedPlugin {
    child: Child,
    pid: u32,
    /// 进程树随宿主退出：句柄关闭即终止插件与其子进程
    #[cfg(windows)]
    job: Option<crate::job::JobObject>,
}

impl std::fmt::Debug for SpawnedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedPlugin")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl SpawnedPlugin {
    /// 拉起进程并启动日志采集线程
    ///
    /// # Errors
    ///
    /// 进程无法创建时返回 [`HostError`]；日志文件失败只让采集降级为丢弃，
    /// 不影响插件运行。
    pub fn spawn(plan: &SpawnPlan) -> Result<Self, HostError> {
        if let Some(parent) = plan.log_file.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                HostError::internal(format!("create plugin log dir {}: {err}", parent.display()))
            })?;
        }
        let (program, args) = command_line(&plan.program, &plan.args);
        let mut child = Command::new(&program)
            .args(&args)
            .current_dir(&plan.cwd)
            .envs(plan.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                HostError::new(
                    port::plugin::ErrorCode::Crashed,
                    format!("failed to spawn {}: {err}", plan.program.display()),
                )
            })?;
        let pid = child.id();
        // 进程树清理是安全兜底：Job Object 建不出来时插件仍可运行，降级为
        // 只杀直接子进程（与 Unix 平台行为一致）。
        #[cfg(windows)]
        let job = match crate::job::JobObject::assign(&child) {
            Ok(job) => Some(job),
            Err(err) => {
                tracing::warn!(plugin = %plan.prefix, error = %err, "job object unavailable; plugin subtree will not follow host exit");
                None
            }
        };
        let writer = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&plan.log_file)
        {
            Ok(file) => Some(Arc::new(Mutex::new(file))),
            Err(_) => None,
        };
        if let Some(writer) = &writer {
            let redact = plan.redact.clone();
            let prefix = plan.prefix.clone();
            if let Some(stdout) = child.stdout.take() {
                collect(stdout, writer.clone(), prefix.clone(), redact.clone());
            }
            if let Some(stderr) = child.stderr.take() {
                collect(stderr, writer.clone(), prefix, redact);
            }
        }
        Ok(Self {
            child,
            pid,
            #[cfg(windows)]
            job,
        })
    }

    /// 是否已把进程树交给 Job Object 管理（Windows）
    #[cfg(all(windows, test))]
    #[must_use]
    pub(crate) fn has_job_object(&self) -> bool {
        self.job.is_some()
    }

    /// 进程号
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 非阻塞回收
    ///
    /// # Errors
    ///
    /// 等待失败时返回 IO 错误
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// 强杀
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // 直接子进程已回收，接着关掉 Job 句柄：插件自己拉起的子进程不能留下。
        #[cfg(windows)]
        {
            drop(self.job.take());
        }
    }

    /// 在 `timeout` 内等待退出；返回是否已退出
    pub fn wait_timeout(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return true,
                Ok(None) if Instant::now() >= deadline => return false,
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    }
}

impl Drop for SpawnedPlugin {
    fn drop(&mut self) {
        // 会话异常结束时不留下孤儿进程。正常停机路径已先行等待退出。
        let _ = self.child.kill();
        let _ = self.child.wait();
        // 关闭 Job 句柄：插件拉起的整棵进程树随宿主一起终止。
        #[cfg(windows)]
        {
            drop(self.job.take());
        }
    }
}

fn collect<R: Read + Send + 'static>(
    reader: R,
    writer: Arc<Mutex<std::fs::File>>,
    prefix: String,
    redact: Vec<String>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut limiter = LineLimiter::new(LOG_LINES_PER_SEC);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let truncated = line.len() > MAX_LOG_LINE_BYTES;
            let body = &line[..line.len().min(MAX_LOG_LINE_BYTES)];
            let text = String::from_utf8_lossy(body);
            let text = redact_values(text.trim_end_matches(['\r', '\n']), &redact);
            if !limiter.allow() {
                continue;
            }
            let suffix = if truncated { " [truncated]" } else { "" };
            if let Ok(mut file) = writer.lock() {
                let _ = writeln!(file, "[{prefix}] {text}{suffix}");
            }
        }
        if let Some(dropped) = limiter.dropped_summary()
            && let Ok(mut file) = writer.lock()
        {
            let _ = writeln!(file, "[{prefix}] dropped {dropped} lines");
        }
    });
}

fn redact_values(text: &str, redact: &[String]) -> String {
    let mut out = text.to_owned();
    for value in redact {
        if !value.is_empty() {
            out = out.replace(value, "***");
        }
    }
    out
}

/// 每秒 200 行的令牌桶；超出丢弃，离开窗口时汇总
struct LineLimiter {
    limit: u32,
    window: Instant,
    count: u32,
    dropped: u64,
}

impl LineLimiter {
    fn new(limit: u32) -> Self {
        Self {
            limit,
            window: Instant::now(),
            count: 0,
            dropped: 0,
        }
    }

    fn allow(&mut self) -> bool {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.count = 0;
        }
        if self.count < self.limit {
            self.count += 1;
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    fn dropped_summary(&mut self) -> Option<u64> {
        if self.dropped == 0 {
            None
        } else {
            Some(std::mem::take(&mut self.dropped))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn platform_key_is_stable_shape() {
        let key = platform_key();
        assert!(key.contains('-'), "{key}");
        assert!(!key.contains('_'), "{key}");
    }

    #[test]
    fn limiter_drops_after_limit() {
        let mut limiter = LineLimiter::new(2);
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(!limiter.allow());
        assert_eq!(limiter.dropped_summary(), Some(1));
        assert_eq!(limiter.dropped_summary(), None);
    }

    #[test]
    fn redaction_is_exact_match() {
        let redacted = redact_values("token=abc123 ok", &["abc123".to_owned()]);
        assert_eq!(redacted, "token=*** ok");
    }

    fn plan() -> SpawnPlan {
        SpawnPlan {
            program: PathBuf::from("/bin/true"),
            cwd: PathBuf::new(),
            args: Vec::new(),
            env: Vec::new(),
            log_file: PathBuf::new(),
            prefix: "demo".to_owned(),
            redact: vec!["handshake-token".to_owned()],
        }
    }

    #[test]
    fn known_secret_values_are_redacted() {
        let mut plan = plan();
        plan.extend_redact([
            SecretValue::from_text("sk-live-0123456789"),
            SecretValue::from_text("s3cret-value"),
        ]);
        assert_eq!(
            plan.redact,
            ["handshake-token", "sk-live-0123456789", "s3cret-value"]
        );
        assert_eq!(
            redact_values("auth sk-live-0123456789 and s3cret-value", &plan.redact),
            "auth *** and ***"
        );
    }

    #[test]
    fn short_and_duplicate_secrets_are_skipped() {
        let mut plan = plan();
        plan.extend_redact([
            SecretValue::from_text("on"),              // 太短
            SecretValue::from_text("handshake-token"), // 已存在
            SecretValue::from_text("long-enough-secret"),
        ]);
        assert_eq!(plan.redact, ["handshake-token", "long-enough-secret"]);
    }

    #[test]
    fn non_utf8_secret_is_skipped() {
        let mut plan = plan();
        plan.extend_redact([SecretValue::from_bytes(vec![0xff, 0xfe, 0xfd, 0xfc, 0xfb])]);
        assert_eq!(plan.redact, ["handshake-token"]);
    }

    #[test]
    fn redact_table_is_bounded() {
        let mut plan = plan();
        plan.redact.clear();
        plan.extend_redact(
            (0..MAX_REDACT_VALUES + 10)
                .map(|index| SecretValue::from_text(format!("secret-value-{index:04}"))),
        );
        assert_eq!(plan.redact.len(), MAX_REDACT_VALUES);
    }

    #[cfg(windows)]
    #[test]
    fn spawn_assigns_a_job_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let program = std::env::var_os("COMSPEC").map_or_else(
            || PathBuf::from(r"C:\Windows\System32\cmd.exe"),
            PathBuf::from,
        );
        let mut plan = plan();
        plan.program = program;
        plan.cwd = dir.path().to_path_buf();
        plan.args = vec!["/C".to_owned(), "exit".to_owned(), "0".to_owned()];
        plan.log_file = dir.path().join("plugin.log");
        let mut spawned = SpawnedPlugin::spawn(&plan).expect("spawn");
        assert!(
            spawned.has_job_object(),
            "spawn must assign the plugin process to a job object"
        );
        assert!(spawned.wait_timeout(Duration::from_secs(5)));
    }

    #[cfg(windows)]
    #[test]
    fn script_entry_runs_through_cmd_exe() {
        let (program, args) = command_line(
            Path::new(r"C:\plugins\demo\bin\run.cmd"),
            &["--endpoint".to_owned(), "tcp://x".to_owned()],
        );
        assert!(
            program
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with("cmd.exe"),
            "{}",
            program.display()
        );
        assert_eq!(args[0], "/C");
        assert_eq!(args[1], r"C:\plugins\demo\bin\run.cmd");
        assert_eq!(args[2], "--endpoint");
        assert_eq!(args[3], "tcp://x");

        let (program, args) = command_line(
            Path::new(r"C:\plugins\demo\bin\run.bat"),
            &["--server".to_owned()],
        );
        assert_eq!(args[0], "/C");
        assert_eq!(args[1], r"C:\plugins\demo\bin\run.bat");
        assert!(
            program
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with("cmd.exe")
        );
    }

    #[cfg(windows)]
    #[test]
    fn exe_entry_runs_directly() {
        let exe = Path::new(r"C:\plugins\demo\bin\demo.exe");
        let (program, args) = command_line(exe, &["--endpoint".to_owned()]);
        assert_eq!(program, exe);
        assert_eq!(args, ["--endpoint"]);
    }

    #[cfg(windows)]
    #[test]
    fn script_plugin_spawns_and_logs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("run.cmd");
        std::fs::write(
            &script,
            "@echo off\r\necho hello-from-plugin\r\nexit /b 0\r\n",
        )
        .expect("script");
        let mut plan = plan();
        plan.program = script;
        plan.cwd = dir.path().to_path_buf();
        plan.log_file = dir.path().join("plugin.log");
        let mut spawned = SpawnedPlugin::spawn(&plan).expect("spawn");
        assert!(spawned.wait_timeout(Duration::from_secs(10)));

        let log = dir.path().join("plugin.log");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut text = String::new();
        while Instant::now() < deadline {
            text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("hello-from-plugin") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(text.contains("hello-from-plugin"), "log: {text}");
    }

    #[test]
    fn missing_entry_is_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entry = EntrySpec {
            kind: "process".to_owned(),
            cmd: "bin/missing".to_owned(),
            platforms: BTreeMap::default(),
        };
        let plan = SpawnPlan::new(
            dir.path(),
            &entry,
            dir.path().join("log"),
            "demo".to_owned(),
            "/tmp/endpoint",
            "token",
            "demo",
            domain::plugin::PluginScope::Machine,
            None,
        );
        assert!(matches!(
            plan.map_err(|err| err.code),
            Err(port::plugin::ErrorCode::NotFound)
        ));
    }
}
