# CLI 子命令设计

`mcsup` 命令采用统一的 `<对象> <动作> <名字> ...` 结构，关键约定：

- **动作总在名字前**：`mcsup server start <name>`、`mcsup server backup create <name>`。
- **建站只有一个入口**：`server create`。整合包是它的一种变体
  （`--variant modrinth-modpack`，短名 `--variant modrinth`，或 `--modpack <ID>`
  隐含该变体）：选定变体后搜索可开服的整合包、展示详情、选版本，再按清单装
  加载器与 mod。变体列表按「用户需要懂多少」排序：`vanilla` 与整合包在前，
  模组加载器其次，服务端分支与代理在后。
- **凡属某个服务器的内容，挂在 `server` 下**：备份（`backup`）、Mod（`mod`）。
- **服务属性统一 `server set`**：JVM 绑定、内存、自启等一律
  `mcsup server set <name> <key> <value>`，不为一类属性单开子命令；
  TTY 下缺 key / value 交互补齐。
- **机器级命令**：`java`（本机 JVM 运行时）、`remote`（远端接入与管理）。
- **平台级命令直接动作**：`doctor` / `config` / `update` 等；工作目录经
  `workspace` 子命令管理。
- **服务器只能建在已注册工作目录下**：`--workspace <名>` 按名选择，
  缺省用当前工作区（`--workspace` > 环境变量 `MCSUP_WORKSPACE` > 当前工作区）；
  目录规范与 init 行为见 `docs/workspace.md`。

## 全局交互约定

所有子命令一致，机器契约以 `docs/embedding.md` 的 CLI 契约一节为准：

- 参数优先级：`flag > 环境变量（MCSUP_WORKSPACE） > 交互提示 / 默认值`。
- **必填参数一律可省略，缺哪项补哪项**：凡是必填参数（服务器名、版本、变体、
  快照 id、工作区名、属性键 / 值等）都既可用位置参数 / flag 一次给全，也可在
  human + TTY 下只给一部分，剩余由交互补齐。例如
  `mcsup server create web --version 1.21.1` 只差变体，就只问变体。
- 必选参数缺失只在 human 输出 + TTY 下进交互补齐；`--output json` 或非 TTY
  下不等待输入，stderr 提示用法并以退出码 2 结束。
- 全局 `-y` / `--yes` 只跳过**确认式提问**（是 / 否）与危险二次确认；它
  **不**跳过必填参数的补参交互（名称、版本、变体、快照 id 等）。因此
  `mcsup server create -y` 在缺名称 / 版本时仍会提问（TTY）或退出 2（非交互）。
  eula 另有 `--accept-eula`；非交互下缺对应开关 → 退出码 2。
- `server create --workspace` 接受工作区**名称或路径**：命中已注册工作区直接使用；
  未命中时配合 `--workspace-dir <路径>` / `--workspace-name <名称>` 新建并落进去；
  名称与路径指向不同工作区、或名称已存在但路径不一致 → 退出码 2，不静默接管。
- `server create` 交互补参里**名称那一格只查本地注册表**：它是早提示，不必为
  一台离线远端付建连等待（远端读走本地缓存，但缓存可能还没同步过）。名称
  唯一性的判定点仍在登记阶段（`ServerRegistry::create` 查全量来源），那里
  可能等远端，因此挂 spinner 回显。
- 耗时步骤（网络列表获取、下载、扫描）走 `apps/cli/src/progress.rs` 的
  单行回显，JSON 模式隐藏进度。
- 交互组件集中在 `apps/cli/src/prompts/`：步骤回显、目录三选项、名称输入、
  可过滤选择、输入名称确认；`workspace init` / `server create` /
  `server remove` / `server set` 共用。组件只做 UI，业务判定仍在
  service / domain。
- 提示行首符号统一：待答 `标签 > 输入`、已答 `> 标签 答案`，不用 inquire
  默认的 `?`；选择器只给名字看不出差别的候选补一句灰色说明（服务端类型里
  `vanilla` 是「官方」、`modrinth` 是「整合包」），说明只影响渲染，不参与
  过滤与提交值。
- `workspace init` 无 `--dir` 且可交互时：目录三选项（数据目录 =
  `{data_dir}/mcsup/workspaces/<名称>`；当前目录；自定义目录，输入时校验、
  非法重输）→ 名称（默认=目录名，不合法退回序号 `workspaceNN`）→ 确认。
  非交互无 `--dir` 保持用当前目录 + service 序号的既有机器行为。

### 补参覆盖范围与已知缺口

已实现命令全部遵守上面的补参规则：`server create / install / start / stop /
restart / status / console / send / logs / props / set / remove / version`、
`server backup create / list / restore / delete / verify / prune`、
`server mod list / add / remove / update / outdated`、
`workspace init / use / remove`、`java install / uninstall`、
`config set / unset`。

`remote` 已全部接入（无参数局域网发现除外，显式报「尚未实现」）。`update`
（自升级）仍未实现，入口直接返回「尚未实现」。

### 界面语言

面向人的文案（帮助、错误提示、进度、doctor 输出）跟随界面语言；机器可读的
输出不随语言变化，见 `docs/embedding.md` 的 CLI 机器契约。

优先级：

```
--lang <auto|zh|en>  >  MCSUP_LANG  >  平台 locale（sys-locale）  >  英文
```

- `--lang auto`（默认）按上面顺序解析；认不出的语言标签逐级向下回退，
  不直接跳英文。例如 `MCSUP_LANG=ja_JP` 且 `LANG=zh_CN.UTF-8` 时给中文。
- 语言标签只取主语言子标签：`zh` / `zh-CN` / `zh_CN.UTF-8` / `zh-Hant-TW`
  一律中文，`en*` 一律英文。
- 终端明确声明非 UTF-8 字符集（如 `LANG=zh_CN.GBK`）时回退英文：中文在那种
  终端上只会是乱码。非终端（管道、重定向）不做此判断。
- 回退只作用于「用户没表态」的情形：`--lang` 或 `MCSUP_LANG` 点名中文时不做
  码页回退，否则等于无视用户指令。
- `--output json` 的 stdout、错误分类、退出码与语言无关，逐字节稳定。
- Windows 的 locale 来自系统设置而非 `LANG` / `LC_ALL`。

文案落点：

| 内容 | 位置 |
| --- | --- |
| 命令元数据（`about` / 参数说明） | `apps/cli/src/meta.rs` |
| 结构化错误提示、doctor 文本 | `apps/cli/src/i18n.rs` |
| 交互提示（步骤、选择器、校验、确认）标签与帮助行 | 调用方用 `i18n::pick` / `i18n::text` 给出双语，`prompts` 只做渲染 |
| 结果文案、表格表头、进度回显（`正在…`、`扫描到`） | `apps/cli/src/i18n.rs` 的 `pick` / `text` |
| clap 硬编码文案（`Usage:` 等） | `apps/cli/src/help.rs` 的替换表 |
| 语言解析 | `apps/cli/src/locale.rs` |

约定：**用户可见的一切文案都不得在命令里硬编码单一语言**。交互组件本身不知
道语言，`Lang` 由调用方从 `Printer::lang()` 取；`prompts::*` 的每个入口都显式
接收 `Lang`，校验错误与帮助行也按它渲染。新增文案时用 `i18n::pick`（带插值）
或 `i18n::text`（纯字面量），不要写 `if lang == Zh { ... }` 的分叉。

命令元数据不能用 `///` 文档注释写文案：clap 在编译期把它固化成字面量，
运行时切不了语言。必须写成 `about = crate::meta::xxx()` 形式的表达式。

