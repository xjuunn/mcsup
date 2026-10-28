//! 被连接端（agent）：把 `transport::Handler` 接到本机 `service` 上
//!
//! `transport` 只搬字节，`service` 只编排业务，两者之间缺的那一层就是这里：
//! **认证**（配对码 → token → 角色）与**分发**（一帧 `Request` → 一次 service
//! 调用 → 一帧 `Response`）。
//!
//! 为什么放在门面而不是 `apps/agent`：`mcsup remote serve` 与独立的
//! `mcsup-agent` 必须跑**同一份**实现。放进入口就意味着两个入口各写一套，而
//! 「同一个判定只能有一处」是本项目的硬约束；门面是唯一能让两个入口共享
//! 而不互相依赖的位置（`apps` 之间禁止 Cargo 依赖）。
//!
//! # 连接生命周期
//!
//! ```text
//! TLS（客户端固定本机指纹）→ Hello → Enroll（仅首次配对）| Authenticate
//!   → 业务帧：每帧一次 service 调用
//! ```
//!
//! 认证与业务都**不缓存**判定结果：名册每次读盘、配对码每次读内存状态。缓存
//! 会让「用户刚撤销了客户端」与「agent 还认这个 token」之间出现窗口。

mod ops;
mod supervise;

use crate::bootstrap::{BootstrapError, Mcsup};
use auth::{
    ClientId, ClientRecord, ClientRegistry, PairingCode, PairingCodeError, PairingCodeStore,
    PairingWindow, Role, Token,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use transport::{
    Enrolled, Greeting, Handler, RemoteFailure, ServerIdentity, SessionInfo, StopHandle,
    TransportError,
};

pub use supervise::{
    SUPERVISE_COMMAND, SuperviseError, SuperviseSpec, is_supervise_invocation, supervise_main,
};

/// SSH 引导的机器契约：一行 JSON 打到 stdout，进程保持前台服务
///
/// `mcsup remote connect user@host` 经 SSH 执行这条入口；本地从 stdout 读出
/// 地址、指纹与配对码完成配对。打印后 agent 继续服务直到 SSH 会话结束——
/// stdout 只在开头出现这一行，之后是配对码轮换（各占一行 JSONL），所以
/// 读取方必须逐行解析而不是读完就关。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SshHandshake {
    /// agent 监听地址（`127.0.0.1:0` 时是系统分配的真实端口）
    pub addr: String,
    /// 证书指纹
    pub fingerprint: String,
    /// 当前有效配对码
    pub pairing_code: String,
    /// 本机名（客户端 `--name` 缺省值）
    pub server_name: String,
}

/// SSH 引导入口：前台起 agent 并把握手信息写 stdout
///
/// 由 `mcsup remote serve-ssh` 与 `mcsup-agent __serve-ssh` 调用；阻塞到
/// 监听结束。任何失败都写 stderr 并以非零退出，stdout 保持纯数据。
///
/// # Errors
///
/// 装配、绑定或 IO 失败时返回 [`AgentError`]
pub fn serve_ssh_main(data_dir: &Path, server_name: &str) -> Result<(), AgentError> {
    let agent = Arc::new(Agent::open(data_dir, server_name)?);
    let code = agent.issue_pairing_code()?;
    let server = agent.bind("127.0.0.1:0")?;
    let addr = server.local_addr()?;
    let stop = server.stop_handle()?;
    let handshake = SshHandshake {
        addr: addr.to_string(),
        fingerprint: agent.fingerprint().as_str().to_owned(),
        pairing_code: code.display_form(),
        server_name: server_name.to_owned(),
    };
    write_ssh_line(&handshake)?;

    // 会话寿命由两条线索决定，任一条断掉就收摊，不留孤儿 agent：
    //
    // - stdin EOF：本地 CLI 退出 → 它持有的管道关闭 → SSH 把 EOF 送到这里。
    //   这是**主要**线索，客户端没有任何「我要走了」的帧可发（无会话状态）。
    // - stdout 写失败：SSH 传输层已断（`EPIPE`），rotate 线程下次写就会撞上。
    //
    // 两处都只 `request_stop`，不 `exit`：监听线程要正常收尾、连接线程要能退出。
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            use std::io::Read as _;
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 64];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            stop.request_stop();
        });
    }

    // 配对码轮换：新码也打一行 JSONL，让配对失败重试的用户总能拿到当前码
    let rotate_stop = stop.clone();
    let watch = Agent::watch_pairing_code(&agent, move |code| {
        let rotation = serde_json::json!({ "pairing_code": code.display_form() });
        if write_ssh_line(&rotation).is_err() {
            // 写不出去说明对端已经走了，交给监听线程收尾
            rotate_stop.request_stop();
        }
    });
    let result = server.run();
    drop(watch);
    result
}

