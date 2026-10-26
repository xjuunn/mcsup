//! 服务端的业务注入点
//!
//! `transport` 只做 TLS 与帧，不知道「谁能连、连上能做什么」。这三件事由
//! 实现 [`Handler`] 的一方回答——agent 侧接的是 `auth` 与 `service`，
//! 测试里接的是内存实现。
//!
//! 把认证与业务都做成 trait 而不是内联，是为了让「同一个传输层既能被 agent
//! 使用，也能被测试与第三方宿主使用」：换实现不动传输代码。

use crate::fingerprint::Fingerprint;
use auth::{ClientId, Role};
use protocol::remote::{Capabilities, Request, Response};

/// 握手时要告诉客户端的信息
#[derive(Debug, Clone)]
pub struct Greeting {
    /// agent 程序版本
    pub agent_version: String,
    /// 本机名（`remote connect` 的缺省远端名）
    pub server_name: String,
    /// 能力
    pub capabilities: Capabilities,
    /// TLS 证书指纹
    pub fingerprint: Fingerprint,
    /// 本机是否已有配对客户端（`false` 时客户端只能先 `Enroll`）
    pub has_clients: bool,
}

/// 配对成功的结果
#[derive(Debug, Clone)]
pub struct Enrolled {
    /// 明文 token，客户端立刻写入凭证存储
    pub token: String,
    /// 服务端分配的客户端身份
    pub client_id: ClientId,
    /// 角色
    pub role: Role,
}

/// 已认证连接的上下文
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// 客户端身份
    pub client_id: ClientId,
    /// 角色
    pub role: Role,
}

impl SessionInfo {
    /// 该角色能否执行某个请求
    #[must_use]
    pub fn permits(&self, request: &Request) -> bool {
        request
            .required_permission()
            .is_none_or(|permission| self.role.permits(permission))
    }
}

/// 被拒绝的操作
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFailure {
    /// 错误种类（`protocol::remote::ErrorKind` 的取值）
    pub kind: &'static str,
    /// 人类可读消息
    pub message: String,
}

impl RemoteFailure {
    /// 构造一条拒绝
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// 未认证 / 权限不足
    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(protocol::remote::ErrorKind::UNAUTHORIZED, message)
    }

    /// 参数非法
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(protocol::remote::ErrorKind::INVALID, message)
    }

    /// 转成协议错误帧
    #[must_use]
    pub fn into_response(self) -> Response {
        Response::Error {
            kind: self.kind.to_owned(),
            message: self.message,
        }
    }
}

/// 服务端业务注入点
///
/// 所有方法都要求 `&self` + `Send + Sync`：一个进程只持有一个 handler，
/// 每条连接在自己的线程里并发调用它。实现内部的共享状态自己加锁，
/// 传输层不做串行化。
pub trait Handler: Send + Sync {
    /// 握手信息（每次连接都取一次：能力与已配对数量可能在运行中变化）
    fn greeting(&self) -> Greeting;

    /// 用配对码换 token
    ///
    /// `read_only` 为真时服务端按 `viewer` 角色登记（`docs/remote.md`「安全」：
    /// 默认 operator，可 `--read-only`）。
    ///
    /// # Errors
    ///
    /// 码不对、已过期、已用尽尝试次数或客户端名非法时返回 [`RemoteFailure`]
    fn enroll(
        &self,
        pairing_code: &str,
        client_name: &str,
        read_only: bool,
    ) -> Result<Enrolled, RemoteFailure>;

    /// 校验 token
    ///
    /// # Errors
    ///
    /// token 无效或已被撤销时返回 [`RemoteFailure`]
    fn authenticate(&self, token: &str) -> Result<SessionInfo, RemoteFailure>;

    /// 执行一个已认证的操作
    fn handle(&self, session: &SessionInfo, request: &Request) -> Response;
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::Permission;

    fn session(role: Role) -> SessionInfo {
        SessionInfo {
            client_id: ClientId::generate(),
            role,
        }
    }

    #[test]
    fn session_permissions_follow_the_role_table() {
        let viewer = session(Role::Viewer);
        assert!(viewer.permits(&Request::Ping));
        assert!(!viewer.permits(&Request::JavaScan));

        let operator = session(Role::Operator);
        assert!(operator.permits(&Request::JavaScan));
        assert!(!operator.permits(&Request::Shutdown));

        let admin = session(Role::Admin);
        assert!(admin.permits(&Request::Shutdown));
        assert!(admin.permits(&Request::Stop {
            server_id: "id".to_owned(),
            grace_secs: 30,
            force: false,
        }));
    }

    /// 认证帧自身不需要权限，否则永远无法完成认证
    #[test]
    fn handshake_frames_need_no_permission() {
        let viewer = session(Role::Viewer);
        for request in [
            Request::Hello {
                protocol_version: protocol::remote::REMOTE_PROTOCOL_VERSION,
                client_version: "0.1.0".to_owned(),
            },
            Request::Enroll {
                pairing_code: "1234-5678".to_owned(),
                client_name: "laptop".to_owned(),
                read_only: false,
            },
            Request::Authenticate {
                token: "a".repeat(64),
            },
        ] {
            assert!(request.required_permission().is_none());
            assert!(viewer.permits(&request));
        }
    }

    #[test]
    fn failure_maps_to_an_error_frame() {
        let response = RemoteFailure::unauthorized("token revoked").into_response();
        match response {
            Response::Error { kind, message } => {
                assert_eq!(kind, protocol::remote::ErrorKind::UNAUTHORIZED);
                assert_eq!(message, "token revoked");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// 权限表是协议的一部分：当前帧集覆盖的每个权限都必须真的被用到，
    /// 否则说明有一个权限项永远不会生效（配了等于没配）。
    ///
    /// `Backup` 暂时豁免：备份帧在阶段 9 落地，届时这里的豁免必须删掉，
    /// 否则「备份权限」会一直是个摆设。
    #[test]
    fn every_permission_in_use_is_exercised_by_some_request() {
        let samples = [
            Request::Ping,
            Request::WorkspaceInit {
                dir: "/srv".to_owned(),
                name: None,
            },
            Request::Start {
                server_id: "id".to_owned(),
                ram_mib: None,
                accept_eula: false,
            },
            Request::Shutdown,
            Request::LogsTail {
                server_id: "id".to_owned(),
                lines: 10,
            },
        ];
        let used: Vec<Permission> = samples
            .iter()
            .filter_map(Request::required_permission)
            .collect();
        for permission in Permission::ALL {
            if *permission == Permission::Backup {
                continue;
            }
            assert!(used.contains(permission), "{permission} 没有任何请求使用");
        }
    }
}
