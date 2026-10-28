# Workspace 与数据目录规范

来自 `AGENTS.md`。

平台数据与工作目录的布局、命名、身份与移动规则、`workspace init` 行为、
统一注册表语义。实现 workspace / 存储代码、改动 create 或 init 落点、
处理路径身份时查阅。

---

# 两级目录

MCSUP 分两级存放数据：平台数据目录（机器级，自动创建）与工作目录
（workspace，用户选择）。

## 平台数据目录

`{data_dir}/mcsup/`，`data_dir` 取系统数据目录（macOS 为
`~/Library/Application Support`）。惰性创建：任一需要元数据的命令首次
运行时创建。

```
{data_dir}/mcsup/
├── config.toml                   # 平台级偏好，不含 workspace 列表
├── db/                           # 元数据：一条记录一个 JSON 文件
│   ├── workspaces/
│   ├── servers/
│   └── java/
└── java/                         # 机器级 JVM（mcsup java）
    ├── 21/
    └── 17/
```

JVM 属机器级命令，禁止放进工作目录：切换工作目录不换 JVM，多工作目录
不重复占盘。workspace 列表存 `db/workspaces/`，不进 `config.toml`，
避免双真相源。

## 工作目录

工作目录 = 用户选中的路径本身，不再套子目录。所有权标记由目录内的
`workspace.toml` 承担，不靠目录名。

```
<用户路径>/
├── workspace.toml                # 自描述：schema / id / name / created
├── servers/                      # 一服一目录
│   └── alpha/
│       ├── mcsup.json            # 服务器身份：schema / id / name
│       ├── server.jar
│       └── ...
├── backups/                      # workspace 级备份，CAS 跨服去重
│   ├── cas/
│   └── snapshots/
└── cache/                        # 下载缓存，可随时删除
```

`backups` 放 workspace 级：随工作目录整体搬迁，CAS 跨服去重；放服务器
目录内会随删服丢失，retention / GC 无法全局执行。

# 命名

- workspace 名默认 `workspace01`、`workspace02`…，序号 = 现存最大序号
  + 1（删除后重新创建不撞名）。
- 创建可用 `--name` 覆盖；字符集与 server 名一致：`[A-Za-z0-9_-]`，
  长度 ≤ 64。
- server 名全局唯一（跨工作目录），`mcsup server start <name>` 永不歧义。
- workspace 名唯一，作为 `--workspace` 选择器。

# 身份与移动

磁盘目录是真相，数据库是索引，数据库中的路径只是缓存。

- workspace 身份 = `workspace.toml` 的 `id`；服务器身份 =
  `servers/<name>/mcsup.json` 的 `id`。
- servers 表只存 `workspace_id + rel_dir`，不存绝对路径。绝对路径 =
  `workspace.path` 拼接 `rel_dir`。
- 移动整个工作目录：只改 `workspaces.path` 一行。
- 移动单个服务器目录：按 marker id 在已注册工作目录的 `servers/` 下重链。
- `jar_path` 相对服务器目录（固定 `server.jar`），不存绝对路径。
- marker 读写 v1 在 service 层直接做；将来 Remote Backend 需要时再抽
  port，禁止提前为单实现建 trait。
- marker 一律**同目录临时文件 → rename** 原子写入：截断的 `mcsup.json` /
  `workspace.toml` 等于身份丢失（读取按损坏拒绝，`server link` /
  `workspace use` 都接不回来）。

# workspace init

```
mcsup workspace init [--dir <path>] [--name <name>]
```

- `--dir` 缺省且可交互时进入向导：目录三选项（数据目录
  `{data_dir}/mcsup/workspaces/<名称>` / 当前目录 / 自定义目录，输入即校验、
  非法重输而不是终止）→ 名称（默认 = 目录名，不合法退回序号 `workspaceNN`）→
  确认页显示规范化绝对路径。非交互无 `--dir` 保持用当前目录、名称交
  service 序号的机器行为；交互组件见 `apps/cli/src/prompts/`。
- 目标不存在：创建（含父目录）。目标存在：必须为空。
- 判空先忽略噪声文件：`.DS_Store`、`Thumbs.db`、`desktop.ini`、
  `.localized`；其余任何条目（含隐藏文件）一律中止。判空收敛在 service，
  入口只负责报错。
- 向导式步骤，逐步执行；v1 仅「创建目录结构」。新增步骤 = 步骤枚举加
  variant + 对应 service 调用，驱动循环不变。`--dir` 模式静默执行全部
  步骤并汇总。
- 执行顺序：判空 → 建目录 → 写 `workspace.toml` → 入库。

## 中断恢复

- 目录有合法 `workspace.toml`、数据库无此 id：视为上次中断，补注册。
- 有 `workspace.toml` 且已在数据库：视为已初始化，提示
  `workspace list` / `use`。
- 非空且无 `workspace.toml`：硬错误「必须是空目录」。

# 统一注册表语义

- 所有工作目录的服务器在同一个数据库中，`server list` / `start` 等按
  名字全局解析，与「当前在哪个目录」无关。
