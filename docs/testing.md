# MCSUP 测试规范

来自 `AGENTS.md`。

设计测试、评估测试覆盖时查阅。

---

# 禁止过早优化

性能优先不等于盲目优化。顺序是 Correctness → Measure → Identify Bottleneck
→ Optimize → Benchmark → Verify。没有 Benchmark 或 Profiling 依据，不得因为
「感觉可能慢」引入复杂缓存、Lock-free 或复杂线程池。

# 测试是开发过程的一部分

任何新功能都必须同时编写测试，禁止「先写代码，以后再补」。顺序是设计 → 测试
设计 → 实现 → 运行测试 → 性能验证 → Review。

# 测试分层

- Unit：Domain、State、Parser、Validator、Backup、Hash、Runtime、Provider、
  Permission、Utility。放 `tests/` 或模块内 `#[cfg(test)] mod tests`。
- Integration：Service、Storage、Runtime、Backup、File、Provider，验证多个
  组件协作。
- API：Request / Response、Validation、AuthN / AuthZ、Error、Pagination、
  WebSocket。
- RPC：Connection、AuthN、Request / Response、Streaming、Timeout、
  Cancellation、版本不匹配、连接失败。
- CLI：command、argument、output、exit code、error、json。

# 四层测试架构

上面按「测什么」分层，这里按「测到哪个接缝」分层。两者并存：前者决定用例
内容，后者决定用例放哪、允许依赖谁。

| 层 | 名称 | 位置 | 允许依赖 | 回答的问题 |
| --- | --- | --- | --- | --- |
| L0 | Unit | 模块内 `#[cfg(test)] mod tests` | 本 crate | 这个函数在边界上对不对 |
| L1 | Contract | 实现 crate 的 `tests/` | crate 本体 + `testkit` | 实现是否满足 trait 的契约 |
| L2 | Component | 实现 crate 的 `tests/` | crate 本体 + `testkit` | 组件在故障下是否自洽 |
| L3 | Orchestration | `crates/service/tests/` | `service` + `testkit` | 多组件协作时时序与收场对不对 |
| L4 | Entry E2E | `apps/*/tests/` | 真实二进制 | 用户看到的退出码、stdout、落盘格式对不对 |

## L0 Unit

判定逻辑与解析函数就近测试。不引入 `testkit`，避免底层测试被夹具绑死。

## L1 Contract

一个 trait 有多个实现时，契约只写一份，放在 `testkit`；每个实现 crate 的
`tests/` 里只做一行装配，不写断言。

```rust
// crates/backup/tests/cas_contract.rs
#[test]
fn cas_store_satisfies_contract() {
    testkit::cas_contract::run(|root| Box::new(CasStore::new(root)));
}
```

契约本身必须自检：`testkit` 里有一条用例喂「哈希与内容无关」的坏实现，
断言它**必须**失败。没有这条自检，契约退化成永远绿的装饰品。

## L2 Component

在 L1 之上加故障注入：用装饰器包住真实实现，而不是替换它。

```rust
let store = FailingContentStore::new(Arc::new(CasStore::new(root)), CasOp::Put);
```

断言的对象是「真实实现出错时上层怎么收场」，不是「桩怎么出错」。

## L3 Orchestration

跨组件时序：`save-off` 之后无论哪一步失败都必须补发 `save-on`、失败的创建
不得留下占位目录、回收一台服务器时不得碰别的服务器的对象。放
`crates/service/tests/`，用 `testkit::ServiceFixture`（真实目录布局）装配。

## L4 Entry E2E

跑真实二进制，断言退出码、stdout / stderr 边界、JSON 输出形状、落盘 marker
格式。这一层是脚本与前端的唯一依据，字段改名必须在这里红。

# 七道门禁

| 门禁 | 内容 | 何时跑 | 失败含义 |
| --- | --- | --- | --- |
| G1 | `scripts/check.sh` | 每次提交、每次 CI | 常规门禁未过 |
| G2 | 变异测试 `cargo mutants` | 手动触发的深度检查 job | 测试发现不了被改坏的实现 |
| G3 | 属性测试反例固化 | 每次提交（`proptest-regressions/` 入库） | 已知反例未纳入回归 |
| G4 | 快照 `insta` | CI `snapshots` job | 机器契约发生了未经确认的变化 |
| G5 | diff 覆盖率 | CI | 新增代码没有被测到 |
| G6 | 失败可复现 | 人工约定 | 失败信息不足以复现 |
| G7 | `cargo-fuzz` | 手动触发的深度检查 job | 解析器在畸形输入上崩溃 |

## G1 常规门禁

`scripts/check.sh` 是唯一入口。跑子集不等于门禁通过，提交前必须跑全量。

`scripts/check-architecture.sh` 只用**一次** `cargo metadata` 取全部内部依赖边，
不在每个包上反复跑 `cargo tree`。后者原先共 67 次 cargo 进程、累计 6 秒，占掉
这一步的大部分时间；依赖方向的判定口径不变（manifest 声明的直接依赖，含
build 依赖、不含 dev 依赖），改的只是取数方式。

