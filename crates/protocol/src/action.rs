//! 动作目录：可执行操作的稳定标识、所需权限与风险等级
//!
//! 每个能读敏感数据或改变状态的操作在这里登记一次。远端帧
//! （[`crate::remote::Request`]）的权限、重试安全与操作名都从这份表取，
//! 禁止再维护第二份「哪个操作要什么权限」的映射——两份表必然分叉。
//!
//! 这里只描述，不执行。谁执行、能不能执行仍由 `domain` / `service` 判定：
//! [`crate::action::ActionSpec::permits`] 给入口在派发前判断「当前角色有没有这项权限」，
//! 不是第二套业务规则。运行态（如运行中拒绝改盘）由 `service` 判定，
//! 权限由调用方（`transport` 的会话、将来的 API / MCP 入口）判定。
//!
//! # 新增一个操作
//!
//! 1. 在下面的 `catalog!` 表里加一行，给出稳定标识、所需权限、风险等级；
//! 2. 远端帧还要在 [`crate::remote::Request::spec`] 的 `match` 里指向它——
//!    该 `match` 没有兜底分支，漏了新变体编译不过；
//! 3. 标识一经发布禁止改名：审计记录、脚本与 MCP 工具名都按它归类。
//!
//! 风险等级取该动作**可能达到的最高影响**。带 `purge` / `dry_run` 之类参数
//! 的动作按最坏情况登记，参数级细化留给审批层，不在这里判断。

use auth::{Permission, Role};
use std::fmt;

/// 动作标识
///
/// 稳定文本，形如 `server.start`。一经发布即是对外承诺，只新增不改名。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId(&'static str);

impl ActionId {
    /// 稳定文本
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ActionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// 动作对持久数据可能造成的最高影响
///
/// 审批层据此决定要不要二次确认；它回答「最坏会怎样」，不回答
/// 「这次会不会这样」——后者要由动作参数决定，不在目录里判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Risk {
    /// 只读：不写持久状态
    ReadOnly,
    /// 可回滚：改动能通过同一动作或既有机制撤销（重新设置、重装、重启、回收站）
    Reversible,
    /// 不可逆：删除或覆盖后没有内置恢复路径（快照、回收站、重装都救不回来）
    Destructive,
}

/// 一个动作的元数据
///
/// 由 [`ACTIONS`] 提供，调用方只读取，不自行构造。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActionSpec {
    /// 稳定标识
    pub id: ActionId,
    /// 所需权限；`None` = 无需认证（握手与认证帧）
    pub permission: Option<Permission>,
    /// 对持久数据可能造成的最高影响
    pub risk: Risk,
    /// 连接断开后能否自动重发
    pub retry_safe: bool,
}

impl ActionSpec {
    /// 该角色能否执行这个动作
    ///
    /// 只做权限表判定；运行态冲突（服务器在跑、目录被占用）不在这里回答。
    #[must_use]
    pub fn permits(self, role: Role) -> bool {
        self.permission
            .is_none_or(|permission| role.permits(permission))
    }
}

