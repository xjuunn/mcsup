# 变更记录

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [未发布]

### 新增

- `protocol::action`：动作目录。每个可执行操作登记稳定标识、所需权限、风险等级
  与断线重试安全性；远端帧的 `required_permission` / `is_retry_safe` /
  `operation` 改为从目录取，`Request::spec` 的 `match` 无兜底分支，新增帧漏
  登记即编译失败。目录覆盖全部远端帧与本地写操作（起停重启、改配置、`server
  mod` 的安装 / 更新 / 停用 / 移除 / 修复），标识一经发布禁止改名。
- 门面 `kit` feature：`crates/mcsup/src/kit` 提供任务级接口（`Mcsup::kit`）。
  任务按领域分挂在访问器上——`kit.servers()`（建服、整合包开服、整合包装已有
  服务器、装核心、起停重启查状态、删服）、`kit.backups()`（建、列、校验、恢复、
  删除、保留策略清理）、`kit.content()`（安装、更新、移除）、`kit.java()`
  （安装、列出运行时）、`kit.workspaces()`（工作目录初始化）；`Kit` 本身只
  持有默认策略与 `core()` 逃生口，`KitConfig` 仍在五个领域间共享。
  `servers().create` 一次完成取回清单（整合包）、建目录、入库、装核心与
  （可选）起服；`servers().install_modpack` 把整合包装到已有服务器，是建服
  中途失败后的重试入口；`content().install` / `content().update` 的 `dry_run`
  只出计划。配置分三层（单次覆盖 > `KitConfig` > 默认值），默认不代接受
  EULA、不自动起服、不默认安装 Java 大版本。
- `crates/mcsup/examples/kit.rs`：kit 全流程使用示例（随 `kit` feature 编译）。
- `Mcsup::has_external_supervisor`：装配时记录宿主是否注入了 supervisor /
  backend，任务级接口据此决定 `supervisor_exe` 是否必填。
- `apps/bench`：CAS 读写、目录扫描、快照清单编解码的 criterion 基准。
- `fuzz/`：`parse_rel_path`、`parse_snapshot_id`、`parse_content_hash` 三个
  fuzz 目标，以及 CI 的 `fuzz`（G7）与 `coverage`（G5）job。
- `service::ConfigDocumentError`：整份配置文本的解析错误改为结构化类型。
- `domain::server::launch::installer_args_files`：Forge / NeoForge 启动参数文件
  的布局判定收敛到领域层，`service` 与 `runtime` 共用。
- `RetentionInput`：保留策略的冲突判定进领域层。
- `BackupErrorKind::RestoreCancelled`：恢复过程可被协作取消。
- `CredentialError::LockBusy`：凭证库并发访问不再互相覆盖。
- 插件系统：常驻宿主进程按需拉起插件子进程，走带帧的 JSON 管道协议
  （`welcome` / `call` / `result` / `event` / `chunk` / `shutdown`），`op` 输入
  按 descriptor 的 schema 校验、超时取 descriptor 声明的 `timeout_ms`。能力
  授权、撤销列表、审计三件套决定插件能做什么；中高风险能力未确认时记录落
  `pending_approval` 且 `enabled=false`。
- `service::plugin` 与 CLI 的 `plugin` 子命令族：安装 / 升级 / 版本切换 / 回滚 /
  卸载 / 启停 / 状态 / 日志 / 调用 / 配置 / 授权 / 封禁 / 隔离 / `verify` /
  `doctor` / `revocations`。安装来源覆盖本地目录、URL 与 git（tag / SHA），
  摘要不匹配即拒绝。
- descriptor `schema 3` 的 `bind.servers`：插件声明从哪些配置路径读服务器名，
  服务器 `start` / `stop` / `restart` 时先启停插件，多台服务器中任一在运行就
  保持插件；没有服务器使用时由常驻看门狗收掉，控制台内、游戏内与崩溃停止
  都走同一条收尾路径。
