# 贡献指南

## 开发环境

- Rust 1.98.1（`rust-toolchain.toml` 固定），MSRV 1.88
- Windows / macOS / Linux 任一平台
- Git Bash 或 WSL（`scripts/` 下的 shell 脚本依赖 `sh`）

## 开发流程

任何改动遵循「先理解后编码」的流程与完成标准，见
[docs/development-workflow.md](docs/development-workflow.md)。
新功能 / bugfix 在 worktree 中完成，任务边界是提交：已提交即删除该 worktree，
未提交则继续在其中做。见
[docs/development-workflow.md](docs/development-workflow.md) 任务交付流程。
架构归属与依赖方向见 [AGENTS.md](AGENTS.md) 的「代码归属与依赖方向」一节；
拿不准某段代码属于哪层时先读 [docs/architecture.md](docs/architecture.md)。

## 质量门禁

每次完成功能后运行：

```console
sh scripts/check.sh
```

依次执行 architecture / fmt / check / clippy / test / doc / feature
powerset / deny。任一失败即中止。

提交钩子（`.githooks/pre-commit`）会先 `cargo fmt` 再跑同一套检查；
启用方式：

```console
git config core.hookspath .githooks
```

CI（[test.yml](.github/workflows/test.yml)）与本门禁保持同一套命令；
fmt 未通过直接报错，不自动格式化。CI 只手动触发（`gh workflow run test.yml
--ref <分支>`），提交与推送都不会自动消耗额度。

## 架构红线（评审必查）

- 依赖单向：入口 → service → port → domain；实现层互不依赖
- 判定逻辑收敛在 domain / service，入口只做错误码 / HTTP 码映射
- CLI JSON 行类型一律取自 `protocol::cli`，禁止在入口自行定义 DTO
- 新增内层 crate 时同步门面三处（可选依赖 / feature 级联 / lib.rs
  重导出）与 `scripts/check-architecture.sh` 的 `facade_allow` 表
- 内层 crate 互依经根 `[workspace.dependencies]` 声明（`path +
  version` 唯一携带处，发布前提），各 manifest 以 `xxx.workspace =
  true` 继承、禁止写版本字面量；版本经 `scripts/bump-version.sh`
  锁步推进（只改根一个文件）

## 发布

- CLI：手动触发 [release.yml](.github/workflows/release.yml)，填入已存在的
  `v*` tag，构建多平台二进制并创建 GitHub Release。打 tag 不会自动发布。
- crate：手动触发 [publish.yml](.github/workflows/publish.yml)
  （先 dry-run）。首次发布前需完成内层改名 `mcsup-*`，见
  [docs/embedding.md](docs/embedding.md) 分发形态一节。

## 提交规范

- 一个提交一个逻辑单元；禁止「顺手」夹带无关改动
- 提交信息简单、中文、明确
- 只需标题时，首行写简单中文标题，如 `feat: 新增服务器备份功能`
- 必须写大量信息的提交：首行标题 + 空行 + 正文；正文明确、简练
- 除非用户明确要求，否则不写正文
- 不要提交 `target/`；`Cargo.lock` 随代码一起提交（CLI 项目策略）