测试一步走 `scripts/test-parallel.sh`，不是直接 `cargo test`：后者把每个测试
目标编译成独立二进制后**串行**执行，而本项目最慢的几个目标各自十几秒、彼此
又在等不同的东西（子进程、IO、超时），串行时总耗时就是它们的和。并行执行器
用同一次 `cargo test --no-run` 产出目标清单，再并发跑，编译与 feature 组合
与 `cargo test --workspace --all-features` 完全一致，失败同样是非零退出。

### 覆盖等价性（改这个脚本时必须守住）

脚本必须跑出与 `cargo test --workspace --all-features` **相同的**用例集合。
它以 `profile.test == true` 挑选产物，这个判据容易踩两个坑：

- 按 `kind` 排除 `bin` 会丢掉 bin 的单元测试。`mcsup` 有 208 个、
  `mcsup_dev_tools` 有 25 个单元测试写在 `main.rs` 里，那样会静默少跑
  233 个用例，而门禁照绿。cargo 对 bin 目标产出两份产物：`test=false`
  是真二进制（用户执行的那个），`test=true` 才是带 harness 的副本。
- doctest **不在** `--no-run` 的产物清单里（由 rustdoc 驱动，没有独立
  可执行文件），必须单独一条 `cargo test --doc` 补上，否则静默少 4 个。

改动后核对一次总数：两边 `passed` 之和必须相等（当前 1407）。

### 低端设备与 CI

并发数自适应，不写死：`< 4` 核不超订（小机器上争抢的代价高于等待的收益），
否则取 1.5 倍核数；再按可用内存封顶，每个并发任务按 64MB 计（实测满载 41
个目标的峰值增量只有 211MB，留了 3 倍余量）。可用 `MCSUP_TEST_JOBS` 覆盖。

Windows 上直接回退到串行 `cargo test`，两个硬性原因：`xargs` 会把
`D:\a\mcsup` 里的反斜杠当转义吃掉（变成 `D:amcsup`），且 `xargs -P` /
`awk` / `getconf` 在 pwsh 下不保证存在。CI 的那两个步骤因此显式写了
`shell: bash`；回退不降低覆盖，只是慢。

写测试时**不要依赖外网**。本地 `server create` 的核心下载、远端 create 在
agent 侧的核心装配都会打真实 HTTP：CI 上要么慢、要么结果随网络漂移。两种
既定手法：

- 子进程侧断网：给 CLI 子进程设 `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`
  指向 `http://127.0.0.1:1`（见 `cli_e2e`、`cli_remote` 的 `run`）
- 进程内换来源：用 `McsupBuilder::jar_sources` 注入立即失败的来源
  （见 `cli_remote` 的 `offline_builder`，经 `Agent::open_with_builder` 装配）

两者都把「核心下载成功与否」钉成确定值。断言要写成对两种结果都成立的形状
（例如「create 退出码必须与核心登记状态一致」），而不是假设下载会成功。

## G2 变异测试

只对 `domain` 与 `service` 开，配置在 `.cargo/mutants.toml`。成本是每个变异体
一次完整构建 + 一次测试，全 workspace 跑一轮要几小时。

基线阶段（首个 2-3 周）**只报告、不设阈值**：先测出存活率，再决定卡在哪。
先定阈值再跑，第一次就会被历史遗留挡住，最后整个门禁被关掉。

## G3 属性测试反例固化

属性测试失败时 `proptest` 会把反例写进 `proptest-regressions/`。那个文件是
**测试资产**，必须随修复一起提交。

生成器必须自检。策略写错会让属性全部「空转通过」——生成器永远产出被拒的
输入，属性成了死代码。每个属性文件至少一条 `strategy_self_check`。

生成器不得产出领域本来就拒绝的输入（如 `.` / `..` 对 `RelPath`）。用它们冒充
合法输入，测的是生成器而不是实现。

## G4 快照

负责钉住机器契约：CLI 的 JSON 输出形状、`workspace.toml` 等落盘格式。

- 更新：本地 `cargo insta review` 逐条确认
- CI：`INSTA_UPDATE=no`，出现 `.snap.new` 即失败
- 归一化：临时路径、随机 UUID、时间戳必须在断言前替换成占位符，只归一化
  「值」，不动字段名与结构

## G5 diff 覆盖率

本地用 `cargo llvm-cov` 看分支覆盖，CI 只对本次 diff 覆盖的新增行设门槛。
覆盖率的用途是**找漏测**，不是刷数字：把没有断言的 `expect` 调用算进覆盖，
是在制造虚假安全感。

## G6 失败可复现

失败信息必须自证。见下面的 AI 约束。

## G7 cargo-fuzz

解析器类模块（Provider 响应、JSON 元数据、路径解析）进 fuzz target，手动触发时跑。
能 panic 的输入先进 `proptest` 反例，再进 fuzz 语料。

# 测试基础设施：`crates/testkit`

`testkit` 是 dev-only crate（`publish = false`），不进 `mcsup` 门面，只有各
crate 的 `tests/` 引用。

