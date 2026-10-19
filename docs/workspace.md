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
`~/Library/Application Support`）。惰性创建：任一需要数据库的命令首次
运行时创建。

```
{data_dir}/mcsup/
├── config.toml                   # 平台级偏好，不含 workspace 列表
├── db/
│   └── mcsup.db                  # 唯一元数据库
└── java/                         # 机器级 JVM（mcsup java）
    ├── 21/
    └── 17/
```

JVM 属机器级命令，禁止放进工作目录：切换工作目录不换 JVM，多工作目录
不重复占盘。workspace 列表存数据库，不进 `config.toml`，避免双真相源。

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

# workspace init

```
mcsup workspace init [--dir <path>] [--name <name>]
```

- `--dir` 缺省时进入交互，输入默认 `.`；确认页显示规范化绝对路径。
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
- 首个 workspace 自动成为 default；后续 init 不修改 default；切换必须
  显式 `mcsup workspace use`。
- default 只决定 `server create` 缺省落点，不隐藏、不过滤任何服务器。
  禁止「工作区上下文」式的可见性切换。
- `--workspace` 按名匹配：精确 → 唯一前缀 → 未找到或歧义时列候选报错。
- `server create` 不提供自由路径参数；服务器只能落在已注册工作目录的
  `servers/` 下。

# 数据库概要

```
workspaces(id PK, name UNIQUE, path UNIQUE, is_default,
           created_at, updated_at)
servers(id PK, name UNIQUE, workspace_id FK, rel_dir, version, variant,
        jar_path, jar_size, checksum_algo, checksum,
        created_at, updated_at, UNIQUE(workspace_id, rel_dir))
```

表结构与迁移细节以 `crates/storage/migrations` 为准。