`domain` / `service` 的结构化错误只携带判定（`kind()`）与参数，`Display`
是英文短句、仅供日志；用户可见文案一律由入口按 `kind()` 渲染。

## 子命令简写

每一级子命令都提供固定简写，**全拼始终有效**，简写只是额外入口：

- 简写最少两个字母，单字母不提供（`mcsup s` 报 `unrecognized subcommand`）。
- 简写是显式登记的别名，不做前缀推断：`mcsup server l` 不会猜成 `list`。
- 歧义与拼错由 clap 报错并列出候选；`--help` 展示 `[alias: ...]`。
- 简写与全拼解析到同一个命令，退出码、stdout / JSON 输出完全一致。
- 别名表在 `apps/cli/src/cli.rs` 的 `#[command(visible_alias = ...)]`，
  由单测保证「≥ 2 个字母」且「同层不重复」。

| 层级 | 简写 |
| --- | --- |
| 顶层 | `ws` workspace · `srv` server · `jv` java · `rmt` remote · `doc` doctor · `cfg` config · `md` mod · `up` update · `comp` completions · `ver` version |
| `workspace` | `in` / `new` init · `ls` list · `sw` use · `rm` remove · `doc` doctor |
| `server` | `ls` list · `cr` create · `in` install · `up` start · `down` stop · `rs` restart · `st` status · `con` console · `snd` send · `log` logs · `prop` props · `cfg` set · `rm` remove · `ln` link · `ver` version · `bak` backup · `md` mod |
| `server backup` | `cr` create · `ls` list · `rt` restore · `rm` delete · `vf` verify · `pr` prune |
| `server mod` | `ls` list · `ad` add · `rm` remove · `up` update · `od` outdated |
| `java` | `ls` list · `sc` scan · `in` install · `un` / `rm` uninstall |
| `mod` | `in` inspect |
| `remote` | `ls` list · `sv` serve · `cd` code · `con` connect · `st` status · `pg` ping · `dc` disconnect · `rv` revoke |
| `config` | `ls` show · `st` set · `us` unset · `ed` edit |

## 命令层级

```
mcsup
├── workspace (ws)            # 工作区：创建 / 注册表 / 当前切换
│   ├── init (in|new) [--dir <path>] [--name <name>] [--origin <名字>]
│   ├── list (ls) [--origin <o>] / use (sw) [<name>] / remove (rm) <name> [--purge] / doctor (doc)
├── server (srv)              # 单台服务：生命周期 + 属主内容
│   ├── list (ls) [--origin <o>] / create (cr) [--origin <名字>] [--modpack <项目>] / link (ln) [--origin <名字>] / install (in)
│   ├── start (up) / stop (down) / restart (rs)
│   ├── status (st) / console (con) / send (snd) / logs (log)
│   ├── props (prop) / set (cfg) / remove (rm) / version (ver)
│   ├── backup (bak)  create (cr) · list (ls) · restore (rt) · delete (rm) · verify (vf) · prune (pr)
│   └── mod (md)      list (ls) · add (ad) · remove (rm) · update (up) · outdated (od)
├── java (jv)                 # 机器级 JVM 运行时
│   └── list (ls) [--origin <o>] / scan (sc) [--origin <名字>] / install (in) [--origin <名字>] / uninstall (un|rm) [--origin <名字>]
├── remote (rmt)              # 远端执行位置（Backend）
│   ├── serve (sv) start [--listen <addr>] [--name <n>] [--foreground] / stop [--force] / status
│   ├── connect (con) [<user@host> | <host[:port]>] [--name <n>] [--read-only]
│   └── list (ls) / status (st) / ping (pg) / disconnect (dc) / revoke (rv)
└── doctor (doc) / config (cfg) / update (up)
    / completions (comp) / version (ver) / help
```

`config` 子命令：`show (ls)` / `set (st)` / `unset (us)` / `edit (ed)`。
`set` / `unset` 缺键时 TTY 下从全部配置键中交互选择，`set` 缺值时交互输入；
非 TTY 缺键 / 缺值 → 退出码 2。

括号内为简写，最少两个字母，全拼始终有效。

CLI 是 Thin Entry Point，不承载业务逻辑；退出码、stdout/stderr、JSON 等
机器契约见 `docs/embedding.md` 的 CLI 契约一节。