- 首个 workspace 自动成为 current；后续 init 不修改 current；切换必须
  显式 `mcsup workspace use`（不带名字时 TTY 下交互选择）。
- current 只决定 `server create` / `server link` 缺省落点，不隐藏、不过滤
  任何服务器。禁止「工作区上下文」式的可见性切换。
- current 是**本机**状态，记 `(origin, workspace_id)`：可以指向远端。
  `workspace use r1/ws` 只改本机指针，**不修改对端 `state.json`**；对端机器上
  直接跑 mcsup 的人仍保留自己的 current。
- `workspace use` 支持 `origin/name` 限定名；裸名在本地与远端重名时报歧义并
  列出候选。`use` 之后 `server create` / `server link` 的缺省落点跟随该选择
  （含远端），但 `server ls` 仍全局可见，裸名重名仍要 `r1/name`。
- current 指向的远端离线或被 disconnect 时，缺省落点明确报「来源不可用」，
  **不退回本地**。
- 删除本地 current 时当前标记移交给剩余**本地**中按名称排序的第一个；无剩余
  则无当前，缺省落点操作报 `NoCurrent`。
- `workspace remove --purge` / `server remove --purge` 的 `cwd` 闸：调用方
  进程的工作目录位于待删目录内时拒绝（`CwdInsideTarget`，退出码 4）。子进程
  改不了父进程的工作目录，真删掉会让调用方终端停在已不存在的路径上，之后
  任何子进程都拿不到工作目录，表现为命令静默不执行。用户 cd 出去后重试即可。
  判定在 service（`workspace::ensure_outside`），入口只传 `current_dir()`。
  交互入口在弹「删除磁盘数据?」之前先调 `WorkspaceService::ensure_purgeable`
  （服务器侧 `LifecycleService::ensure_server_purgeable`），删盘条件与
  `remove` 同源，不满足时立刻报错，不让用户先答完确认框与名称才被拒。
- 删除结果必须如实区分「请求了删盘」与「磁盘真的删了」：`remove` 返回
  `RemoveOutcome.disk_purged`，JSON 同时带 `purged` 与 `disk_purged`。目录
  本就不存在时注销照常成功，但 `disk_purged=false`，人类输出报「磁盘数据本就
  不存在」，不得说成已删除。
- 目录已登记但 `workspace.toml` 丢失：`workspace init --dir <同一路径>` 按
  记录里的 id / 名称 / `created_at` 重建 marker（`InitOutcome::MarkerRestored`），
  不新建记录、不改 id。未登记的目录仍走判空闸，不被 init 接管。
- `--workspace` 按名匹配：精确 → 唯一前缀 → 未找到或歧义时列候选报错。
  选择器优先级：flag > 环境变量 `MCSUP_WORKSPACE` > 当前工作区。
- `server create` 不提供自由路径参数；服务器只能落在已注册工作目录的
  `servers/` 下。外部已有目录用 `server link` 引入（符号链接）。
- `server link` 约束：目标不能位于任一工作区 `servers/` 子树内；目标目录
  带合法 `mcsup.json` 时复用其 id，损坏则拒绝，id 已注册也拒绝；注册失败
  回滚符号链接。链接服务器 `server remove --purge` 直接拒绝（目标在工作区
  外，删除不可恢复），普通 `remove` 仅移除链接与登记。
- `workspace doctor` 只读体检，检查项固定为枚举（`service::CheckKind`），
  每项带稳定 kebab-case 标识；判定与文案在 `service`，CLI 只做渲染与退出码：

  | 检查项 | 级别 | 判据 |
  | --- | --- | --- |
  | `no-current-workspace` | warning | 有工作目录但当前标记为空（一个都没有是合法首次状态，不报） |
  | `workspace-dir-missing` | error | 记录中的目录在磁盘上不存在，该工作区后续检查到此为止 |
  | `workspace-marker-missing` | error | 目录在但缺 `workspace.toml`，无法确认身份 |
  | `workspace-marker-mismatch` | error | `workspace.toml` 的 id 与记录不一致 |
  | `servers-dir-unreadable` | error | `servers/` 无法列举（不吞成「无问题」） |
  | `dangling-link` | error | `servers/` 下符号链接目标不存在 |
  | `on-disk-not-registered` | warning | 磁盘有目录 / 链接但注册表里没有 |
  | `registered-but-missing` | error | 已登记但磁盘上找不到 |
  | `rel-dir-not-direct-child` | error | `rel_dir` 不是单层相对路径（`..` / 分隔符 / 绝对路径） |
  | `orphan-server-record` | error | 服务器记录指向的工作目录不在注册表里 |

  有 error 级发现 → 退出码 1（`AppDiagnostic`，不是 `anyhow` 兜底）；注册表
  自身读失败 → 5（后端失败），与「数据有问题」区分。`--strict` 让 warning
  也算失败，`--only <kind,...>` 过滤展示与退出码判定（检查仍然全跑）。
- `server move` 暂不实现（v1 之后再评估，与 `link` 一起做语义会交叉）。

# 元数据存储布局