- TypeScript 插件 SDK（`sdks/typescript`，包名 `mcsup-plugin`）：握手、op 调用、
  流式返回、事件订阅、配置与密钥、`ctx.contract.call` 与严格帧解码层，附
  conformance 用例与宿主实现互校。
- `docs/plugins.md`：插件契约主文档（manifest / descriptor、能力风险与授权、
  帧协议与限额、安装流水线与回滚、CLI 契约）；`docs/plugins-testing.md` 给出
  可映射到测试的用例编号。
- TypeScript SDK 的 `Builder.config(schema)`：把 config schema 绑到 op 片段上，
  `run` 里的 `ctx.config` 才有推导类型（`docs/sdk-design.md` §6.1）。

### 变更

- 下载分段阈值下调：`MIN_SEGMENTED_SIZE` 由 8 MiB 降到 2 MiB，单段由 2 MiB
  降到 1 MiB。跨境 CDN 单连接只有 20–45 KiB/s，5–8 MiB 的中等文件原本只能
  单流、构成下载尾部最慢的一段；现在至少切 4 段并发拉取。
- 整合包布置跳过 overrides 里的 `saves/` 存档：存档是客户端世界，不是服务端
  部署目标，随包携带时常是最大单项。其余 overrides 的解压总量上限由 1 GiB
  提到 4 GiB；超限时按单文件 / 总量给出体积与处置建议，不再只报一句
  `exceeds the extraction limit`。
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

- manifest 的 `schema` 改为范围校验（`1..=MANIFEST_SCHEMA_VERSION`），与
  descriptor 一致。此前是相等校验：宿主把 `MANIFEST_SCHEMA_VERSION` 升到 2、
  新增一个字段，所有 `schema: 1` 的已发布插件会被新宿主一并拒装。现在旧产物
  继续装得上，新增字段按默认值退化成「未声明」。`UnsupportedSchema` 的错误
  文案同时改为 `host supports 1..=N`。
- descriptor 的校验只在必要时做一次：按「路径 + 大小 + mtime」缓存校验结果，
  调用入口不再每次 `call` / `stream` 都重新读盘并解析一份 ≤256 KiB 的 JSON；
  启动插件前先校验一次，产物不合法在启动这一步报可读错误，而不是拉起进程后
  每个 op 各自 `internal` 失败一次。产物被换掉（升级换版本目录、原地重写）时
  指纹变化，缓存自动失效。
- TypeScript SDK 的 descriptor / schema 限值改由生成契约提供：`MAX_SCHEMA_DEPTH`
  等 12 个常量随 `contracts/v1.ts` 从 `domain::plugin::descriptor` 生成，
  `src/validate.ts` 不再手抄一份。此前宿主改一个限值，SDK 会在构建期放行、
  等安装期才被拒。
- `docs/sdk-design.md` 的示例按真实 API 改正：Builder 片段回填到
  `queries` / `actions` / `streams`（不是不存在的 `ops`）、`ctx.sessionId`
  取代不存在的 `ctx.state`、`ctx.log.info(...)` 取代 `ctx.log(...)`、
  生命周期键 `onStart` 取代 `start`、`ctx["console:read"]` 取代 `ctx.console`。
  改正后的片段已按 SDK 类型实测通过。
- `docs/plugins.md` 的常量表按实现更新：`DESCRIPTOR_SCHEMA_VERSION` 早已因
  `setup`（2）与 `bind`（3）升到 3，文档仍写 1；manifest 的 `schema` 行改写成
  范围语义；§14.3 补「schema 范围接受 + 未知顶层字段拒绝 + 收紧校验视同破坏性
  变更」三条演进规则。