## mcsup server（服务端生命周期）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `list` | `mcsup server list [--origin <all\|local\|remote\|名字>]` | 列出所有服务与状态；`--origin` 按来源过滤（缺省 `all`），见 `docs/remote.md` |
| `create` | `mcsup server create [<name>] [--version <ver>] [--variant <v>] [--workspace <名或路径>] [--workspace-name <名>] [--workspace-dir <路径>] [--origin <名字>] [--modpack <项目>]` | 建站并下载核心 JAR；`--origin <远端>` 在对端创建（普通变体：登记 + 下载核心；`--modpack`：对端读清单、装加载器、铺 overrides，一次调用完成；TTY 下缺 `--variant` / `--version` 时从对端变体 / 版本列表交互补齐），本地缺省行为不变（变体从可用能力列选，缺省 vanilla；版本交互从列表选，也接受 `latest` / `lts` / `latest-snapshot` 别名，`lts` 等价 `latest`）。`--modpack` 隐含整合包流程，`--version` 此时是整合包版本（可省略取最新正式版），与只会选加载器的 `--variant` 互斥（同用 → 退出码 2）；只给 `--variant modrinth-modpack` 而缺 `--modpack` → 2。`--workspace` 命中已有工作区直接使用，未命中时配合 `--workspace-name` / `--workspace-dir` 新建；名称与路径不一致 → 退出码 2。TTY 缺名/缺版本进向导（名称重输直到唯一，步骤回显 `[n/3]`，参数给定则跳过对应步）；无工作区且可交互时先复用 `workspace init` 向导再继续；非 TTY 缺 `--version` 或缺名 → 退出码 2；显式变体未知/未支持 → 2 并列出可用变体；下载失败保留已注册结果，提示 `mcsup server install <name>` 重试。`--variant forge` 走「下载安装器 + 运行 `--installServer`」两步；`neoforge` 同理。`quilt` 的安装器是子命令式的（`install server <mc> <loader> --download-server --install-dir=<dir>`），产物是 `quilt-server-launch.jar` + `libraries/`，启动方式为单 JAR |
| `install` | `mcsup server install [<name>] [--version <ver>]` | 给既有服务安装/更换服务端核心（JAR），校验后落盘；交互无 `--version` 从版本列表选择（Release/Snapshot/旧版标注），非 TTY 缺省 `latest` 解析为 manifest 当前稳定版；服务器不存在 → 退出码 3。目标在对端时下载、校验、安装器执行全部在对端，支持裸名指向唯一远端；此时未给 `--version` 沿用在册版本（远端没有「列版本」帧，避免不可见的交互等待）。变体为 `forge` / `neoforge` 时要先运行安装器，缺可用 Java → 退出码 3 并提示先装 Java |
| `start` | `mcsup server start [<name>] [--accept-eula]` | 经后台 supervisor 拉起 JVM（`server set java-version` 绑定了大版本则用该版本，否则取已注册 Java 最高大版本；核心 JAR 取已安装项；堆上限 `server set ram` 优先、缺省 2048 MiB），返回 supervisor PID；EULA 未同意时 TTY 询问，非交互缺 `--accept-eula` → 退出码 2；已在运行 → 4；无核心 / 无 Java / 绑定大版本无运行时 → 3；spawn / IPC 失败 → 5 |
| `stop` | `mcsup server stop [<name>] [--grace <sec>] [--force]` | 先优雅停机（默认宽限 30s，上限 3600s，超过按用法错误退出 2），宽限后 `--force` 强杀；未在运行幂等成功（exit 0）；超时未 `--force` → 退出码 1 并提示 `--force` |
| `restart` | `mcsup server restart [<name>] [--accept-eula] [--grace <sec>] [--force]` | 先停后启：停机规格与 EULA 同 start / stop，停机超时中止不拉起 |
| `status` | `mcsup server status [<name>]` | 注册版本 / 变体 + 实时状态、进程、运行时长、退出码；未在运行显示 `stopped`（exit 0） |
| `console` | `mcsup server console [<name>]` | 交互附加控制台；附加时先回放 `console.log` 末尾 30 行历史，再持续输出新增；`Ctrl+D` 只分离不断开进程；Windows 无 EOF 键，`Ctrl+D` / `Ctrl+Z` 随行到达、回车生效，Unix 行首 `Ctrl+D` 立即生效；键位表统一在 `keymap`，未绑定的控制字符按文本注入；非 TTY → 退出码 2。服务器在对端时经帧跟随日志、按对端 id 注入与探活（同名本地服务器不会被误注入） |
| `send` | `mcsup server send [<name>] ["<命令>"]` | 单条命令注入服务端 stdin；未在运行 → 退出码 4 |
| `logs` | `mcsup server logs [<name>] [--tail <n>] [--follow]` | 查看日志（`--tail` 默认 30），`--follow` 持续跟踪（JSON 模式 JSONL，一行一个对象） |
| `props` | `mcsup server props [<name>] [key=value ...]` | 无参列出 `server.properties`（TTY 下随后进入交互：选键改值、逐项写入、Esc 退出）；`key=value` 直接写入并提醒重启生效；单独 `key` 缺值时 TTY 交互输入、非 TTY → 退出码 2；键 / 值非法 → 2。键不得含 `=`、`:`、`#`、`!`、`\` 或空白（含空格、制表符）：这些字符在 Java `Properties` 里是分隔符 / 注释前缀 / 转义符，写进去服务器读到的键会与我们写的不同（`bad key=v` 在 Java 侧是键 `bad`、值 `key=v`）；值不得含换行。服务器在对端时文件经帧读写：传原文、两端各自解析，注释与顺序保留 |
| `set` | `mcsup server set [<name>] [<key>] [<value>]` | 设置服务属性，见下表；TTY 下缺 key 从键列表选、缺 value 按类型补齐（java-version 从已注册大版本选、ram 输入、autostart 二选一）；非 TTY 缺参 → 退出码 2；`java-version` 绑定无运行时的大版本 → 3。服务器在对端时值在对端校验并持久化，`java-version` 候选取对端已注册运行时 |
| `link` | `mcsup server link <path> [--name <name>] [--workspace <ws>] [--origin <名字>]` | 注册服务器目录：外部目录建符号链接进工作区（目标不能在任一工作区 `servers/` 内）；目标是某工作区 `servers/` 的直接子目录时不建链接、就地注册（恢复注册丢失的服务器，名称必须与目录名一致，`--workspace` 必须与目录归属一致）；带 `mcsup.json` 时复用其身份。`--origin <远端>` 时 `<path>` 是**目标机器**上的路径，链接、marker 与入库全在对端完成 |
| `remove` | `mcsup server remove [<name>] [--purge] [--origin <名字>]` | `--origin <远端>` 在对端注销（可选删盘）。缺名称时交互选择要删除的服务器；默认注销保留磁盘；交互下询问是否删除磁盘数据（默认不删），同意后红色警示并要求输入服务器名称确认（`-y` 跳过这两道确认）；`--purge` 跳过询问直接删目录（链接服务器拒绝 `--purge`，仅移除链接）；非交互不询问；终端工作目录就在该服务器目录内时拒绝（退出码 4，cd 出去后重试）；删盘的前置条件在确认框之前就校验，不满足立刻报错；JSON 同时输出 `purged`（请求了删盘）与 `disk_purged`（磁盘真的删了），目录本就不存在时后者为 `false` |
| `version` | `mcsup server version [<name>]` | 当前安装的版本、变体、来源（provider 名）、核心大小与校验和；未安装核心提示 `server install` |

### 内部入口（隐藏）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `__supervise` | `mcsup __supervise --dir <path> --java <path> --jar <path> [--ram <mib>] [--arg-file <path> ...]` | `server start` 拉起的 supervisor 进程本体：托管 JVM、写 `console.log`、经本地 socket 提供状态 / 停机 / 注入 IPC；不在 `--help` 出现，退出码 0 正常停机 / 1 异常。`--jar` 与重复的 `--arg-file` 二选一：前者是单一核心 JAR，后者是参数文件启动（Forge 安装器产出的 `user_jvm_args.txt` + 平台 `*_args.txt`） |

### server set（服务属性）

属性键统一 kebab-case，随领域扩展追加；设置会校验并持久化，非法值报用法错误（2），`java-version` 绑定无已注册运行时的大版本报缺资源（3）。`ram` 规范化后展示（如 `2G` → `2048M`）；`start` 时优先级为显式选项 > 服务属性 > 平台默认。

| 键 | 值 | 说明 |
| --- | --- | --- |
| `java-version` | 运行时版本 | 绑定本服务的 JVM 大版本（从 `mcsup java` 已注册运行时中按 major 匹配） |
| `ram` | 如 `2G` / `1024M` | 启动内存上限 |
| `autostart` | `true` / `false` | 机器启动时自动拉起 |
| `backup-scope` | 如 `world,mods` 或 `all` | 备份范围：逗号分隔的顶层目录；缺省（`all`）= 整个服务器目录 |

## mcsup server backup（备份，属于服务端）

备份按工作区共享 CAS 存储：`{workspace}/backups/cas` 存内容对象（SHA-256 两级分片），`{workspace}/backups/snapshots` 存快照记录。同一工作区内多台服务器、多个快照对相同内容只存一份。

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `create` | `mcsup server backup create [<name>] [--label <text>] [--scope <a,b>] [--no-reuse]` | 默认全量，有历史时按父快照增量复用未改动文件。运行中的服务器先发 `save-off`，完成后发 `save-on`（不停服）。`--scope` 覆盖服务属性 `backup-scope`，缺省为整个服务器目录。`--no-reuse` 强制重读全部文件 |
| `list` | `mcsup server backup list [<name>] [--page <n>] [--page-size <n>]` | 分页历史表格：ID、时间、状态、文件数、大小、标签（缺省 20 条/页）。`--page` / `--page-size` 必须大于 0，0 是用法错误 → 退出码 2（页码 1 基，不做静默纠正） |
| `restore` | `mcsup server backup restore [<name>] [<id>] [--to <新名>]` | 恢复。缺 `id` 时交互选择。**原地恢复要求服务器已停止**（运行中返回退出码 4），恢复前自动生成安全快照；`--to` 恢复到新服务，原服务不动：新名必须是合法服务器名、目标目录必须不存在，恢复出的目录带全新身份（`mcsup.json` 新 id），之后可直接 `server link` 注册 |
| `delete` | `mcsup server backup delete [<name>] [<id>]` | 删除某个快照记录；内容对象不即时删除，交由 `prune` 回收 |
| `verify` | `mcsup server backup verify [<name>] [<id>]` | 校验快照引用的所有对象存在且哈希匹配；有损坏时返回退出码 1 |
| `prune` | `mcsup server backup prune [<name>] [--keep <n> \| --older-than <dur>] [--dry-run]` | 按保留数或年龄删除快照，并回收整个工作区中不再被任何快照引用的 CAS 对象。`--dry-run` 只预览 |

备份范围也可由服务属性 `backup-scope` 设定为仅备份个别顶层目录（见 `mcsup server set`）。

## mcsup server mod（Mod，属于服务端）

操作**某台服务端**的 `mods/`（或 `--kind plugin` 的 `plugins/`）目录。与
`mcsup mod inspect` 互补：这里回答「这台服务端装了什么、该装什么」，那里回答
「这个 jar 能不能进服务端」。

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `list` | `mcsup server mod list [<name>] [--kind <k>] [--project-info] [--source <s>] [--outdated]` | 扫描 `mods/` 的直接子项并做本地检查（加载器、客/服归属、依赖闭包、风险、**与这台服务端的匹配**）。默认**不联网**也不建表：目录本身是唯一权威，手工丢进去的文件同样会被列出。「适配」列答「装到这台服务端对不对得上」（加载器 / MC 版本 / Java），与「风险」列正交；`rejected` 的条目在表下逐条给出理由。`blocking` 的条目同样逐条给出原因（缺哪个必需依赖 / 仅客户端）——只打标记的话，用户只能从服务端日志里那行加载器报错反推。`--project-info` 才算 SHA1 反查上游项目与版本；`--outdated` 在此基础上逐项查候选版本，只列有更新的条目 |
| `add` | `mcsup server mod add [<name>] [<spec>] [--version <id>] [--kind <k>] [--source <s>] [--prerelease] [--force] [--dry-run]` | `spec` 支持项目 ID / slug、`https://modrinth.com/<type>/<slug>` 链接、`@search <关键词>`。**不给 `spec` 时交互搜索**：打开即列这台服务端能跑的 mod（按下载量排序），输入框与列表同屏，回车按关键词远端搜索；命中项超过一页时列表末尾出现「下一页」「上一页」，Esc 逐级返回（搜索结果 → 推荐列表 → 取消）。列表只含**能在服务端运行**的项目（上游 `server_side` facet 过滤，仅客户端的光影加载器、小地图一类不会出现）。选中项目后同屏展示详情并选版本（默认只列正式版，`--prerelease` 放开），版本列表也由服务端的加载器与 MC 版本过滤。选完走两阶段：先算计划并展示（来源、版本、落点、是否替换），确认后才下载。`--version` 精确指定版本（版本 ID 与版本号都认），缺省按服务端加载器与 MC 版本自动挑最优候选。旧名 `install` / `in` 保留为别名 |
| `remove` | `mcsup server mod remove [<name>] [<mod>] [--kind <k>] [--dry-run]` | 按文件名（可省略 `.jar`）或 modid 定位；modid 命中多个文件时不猜，列出候选并退 2。文件同样进 `.mcsup-trash/`，不直接删除 |
| `update` | `mcsup server mod update [<name>] [<mod>] [--kind <k>] [--source <s>] [--force] [--dry-run]` | 识别 → 逐项查候选 → `decide_update` → 展示清单 → 确认后逐个安装。不给 `<mod>` 时处理全部可更新项。换名更新会把旧文件删掉，避免两份同 modid 的 jar |
| `outdated` | `mcsup server mod outdated [<name>] [--kind <k>] [--source <s>]` | 只读版 `update`：只列可更新项，不下载 |
| `disable` | `mcsup server mod disable [<name>] <文件>…` | 把文件移进 `<server>/mods/disabled/`：加载器不扫描子目录，原名不变，随时能搬回来。与 `remove` 的区别是语义——移除表达「不要了」，停用表达「暂时不用」 |
| `fix` | `mcsup server mod fix [<name>] [--kind <k>] [--from-log] [--yes]` | 按本地审计算出修复计划（补装缺失依赖 / 移出无法装载的文件）→ 展示 → 确认 → 执行。**移除是移进 `<server>/.mcsup-trash/`，不是删除**：判定是工具的推断，判错时用户能原样取回。`--from-log` 换掉计划来源：读 `console.log` 末尾 500 行，把崩溃栈里非平台包的帧归因到 `mods/` 下的具体文件并规划移除，同时展示缺失类与原始栈帧；归因不到的类进 `unattributed`，不猜文件。两条来源不合并——元数据判定答「装之前能不能装」，崩溃归因答「装完了为什么起不来」 |

