# MCSUP

一站式 Minecraft 服务器管理工具。

以 `mcsup` 命令管理服务器实例：创建工作区、安装服务端（原版 / Fabric / Forge /
NeoForge / Paper / Purpur / Quilt / Sponge / 整合包）、启停与查看控制台、
备份与恢复到快照、装 Mod 与资源包、管理本机 JVM 运行时，并可把多台机器接入
为「远端」，用同一套命令管理对端服务器。

## 安装

```
cargo install --path apps/cli     # 装出 mcsup
cargo install --path apps/agent   # 装出 mcsup-agent（被连接端）
```

## 快速开始

```
mcsup workspace init --dir ~/mc --name main
mcsup server create web --version 1.21.1 --variant paper
mcsup server start web
mcsup server console web          # 实时控制台
mcsup server backup create web    # 打快照
```

`mcsup server --help` 与 `docs/cli-commands.md` 是子命令的完整说明。

## 作为库使用

门面 `mcsup` 是唯一依赖入口，按 feature 选择能力：

```toml
[dependencies]
mcsup = { version = "0.1", features = ["service", "backup"] }
```

装配统一走组合根，第三方宿主无需自己拼 `JsonStore` / 下载器 / Java 服务：

```rust
use mcsup::bootstrap::Mcsup;

let mc = Mcsup::open(&data_dir)?;
let servers = mc.registry.list()?;
```

`McsupBuilder` 可注入自定义存储、下载器、进程监管与远端执行位置；能力矩阵与
注入点见 `docs/embedding.md`。

## 文档

| 文件 | 内容 |
| --- | --- |
| `docs/architecture.md` | 架构、分层、依赖方向 |
| `docs/cli-commands.md` | 子命令与交互约定 |
| `docs/embedding.md` | 作为库嵌入、feature 矩阵、CLI 机器契约 |
| `docs/remote.md` | 远端接入与管理 |
| `docs/development-workflow.md` | 开发流程与质量门禁 |

## 开发

```
scripts/check.sh      # 架构 / fmt / clippy / 测试 / e2e / doc / feature / deny
cargo bench -p bench  # 热路径基准
```

## 许可证

MIT
