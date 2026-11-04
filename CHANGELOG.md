# 变更记录

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [未发布]

### 新增

- `apps/bench`：CAS 读写、目录扫描、快照清单编解码的 criterion 基准。
- `fuzz/`：`parse_rel_path`、`parse_snapshot_id`、`parse_content_hash` 三个
  fuzz 目标，以及 CI 的 `fuzz`（G7）与 `coverage`（G5）job。
- `service::ConfigDocumentError`：整份配置文本的解析错误改为结构化类型。
- `domain::server::launch::installer_args_files`：Forge / NeoForge 启动参数文件
  的布局判定收敛到领域层，`service` 与 `runtime` 共用。
- `RetentionInput`：保留策略的冲突判定进领域层。
- `BackupErrorKind::RestoreCancelled`：恢复过程可被协作取消。
- `CredentialError::LockBusy`：凭证库并发访问不再互相覆盖。

### 变更

- CLI 的远端装配改经 `McsupBuilder`，不再手拼第二套组合根。
- 库侧不再读取 `MCSUP_CREDENTIAL_STORE` 环境变量：改由入口调用
  `CredentialVault::open_with`。
- 库侧错误文案改为英文短语 + `kind()`，中文本地化由入口负责。
- `apps/agent` 补齐参数解析测试。

### 修复

- `storage::load_state` 对非 Java 集合也拿 `JAVA_STATE_VERSION` 比对，导致
  工作区状态恒被判为损坏。
- `restore --to` 失败时会在目标路径留下半个目录；改为暂存后 rename。
- CAS 读取与 snapshot 会话落盘缺少 fsync / 原子替换，掉电可能得到空文件。
- IPC 客户端与服务端缺少收发超时，对端不响应时无限等待。
- `rel_dir` 写入未校验，越界路径可把服务器目录指到工作区外。
- `async-executor` 升级到 1.14.1（1.14.0 已 yanked）。