| 模块 | 接缝 | 用途 |
| --- | --- | --- |
| `cas_contract` | `port::ContentStore` | 「任何 CAS 实现都必须满足」的契约 |
| `fault` | `SupervisorPort` / `ArtifactDownloader` / `ContentStore` | 故障注入装饰器 |
| `fixture` | 工作区 / 服务器目录 / 注册表记录 | 服务层装配样板 |
| `http` | HTTP 上游 | 回环假服务，替代真实网络 |
| `assert` | 断言本身 | 失败信息自带「期望 / 实际 / 现场」 |

依赖约束：`testkit` 只依赖 `domain` / `port`。它不认识 `service` / `storage` /
`runtime`——那些是交付 trait 的对象，由各自的 `tests/` 决定怎么装配。

禁止把业务逻辑搬进 `testkit`。它提供「实现必须满足什么」和「失败时怎么把
话说清楚」，不提供「怎么算对」。

# AI 开发硬性约束

以下五条对本项目所有 AI 辅助开发生效，违背即视为未完成。

## 1. 失败信息自证

失败输出必须能让人不读源码就复现。一条合格的失败信息包含：

- 违反的不变量（一句话）
- 期望值
- 实际值
- 现场（相关磁盘树、命令记录、输入）

```rust
// 禁止
assert!(page.total == 0);

// 必须
testkit::assert_invariant(page.total == 0, || {
    Failure::invariant("失败的创建不得留下快照记录")
        .expected(0)
        .actual(page.total)
        .tree(&snapshots_root)
});
```

## 2. 禁止削弱断言

测试红了，只能改实现或改夹具，**不得**通过以下方式让它变绿：

- 删断言、放宽区间、加 `unwrap_or` 兜底
- 把 `assert_eq!` 换成 `assert!` / `println!`
- 把失败用例改成 `#[ignore]`、`return` 提前退出、加环境变量开关
- 把不确定的输出从快照里整个删掉

跳过必须显式：需要真实网络或外部依赖的用例用 `#[ignore = "原因"]`，让它在
结果里单独计数，而不是靠环境变量提前 `return`（那会显示成 `passed`）。

确需放宽时，在用例注释里写清「为什么原来的断言不成立」，并取得用户确认。

## 3. 跳过必须显式

`#[ignore]` 必须带原因字符串。CI 里用 `--ignored` 显式执行的组只有手动触发的深度检查 job。

## 4. 文案断言必须钉住语言

集成测试里断言面向人的文案（中文或英文）时，必须在命令里显式给 `--lang`
（或显式设置 `MCSUP_LANG`），不能用「机器默认值」：

```rust
// 禁止：断言中文却让语言随机器漂移
assert!(stderr.contains("指纹"), "{stderr}");

// 必须
run(home, &["remote", "connect", &addr, "--code", &code, "--output", "json", "--lang", "zh"]);
assert!(stderr.contains("指纹"), "{stderr}");
```

原因是三平台取 locale 的方式不同：Linux 读 `LANG` / `LC_ALL` / `LC_MESSAGES`，
macOS 读系统偏好（`CFLocaleCopyPreferredLanguages`，不读 `LANG`），Windows 读
系统设置。测试的 `run` 辅助函数为避免用户环境影响，会 `env_remove` 掉这些变量，
于是 Linux 上 `sys_locale` 返回 `None`、语言回退英文，macOS 上仍读得到中文——
**同一份断言本地绿、CI 红**。这类失败不会在开发机上暴露，只能靠这条约定避免。

只断言机器可读内容（JSON 字段、退出码、路径、`--version` 版本号）时不需要钉住
语言：`--output json` 的 stdout、错误分类与退出码与语言无关。

## 5. 修复循环收敛

测试失败后的循环是：读失败信息 → 定位是「实现错」还是「测试错」→ 改一处
→ 重跑。

三轮仍未收敛，停止改动，向用户报告：卡在哪一层、已排除哪些假设、需要什么
信息。禁止在同一处反复试错，也禁止把范围越改越大——「顺手重构一下」必须
先说明并取得同意。

# 测试矩阵

每个功能都要有完整矩阵。以 `backup create` 为例，至少覆盖：正常、空目录、大
文件、重复文件、修改文件、删除文件、权限错误、磁盘空间不足、Hash / IO 错误、
并发备份、中断、恢复、Snapshot 一致性、GC、Restore。禁止只写
`test_backup_success()`。

# 测试优先级

必须覆盖 Happy Path、Boundary、Invalid Input、Failure、Concurrency、
Cancellation、Timeout、Recovery、Persistence、Compatibility、Performance。
Backup、File Transfer、RPC、Runtime、Storage 等涉及数据和系统资源的模块必须
重点测试失败路径。

# 测试与实现同步

实现一个函数 / Service / API / RPC / Backup 功能，就立即测试对应单元，不要积累
大量未测试代码。

# 性能测试

Backup、CAS、Snapshot、Hashing、Restore、File Transfer、Serialization、RPC、
Provider Parsing 等高成本模块必须有 Benchmark，能回答 Throughput、Latency、
Memory、CPU、Allocation。不能只声称「性能很好」。
