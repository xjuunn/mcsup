# CLI 体积优化记录

## 测量口径

2026-10-25，本机 Windows x86_64 MSVC，Rust 1.98.1。
基线已开启 `opt-level = "z"`、fat LTO、单 codegen unit、strip、panic abort。
命令：`cargo build --release --locked -p cli`。

| 产物 | 优化前（字节） | 优化后（字节） | 减少 |
| --- | ---: | ---: | ---: |
| `target/release/mcsup.exe` | 5,231,616 | 4,901,376 | 6.3% |
| 仅 EXE 的 gzip，.NET Optimal | 2,653,992 | 2,459,471 | 7.3% |

gzip 是统一压缩口径下的下载体积参考，不是 GitHub Release ZIP 的实测值。
原始 EXE 从约 4.99 MiB 降至 4.67 MiB；其他平台需由各自构建测量。

## 保留的改动

- `.cargo/config.toml`：关闭项目元数据库未使用的 SQLite FTS3/FTS4、
  FTS5、RTREE 扩展；普通表、事务、WAL、外键、线程安全和迁移保留。
  当前元数据库和迁移没有这些虚拟表。手工加入这些虚拟表的外部数据库
  不在本次兼容性结论内；需要时可用外部 `LIBSQLITE3_FLAGS` 覆盖。
- 同一配置针对 `x86_64-pc-windows-msvc` 设置 `/Gw /Ob1`，
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
- 未裁掉 HTTP/2、日志环境过滤、交互或命令，也未替换数据库。

## 验证

- workspace 全 feature 测试：308 个测试和 1 个文档测试通过。
- 新旧 release EXE 对比 18 个命令：stdout、stderr、退出码完全一致，
  包含帮助、五种补全、参数错误和不存在的服务器。
- 临时目录内：旧 EXE 创建元数据库，新 EXE 读取和写入，旧 EXE 再读通过。
- fmt、全 workspace / all-targets / all-features clippy、架构检查通过。
- cargo-hack 单 feature powerset 检查通过（22 个组合）。

这些检查覆盖当前自动化用例及上述兼容性路径，不等于穷尽所有运行场景。
Linux、macOS、ARM64 和真实交互终端尚未在本机验证。