`--kind` 只接受 `mod` 与 `plugin`（默认 `mod`）：这两个类别由服务端从顶层目录
直接装载，改动的就是 `mods/` 与 `plugins/`。其余类别落点不在这里——数据包在
`world/datapacks/`，服务端资源包由 `server.properties` 下发——因此直接报用法
错误（退出码 2），而不是把文件放进一个永远不会被读到的目录。

约定：

- **服务端运行中一律拒绝改动**（计划与落盘都拒，退出码 4）。判定问 supervisor
  的实时状态，不猜「应该没在跑」；状态取不到时按失败处理，不当作已停机。
- **落盘顺序**：下载到 `<server>/.mcsup-staging/<n>/mods/` → 用 `ZipModInspector`
  检查 → 检查通过才原子替换进 `mods/`。暂存目录在用完（含失败）后删除。
- **检查拒绝时不落盘**（退出码 4）：明确不可用于服务端、或缺少必需依赖。
  `--force` 放行，但风险与依据照样打印，不让强制变成静默。
- `fix` 逐项汇总，不整批失败：**依赖按链解析**（补装的 mod 自己还缺依赖时
  继续往下补，成环或超深时停下并报告），一项装不上只记一笔并说明原因——
  缺依赖、上游标注仅客户端、上游没有可装版本各不相同，都写到条目上而不是
  一句「被拒绝」。正式版优先，上游只有预发布时才接受预发布（部分依赖确实
  只发 beta），否则必需依赖永远补不上。
- **补装的提供方按「谁提供这个 modid」找，不按「服务端可用」筛**：库类 mod
  的侧别标注经常有误（`satin`、`melody` 都是服务端必需的依赖，上游却标
  `server_side: unsupported`），先按该标注筛会把本体整条滤掉，只剩名字里
  恰好含该词的近似项目，补装必然失败并报成「上游找不到提供方」。检索取
  `slug` / `id` 与 modid **相等**的命中优先，`<modid>-<后缀>` 次之，只包含
  的最后；侧别交给安装检查判——真正只有客户端代码的 jar 会得 `blocking`
  并被拒，带 `main` 入口的库只会得 `review` 并放行。
- **候选逐个试，不因第一个不合格就放弃**：上游按名字检索，名字最像的未必
  提供该 modid。实测 `satin` 首位是 `satin-free-wakes`（modid 是 `wakes`），
  本体 `satin-api` 排第二；只取首位会让补装被 `expect_modid` 拒掉，然后
  整条依赖被误报成「上游找不到提供方」——而提供方就在结果里。现在同一
  modid 的全部候选都排进计划，安装时按 modid 逐个校验，第一个不提供就换
  下一个；只有候选全部试完才记进 `unresolved`。
