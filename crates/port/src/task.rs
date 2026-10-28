//! 耗时任务的观察者：本地与远端**同一套**阶段 / 进度 / 输出回调
//!
//! 一段安装（下载核心、装 Java、铺整合包、写快照）在本地由 `service` 直接
//! 执行，在远端由 agent 上的**同一份** `service` 执行。两者的差别只有「在
//! 哪里跑」，用户看到的东西必须一致：同样的阶段文案、同样的进度条、同样的
//! 安装器输出。
//!
//! 因此观察者定义在契约层，而不是各入口：
//!
//! - `service` 只往 [`TaskObserver`] 上抛事件，不关心谁在听
//! - 本地入口把它接到终端渲染（`apps/cli/src/progress.rs`）
//! - 远端把同一组事件装进 `Response::Task` 帧回传，客户端用**同一个**渲染器
//!
//! 事件是旁路：观察者写失败（终端关了、连接断了）不得影响业务本身，实现
//! 约定是「回调不返回错误」。
//!
//! # 与下载 / 安装器细节的关系
//!
//! [`TaskObserver`] 是**入口级**的唯一观察者；[`ProgressSink`] 与
//! [`InstallerOutputSink`] 是下载器 / 安装器执行器需要的窄接口（只要字节或
//! 只要一行输出）。前者是后两者的超 trait，因此一个观察者可以按需上转成
//! 窄接口交给下层，不需要在每层再传一遍。
//!
//! [`ProgressSink`]: crate::ProgressSink
//! [`InstallerOutputSink`]: crate::InstallerOutputSink

use crate::{InstallerOutputSink, ProgressSink};
use serde::{Deserialize, Serialize};

/// 耗时任务的阶段
///
/// 文案由领域层给出，所有入口共用：入口不得另写一份「正在下载…」之类的话，
/// 否则本地与远端、CLI 与将来的 API 会各说各的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TaskStage {
    /// 探测并排序下载源
    Probing,
    /// 查询可用版本
    Listing,
    /// 解析下载地址
    Resolving,
    /// 下载产物
    Downloading,
    /// 下载安装器本体（`Forge` / `NeoForge` 等 install 方式的来源）
    DownloadingInstaller,
    /// 运行安装器产出服务端目录：耗时可长达数分钟
    Installing,
    /// 解压并注册
    Extracting,
    /// 登记结果（写注册表 / 元数据）
    Finalizing,
    /// 扫描待处理文件
    Scanning,
    /// 写入数据（快照 / 恢复）
    Writing,
    /// 校验现有数据
    Verifying,
    /// 恢复快照
    Restoring,
    /// 部署整合包文件
    Applying,
}

impl TaskStage {
    /// 面向用户的短文案
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Probing => "探测下载源…",
            Self::Listing => "查询可用版本…",
            Self::Resolving => "解析下载地址…",
            Self::Downloading => "下载中…",
            Self::DownloadingInstaller => "下载安装器…",
            Self::Installing => "运行安装器…",
            Self::Extracting => "解压并注册…",
            Self::Finalizing => "登记服务端核心…",
            Self::Scanning => "扫描文件…",
            Self::Writing => "写入数据…",
            Self::Verifying => "校验数据…",
            Self::Restoring => "恢复快照…",
            Self::Applying => "部署整合包…",
        }
    }

    /// 该阶段是否处于「传输字节」状态
    ///
    /// 渲染层用它决定要不要把这一行交给进度条：只有下载才谈得上
    /// `transferred / total`，其余阶段是旋转文案。
    #[must_use]
    pub const fn is_transfer(self) -> bool {
        matches!(self, Self::Downloading | Self::DownloadingInstaller)
    }
}

/// 一次任务事件
///
/// 线上（`protocol::remote::Response::Task`）与本地回调共用同一份定义：
/// 事件的种类只能有一处，新增种类时两端一起变，不会出现「本地有、远端没有」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TaskEvent {
    /// 进入新阶段
    Stage {
        /// 阶段
        stage: TaskStage,
    },
    /// 传输进度；总量未知时为 `None`
    Progress {
        /// 已处理的字节 / 条目数
        done: u64,
        /// 总量
        total: Option<u64>,
    },
    /// 子进程（安装器）输出的一行
    Output {
        /// 已去掉行尾换行的原文
        line: String,
    },
}