`{data_dir}/mcsup/db/` 下按集合分目录，**一条记录一个 JSON 文件**：

```
{data_dir}/mcsup/db/
├── .lock                          # 跨进程写锁文件（常驻，锁由内核持有）
├── workspaces/
│   ├── state.json                 # 唯一的当前工作区指针
│   ├── 495493ae-....-36d7c9.json  # 一条工作目录
│   └── b76d0a24-....-166fd31.json
├── servers/
│   └── 60e3cfde-....-a524ecf.json # 一台服务器
└── java/
    ├── state.json                 # 上次扫描时间
    └── 43c75465-....-66922266e8.json
```

## 为什么这样切

- **故障隔离**：一条记录一个文件。某个文件损坏只让那一条读不出，
  `workspace list` / `server list` / `java list` 跳过它照常返回其余项；
  删除损坏文件即可恢复，不需要修整份数据。
- **写入量恒定**：改一台服务器只重写它自己的文件，与记录总数无关。
- **当前工作区只有一处真相**：`workspaces/state.json` 的 `current` 指针。
  记录文件里不存 `is_current`——单值语义若拆成 n 个布尔，切换当前必须改
  两个文件，中途崩溃就会出现两个或零个当前。
- **指针悬空是安全失败**：`current` 指向已删除或损坏的记录时，读取按
  「无当前」处理（下次 `workspace use` 自然覆盖），不改写文件。

## 记录文件格式

每条记录自描述，文件名（去掉 `.json`）必须等于记录 `id`：

```json
// workspaces/<id>.json
{
  "version": 1,
  "id": "<uuid>",
  "name": "alpha",
  "path": "C:\\Users\\me\\servers",
  "created_at": 1700000000,
  "updated_at": 1700000000
}
```

```json
// workspaces/state.json（v2；v1 的裸 uuid 读作 origin=local）
{ "version": 2, "current": { "origin": "local|r1", "workspace": "<uuid>" } | null }
```

```json
// java/state.json
{ "version": 1, "scanned_at": 1700000000 }
```

`servers/<id>.json` 字段：`version`（记录格式）、`id`、`name`、`workspace_id`、
`rel_dir`、`version`（Minecraft 版本号，与记录格式版本同名但值不同）、
`variant`、`jar_path`、`jar_size`、`checksum_algo`、`checksum`、
`created_at`、`updated_at`、`java_major`、`required_java_major`、
`ram_mib`、`autostart`。

`java/<id>.json` 字段：`schema`（记录格式，因 `version` 已被 JDK 版本占用）、
`id`、`path`、`major`、`version`（JDK 版本）、`vendor`、`managed`、
`created_at`、`updated_at`。

## 约束

- `workspaces.name`、`workspaces.path` 各自全局唯一
- `servers.name` 全局唯一，`(workspace_id, rel_dir)` 组内唯一
- `servers.workspace_id` 必须指向存在的 workspace 记录
- 当前工作区至多一个（由 `state.json` 的 `(origin, workspace)` 指针保证，不再是分散的布尔）
- 记录 `id` 只含字母、数字、`-`、`_`
- `jar_*` 四项同生同灭

文件系统没有唯一索引和外键，这些约束在**写入前扫目录核对**。校验时
遇到损坏文件直接失败而不是跳过：看不见的记录可能正好与待写项冲突。

`remove_by_workspace` 是例外：损坏的 server 记录无法判断属于哪个工作区，
删工作区时一并清理。留着它会在之后每次写入时挡住校验，且已无从归属。

`id` 进文件名前必须校验。它既来自目录扫描也来自记录内容，含 `..` 或
分隔符的 id 会把读写引到集合目录之外，这类 id 按损坏拒绝。

## 一致性保证

- **原子写入**：同目录 `.xxx.json.tmp` → `fsync` → `rename` 覆盖。中断只会
  留下替换前或替换后的完整文件；失败时清理临时文件，旧文件不动。
- **跨进程锁**：进程内 `Mutex` + 数据根目录 `.lock` 文件上的**操作系统文件锁**
  （Unix `flock` / Windows `LockFileEx`）。两个 mcsup 进程同时写入时串行化，
  避免「都通过唯一性校验、各自落盘」产生重名。锁挂在持有进程的文件句柄上，
  进程崩溃即自动释放，不需要 pid 存活探测；锁文件因此常驻、不删除——删文件
  会让等待者在新旧 inode 上各持一把锁。等锁上限 30 秒。
- **身份自校验**：文件名与内部 `id` 不一致、`version` / `schema` 不符、
  JSON 非法、`id` 越界，均报 `corrupt repository record` 并带上相对路径。
- **读路径不因 `state.json` 瘫痪**：`state.json` 是全局单点。它损坏时
  `workspace list` / `server list` 按「无当前」降级照常返回，与悬空指针
  同一语义；写入仍直接报错，不静默覆盖一份读不懂的状态文件。

实现见 `crates/storage/src/lib.rs`，字段映射见同目录 `models.rs`。
旧版 `mcsup.db` / `registry.json` 不做迁移。
