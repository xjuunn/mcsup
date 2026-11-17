//! 单次调用的执行期参数：进度观察者与取消令牌
//!
//! 这两项是「这次怎么跑」而不是「跑什么」，因此不进 [`super::KitConfig`]：
//! 取消令牌若跨调用复用，取消一次就会把之后所有调用一起取消。

use port::{CancellationToken, TaskObserver};
use std::fmt;
use std::sync::Arc;

/// 单次任务的执行期参数
///
/// 默认两者都没有：没有观察者时进度被丢弃，没有取消令牌时内部新建一个
/// （永不取消）。长任务（装核心、装 mod）应当挂上观察者，否则调用方看不到
/// 阶段与字节进度。
///
/// ```no_run
/// use mcsup::kit::RunOptions;
/// use mcsup::port::CancellationToken;
///
/// let cancel = CancellationToken::new();
/// let run = RunOptions::default().cancel(cancel.clone());
/// // 别的线程调用 cancel.cancel() 即可协作取消
/// ```
#[derive(Clone, Default)]
pub struct RunOptions {
    observer: Option<Arc<dyn TaskObserver>>,
    cancel: Option<CancellationToken>,
}

impl RunOptions {
    /// 挂上进度观察者
    ///
    /// 用 `Arc` 而不是借用：观察者经常同时被入口与后台任务持有，借用会让
    /// 每次调用都多一个生命周期参数。
    #[must_use]
    pub fn observer(mut self, observer: Arc<dyn TaskObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// 挂上取消令牌
    #[must_use]
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// 本次调用使用的观察者
    pub(crate) fn observer_ref(&self) -> Option<&dyn TaskObserver> {
        self.observer.as_deref()
    }

    /// 本次调用使用的取消令牌；没设置时新建一个永不取消的
    pub(crate) fn token(&self) -> CancellationToken {
        self.cancel.clone().unwrap_or_default()
    }
}

impl fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunOptions")
            .field("observer", &self.observer.is_some())
            .field("cancel", &self.cancel.is_some())
            .finish()
    }
}
