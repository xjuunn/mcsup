//! 契约路由：宿主中介的插件间调用
//!
//! 插件只发一行 `call`，带 `contract`（可带 `target` 与 `trace`）；宿主解析
//! 契约、找提供方、按多重性选目标、压调用链、执行并把结果聚合回调用方。
//! 插件之间没有直连，端点到进程的细节都在 [`PluginHost`]。
//!
//! # 调用链
//!
//! 根调用由宿主分配 `trace`，与当前调用链、截止时间和共享计数一起登记。
//! 提供方嵌套调用时把 `trace` 原样带回；宿主据此拒绝循环、深度越界与扇出
//! 越界。判定在 [`domain::plugin::contract`]，本模块只执行。
//!
//! # 广播
//!
//! 广播给所有提供方，逐个执行并收集结果。只要有一个成功就返回 `Ok`，全部
//! 失败才返回错误；`failures` 原样保留，不静默吞掉。

use crate::host::PluginHost;
use domain::plugin::PluginId;
use domain::plugin::contract::{CallChain, CallChainError, Contract, ContractId, ensure_fanout};
use port::plugin::{
    AuditEvent, AuditResult, ContractCall, ContractInvoker, HostError, PluginRecord,
};
use protocol::plugin::WireError;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// 宿主向提供方投递契约调用时使用的 op 名
pub const CONTRACT_OP: &str = "contract.call";

/// 调用链登记项
struct TraceState {
    /// 到此为止的调用链
    chain: CallChain,
    /// 整棵调用树共享的调用计数
    calls: Arc<Mutex<u32>>,
    /// 整棵调用树的截止时间
    deadline: Instant,
}

/// 宿主内契约路由器
pub(crate) struct ContractRouter {
    /// 自引用弱指针：路由需要会话表，环由 [`PluginHost::new`] 在构造后补齐
    host: OnceLock<Weak<PluginHost>>,
    /// 进行中的调用树
    traces: Mutex<HashMap<String, TraceState>>,
}

impl std::fmt::Debug for ContractRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let active = self.traces.lock().map_or(0, |traces| traces.len());
        f.debug_struct("ContractRouter")
            .field("active_traces", &active)
            .finish_non_exhaustive()
    }
}

impl ContractRouter {
    pub(crate) fn new() -> Self {
        Self {
            host: OnceLock::new(),
            traces: Mutex::new(HashMap::new()),
        }
    }

    /// 构造后立即绑定宿主；重复绑定忽略
    pub(crate) fn attach(&self, host: &Arc<PluginHost>) {
        let _ = self.host.set(Arc::downgrade(host));
    }

    fn host(&self) -> Result<Arc<PluginHost>, HostError> {
        self.host
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| HostError::internal("contract router is not attached to a host"))
    }

    /// 登记一次调用并返回子 trace 与截止时间
    fn enter(
        &self,
        call: &ContractCall,
        contract: Contract,
        providers: u32,
    ) -> Result<(String, Instant), HostError> {
        let now = Instant::now();
        let contract_deadline = now + contract.timeout();
        let mut traces = self
            .traces
            .lock()
            .map_err(|_| HostError::internal("contract trace table poisoned"))?;
        traces.retain(|_, state| state.deadline > now);
        let (mut chain, calls, parent_deadline) = match &call.trace {
            Some(trace) => {
                let state = traces.get(trace).ok_or_else(|| {
                    HostError::conflict(format!("unknown or expired contract trace {trace}"))
                })?;
                (
                    state.chain.clone(),
                    Arc::clone(&state.calls),
                    Some(state.deadline),
                )
            }
            None => (CallChain::new(), Arc::new(Mutex::new(0)), None),
        };
        chain
            .push(call.caller.clone(), contract)
            .map_err(|err| match err {
                CallChainError::Cycle { .. } => HostError::conflict(err.to_string()),
                CallChainError::TooDeep | CallChainError::TooBroad => {
                    HostError::limit_exceeded(err.to_string())
                }
            })?;
        let deadline =
            parent_deadline.map_or(contract_deadline, |parent| parent.min(contract_deadline));
        {
            let mut count = calls
                .lock()
                .map_err(|_| HostError::internal("contract fanout counter poisoned"))?;
            *count = ensure_fanout(*count, providers).map_err(|err| {
                HostError::limit_exceeded(format!("{} (caller: {})", err, call.caller))
            })?;
        }
        let trace = crate::host::new_token();
        traces.insert(
            trace.clone(),
            TraceState {
                chain,
                calls,
                deadline,
            },
        );
        Ok((trace, deadline))
    }

    fn finish(&self, trace: &str) {
        if let Ok(mut traces) = self.traces.lock() {
            traces.remove(trace);
        }
    }

    /// 路由一次契约调用
    fn route(&self, call: &ContractCall) -> Result<Value, HostError> {
        let host = self.host()?;
        let id = ContractId::parse(&call.contract)
            .map_err(|err| HostError::invalid_input(format!("unknown contract: {err}")))?;
        let contract = id.contract();
        let caller = host.record_of(&call.caller)?;
        if !declares(&caller.manifest.consumes, id) {
            return Err(HostError::denied(format!(
                "plugin {} does not declare consumes for {}",
                call.caller, call.contract
            )));
        }
        let providers = providers_of(&host.records()?, id);
        let targets = select_targets(contract, &providers, call.target.as_ref())?;
        if targets.is_empty() {
            // 广播没有提供方：空结果不算失败。
            return Ok(json!({ "responses": [], "failures": [] }));
        }
        let additional = u32::try_from(targets.len()).unwrap_or(u32::MAX);
        let (trace, deadline) = self.enter(call, contract, additional)?;
        let result = match contract.multiplicity() {
            domain::plugin::contract::Multiplicity::Broadcast => {
                broadcast(&host, call, &targets, &trace, deadline)
            }
            domain::plugin::contract::Multiplicity::Unicast => {
                unicast(&host, call, &targets, &trace, deadline)
            }
        };
        self.finish(&trace);
        result
    }
}

