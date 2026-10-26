//! 认证与授权：配对码、token、角色与撤销
//!
//! 这一层只做**判定**与**凭证格式**，不做网络、不碰配置文件、不决定存哪：
//!
//! - 配对码（[`PairingCode`]）：短时有效、单次使用、限速尝试
//! - token（[`Token`]）：不透明随机串，服务端只存摘要（[`TokenDigest`]）
//! - 角色与权限（[`Role`] / [`Permission`]）：Admin / Operator / Viewer
//! - 客户端身份（[`ClientId`]）与撤销（[`RevocationList`]）
//! - 凭证落盘形态（[`CredentialStore`] / [`FileCredentialStore`]）：0600 文件
//!
//! 放在独立 crate 而不是 `service`，是因为它同时被两侧使用：agent 侧要签发与
//! 校验，客户端侧要保存与出示；而 `service` 不允许依赖任何实现层，把凭证格式
//! 塞进 `service` 会让「存到哪里」渗进业务编排。
//!
//! 与 `domain` 无关：身份与权限不是 Minecraft 的领域概念，不该进 `domain`。

#![warn(missing_docs)]

pub mod code;
pub mod credential;
pub mod token;

pub use code::{PairingCode, PairingCodeError, PairingCodeStore, PairingWindow};
pub use credential::{CredentialError, CredentialStore, FileCredentialStore, StoredCredential};
pub use token::{ClientId, Permission, RevocationList, Role, Token, TokenDigest, TokenError};