/// 耗时任务的观察者
///
/// 三个回调都有默认空实现：只关心阶段或只关心进度的实现不必写满。
///
/// 实现约定：`on_*` 必须**不阻塞、不失败**到影响调用方业务——事件是旁路，
/// 不能让「终端写不进去」把安装变成失败。
pub trait TaskObserver: ProgressSink + InstallerOutputSink + Send + Sync {
    /// 进入新阶段
    fn on_stage(&self, _stage: TaskStage) {}

    /// 把一条线上事件转成回调
    ///
    /// 本地与远端共用它：远端把事件装帧回传，客户端解帧后仍走这里，
    /// 因此「收到一条事件该做什么」只有一处实现。
    fn apply(&self, event: &TaskEvent) {
        match event {
            TaskEvent::Stage { stage } => self.on_stage(*stage),
            TaskEvent::Progress { done, total } => self.on_progress(*done, *total),
            TaskEvent::Output { line } => self.on_output(line),
        }
    }
}

/// 丢弃全部事件的默认观察者
///
/// 没有订阅者时用它占位，避免 `Option<&dyn TaskObserver>` 到处判断。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTaskObserver;

impl ProgressSink for NoopTaskObserver {}

impl InstallerOutputSink for NoopTaskObserver {}

impl TaskObserver for NoopTaskObserver {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<String>>,
    }

    impl ProgressSink for Recorder {
        fn on_progress(&self, done: u64, total: Option<u64>) {
            self.seen
                .lock()
                .expect("lock")
                .push(format!("progress:{done}/{total:?}"));
        }
    }

    impl InstallerOutputSink for Recorder {
        fn on_output(&self, line: &str) {
            self.seen
                .lock()
                .expect("lock")
                .push(format!("output:{line}"));
        }
    }

    impl TaskObserver for Recorder {
        fn on_stage(&self, stage: TaskStage) {
            self.seen
                .lock()
                .expect("lock")
                .push(format!("stage:{}", stage.message()));
        }
    }

    /// `apply` 是本地与远端共用的唯一分发点：三种事件都要落到对应回调
    #[test]
    fn apply_dispatches_every_event_kind() {
        let recorder = Recorder::default();
        recorder.apply(&TaskEvent::Stage {
            stage: TaskStage::Downloading,
        });
        recorder.apply(&TaskEvent::Progress {
            done: 7,
            total: Some(9),
        });
        recorder.apply(&TaskEvent::Output {
            line: "libraries".to_owned(),
        });
        assert_eq!(
            recorder.seen.lock().expect("lock").as_slice(),
            [
                format!("stage:{}", TaskStage::Downloading.message()),
                "progress:7/Some(9)".to_owned(),
                "output:libraries".to_owned(),
            ]
        );
    }

    /// 未覆写的回调必须静默：只关心进度的实现不该被迫写空函数
    #[test]
    fn default_callbacks_are_noops() {
        struct OnlyProgress;
        impl ProgressSink for OnlyProgress {
            fn on_progress(&self, _done: u64, _total: Option<u64>) {}
        }
        impl InstallerOutputSink for OnlyProgress {}
        impl TaskObserver for OnlyProgress {}
        OnlyProgress.on_stage(TaskStage::Probing);
        OnlyProgress.on_output("x");
        NoopTaskObserver.on_progress(1, None);
    }

    /// 阶段 → 线的语义由契约给出：下载交给进度条，其余交给 spinner
    #[test]
    fn transfer_stages_are_the_download_ones() {
        assert!(TaskStage::Downloading.is_transfer());
        assert!(TaskStage::DownloadingInstaller.is_transfer());
        assert!(!TaskStage::Installing.is_transfer());
        assert!(!TaskStage::Scanning.is_transfer());
    }

    /// 线上编码稳定：远端帧按这些名字收发，改名即破坏兼容
    #[test]
    fn stage_names_are_stable_on_the_wire() {
        let json = serde_json::to_string(&TaskEvent::Stage {
            stage: TaskStage::DownloadingInstaller,
        })
        .expect("encode");
        assert_eq!(json, r#"{"event":"stage","stage":"downloading_installer"}"#);
    }
}