/// 把一行 JSON 契约写到 stdout 并冲刷
///
/// SSH 路径的 stdout 是唯一的机器通道：握手、配对码轮换都走这里，stderr
/// 留给日志。写失败意味着 SSH 会话已断，按传输错误上报。
fn write_ssh_line<T: serde::Serialize>(payload: &T) -> Result<(), AgentError> {
    use std::io::Write as _;
    let mut stdout = std::io::stdout();
    let json = serde_json::to_string(payload).map_err(|err| AgentError::SshOutput(err.into()))?;
    writeln!(stdout, "{json}")
        .and_then(|()| stdout.flush())
        .map_err(AgentError::SshOutput)
}

/// 配对码轮换的检查间隔
///
/// 只影响「过期后多久换上屏幕」，与码的有效期无关；取值远小于
/// [`auth::code::DEFAULT_TTL_SECS`]，用户不会看到过期的码。
pub const PAIRING_ROTATE_INTERVAL: Duration = Duration::from_secs(5);

/// 身份文件目录（`cert.pem` / `key.pem`）
#[must_use]
pub fn identity_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("remote").join("identity")
}

/// 已配对客户端名册文件
#[must_use]
pub fn clients_path(data_dir: &Path) -> PathBuf {
    data_dir.join("remote").join("clients.toml")
}