- TypeScript SDK 的构建产物自带运行时依赖：`mcsup-plugin build` 把生产依赖整棵
  vendor 进 `<out>/node_modules`——`dependencies` 声明的包、`mcsup-plugin` 自身、
  能解析到的 `peerDependencies`（如 `zod`）及其传递依赖，`devDependencies` 不进
  产物。此前产物只有 `dist/` 与启动器，装到宿主目录后 `import "mcsup-plugin"`
  解析不到任何东西，进程以 `ERR_MODULE_NOT_FOUND` 退出，宿主只看到
  `handshake_timeout`。拷入时把符号链接展开成实体文件（`fs.cpSync` 的
  `dereference` 在 Node 24 上对文件链接不生效），宿主的产物校验会拒绝链接。
- TypeScript SDK 的 `ctx.config` 不再丢类型：op 片段按 `config` schema 推导
  `ctx.config`，Builder 用 `.config(schema)` 绑定（必须是 `definePlugin` 的同一个
  schema 对象，绑错在装配时报错）。此前 `config` 已声明、`run` 里 `ctx.config`
  仍是 `unknown`（`TS18046`），只能手写断言。
- 未声明 `.output()` 的 op 不再必然运行期失败：descriptor 仍落空 object 占位
  （`docs/plugins.md` §11.2 要求该字段必填），但运行期跳过输出校验，返回
  `undefined` 回 `{}`；流式 op 的分块校验同此。此前任何返回对象都被判成
  `internal: op … returned an invalid output: ✖ Unrecognized key`。
- 插件安装失败不再留下无人引用的版本目录：目录存在但记录不指向该版本时（上次
  升级在该版本上启动失败）先删再装，同一版本不再永远报 already exists；
  `restore_after_failed_start` 回滚后也删掉起不来的版本目录，`update` 不再留下
  「装着装不回来、回滚又没目标」的版本。
- `plugin grant` / `plugin revoke` / `plugin status` 等命令的 id 与能力名解析
  错误不再中英各来一行：文案随 `--lang` 只出一种语言，可用能力清单与保留名提示
  同样本地化，退出码仍是 2。
- 撤销列表的权限检查结果不再被丢弃：`load_revocation_list` 把
  `check_permissions` 的错误传播出去，权限过宽的撤销列表不再照常加载。
- 批量下载的进度记账改为按单文件高水位：分段重试退款、换源或「分段回退单流」
  从 0 重来时，重传的字节不再重复计入，进度条不再显示超过总量的数字
  （实测 `88.93 MiB/88.41 MiB`），速度也不会虚高。单文件以清单声明大小封顶。
- 整合包第二阶段（清单哈希过期后回源重下）不再向整包进度条汇报，避免位置
  从整包口径被改写成单文件口径而倒退。
- 整合包布置的进度文案不再预写文件数：仅客户端模组要等上游元数据返回后才剔除，
  提前报数会与结束后的实际数量不一致。
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
- 整合包下载并发缺省值调高：单连接被 Fastly CDN 限速在约 45 KiB/s、聚合吞吐随
  在途连接数近似线性增长，旧的缺省（批量 8 / 分段 24 / 在途 48）把 109 文件 /
  594 MiB 的整合包压在约 1.4 MiB/s；新缺省（批量 24 / 分段 128 / 在途 256）实测
  约 8 MiB/s，「取回整合包」单文件由 24 段约 1.0 MiB/s 提到 128 段约 7.5 MiB/s。
  同时已知大小且低于分段阈值的小文件不再先发 HEAD 探测。
- 整合包布置按体积降序下载（大文件优先，LPT 调度）：改掉此前按清单原始顺序
  下载时大文件拖尾、在途连接数一路衰减导致的「越下越慢」，同时缩短整批完成时间。
- 插件写路径改独立写线程 + `ack` 确认：Windows 命名管道不支持写超时，原先一次
  阻塞写会把整个会话卡死。
- 插件帧上限 1 MiB → 24 MiB，且先序列化再判超限：原来超限时半帧已经写出，对端
  只能看到损坏的帧；`result` 帧超限改发 `limit_exceeded` 错误帧。
- 插件安装 / 版本切换 / 卸载改用内核文件锁（`plugins/.<id>.install.lock`）：原先
  「文件存在即加锁」，崩溃后最长锁死一小时，且 `update --to` 与 `remove` 根本
  不吃锁，可与安装并发互相删目录。锁文件永不删除——删掉会让等待者与新来者各持
  一把锁。
