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

- 发布形态：内层 crate 的 package 名统一为 `mcsup-*`，用 `[lib] name` 保留
  单词库名；发布顺序、版本锁步与 `scripts/bump-version.sh` 一并覆盖内层
  互依的 `version` 字面量。发布 workflow 改为按依赖拓扑序单 job 执行。
- 公开枚举默认加 `#[non_exhaustive]`；处置型枚举（`StopOutcome`、
  `RestoreTarget`、`UpdateDecision`、`Risk`、`Origin` 等）保持封闭，新增
  变体必须逐处更新线上词表与入口映射。
- CLI 的 `remote connect` / `reconnect` / `disconnect` 增加网络阶段回显。
- 远端协议错误词表新增 `failure` / `cancelled`（加性变更，不升协议版本）。
- 远端备份帧的载荷由完整快照清单改为摘要（`WireSnapshotSummary`：元数据 +
  清单规模）：完整清单随文件数线性增长，一页 20 份 5000 文件的快照达 14 MB，
  超出单帧上限。`REMOTE_PROTOCOL_VERSION` 升到 3。
- 备份 mtime 精度由秒改为纳秒，避免同秒同大小改写命中复用。
- CLI 的 JSON 输出（doctor / workspace doctor / settings）固定英文文案，
  不随 `--lang` 或平台 locale 变化。

- CLI 的远端装配改经 `McsupBuilder`，不再手拼第二套组合根。
- 库侧不再读取 `MCSUP_CREDENTIAL_STORE` 环境变量：改由入口调用
  `CredentialVault::open_with`。
- 库侧错误文案改为英文短语 + `kind()`，中文本地化由入口负责。
- `apps/agent` 补齐参数解析测试。

### 修复

- 远端帧改为两阶段解码：先校验版本再解帧体，未知 `op` 的新版本帧不再被
  误报成 JSON 非法；出站帧同样受 8 MiB 上限约束，避免写出半帧。
- 远端日志尾部与增量读取增加行数 / 字节双上限，格式正确的请求无法再让
  agent 把任意大的日志读进内存；回复在封帧前按序列化字节裁剪（保留最新行），
  避免 `MAX_LOG_TAIL_BYTES` 与 `MAX_FRAME_BYTES` 同为 8 MiB、JSON 转义后必然
  超出出站上限。
- 安装器输出缓冲有上限：未换行的长行按 4 KiB 分段交出，原始输出只保留末尾
  256 KiB，刷屏的安装器不再让 agent 常驻数 GB。
- CAS 去重命中现在校验对象大小，截断对象报 `Corrupt` 而不是继续复用；
  对象落盘后同步父目录，掉电不再出现「文件在、目录项不在」。
- `.mrpack` overrides 解压增加单文件 256 MiB 与总量 1 GiB 上限，超限删除
  半成品并报错。
- HTTP 客户端限制重定向：只跟随 HTTP(S)、禁止 HTTPS 降级到 HTTP、最多 5 跳，
  并在发起请求前校验 URL scheme。
- Java 归档下载改用上游声明的哈希校验（`checksum` / `sha256_hash`），按摘要
  长度识别 SHA-256 / SHA-1，识别不出时退回只校验大小。
- `storage::load_state` 对非 Java 集合也拿 `JAVA_STATE_VERSION` 比对，导致
  工作区状态恒被判为损坏。
- `restore --to` 失败时会在目标路径留下半个目录；改为暂存后 rename。
- CAS 读取与 snapshot 会话落盘缺少 fsync / 原子替换，掉电可能得到空文件。
- IPC 客户端与服务端缺少收发超时，对端不响应时无限等待。
- `rel_dir` 写入未校验，越界路径可把服务器目录指到工作区外。
- `async-executor` 升级到 1.14.1（1.14.0 已 yanked）。