fn unicast(
    host: &PluginHost,
    call: &ContractCall,
    targets: &[PluginId],
    trace: &str,
    deadline: Instant,
) -> Result<Value, HostError> {
    let target = &targets[0];
    let started = Instant::now();
    let result = invoke(host, target, call, trace, deadline);
    audit(host, call, target, started.elapsed(), &result);
    result
}

fn broadcast(
    host: &PluginHost,
    call: &ContractCall,
    targets: &[PluginId],
    trace: &str,
    deadline: Instant,
) -> Result<Value, HostError> {
    let mut responses = Vec::new();
    let mut failures = Vec::new();
    let mut first_error = None;
    for target in targets {
        let started = Instant::now();
        let result = invoke(host, target, call, trace, deadline);
        audit(host, call, target, started.elapsed(), &result);
        match result {
            Ok(output) => responses.push(json!({
                "plugin_id": target.as_str(),
                "output": output,
            })),
            Err(err) => {
                let code = err.code;
                let wire = WireError::from_code(code, err.message.clone());
                failures.push(json!({
                    "plugin_id": target.as_str(),
                    "error": serde_json::to_value(&wire).unwrap_or(Value::Null),
                }));
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }
    if responses.is_empty() {
        let failures_text = serde_json::to_string(&failures).unwrap_or_else(|_| "[]".to_owned());
        let code = first_error.map_or(port::plugin::ErrorCode::Unavailable, |err| err.code);
        return Err(HostError::new(
            code,
            format!("all contract providers failed: {failures_text}"),
        ));
    }
    Ok(json!({ "responses": responses, "failures": failures }))
}

fn audit(
    host: &PluginHost,
    call: &ContractCall,
    provider: &PluginId,
    elapsed: Duration,
    result: &Result<Value, HostError>,
) {
    let detail = json!({
        "caller": call.caller.as_str(),
        "provider": provider.as_str(),
        "contract": call.contract,
        "result": match result {
            Ok(_) => "ok",
            Err(err) => err.code.as_str(),
        },
        "duration_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    });
    host.audit().record(&AuditEvent {
        at: crate::host::now_secs(),
        plugin: provider.to_string(),
        action: "plugin.contract".to_owned(),
        capability: None,
        result: match result {
            Ok(_) => AuditResult::Allowed,
            Err(err) if err.code == port::plugin::ErrorCode::Denied => AuditResult::Denied,
            Err(_) => AuditResult::Failed,
        },
        detail: Some(detail.to_string()),
    });
}

impl ContractInvoker for ContractRouter {
    fn invoke(&self, call: ContractCall) -> Result<Value, HostError> {
        self.route(&call)
    }
}

fn invoke(
    host: &PluginHost,
    target: &PluginId,
    call: &ContractCall,
    trace: &str,
    deadline: Instant,
) -> Result<Value, HostError> {
    let timeout = deadline.saturating_duration_since(Instant::now());
    if timeout.is_zero() {
        return Err(HostError::new(
            port::plugin::ErrorCode::Timeout,
            format!(
                "contract {} timed out before provider {target} could run",
                call.contract
            ),
        ));
    }
    let input = json!({
        "contract": call.contract,
        "caller": call.caller.as_str(),
        "trace": trace,
        "input": call.input,
    });
    host.call_plugin(target, CONTRACT_OP, input, timeout)
}

/// 记录里是否声明了某个契约
fn declares(declared: &[String], id: ContractId) -> bool {
    declared
        .iter()
        .filter_map(|raw| ContractId::parse(raw).ok())
        .any(|candidate| candidate == id)
}

fn providers_of(records: &[PluginRecord], id: ContractId) -> Vec<PluginId> {
    records
        .iter()
        .filter(|record| runnable(record))
        .filter(|record| declares(&record.manifest.provides, id))
        .map(|record| record.id.clone())
        .collect()
}

fn runnable(record: &PluginRecord) -> bool {
    record.enabled && !record.blocked && !record.quarantined && !record.needs_approval()
}

fn select_targets(
    contract: Contract,
    providers: &[PluginId],
    target: Option<&PluginId>,
) -> Result<Vec<PluginId>, HostError> {
    use domain::plugin::contract::Multiplicity;
    match contract.multiplicity() {
        Multiplicity::Broadcast => {
            if target.is_some() {
                return Err(HostError::invalid_input(format!(
                    "{contract} is broadcast; target is only valid on unicast contracts"
                )));
            }
            Ok(providers.to_vec())
        }
        Multiplicity::Unicast => {
            if let Some(target) = target {
                if providers.iter().any(|provider| provider == target) {
                    Ok(vec![target.clone()])
                } else {
                    Err(HostError::not_found(format!(
                        "plugin {target} does not provide {contract}"
                    )))
                }
            } else if providers.is_empty() {
                Err(HostError::not_found(format!("no provider for {contract}")))
            } else if providers.len() == 1 {
                Ok(providers.to_vec())
            } else {
                Err(HostError::conflict(format!(
                    "{contract} has {} providers; target is required",
                    providers.len()
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: &str) -> PluginId {
        PluginId::parse(raw).expect("id")
    }

    fn record(raw: &str, provides: &[&str], enabled: bool) -> PluginRecord {
        PluginRecord {
            id: id(raw),
            version: "0.1.0".to_owned(),
            api: 1,
            scope: domain::plugin::PluginScope::Machine,
            server: None,
            enabled,
            approved: std::collections::BTreeSet::new(),
            manifest: domain::plugin::manifest::Manifest {
                schema: 1,
                id: raw.to_owned(),
                name: raw.to_owned(),
                description: None,
                version: "0.1.0".to_owned(),
                api: 1,
                entry: domain::plugin::manifest::EntrySpec {
                    kind: "process".to_owned(),
                    cmd: "bin/plugin".to_owned(),
                    platforms: std::collections::BTreeMap::new(),
                },
                scope: None,
                resident: false,
                capabilities: Vec::new(),
                net: None,
                subscriptions: Vec::new(),
                provides: provides.iter().map(|raw| (*raw).to_owned()).collect(),
                consumes: Vec::new(),
                resources: None,
                publisher: None,
                license: "MIT".to_owned(),
                source: None,
            },
            artifact_dir: std::path::PathBuf::new(),
            installed_at: 0,
            updated_at: 0,
            last_state: domain::plugin::state::PluginState::Stopped,
            last_error: None,
            previous_version: None,
            source: None,
            sha256: None,
            signed: false,
            blocked: false,
            quarantined: false,
        }
    }

    #[test]
    fn broadcast_without_target_lists_all_runnable_providers() {
        let contract = ContractId::parse("chat.publish@1").expect("contract");
        let records = vec![
            record("alpha", &["chat.publish@1"], true),
            record("beta", &["chat.publish@1"], false),
            record("gamma", &["metrics.sample@1"], true),
        ];
        let providers = providers_of(&records, contract);
        assert_eq!(providers, vec![id("alpha")]);
        assert_eq!(
            select_targets(contract.contract(), &providers, None).expect("targets"),
            vec![id("alpha")]
        );
        assert!(select_targets(contract.contract(), &providers, Some(&id("beta"))).is_err());
    }

    #[test]
    fn unicast_requires_target_when_multiple_providers() {
        let contract = ContractId::parse("chat.ask@1").expect("contract");
        let providers = vec![id("alpha"), id("beta")];
        assert_eq!(
            select_targets(contract.contract(), &providers, None)
                .expect_err("ambiguous")
                .code,
            port::plugin::ErrorCode::Conflict
        );
        assert_eq!(
            select_targets(contract.contract(), &providers, Some(&id("beta"))).expect("target"),
            vec![id("beta")]
        );
        assert_eq!(
            select_targets(contract.contract(), &providers, Some(&id("gamma")))
                .expect_err("not provider")
                .code,
            port::plugin::ErrorCode::NotFound
        );
    }

    #[test]
    fn unicast_without_provider_is_not_found() {
        let contract = ContractId::parse("agent.converse@1").expect("contract");
        assert_eq!(
            select_targets(contract.contract(), &[], None)
                .expect_err("none")
                .code,
            port::plugin::ErrorCode::NotFound
        );
    }
}