- 插件升级：启动（含握手）成功前不再动旧版本目录，失败自动切回旧版本并重新拉起；
  `--prune` 只清更旧目录，始终保留回滚目标；停机后的非启动类失败也会删掉孤儿
  目录并恢复旧进程。
- 插件版本切换不再无条件写 `stopped`：已停用的插件切版本后既 `enable` 不了、又能
  绕过待审批门禁；现在按 `enabled` 与待授权能力重算，且拒绝切到当前版本（否则会
  覆盖真正的回滚目标）。
- 插件记录读取只把 `NotFound` 当「没装过」：记录损坏 / IO 错误原先降级成全新安装，
  会清零授权、停掉运行中的进程并删掉旧版本目录。
- 插件卸载改为先删产物 / 数据 / 密钥、最后删记录：反序时一次删除失败就留下无法
  清理的孤儿（重装同版本永远 `AlreadyExists`，密钥残留还会被重装的同名插件继承）。
- `plugin verify <id>` 改用安装记录的目录树摘要比对：原先拿旁路 `.sha256` 比目录树，
  被改过的产物也报「校验通过」。
- 插件 `update` 按记录来源升级时校验产物 id，本地来源规范成绝对路径；git 来源的
  记录格式改 `git:<url>|<rev>`（原 `@` 会把 `git@host:org/repo` 这类 URL 拆坏，
  SSH 装的插件永远升级不了）。
- `plugin logs` 对不存在的插件不再静默成功（`--follow` 原本永久空转）；`plugin
  stop --grace` 超 3600 秒按用法错误拒绝，不再静默夹紧到 300 秒。
- 崩溃残留的安装临时目录按名字里的创建时刻清理（超过一小时），解包期间的目录
  不会被误删。
- 插件宿主看门狗以 `(插件 id, session_id)` 为观察单位，撤销判定下沉到宿主内部：
  插件崩溃重启、配置热更后不再漏判；控制台内与游戏内停机也会收掉绑定插件。
- `plugin update` / `plugin install` 不再重复重启插件：服务层已在新版本启动成功后
  才算成功，CLI 只在没有会话时补启动。
- TypeScript 插件 SDK 严格解码每个帧字段并补齐 `console:read.tail`、KV / secret、
  `server:lifecycle.stop` 三态、backup / content / java 包装、分页四键与
  `ctx.contract.call`；成功流不再多发空的 `chunk` 结束帧。
- `url` 升到 2.5.9（2.5.8 的 `Url::origin()` 在嵌套 `blob:` URL 上栈溢出，
  RUSTSEC-2026-0592）。
- 插件宿主不再继承拉起它的那个 CLI 的控制台（Windows 上脱离终端 + 新进程组），
  插件进程也自取一个无窗口控制台（`CREATE_NO_WINDOW`）。控制台是继承来的：
  拉起宿主的终端（含无窗口的伪控制台）销毁后，常驻宿主手里留着一个死掉的
  控制台句柄，此后它再拉起 `cmd.exe`（`.cmd` 入口）会在控制台初始化阶段直接
  失败，退出码 `0xC0000142`（`STATUS_DLL_INIT_FAILED`），且插件日志一行都不长。
- 插件启动失败的原因文本补上 NTSTATUS 符号名与成因（`STATUS_DLL_INIT_FAILED`、
  `STATUS_DLL_NOT_FOUND`、`STATUS_INVALID_IMAGE_FORMAT`、`STATUS_STACK_BUFFER_OVERRUN`
  等），不再只有一串十六进制退出码。
- `plugin start` / `plugin restart` 先判本机状态：插件被拉黑、隔离、停用或还有
  待授权能力时直接报出真实原因与应执行的命令，不再被「绑定的服务器都没在运行」
  盖成一次 exit 0 的「已跳过启动」。