/// 声明一个动作：生成同名的 crate 内常量，并追加到 [`ACTIONS`]
///
/// 字段全写，不给默认值——权限与风险必须逐个显式决定，不靠「和上一条一样」。
macro_rules! catalog {
    ($(
        $(#[$meta:meta])*
        $name:ident: $id:literal {
            permission: $permission:expr,
            risk: $risk:ident,
            retry_safe: $retry:literal $(,)?
        }
    ),* $(,)?) => {
        $(
            $(#[$meta])*
            pub(crate) const $name: ActionSpec = ActionSpec {
                id: ActionId($id),
                permission: $permission,
                risk: Risk::$risk,
                retry_safe: $retry,
            };
        )*

        /// 全部动作，按「会话 → 工作区 → 服务器 → Java → 客户端 → 备份 → 内容」排列
        ///
        /// 顺序是展示顺序，不是契约；按标识查找用 [`get`]。
        pub const ACTIONS: &[ActionSpec] = &[$($name),*];
    };
}

catalog! {
    // ---------------------------------------------------------------- 会话
    /// 握手：协商协议版本
    HELLO: "hello" {
        permission: None,
        risk: ReadOnly,
        retry_safe: false,
    },
    /// 首次配对：登记客户端
    ENROLL: "enroll" {
        permission: None,
        risk: Reversible,
        retry_safe: false,
    },
    /// 出示 token 认证
    AUTHENTICATE: "authenticate" {
        permission: None,
        risk: ReadOnly,
        retry_safe: false,
    },
    /// 探活
    PING: "ping" {
        permission: Some(Permission::Read),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 关闭远端服务
    SHUTDOWN: "shutdown" {
        permission: Some(Permission::Manage),
        risk: Reversible,
        retry_safe: false,
    },

    // -------------------------------------------------------------- 工作区
    /// 列出工作区
    WORKSPACE_LIST: "workspace.list" {
        permission: Some(Permission::Read),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 初始化或复用工作目录
    WORKSPACE_INIT: "workspace.init" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 切换当前工作区
    WORKSPACE_USE: "workspace.use" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 注销工作区；`purge` 时删除目录
    WORKSPACE_REMOVE: "workspace.remove" {
        permission: Some(Permission::Write),
        risk: Destructive,
        retry_safe: false,
    },

    // -------------------------------------------------------------- 服务器
    /// 列出服务器
    SERVER_LIST: "server.list" {
        permission: Some(Permission::Read),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 列出可用的服务端变体
    SERVER_VARIANTS: "server.variants" {
        permission: Some(Permission::Read),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 列出某个变体的可用版本
    SERVER_VERSIONS: "server.versions" {
        permission: Some(Permission::Read),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 查询单台服务器的实时状态
    SERVER_STATUS: "server.status" {
        permission: Some(Permission::Read),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 批量查询实时状态
    STATUS_MANY: "status.many" {
        permission: Some(Permission::Read),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 建服：目录、登记、装核心
    SERVER_CREATE: "server.create" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 用整合包清单建服
    SERVER_CREATE_MODPACK: "server.create_modpack" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 把整合包装进已有服务器
    SERVER_INSTALL_MODPACK: "server.install_modpack" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 安装或更换服务端核心
    SERVER_INSTALL: "server.install" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 把外部目录链接为服务器
    SERVER_LINK: "server.link" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 删除服务器；`purge` 时删除目录
    SERVER_REMOVE: "server.remove" {
        permission: Some(Permission::Write),
        risk: Destructive,
        retry_safe: false,
    },
    /// 启动服务器
    SERVER_START: "server.start" {
        permission: Some(Permission::Control),
        risk: Reversible,
        retry_safe: false,
    },
    /// 停止服务器
    SERVER_STOP: "server.stop" {
        permission: Some(Permission::Control),
        risk: Reversible,
        retry_safe: false,
    },
    /// 重启服务器
    SERVER_RESTART: "server.restart" {
        permission: Some(Permission::Control),
        risk: Reversible,
        retry_safe: false,
    },
    /// 向控制台注入一行
    SERVER_SEND: "server.send" {
        permission: Some(Permission::Control),
        risk: Reversible,
        retry_safe: false,
    },
    /// 读控制台日志
    ///
    /// 输出可能含玩家名、IP、管理员敲下的命令，比元数据更敏感，单列
    /// `Logs` 而不是并进 `Read`。
    SERVER_LOGS: "server.logs" {
        permission: Some(Permission::Logs),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 读 `server.properties`
    SERVER_PROPS_GET: "server.props.get" {
        permission: Some(Permission::Read),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 写 `server.properties` 的单项
    SERVER_PROPS_SET: "server.props.set" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 写服务属性（内存、Java 版本等）
    SERVER_SET: "server.set" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },

    // ---------------------------------------------------------------- Java
    /// 列出已注册的运行时
    JAVA_LIST: "java.list" {
        permission: Some(Permission::Read),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 扫描本机运行时并登记
    JAVA_SCAN: "java.scan" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 下载安装运行时
    JAVA_INSTALL: "java.install" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 删除托管运行时或注销系统运行时
    JAVA_UNINSTALL: "java.uninstall" {
        permission: Some(Permission::Write),
        risk: Destructive,
        retry_safe: false,
    },

    // -------------------------------------------------------------- 客户端
    /// 列出已配对客户端
    ///
    /// 名册是访问控制数据：能看就能做社会工程，按 `Manage` 处理。
    CLIENT_LIST: "client.list" {
        permission: Some(Permission::Manage),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 撤销某个客户端
    CLIENT_REVOKE: "client.revoke" {
        permission: Some(Permission::Manage),
        risk: Reversible,
        retry_safe: false,
    },
    /// 撤销自己
    ///
    /// 删掉自己不改变他人的可达性，不构成提权；要求 `Manage` 会让默认的
    /// `operator` 无法自己退出名册。
    CLIENT_SELF_REVOKE: "client.self.revoke" {
        permission: None,
        risk: Reversible,
        retry_safe: false,
    },

    // ---------------------------------------------------------------- 备份
    // 备份写对端磁盘并可能覆盖服务器目录，比 `Write` 更重，整组单列 `Backup`。
    /// 建快照
    BACKUP_CREATE: "backup.create" {
        permission: Some(Permission::Backup),
        risk: Reversible,
        retry_safe: false,
    },
    /// 快照列表
    BACKUP_LIST: "backup.list" {
        permission: Some(Permission::Backup),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 恢复快照（原地恢复会覆盖服务器目录）
    BACKUP_RESTORE: "backup.restore" {
        permission: Some(Permission::Backup),
        risk: Destructive,
        retry_safe: false,
    },
    /// 删除快照记录
    BACKUP_DELETE: "backup.delete" {
        permission: Some(Permission::Backup),
        risk: Destructive,
        retry_safe: false,
    },
    /// 校验快照引用的 CAS 对象
    BACKUP_VERIFY: "backup.verify" {
        permission: Some(Permission::Backup),
        risk: ReadOnly,
        retry_safe: true,
    },
    /// 按保留策略清理快照
    BACKUP_PRUNE: "backup.prune" {
        permission: Some(Permission::Backup),
        risk: Destructive,
        retry_safe: false,
    },

    // ---------------------------------------------------------------- 内容
    /// 安装 mod / 插件
    CONTENT_INSTALL: "content.install" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 更新已装的 mod / 插件
    CONTENT_UPDATE: "content.update" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
    /// 移除 mod / 插件（移进回收站）
    CONTENT_REMOVE: "content.remove" {
        permission: Some(Permission::Write),
        risk: Reversible,
        retry_safe: false,
    },
}

/// 按标识查找动作
///
/// 未登记返回 `None`：调用方应把它当作参数错误，不要猜一个默认权限。
#[must_use]
pub fn get(id: &str) -> Option<ActionSpec> {
    ACTIONS.iter().copied().find(|spec| spec.id.as_str() == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 标识唯一：审计记录、脚本与工具名都按它归类
    #[test]
    fn ids_are_unique() {
        let mut seen = HashSet::new();
        for spec in ACTIONS {
            assert!(
                seen.insert(spec.id.as_str()),
                "{} 重复登记",
                spec.id.as_str()
            );
        }
    }

    /// 标识形如 `domain.operation`：小写字母、数字与下划线，段间用点
    ///
    /// 规范化是为了让 `get` 的用法可预测；大小写混排会让同一个操作出现两种写法。
    #[test]
    fn ids_are_well_formed() {
        for spec in ACTIONS {
            let id = spec.id.as_str();
            assert!(!id.is_empty(), "空标识");
            for segment in id.split('.') {
                assert!(!segment.is_empty(), "{id} 含空段");
                assert!(
                    segment
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "{id} 的段 {segment} 只允许小写字母、数字与下划线"
                );
            }
        }
    }

    /// 不可逆动作必须要求权限：没有权限的破坏性操作无法拦住
    #[test]
    fn destructive_actions_require_permission() {
        for spec in ACTIONS {
            if spec.risk == Risk::Destructive {
                assert!(
                    spec.permission.is_some(),
                    "{} 是破坏性动作，必须要求权限",
                    spec.id.as_str()
                );
            }
        }
    }

    /// 破坏性动作不得在断线后自动重发：重发会造成第二次删除
    #[test]
    fn destructive_actions_are_not_retry_safe() {
        for spec in ACTIONS {
            if spec.risk == Risk::Destructive {
                assert!(
                    !spec.retry_safe,
                    "{} 是破坏性动作，不能自动重发",
                    spec.id.as_str()
                );
            }
        }
    }

    /// 标识的两种文本形态一致
    #[test]
    fn id_display_matches_as_str() {
        let id = get("server.start").expect("server.start").id;
        assert_eq!(id.as_str(), "server.start");
        assert_eq!(id.to_string(), "server.start");
    }

    /// 查找往返，未知标识返回 `None`
    #[test]
    fn lookup_round_trips_and_rejects_unknown() {
        for spec in ACTIONS {
            let found = get(spec.id.as_str()).expect("已登记的动作必须能查到");
            assert_eq!(found, *spec);
        }
        assert!(get("server.nope").is_none());
        assert!(get("").is_none());
        assert!(get("SERVER.START").is_none(), "查找区分大小写");
    }

    /// 权限判定与角色表一致
    #[test]
    fn permits_follows_the_role_table() {
        let read = get("server.list").expect("server.list");
        assert!(read.permits(Role::Viewer));
        assert!(read.permits(Role::Operator));

        let control = get("server.start").expect("server.start");
        assert!(!control.permits(Role::Viewer), "viewer 不得控进程");
        assert!(control.permits(Role::Operator));

        let manage = get("client.revoke").expect("client.revoke");
        assert!(!manage.permits(Role::Operator), "operator 不得管理名册");
        assert!(manage.permits(Role::Admin));

        let session = get("hello").expect("hello");
        for role in [Role::Viewer, Role::Operator, Role::Admin] {
            assert!(session.permits(role), "握手不要求权限");
        }
    }

    /// 已发布的标识集合
    ///
    /// 这份清单是对外承诺的冻结快照：新增动作要同时加进来，改名或删除会
    /// 直接让测试失败。审计记录、脚本与将来的 MCP 工具名都按这些标识归类。
    #[test]
    fn published_ids_are_stable() {
        let published = [
            "authenticate",
            "backup.create",
            "backup.delete",
            "backup.list",
            "backup.prune",
            "backup.restore",
            "backup.verify",
            "client.list",
            "client.revoke",
            "client.self.revoke",
            "content.install",
            "content.remove",
            "content.update",
            "enroll",
            "hello",
            "java.install",
            "java.list",
            "java.scan",
            "java.uninstall",
            "ping",
            "server.create",
            "server.create_modpack",
            "server.install",
            "server.install_modpack",
            "server.link",
            "server.list",
            "server.logs",
            "server.props.get",
            "server.props.set",
            "server.remove",
            "server.restart",
            "server.send",
            "server.set",
            "server.start",
            "server.status",
            "server.stop",
            "server.variants",
            "server.versions",
            "shutdown",
            "status.many",
            "workspace.init",
            "workspace.list",
            "workspace.remove",
            "workspace.use",
        ];

        let mut actual: Vec<&str> = ACTIONS.iter().map(|spec| spec.id.as_str()).collect();
        actual.sort_unstable();
        assert_eq!(actual, published);
    }
}
