# MCSUP

专业级 Minecraft 服务器管理平台。Rust 编写，Cargo Workspace 多 crate + `mcsup` 门面。

一站式管理本地与远程 Minecraft 服务器：建站、启停、备份、Java 运行时、Mod / 整合包，
以及经 Remote Agent 纳管的远端主机。

## 状态

项目处于早期开发阶段（`0.1.0`，未发布到 crates.io）。API 与 CLI 契约仍在演进。

## 组成

**入口**（`apps/`，并行可执行）

| 包 | 二进制 | 说明 |
|---|---|---|
| `apps/cli` | `mcsup` | 命令行与 TUI |
| `apps/agent` | `mcsup-agent` | Remote Agent，被控端 |
| `apps/dev` | `mcsup-dev-tools` | 开发辅助工具 |
| `apps/bench` | — | criterion 性能基准 |

**库**（`crates/`）

- `mcsup` — 门面 crate，按 feature 级联重导出内层，是第三方依赖的唯一入口
- `domain` — 领域模型与规则（不依赖任何模块）
- `port` — 能力 trait 定义
- `service` — 统一业务服务层与编排
- 实现层：`backend`（Local / Remote 执行位置）、`runtime`（进程与运行管理）、
  `storage`、`file`、`backup`（CAS / 快照）、`java`、`provider`、`modkit`
- 契约层：`protocol`（帧载荷）、`transport`（TLS + 长度前缀帧）、`auth`
- `testkit` — 测试基础设施（仅 dev-dependency，不发布、不进 `mcsup` 门面）

依赖方向单向向下：`apps → service → port → domain`，实现层向上实现 `port`。
详见 [docs/architecture.md](docs/architecture.md)。

## 构建

需要 Rust 1.98.1（`rust-toolchain.toml` 固定），MSRV 1.89。

```console
cargo build --workspace
cargo build --release -p cli     # 产物：target/release/mcsup
```

## 快速开始

```console
mcsup workspace init --dir ./mc
mcsup server create web --version 1.21.1 --variant vanilla
mcsup server start web
mcsup server backup create web --label before-update
```

完整子命令与交互约定见 [docs/cli-commands.md](docs/cli-commands.md)。

## 质量门禁

提交前运行：

```console
sh scripts/check.sh
```

依次执行 architecture / fmt / fmt (fuzz) / check / clippy / test / e2e / doc /
feature powerset / deny，任一失败即中止。也可只跑指定步骤：`sh scripts/check.sh fmt clippy`。

## 文档

| 文档 | 内容 |
|---|---|
| [AGENTS.md](AGENTS.md) | 开发总规范（每次开发必读） |
| [docs/architecture.md](docs/architecture.md) | 架构分层与依赖模型 |
| [docs/cli-commands.md](docs/cli-commands.md) | CLI 子命令设计 |
| [docs/embedding.md](docs/embedding.md) | 作为库嵌入、feature 矩阵与分发形态 |
| [docs/testing.md](docs/testing.md) | 测试分层与写法 |
| [docs/remote.md](docs/remote.md) | Remote Agent 与远端链路 |
| [docs/workspace.md](docs/workspace.md) | 工作目录规范 |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 贡献流程 |

## 许可

MIT，见 [LICENSE](LICENSE)。
