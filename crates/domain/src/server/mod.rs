// TODO: 领域类型尚未接入状态机与规则
#![allow(dead_code)]

pub(crate) struct ServerVersion {
    version: String,
}

pub(crate) enum ServerVariant {
    Vanilla,
    Fabric,
    Forge,
    NeoForge,
    Paper,
}