- **能从整合包清单读到的，一律不搜索**：装整合包时把清单留在
  `<server>/.mcsup-modpack/manifest.json`（详见
  [整合包清单快照](#整合包清单快照)）。清单里的每个文件都带**项目 ID 与版本
  ID**——Modrinth 的下载地址形如
  `https://cdn.modrinth.com/data/{project_id}/versions/{version_id}/{file}`，
  两个 ID 都在地址里。`fix` 先按 modid 查这份快照，命中就按项目 ID + 版本 ID
  直装，一个检索请求都不发；只有快照里没有时才退回按名字检索。搜索是猜，
  按 ID 是定位。
- 上游不认识某个文件时 `update` / `outdated` 把它列进「无法判断」，不当作
  「已是最新」。
- `remote` 条目（`--origin` 指向远端服务器）在本地直接拒绝（退出码 2）：
  远端 mod 管理需要协议帧，排在后续阶段。
- **表格不画框线**：列之间只留空格，表头靠首行位置区分。框线按字符数排、
  内容按显示宽度排，中文一出现就对不齐，列多时比没有框更难看。`mod` 相关
  命令统一走 `output::compact_table()`。

## mcsup mod（Mod 检测，不属于具体服务端）

对任意 jar 做环境检测，不依赖已安装的服务端。与 `server mod`（操作某个
服务端里的已装 mod）互补：前者回答「这个 jar 能不能进服务端」，后者回答
「这台服务端装了什么」。

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `inspect` | `mcsup mod inspect <path>... [--offline] [--loader <加载器>] [--mc <版本>] [--server <名称>]` | 检测 jar 文件或目录（只取直接子项）。报告加载器、MC 区间、Java 版本、客/服归属、风险等级。`--offline` 只做本地检查，不访问上游 API；`--loader` 指定目标加载器（缺省按整批文件共同支持的加载器推断，见下）。给了 `--mc` / `--server` 时额外判定「能不能装到这台服务端」，见「目标服务端匹配」 |

### 判定口径

| 风险 | 含义 | 处置建议 |
| --- | --- | --- |
| `ok` | 服务端侧明确可用 | 正常安装 |
| `review` | 判不出服务端归属，或认不出加载器 | **默认仍装**，标记后人工确认 |
| `blocking` | 服务端侧明确不支持，或缺少必需依赖 | 从 `mods/` 排除 |

客/服归属给**四档**（`required` / `optional` / `unsupported` / `unknown`），
不是二元。`unknown` 绝不折叠成 `ok`：那会让用户以为已经检查过了。

判不出的条目在人类输出里必定列出「依据」与「建议」两条，不允许只打结论。

### 检查项与可靠度

| 检查项 | 依据 | 可靠度 |
| --- | --- | --- |
| 加载器类型 | `fabric.mod.json` / `mods.toml` / `neoforge.mods.toml` / `mcmod.info` / `plugin.yml` | 确定 |
| 客/服（显式声明） | Fabric `environment`、Forge / NeoForge `[[mods]] side` | Fabric 确定；Forge 的 `[[mods]] side` 只作证据（见下） |
| Java 编译版本 | class 文件头第 6-7 字节，换算 `major - 44` | 确定 |
| 依赖 / 不兼容 | `depends` / `recommends` / `breaks`，含 `provides` 别名与嵌套 jar | 确定 |
| 依赖归属 | 依赖按声明它的清单归类（fabric / quilt / forge / neoforge），只采信目标加载器那份 | 确定 |
| 依赖生效侧 | Forge / NeoForge `[[dependencies.x]] side`：`CLIENT` 的依赖在服务端不参与校验 | 确定 |
| Quilt 条件依赖 | `unless` / `unless_any`：豁免项存在时该依赖不再必需 | 确定 |
| 目标 MC 区间 | Fabric `depends.minecraft`、Forge `versionRange`，**按加载器分开记** | 高 |
| 客/服（入口点推断） | 只有 `client` 入口、无 `main`/`server` | 中（有双端库反例） |
| 客/服（字节码） | 常量池引用 `net/minecraft/client/`、`blaze3d` | 中，仅非 Fabric 生态采信 |

**依赖按加载器校验**：一个 jar 可以同时带多份清单（实测 `lukis-*` 同时有
`fabric.mod.json` 与 `quilt.mod.json`），把不同加载器的依赖合并会造出假缺失
（Quilt 侧的 `quilt_resource_loader` 在 Fabric 服务端根本不需要）。因此：

- 给了 `--loader` 就只采信那份清单
- 没给则由这批文件**共同支持的唯一加载器**推断（取交集；含资源包 / 认不出
  的文件不参与交集）
- 交集不唯一（分属多个加载器）时目标加载器为「未确定」，带加载器归属的依赖
  一律不参与校验，只校验与加载器无关的依赖——宁可不判，也不误报
- 输出会明确写出本次按哪个加载器校验，JSON 的 `target_loader` 同义

**Fabric 的字节码扫不出客户端类**：Fabric mod 运行在 intermediary 名字空间，
引用的是 `net/minecraft/class_2338` 而非 `net/minecraft/client/Minecraft`。
这是刻意保留的已知限制，因此该信号对 Fabric 生态**不参与判定**，只作证据。

### Forge 的 `side` 字段

`mods.toml` 里有两种 `side`，含义完全不同，不能一起处置：

| 写法 | 加载器是否读 | 判定 |
| --- | --- | --- |
| `[[mods]] side` | **不读**。Forge 1.19.2 的 `ModInfo` / `ModFileInfo` 常量池里没有这个字段名，它既不参与装载筛选也不参与依赖校验 | 只记证据，服务端侧留「判不出」，风险最多到 `review` |
| `[[dependencies.x]] side` | 读。校验依赖前按当前 dist 过滤，dedicated server 上 `CLIENT` 整条跳过 | 计入判定：`CLIENT` 的依赖在服务端不算缺失 |

实测反例（两条都由现场启动日志确认）：`CreativeCore` 写着
`[[mods]] side="CLIENT"` 却能在 dedicated server 上装载；`embeddiumplus` 的
四个依赖块全写 `side="CLIENT"`，服务端照常进 Done。旧口径把前者判成
`blocking`、把后者报成「缺少必需依赖 `embeddium`」，两者都是误判。

**MC 区间按加载器分开记**：同一 jar 可以同时带 `fabric.mod.json` 与
`META-INF/mods.toml`，两份清单各写各的区间（实测 `Cell-Forge-1.19.2.jar`
前者是 `~1.20`、后者是 `[1.19.2]`）。判定与展示都按**目标加载器**那一份取值，
合成一格会必然错一半。

### 目标服务端匹配

风险等级回答「这个 jar 本身是不是服务端的东西」，与装到哪台服务端无关。
「能不能装到这台服务端」是另一个正交问题，由**适配**（`fit`）回答：

| 维度 | 判定 | 说明 |
| --- | --- | --- |
| 加载器 | 文件声明的加载器里有目标加载器 | `Quilt` 目标接受 Fabric mod（Quilt Loader 保留兼容层），反向不成立；其余跨加载器兼容随版本变化，不猜 |
| MC 版本 | 文件声明的区间是否覆盖目标版本 | 没声明区间算满足：加载器对「没有 `minecraft` 依赖」的处理就是不施加版本限制，数据包类 mod 普遍如此 |
| Java | class 文件头实测的最大版本 ≤ 服务端实际用的版本 | 只可能给出不匹配：没扫到 class（数据包、扫描预算用尽）说明不了任何问题 |

结论三档：`installable`（三维度都满足）、`unclear`（没有不匹配，但有维度
判不出）、`rejected`（至少一个维度明确不满足）。`unclear` 只在加载器维度
出现（jar 里没有可识别的加载器元数据），因此它是少数派而不是常态——一台
正常服务端的列表里不该有一大片「待确认」。

- 目标条件来自 `--server`（读该服务端的加载器 / MC 版本 / Java）或
  `--loader` + `--mc`（描述一台尚未登记的服务端）；两者互斥
- `--server` 与 `server mod list` 走同一个构造函数，Java 大版本优先取服务端
  元数据记录的，旧记录按 MC 版本推算——与启动判定同源
- 没问的维度不参与结论：只给 `--loader` 时，MC / Java 未知不会把结论拖成
  `unclear`
- `server mod list` 的「适配」列与 `mod inspect` 的 `fit` 字段是同一份形状，
  脚本不必按命令名分两套解析
- 已知限制：原版服务端没有 mod 加载器，加载器维度因此不参与判定（它本来也
  不读 `mods/`）。这类服务端上只看 MC 版本与 Java 两个维度

### 不做什么

只报告，不改开服流程：不删文件、不改清单、不阻断安装。是否排除某个 mod
由用户决定。

## mcsup workspace（工作目录）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `init` | `mcsup workspace init [--dir <path>] [--name <name>] [--origin <名字>]` | 创建并注册工作目录；无 `--dir` 进入交互（默认当前目录），目录必须为空；`--dir` 是目标机器上的路径，`--origin` 选择远端，缺省本地 |
| `list` | `mcsup workspace list [--origin <all\|local\|remote\|名字>]` | 已注册工作目录：名称、来源、路径、服务器数、是否当前；`--origin` 按来源过滤（缺省 `all`） |
| `use` | `mcsup workspace use [<name>] [--origin <名字>]` | 切换当前工作区（`--origin <远端>` 切换对端的当前工作区）；接受 `origin/name` 限定名；不带名字在 TTY 下交互选择；只影响 `create` / `link` 缺省落点，不隐藏其他目录的服务器 |
| `remove` | `mcsup workspace remove [<name>] [--purge] [--origin <名字>]` | `--origin <远端>` 在对端注销（可选删盘）。本地注销：同时清除该工作目录下的服务器登记；交互下询问是否删除磁盘数据（默认不删），同意后红色警示并要求输入工作区名称确认，`--purge` 跳过询问直接删除；删盘的前置条件（终端工作目录不在该工作区内、目录里的 `workspace.toml` 属于该记录）在确认框之前就校验，不满足立刻报错；终端工作目录就在该工作区内时拒绝（退出码 4，cd 出去后重试）；删除失败（目录被占用）保留注册，退出占用后重试即可；JSON 同时输出 `purged`（请求了删盘）与 `disk_purged`（磁盘真的删了），目录本就不存在时后者为 `false`；删除当前工作区时当前标记移交给剩余中按名称排序的第一个 |
| `doctor` | `mcsup workspace doctor [--only <kind,...>] [--strict]` | 只读体检工作区与注册表一致性：10 项检查（见 `docs/workspace.md` 的表），error 级非零退出；`--only` 按检查项标识过滤，`--strict` 让 warning 也算失败 |

## 整合包（`server create --variant modrinth-modpack`）

整合包不是独立命令，而是 `server create` 的一种变体。选定后进入显式向导：

```
推荐列表（默认列表，无关键词，按下载量）+ 输入框
  │ 输入关键词 + 回车
  ↓
搜索结果（「下一页 / 上一页」翻页） ──Esc──→ 推荐列表
  ↓ 选中
详情 + 版本 ──Esc──→ 结果
  ↓ 选中版本 → 确认（默认 Y，输入 n 回到选版本）
读 `.mrpack` 清单 → 装加载器核心 → 铺 `overrides/` → 下载清单文件
```

- 打开即显示**默认列表**：不发送关键词，上游按下载量返回能在服务端运行的
  整合包。输入框与列表同屏：不输关键字就是推荐列表，输入关键字回车即转成
  搜索结果，不需要先进单独的搜索页。
- 搜索是**真实的服务端搜索**：关键词直接作为 `ModpackQuery::keyword` 发给
  Modrinth，不是先拉一页再本地过滤，因此能搜到第一页之外的结果。输入过程中
  列表只按当前这页做即时过滤，回车才发远端搜索。
- 结果按**下载量**排序（上游 `index=downloads`），每项先显示 ID（灰色）再显示标题。
- 每页 20 条；结果页底部按需出现翻页项，上一页在上、下一页在下。搜索结果页按
  Esc 回到推荐列表；推荐列表本身按 Esc 取消整个建站。
- 搜索返回 0 条也留在结果页：标题说明没有匹配，继续输入新关键词回车即可
  重搜，按 Esc 回到推荐列表，而不是直接结束整个建站流程。
- 搜索结果只包含支持服务端的整合包。上游 facet 是粗筛：实测带
  `server_side:required|optional` 的查询仍会返回 `server_side=unsupported` 的
  整合包（Sodium Plus 一类纯客户端包），因此命中项还会按响应体里的字段再筛
  一遍——判定口径一处收敛，不额外发请求。
- 详情与版本列表在同一屏：两者并行取回后一次性呈现，详情打完紧接着就是版本
  选择，不会先出简介再刷出版本列表。版本列表即使只有一个候选也照常提问，
  选中后还必须确认一次（默认「是」）才开始下载——整合包动辄几百 MB，不能让
  用户没看见版本号就开始装；确认时输入 `n` 回到版本选择，Esc 回到上一步，
  Ctrl+C 取消整个命令。显式给了 `--version` 时视为已经指定，不再追问。
- 详情按「标签列 + 值列」对齐输出，值列起点固定（按显示宽度补齐，中文标签算
  双宽），标签与值之间不加冒号；不画表格边框，避免挤占宽度。可选字段（slug /
  作者 / 许可证）缺省时不占行。

| 用法 | 说明 |
| --- | --- |
| `mcsup server create <name> --variant modrinth` | 交互向导（`modrinth-modpack` 等价） |
| `mcsup server create <name> --modpack <ID或slug>` | 直接指定整合包（隐含该变体；非交互可用） |
| `mcsup server create <name> --modpack <ID> --version <版本ID或版本号>` | 固定版本，脚本可复现 |

非交互模式必须用 `--modpack` 指定项目；只用 `--variant modrinth` 且没有
`--modpack` 会以用法错误退出，而不是静默挑一个包。

注册表记录的是清单加载器对应的真实变体（`fabric` / `forge` / `neoforge` /
`quilt` / `vanilla`），因此建站后 `server install` 能按变体找到核心来源；
`modrinth-modpack` 只作为「走整合包流程」的入口标记。

同一版本同时提供客户端与服务端文件时，优先下载文件名带 `[server]` 的
`.mrpack`；没有服务端归档才退回主文件。不指定版本时，默认跳过没有服务端
归档的最新正式版。版本列表里带服务端归档的版本会标记「服务端」并排在前面。
客户端包里仅客户端模组可能被错误标记为 `server: required`，用服务端归档可
避免启动崩溃。

清单的 `env` 字段不是权威数据。安装时还会把 `mods/` 下 jar 的 sha1 批量
反查 Modrinth（`POST /version_files` → `POST /projects`），取上游的项目级
标注与包内依赖关系。

**上游标注 `server_side = unsupported` 不等于可以丢掉**。判定是「仅客户端
**且** 包内无人依赖」才跳过。库类项目（提供 API 的 `satin` / `melody` 一类）
常被标成仅客户端，实际却是别的 mod 跑起来的硬前提：服务端加载器不加载它们，
但依赖声明要求它们**存在于 `mods/`**。实测 Aged 4.0.1 里的
`satin-2.0.0.jar` 上游标 `unsupported`，而 `environmentz` 硬依赖 modid
`satin`；按旧口径跳过它，服务端启动直接
`HARD_DEP_NO_CANDIDATE` 起不来。被跳过文件的旧落盘会被删除。查询失败时
中止安装（避免装出一个启动必崩的服务端），来源不支持这项能力时退回「按清单
装」。JSON 输出里的 `skipped_client_only` 与人类可读输出都会报告跳过数量。

**「谁被依赖」要迭代到不动点**。被跳过的成员不该为别人提供保护：`sodium`
被 `indium` / `iris` / `reeses` 依赖，而这三个自己都是仅客户端、都会被跳过，
一次统计会把 `sodium` 判成「有人依赖」而留下，服务端照样起不来。判定因此从
「服务端可用」的成员出发，沿必需依赖反复扩张到不再有新成员（详见
`PackMemberGraph::retained`）。反过来，留下的成员需要的库即使自己被标成仅
客户端也必须装，同一条规则两向都成立。

**装完再按文件本身复核一次**。前面两步信的都是声明：清单的 `env`、上游项目的
`server_side`。声明与实际不符时只有读 jar 才能发现（Fabric 的
`environment: client`、Forge 的 `displayMode` 都在文件里）；依赖齐不齐、加载器
对不对得上，更是只有把整批文件摆在一起才判得出来。`apply` 结束后对整台服务端的
`mods/` 与 `plugins/` 跑一次本地检测，列出会让服务端起不来的条目：缺必需依赖、
仅客户端、与目标服务端不匹配（加载器 / MC 版本 / Java）。

判定与 `server start` 的预检是**同一份代码**（`ModInspectService::blockers_in_dir`），
因此这里报空就等于启动不会被 mod 拦住。两处各判一套迟早分叉成「装完说没事、
启动却被拦下」。

安装本身**不动这些文件**：怎么处置是用户的决定。交互式终端里会打印一张无边框
表格（序号 / 文件 / 原因），然后收一个选择：

```
#  文件                                          原因
1  numismatic-overhaul-0.2.18+1.20.jar           只支持 fabric，服务端是 forge（可停用或换适配版本）
2  PuzzlesLib-v8.1.33-1.20.1-Forge.jar            缺少依赖 puzzlesaccessapi（可 mod fix 补装）
3  owo-lib-0.11.2+1.20.jar                        只支持 fabric，服务端是 forge（可停用或换适配版本）
4  sophisticatedbackpacks-1.20.1-3.24.12.1411.jar 缺少依赖 sophisticatedcore（可 mod fix 补装）

停用哪些 [N]: 1,3
```

输入逗号分隔的序号（全角逗号也接受）、`A` 全选、`N` 不选（回车即 `N`）。输入
非法时说明错在哪（不是序号 / 序号越界 / 没读到序号）再问一次，不把用户踢回上
一步。选中的文件移进 `<server>/mods/disabled/`：加载器不扫描子目录，原名不变，
用户随时能搬回来。非交互（`--output json` / 管道）不提问，只打印结论与下一步。

库类项目不参与「仅客户端」这一类（`environment: client` 是误标高发区）。

装了 Sinytra Connector（Forge↔Fabric 桥）的服务端会被自动识别：桥 jar 本身不再
被误判成「仅客户端」，目录里的 Fabric mod 也不再被算作「只支持 fabric，服务端是
forge」——桥把 Fabric 并入了目标服务端可装载的加载器集合，因此这些 mod 会保留而
不是默认停用。

清单里 `server: unsupported` 的文件不会装到服务端；`optional` 视为需要安装。
`.mrpack` 由内置 zip 解析直接解开，不调用系统 `unzip`，归档内的 UTF-8
文件名按原样落盘。

清单里的 sha1 是**打包时的快照**。整合包作者或上游事后重新上传同一版本的文件
（修 bug、换文件）后，清单哈希就会过期，使下载本应成功却卡在
`checksum mismatch`。遇到这种情况，安装会用清单里的下载地址回源（Modrinth
`GET /version/{id}`）取**当前**权威 sha1：若与下载到的字节一致，就接受该文件并
继续安装（人类可读输出与 JSON 的 `stale_hashes` 都会提示数量），只有权威哈希也
不符时才如实报错。来源不支持回源或地址不是 Modrinth CDN 时，保持原有的严格校验
行为。

建好的整合包服务与 `server create` 建的服务是同一种对象，后续运维都走
`mcsup server ...`；若建站时因上游波动导致核心或文件下载失败，可用
`mcsup server install <name>` 重试。

### 整合包清单快照

安装完成时把清单落在 `<server>/.mcsup-modpack/manifest.json`。它记的是
`.mrpack` 里 `modrinth.index.json` 的**全部内容**，加上整合包身份与加载器：

| 字段 | 用途 |
| --- | --- |
| `project_id` / `version_id` | 整包的项目与版本 ID |
| `name` / `version_number` / `pack_version` | 包名与两个口径的版本号 |
| `minecraft` / `loader` | 清单声明的 MC 版本与加载器 |
| `dependencies` | 清单的 `dependencies` 原样保留（含未知键） |
| `files[]` | 每个文件一条：`path`、`downloads`、`size`、`sha1`/`sha512`、`client`/`server`、**`project_id`/`version_id`**、`modids`、`mod_version` |

两个 ID 从下载地址解析（`/data/{project_id}/versions/{version_id}/`），装完
就有了。`modids` 是装好之后读 jar 回填的：清单只说文件名，不说 modid，
读一次才知道谁提供什么。

**为什么留这份**：

- **后续操作按 ID 定位，不搜索**。`mod fix` 缺 `satin` 时直接查到提供它的
  是项目 `fRbqPLg4` 的版本 `Tq9qJzQz`，按 ID 装即可。搜索是按名字猜，
  实测搜 `satin` 首位是毫不相干的 `satin-free-wakes`。
- **整合包更新要按项目 ID 比对前后版本**。清单里的 `files[]` 保留**全部**
  条目，包括没装到服务端的仅客户端文件——只留已装文件会漏掉「上游把某个
  仅客户端库改成服务端必需」这种情况。
- **清单哈希是打包时的快照**，回源校验要用下载地址里的版本 ID；丢了清单
  就只能重下整个整合包。

快照是缓存不是契约：文件不存在、JSON 损坏、`format` 版本对不上时一律当作
「没有快照」，退回按名字检索的旧路径，不让可疑数据影响安装。

## mcsup java（机器级 JVM 运行时）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `list` | `mcsup java list [--origin <all\|local\|remote\|名字>]` | 已注册运行时；未扫描过则自动扫描本机；`--origin` 按来源过滤（缺省 `all`） |
| `scan` | `mcsup java scan [--origin <名字>]` | 扫描本机 Java 并登记；`--origin <远端>` 让对端扫描并返回其运行时列表 |
| `install` | `mcsup java install [<version>] [--jre] [--vendor <name>] [--origin <名字>]` | 从远端下载安装 JDK（默认）或 JRE；缺省版本时交互选择。`--origin <远端>` 在目标机器上查询来源、下载、解压、登记，客户端只搬大版本 / 包类型 / 厂商 |
| `uninstall` | `mcsup java uninstall [<version或路径>] [--dir <路径>] [--origin <名字>]` | 卸载；系统 Java 仅移除注册记录，文件保留。缺目标时 TTY 下从已注册运行时中交互选择（无注册运行时提示先 `java scan`）；同一 major 多个运行时且在 TTY 下交互选择；非 TTY 须用 `--dir` 显式指定路径。`--origin <远端>` 在对端卸载，目标解析用对端返回的运行时列表 |

JVM 安装在平台数据目录 `{data_dir}/mcsup/java/`，不属于任何工作目录。
不同服务器通过 `server set java-version <major>` 绑定各自的大版本，不设全局 default。
下载源按启动时探测延迟排序（foojay / adoptium / azul），单源失败自动切换下一个。

## mcsup remote（远端执行位置 = Backend）

接入只需两侧各两条命令：被连接端 `mcsup remote serve start` 后再按需
`mcsup remote code`（后台进程，配对码按需发放；系统服务注册未实现），连接端
`mcsup remote connect <host[:port]> --code <配对码>`。
之后 `workspace` / `server` / `java` / `backup` 命令不变，远端资源以
`ORIGIN` 列出现在原有列表中。接入有两条路：`user@host` 走 SSH 引导（经
SSH 起远端 agent + 本地 `ssh -L` 隧道，每条命令临时建立），
`host[:port]` 直连（远端 `remote serve` 常驻）。无参数局域网发现**尚未
实现**。完整设计见 `docs/remote.md`。

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `serve start` | `mcsup remote serve start [--listen <addr>] [--name <n>] [--foreground]` | 本机作为被连接端：**后台**启动 agent 并打印名称 / 地址 / 指纹 / 日志路径；`--foreground` 留前台调试。启动**不生成配对码** |
| `serve stop` | `mcsup remote serve stop [--force]` | 发 `SIGTERM` 并等实例锁释放；`--force` 强杀；未运行时幂等成功 |
| `serve status` | `mcsup remote serve status` | 离线读是否运行、地址、指纹、pid、已配对客户端、配对码剩余时间（**不回显码**） |
| `code` | `mcsup remote code [--ttl <秒>] [--clear]` | 在本机生成一枚一次性配对码（缺省 300 秒，范围 30–3600，只显示这一次）；`--clear` 作废当前码 |
| `connect` | `mcsup remote connect <host[:port]> [--name <名>] [--code <码>] [-y]` | 直连配对：probe 捕获指纹 → 展示确认（非交互需 `-y`）→ 交互命名（默认最小空闲序号 `r1` / `r2` …，`--name` 跳过）→ `Enroll` → 写配置与凭证 |
| `reconnect` | `mcsup remote reconnect <name>` | 复用记录重新认证；指纹变化要求重新 connect |
| `list` | `mcsup remote list` | 已配对远端、连接状态（`ok` / `unreachable` / `unauthorized`）、角色、agent 版本 |
| `status` | `mcsup remote status [<name>]` | 单个远端详情（缺名等价 list） |
| `ping` | `mcsup remote ping <name>` | 连通性 / 版本 / 能力，供脚本使用 |
| `disconnect` | `mcsup remote disconnect <name>` | 删除本地配对与凭证，远端不受影响；当前工作区指向该远端时自动切回本地最后使用的工作区 |
| `revoke` | `mcsup remote revoke <client>` | 在 serve 端**本机**离线删除客户端记录；该会话在下一帧请求前被拒（token 立即失效） |

### 来源（origin）与过滤

每个资源带一个来源：`local` 或远端名。名字在本地与所有远端之间可能重名，
解析顺序为「`origin/name` 限定名 → 全局唯一 → 报错并列出候选」。

列表命令统一支持 `--origin`：

| 取值 | 含义 |
| --- | --- |
| `all`（缺省） | 本地 + 全部已配对远端 |
| `local` | 仅本地 |
| `remote` | 全部远端，不含本地 |
| `<name>` | 指定远端 |

- 缺省值可由环境变量 `MCSUP_ORIGIN` 覆盖，优先级 `flag > 环境变量 > all`。
- `all` / `local` / `remote` 是保留字，不能用作远端名；远端名禁止包含 `/`。
- 过滤只影响展示与返回集合，不改变命令语义；过滤后为空是成功（退出码 0）。
- 人类输出增加 `ORIGIN` 列；JSON 每项增加 `origin` 字段。
- 远端不可达时该来源的条目省略并记 warn（本地缓存未实现），本地条目照常返回。

## 平台级

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `doctor` | `mcsup doctor [--only <kind,...>] [--strict]` | 平台环境自检：数据目录可写、配置可解析、Java 可用、来源已装配。完全离线，不发任何网络请求。error 级非零退出；`--only` 按检查项标识过滤（只影响展示与退出码，检查本身全跑），`--strict` 让 warning 也算失败 |
| `config` | `mcsup config show \| set <key> <value> \| unset <key> \| edit` | 查看/修改/重置/编辑平台配置；`edit` 打开 `$VISUAL` / `$EDITOR`，非交互下退 2 |
| `update` | `mcsup update` | 升级 mcsup 自身。**尚未实现**：入口直接返回「尚未实现」（退出码 1）。自升级需要 release 产物地址与校验策略，先排期再实现；`--version <v>` 的取值语义（跟随该版本）在实现时一并定义 |
| `completions` | `mcsup completions <zsh\|bash\|fish>` | 生成 shell 补全脚本 |
| `version` | `mcsup version` | 版本、构建与协议信息（`mcsup --version` 只打一行 `mcsup <ver>`，两者互不影响） |
| `help` | `mcsup help [<group>]` | 分组帮助 |

### doctor 检查项

`--only` 的取值即 JSON 的 `kind`，顺序稳定：

| `kind` | 级别 | 含义 |
| --- | --- | --- |
| `data-dir-missing` | warning | 数据目录尚未创建（首次写元数据时自动创建） |
| `data-dir-not-writable` | error | 数据目录存在但写入探测失败 |
| `config-invalid` | error | `config.toml` 无法解析或取值非法 |
| `no-java` | error | 没有可用的 Java 运行时 |
| `no-java-scanned` | warning | 已扫描过本机但未发现 Java |
| `java-missing` | error | 登记的 Java 路径已不在磁盘上 |
| `no-jar-source` | error | 没有可用的服务端下载源（内置来源装配失败） |

`doctor` 只读：不建目录、不改配置、不写注册表，修复由报告里的 `fix` 提示引导用户执行。
自检**完全离线**：只读本机状态，不检查来源连通性、不拉取版本列表——上游可达性属于
「现在能不能下载」而非「环境有没有问题」。

### config.toml 键表

文件位于 `{data_dir}/config.toml`，`--config <path>` 可覆盖。键名 kebab-case，
未写入的键取平台缺省；`config unset <key>` 即删除该项回到缺省（已是缺省时为幂等成功）。

| 键 | 类型 | 取值 | 缺省 |
| --- | --- | --- | --- |
| `data-dir` | 路径 | 绝对路径；`~` 展开，相对路径按当前目录展开，只做词法规整不解析符号链接 | 系统数据目录下的 `mcsup` |
| `download-concurrency` | 整数 | `1`–`64`；批量下载同时进行的文件数 | `8` |
| `download-sources` | 列表 | 逗号分隔的 `http(s)://` 前缀，必须是纯前缀（不带查询串与锚点）；为空表示只用清单给的地址 | 空 |
| `download-inflight` | 整数 | `1`–`512`；批量并发与分段并发合起来的同时在途连接数 | `48` |

`download-sources` 存的是**前缀**而不是完整地址：镜像站按上游同样的路径回源
（Modrinth 是 `data/<项目>/versions/<版本>/<文件名>`），下载时只替换 scheme 与
host，路径原样接上。前缀按目录理解，`https://m.example/prefix` 与
`https://m.example/prefix/` 等价。每条前缀成为一个独立候选源，参与自动选源
（见 `docs/architecture.md` 的「下载源（镜像）」）：首次使用时并发探测，取最快
的那个并落盘记住，之后同一个上游面沿用该选择；下载失败会作废选择、重新探测。
非法条目被忽略并记一条警告，不影响其余条目与平台启动。

内置的镜像源（BMCLAPI、MCIM）无需配置即生效；本键用于追加**你自己的**镜像。

写入走「临时文件 → fsync → rename」，写入失败时旧配置原样保留；校验在落地前完成，
取值非法时文件不会被创建或改动。