/// agent 装配 / 运行失败
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AgentError {
    /// 本机服务装配失败
    #[error(transparent)]
    Bootstrap(#[from] BootstrapError),
    /// TLS 身份、监听或配对窗口失败
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// 配对窗口锁被 poison（只有 panic 中的线程才会留下这种状态）
    #[error("pairing code store lock is poisoned")]
    PairingPoisoned,
    /// 系统随机源不可用
    #[error(transparent)]
    Pairing(#[from] PairingCodeError),
    /// SSH 引导握手写 stdout 失败（会话已断）
    #[error("failed to write the ssh handshake: {0}")]
    SshOutput(#[source] std::io::Error),
}

/// 被连接端
///
/// 一个进程一个实例；每条连接在自己的线程里并发调用它，共享状态自己加锁。
pub struct Agent {
    core: Mcsup,
    identity: ServerIdentity,
    clients: ClientRegistry,
    /// 配对窗口：单码、单次使用、限速（`auth::code`）
    pairing: Mutex<PairingCodeStore>,
    /// 名册写入串行化：`insert` / `revoke` 是读-改-写，并发会丢更新
    registry_lock: Mutex<()>,
    /// 本机名（`remote connect` 的缺省远端名）
    server_name: String,
    /// 拉起远端 supervisor 的可执行文件：本进程自身（见 [`SuperviseSpec`]）
    supervisor_exe: PathBuf,
    /// 进程工作目录：删盘前要据此拒绝「删掉自己脚下的目录」
    cwd: PathBuf,
    /// 进程启动时刻：`Ping` 的 uptime
    started: Instant,
    /// 停止句柄：`Shutdown` 帧用；绑定监听后才可用
    stop: OnceLock<StopHandle>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("server_name", &self.server_name)
            .field("supervisor_exe", &self.supervisor_exe)
            .field("cwd", &self.cwd)
            .field("fingerprint", &self.identity.fingerprint())
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// 按数据目录装配：仓库在 `<data_dir>/db`，身份与名册在 `<data_dir>/remote/`
    ///
    /// `server_name` 由入口给出（库不读环境变量）；它只是客户端的缺省远端名，
    /// 不参与任何判定。
    ///
    /// supervisor 默认取本进程可执行文件：它必须能响应 [`SuperviseSpec`] 的
    /// 参数，两个入口（`mcsup` 与 `mcsup-agent`）都实现了这条隐藏入口。
    ///
    /// # Errors
    ///
    /// 仓库打开、身份读写或目录创建失败时返回 [`AgentError`]
    pub fn open(data_dir: &Path, server_name: impl Into<String>) -> Result<Self, AgentError> {
        let core = Mcsup::open(data_dir)?;
        let identity = ServerIdentity::load_or_create(&identity_dir(data_dir))?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let supervisor_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mcsup"));
        Ok(Self {
            core,
            identity,
            clients: ClientRegistry::new(clients_path(data_dir)),
            pairing: Mutex::new(PairingCodeStore::new(PairingWindow::default())),
            registry_lock: Mutex::new(()),
            server_name: server_name.into(),
            supervisor_exe,
            cwd,
            started: Instant::now(),
            stop: OnceLock::new(),
        })
    }

    /// 指定拉起远端 supervisor 的可执行文件
    ///
    /// 只有宿主把 agent 逻辑嵌进自己的二进制时才需要：那时 `current_exe()`
    /// 是宿主程序，未必实现了隐藏的 supervisor 入口。
    #[must_use]
    pub fn with_supervisor_exe(mut self, exe: impl Into<PathBuf>) -> Self {
        self.supervisor_exe = exe.into();
        self
    }

    /// 本机服务句柄（诊断与测试用；业务访问经 [`Handler`]）
    #[must_use]
    pub fn core(&self) -> &Mcsup {
        &self.core
    }

    /// 本机证书指纹（`remote connect` 首次确认时展示给用户）
    #[must_use]
    pub fn fingerprint(&self) -> &transport::Fingerprint {
        self.identity.fingerprint()
    }

    /// 已配对客户端名册（`remote status` 的服务端视角）
    #[must_use]
    pub fn clients(&self) -> &ClientRegistry {
        &self.clients
    }

    /// 拉起 supervisor 用的可执行文件
    #[must_use]
    pub fn supervisor_exe(&self) -> &Path {
        &self.supervisor_exe
    }

    /// 当前有效配对码
    ///
    /// # Errors
    ///
    /// 配对窗口锁被 poison 时返回 [`AgentError::PairingPoisoned`]
    pub fn current_pairing_code(&self) -> Result<Option<PairingCode>, AgentError> {
        Ok(self.pairing()?.current(now_secs()).cloned())
    }

    /// 发一枚新配对码（旧码立即作废）
    ///
    /// # Errors
    ///
    /// 随机源不可用或锁被 poison 时返回 [`AgentError`]
    pub fn issue_pairing_code(&self) -> Result<PairingCode, AgentError> {
        let mut store = self.pairing()?;
        Ok(store.issue(now_secs())?)
    }

    /// 码自然过期时轮换；未过期返回 `None`
    ///
    /// # Errors
    ///
    /// 随机源不可用或锁被 poison 时返回 [`AgentError`]
    pub fn rotate_pairing_code(&self) -> Result<Option<PairingCode>, AgentError> {
        let mut store = self.pairing()?;
        Ok(store.rotate_if_expired(now_secs())?)
    }

    /// 绑定监听并返回可运行的服务器
    ///
    /// # Errors
    ///
    /// 地址无法解析、绑定失败或 TLS 配置失败时返回 [`AgentError`]
    pub fn bind(self: &Arc<Self>, addr: &str) -> Result<AgentServer, AgentError> {
        let server =
            transport::Server::bind(addr, &self.identity, Arc::clone(self) as Arc<dyn Handler>)?;
        let handle = server.stop_handle()?;
        let _ = self.stop.set(handle);
        Ok(AgentServer { server })
    }

    /// 绑定并阻塞服务，直到收到 `Shutdown` 或监听套接字失效
    ///
    /// # Errors
    ///
    /// 见 [`Agent::bind`] 与 [`AgentServer::run`]
    pub fn serve(self: &Arc<Self>, addr: &str) -> Result<(), AgentError> {
        self.bind(addr)?.run()
    }

    /// 把「当前配对码变了」这件事推给入口
    ///
    /// 常驻进程需要让屏幕上**总有**当前有效码：码会过期、用掉后自动换新。
    /// 库不打印（库不写 stdout），这里只做「发现变化就回调」，打印由入口决定。
    ///
    /// 返回的 [`PairingWatch`] 在 drop 时停止轮换线程。
    #[must_use]
    pub fn watch_pairing_code<F>(agent: &Arc<Self>, mut on_change: F) -> PairingWatch
    where
        F: FnMut(&PairingCode) + Send + 'static,
    {
        let agent = Arc::clone(agent);
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        let join = std::thread::spawn(move || {
            let mut last: Option<String> = None;
            while !flag.load(Ordering::Relaxed) {
                // 过期即换新；换不出来（锁坏 / 随机源坏）就只记一笔，不打断服务
                if let Err(err) = agent.rotate_pairing_code() {
                    tracing::warn!(%err, "pairing code rotation failed");
                }
                match agent.current_pairing_code() {
                    Ok(Some(code)) => {
                        let text = code.display_form();
                        if last.as_deref() != Some(text.as_str()) {
                            on_change(&code);
                            last = Some(text);
                        }
                    }
                    // 已用掉或猜错锁死：保持关闭，不自动重开窗口
                    Ok(None) => last = None,
                    Err(err) => tracing::warn!(%err, "pairing code read failed"),
                }
                std::thread::sleep(PAIRING_ROTATE_INTERVAL);
            }
        });
        PairingWatch {
            stopped,
            join: Some(join),
        }
    }

    /// 取配对窗口
    fn pairing(&self) -> Result<std::sync::MutexGuard<'_, PairingCodeStore>, AgentError> {
        self.pairing.lock().map_err(|_| AgentError::PairingPoisoned)
    }
}

/// 配对码轮换线程的句柄
///
/// drop 即停止：常驻进程退出时不必显式 `stop`，也不会留下一个还在打印的线程。
#[derive(Debug)]
pub struct PairingWatch {
    stopped: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PairingWatch {
    /// 停止轮换并等待线程退出
    ///
    /// # Errors
    ///
    /// 轮换线程 panic 时返回其错误（不影响 agent 本身）
    pub fn stop(mut self) -> std::thread::Result<()> {
        self.stopped.store(true, Ordering::Relaxed);
        match self.join.take() {
            Some(join) => join.join(),
            None => Ok(()),
        }
    }
}

impl Drop for PairingWatch {
    fn drop(&mut self) {
        // 线程每轮都会看这个标志，最迟一个间隔后退出
        self.stopped.store(true, Ordering::Relaxed);
    }
}

/// 已绑定的 agent（`run` 之前可查真实端口，测试与「自动选端口」用）
pub struct AgentServer {
    server: transport::Server,
}

impl std::fmt::Debug for AgentServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentServer")
            .field("addr", &self.server.local_addr().ok())
            .finish_non_exhaustive()
    }
}

impl AgentServer {
    /// 实际监听地址（`port = 0` 时由系统分配）
    ///
    /// # Errors
    ///
    /// 取地址失败时返回 [`TransportError`]
    pub fn local_addr(&self) -> Result<std::net::SocketAddr, TransportError> {
        self.server.local_addr()
    }

    /// 从别的线程请求停止
    ///
    /// # Errors
    ///
    /// 取监听地址失败时返回 [`TransportError`]
    pub fn request_stop(&self) -> Result<(), TransportError> {
        self.server.request_stop()
    }

    /// 可跨线程持有的停止句柄
    ///
    /// `run` 会消费 `self`，句柄必须先取出来：`remote serve --stop` 与测试
    /// 都需要在 `run` 阻塞之后才决定何时停。
    ///
    /// # Errors
    ///
    /// 取监听地址失败时返回 [`TransportError`]
    pub fn stop_handle(&self) -> Result<StopHandle, TransportError> {
        self.server.stop_handle()
    }

    /// 接受连接直到停止
    ///
    /// # Errors
    ///
    /// 监听套接字本身失败时返回 [`AgentError`]
    pub fn run(self) -> Result<(), AgentError> {
        self.server.run()?;
        Ok(())
    }
}

impl Handler for Agent {
    fn greeting(&self) -> Greeting {
        // 名册读失败时按「已有客户端」报：让客户端走认证而不是误以为这是首次配对
        let has_clients = self.clients.is_empty().map_or(true, |empty| !empty);
        Greeting {
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            server_name: self.server_name.clone(),
            capabilities: protocol::remote::Capabilities::all(),
            fingerprint: self.identity.fingerprint().clone(),
            has_clients,
        }
    }

    fn enroll(
        &self,
        pairing_code: &str,
        client_name: &str,
        read_only: bool,
    ) -> Result<Enrolled, RemoteFailure> {
        // 名字先过一遍：名字非法不该消耗一次性配对码（用户会以为是码不对）
        let name = client_name.trim();
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err(RemoteFailure::invalid(
                "client name must be 1..=64 printable chars",
            ));
        }

        {
            let mut store = self.pairing().map_err(|err| {
                RemoteFailure::new(protocol::remote::ErrorKind::IO, err.to_string())
            })?;
            store.redeem(pairing_code, now_secs()).map_err(|err| {
                // 对外不区分「过期」「用掉」「猜错太多次」：那等于告诉猜码的人离成功多近
                tracing::warn!(%err, client = %name, "remote enroll rejected");
                RemoteFailure::unauthorized("pairing code is not usable")
            })?;
            // 成功即换新码：屏幕上始终有当前有效码，不必重启 agent 才能配对第二台
            store.issue(now_secs()).map_err(|err| {
                RemoteFailure::new(protocol::remote::ErrorKind::IO, err.to_string())
            })?;
        }

        let token = Token::generate()
            .map_err(|err| RemoteFailure::new(protocol::remote::ErrorKind::IO, err.to_string()))?;
        let role = if read_only {
            Role::Viewer
        } else {
            Role::Operator
        };
        let record = ClientRecord {
            client_id: ClientId::generate(),
            name: name.to_owned(),
            digest: token.digest(),
            role,
            paired_at: now_secs().cast_signed(),
            expires_at: Some(now_secs().cast_signed() + auth::token::DEFAULT_TOKEN_TTL_SECS),
        };
        let client_id = record.client_id.clone();
        {
            let _guard = self.registry_lock.lock().map_err(|_| {
                RemoteFailure::new(
                    protocol::remote::ErrorKind::IO,
                    "client registry lock is poisoned",
                )
            })?;
            self.clients.insert(record).map_err(|err| {
                RemoteFailure::new(protocol::remote::ErrorKind::IO, err.to_string())
            })?;
        }
        tracing::info!(client = %client_id, %name, ?role, "remote client enrolled");
        Ok(Enrolled {
            token: token.expose().to_owned(),
            client_id,
            role,
        })
    }

    fn authenticate(&self, token: &str) -> Result<SessionInfo, RemoteFailure> {
        let parsed =
            Token::parse(token).map_err(|_| RemoteFailure::unauthorized("malformed token"))?;
        let record = self
            .clients
            .find_by_digest(&parsed.digest())
            .map_err(|err| RemoteFailure::new(protocol::remote::ErrorKind::IO, err.to_string()))?
            .ok_or_else(|| RemoteFailure::unauthorized("token is unknown or revoked"))?;
        if record.is_expired(now_secs().cast_signed()) {
            return Err(RemoteFailure::unauthorized(
                "token expired; run `remote connect` again",
            ));
        }
        Ok(SessionInfo {
            client_id: record.client_id,
            role: record.role,
        })
    }

    /// 每帧前重查名册：撤销 / 过期立即生效（`docs/remote.md`「安全」）
    fn still_authorized(&self, session: &SessionInfo) -> bool {
        match self.clients.find(&session.client_id) {
            Ok(Some(record)) => !record.is_expired(now_secs().cast_signed()),
            Ok(None) => false,
            Err(err) => {
                // 名册读不出来时按无效处理（fail-closed），并记一笔
                tracing::warn!(%err, "client registry read failed; treating session as invalid");
                false
            }
        }
    }

    fn handle(
        &self,
        session: &SessionInfo,
        request: &protocol::remote::Request,
    ) -> protocol::remote::Response {
        self.audited(session, request, &port::NoopTaskObserver)
    }

    fn handle_with(
        &self,
        session: &SessionInfo,
        request: &protocol::remote::Request,
        progress: &dyn port::TaskObserver,
    ) -> protocol::remote::Response {
        self.audited(session, request, progress)
    }
}

impl Agent {
    /// 执行 + 审计：正常路径与带进度路径共用一段记录逻辑
    fn audited(
        &self,
        session: &SessionInfo,
        request: &protocol::remote::Request,
        progress: &dyn port::TaskObserver,
    ) -> protocol::remote::Response {
        let response = self.dispatch_with(request, progress);
        let ok = !matches!(response, protocol::remote::Response::Error { .. });
        // 审计：调用方、操作、结果。参数不进日志（可能含路径与控制台输入）
        tracing::info!(
            client = %session.client_id,
            role = ?session.role,
            op = request.operation(),
            ok,
            "remote op"
        );
        response
    }
}

/// 当前 Unix 秒（配对窗口的时间口径）
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
