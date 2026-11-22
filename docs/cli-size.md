# CLI 体积优化记录

## 测量口径

2026-10-25，本机 Windows x86_64 MSVC，Rust 1.98.1。
基线已开启 `opt-level = "z"`、fat LTO、单 codegen unit、strip、panic abort。
命令：`cargo build --release --locked -p cli`。

| 产物 | 最初（字节） | 移除 SQLite 后（字节） | 累计减少 |
| --- | ---: | ---: | ---: |
| `target/release/mcsup.exe` | 5,231,616 | 4,088,320 | 21.9% |
| 仅 EXE 的 gzip，.NET Optimal | 2,653,992 | 2,010,288 | 24.3% |

gzip 是统一压缩口径下的下载体积参考，不是 GitHub Release ZIP 的实测值。
原始 EXE 从约 4.99 MiB 降至 3.90 MiB；其他平台需由各自构建测量。

## 当前体积（发布前复测）

上面那张表是「移除 SQLite」那一次的对照记录，**不是当前值**。发布前用同一条
命令复测，CLI 已经翻倍：

| 口径 | 2026-10-25 | 当前 | 变化 |
| --- | ---: | ---: | ---: |
| `target/release/mcsup.exe` | 4,088,320 | 8,332,800 | +103.8% |
| 仅 EXE 的 gzip（.NET `Optimal`） | 2,010,288 | 3,876,645 | +92.8% |

Rust 1.98.1、本机 Windows x86_64 MSVC，`cargo build --release --locked -p cli`。

增长来自那之后新接进 CLI 的子系统，不是某一次改动的回退。`apps/cli` 对门面的
feature 从

```
service, file, java, provider, storage, runtime, bootstrap
```

变成

```
service, backup, file, java, modkit, provider, storage, runtime,
bootstrap, backend, transport, auth, remote
```

其中 `remote` 连带 `plugin`、`protocol`、`transport`（rustls + ring 的 TLS、
agent、mirror）以及 `service` 侧的远端编排。这是「CLI 里同时有 `mcsup remote`
与 `mcsup plugin` 命令族」的必然代价，不是可以顺手砍掉的冗余。

`[待办]` 下载体积已从约 1.9 MiB 涨到约 3.7 MiB（gzip）。若要压回去，方向是把
`remote` / `plugin` 从默认二进制里拆成可选 feature 或独立子命令二进制；这会
改变「装一个 mcsup 什么都能干」的现状，属于需要单独决策的架构变更，本次未动。

## 元数据改成 JSON 文件

元数据存在 `{data_dir}/mcsup/db/` 下的 `workspaces/`、`servers/`、`java/`
三个目录，**一条记录一个 JSON 文件**，`state.json` 只存全局指针
（当前工作区、Java 扫描时间）。布局与格式见 `docs/workspace.md`，
实现见 `crates/storage`。

体积下降主要来自不再编译进二进制的关系数据库引擎与其查询层、迁移宏。
`Cargo.lock` 相应移除引擎驱动、ORM、迁移工具及其连带依赖
（含 `time`、`toml`、`vcpkg`、`pkg-config` 等构建期依赖）。
`.cargo/config.toml` 也不再需要引擎专用编译参数。

## 保留的改动

- `.cargo/config.toml` 针对 `x86_64-pc-windows-msvc` 设置 `/Gw /Ob1`，
  允许裁剪无用全局数据，限制 C 代码自动内联。其他目标不使用该参数。
- CLI 的 miette 使用 `fancy-no-backtrace` 并关闭未使用的 derive。
  项目手工实现 Diagnostic，仍用 GraphicalReportHandler；没有调用
  miette 的 panic hook。现有 std panic hook、anyhow、日志过滤、
  HTTP/2、TLS、交互、表格、进度和五种 shell 补全保持原配置。

Cargo 的仓库配置作用于在本仓库内运行的构建和测试；下游项目依赖库时
不继承这份配置。环境变量没有设置 `force`，显式环境配置优先。
因此外部覆盖、不同编译器版本可能改变最终体积。

## 未采用的实验

- 去掉 reqwest charset：EXE 反而增加 512 字节，已恢复。
- thin LTO：5,317,632 字节，比保留的 fat LTO 大。
- 16 codegen units + fat LTO：5,086,720 字节，比单单元大。
- MSVC `/GL`：当前 Rust/C 静态链接组合出现未解析符号，未保留。
- 未裁掉 HTTP/2、日志环境过滤、交互或命令。

## 验证

- workspace 全 feature 测试：334 个测试和 1 个文档测试通过，
  storage 41 个（含一条记录一个文件、跨 open 重开、单条损坏隔离、
  损坏文件挡住写入、文件名与 id 不符、schema 版本不符、唯一键、
  引用完整性、悬空当前指针、`state.json` 损坏时读降级写拒绝、
  越界 id 拒绝、跨进程锁与按 pid 判断的残留锁回收、
  存活进程的锁不被回收、写盘失败不破坏旧文件）。
- fmt、架构检查、全 workspace / all-targets / all-features clippy、
  cargo doc、cargo-hack 单 feature powerset 检查通过。
- release 二进制在隔离 HOME 下真实跑通：`workspace init/list/use`、
  `server create/list/remove`、`java list`；错误路径退出码与分类不变
  （缺当前工作区 3、重名 4、未找到 3、参数错误 2）。
- 损坏 JSON 与未知 `version` 均报 `corrupt repository record` 并保留原文件；
  另一进程可读取前一进程写入的数据；写入不残留 `.tmp` 文件。
- 手工损坏 `servers/*.json` 后：`workspace list` 不受影响，
  `server list` 跳过损坏项仍成功，`server create` 明确报损坏路径并拒绝写入。
- 8 个 mcsup 进程并发 `workspace init`：8 条全部落盘、无重名、
  无残留 `.lock` / `.tmp` 文件。

这些检查覆盖当前自动化用例及上述兼容性路径，不等于穷尽所有运行场景。
Linux、macOS、ARM64 和真实交互终端尚未在本机验证。
旧版 `mcsup.db` 与 `registry.json` 不做自动迁移。
