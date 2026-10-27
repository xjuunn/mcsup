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
- 耗时步骤（网络列表获取、下载、扫描）走 `apps/cli/src/progress.rs` 的
  单行回显，JSON 模式隐藏进度。
- 交互组件集中在 `apps/cli/src/prompts/`：步骤回显、目录三选项、名称输入、
  可过滤选择、输入名称确认；`workspace init` / `server create` /
  `server remove` / `server set` 共用。组件只做 UI，业务判定仍在
  service / domain。
- `workspace init` 无 `--dir` 且可交互时：目录三选项（数据目录 =
  `{data_dir}/mcsup/workspaces/<名称>`；当前目录；自定义目录，输入时校验、
  非法重输）→ 名称（默认=目录名，不合法退回序号 `workspaceNN`）→ 确认。
  非交互无 `--dir` 保持用当前目录 + service 序号的既有机器行为。

### 补参覆盖范围与已知缺口

已实现命令全部遵守上面的补参规则：`server create / install / start / stop /
restart / status / console / send / logs / props / set / remove / version`、
`server backup create / list / restore / delete / verify / prune`、
`workspace init / use / remove`、`java install / uninstall`、
`config set / unset`。

尚未实现的命令（`server mod`）参数声明已按规则改为可选，但入口在 service
能力落地前直接返回「尚未实现」，因此缺失必填参数时给的是未实现错误而非
补参交互。service 层实现后，入口只需接上已有的 `require_server_name` /
补参组件即可。`remote` 已全部接入（无参数局域网发现除外，显式报「尚未
实现」）。

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
| 顶层 | `ws` workspace · `srv` server · `jv` java · `rmt` remote · `doc` doctor · `cfg` config · `up` update · `comp` completions · `ver` version |
| `workspace` | `in` / `new` init · `ls` list · `sw` use · `rm` remove · `doc` doctor |
| `server` | `ls` list · `cr` create · `in` install · `up` start · `down` stop · `rs` restart · `st` status · `con` console · `snd` send · `log` logs · `prop` props · `cfg` set · `rm` remove · `ln` link · `ver` version · `bak` backup · `md` mod |
| `server backup` | `cr` create · `ls` list · `rt` restore · `rm` delete · `vf` verify · `pr` prune |
| `server mod` | `ls` list · `in` install · `rm` remove · `up` update · `od` outdated |
| `java` | `ls` list · `sc` scan · `in` install · `un` / `rm` uninstall |
| `remote` | `ls` list · `sv` serve · `con` connect · `st` status · `pg` ping · `dc` disconnect · `rv` revoke |
| `config` | `ls` show · `st` set · `us` unset · `ed` edit |

## 命令层级

```
mcsup
├── workspace (ws)            # 工作区：创建 / 注册表 / 当前切换
│   ├── init (in|new) [--dir <path>] [--name <name>] [--origin <名字>]
│   ├── list (ls) [--origin <o>] / use (sw) [<name>] / remove (rm) <name> [--purge] / doctor (doc)
├── server (srv)              # 单台服务：生命周期 + 属主内容
│   ├── list (ls) [--origin <o>] / create (cr) [--origin <名字>] / link (ln) / install (in)
│   ├── start (up) / stop (down) / restart (rs)
│   ├── status (st) / console (con) / send (snd) / logs (log)
│   ├── props (prop) / set (cfg) / remove (rm) / version (ver)
│   ├── backup (bak)  create (cr) · list (ls) · restore (rt) · delete (rm) · verify (vf) · prune (pr)
│   └── mod (md)      list (ls) · install (in) · remove (rm) · update (up) · outdated (od)
├── java (jv)                 # 机器级 JVM 运行时
│   └── list (ls) [--origin <o>] / scan (sc) [--origin <名字>] / install (in) / uninstall (un|rm)
├── remote (rmt)              # 远端执行位置（Backend）
│   ├── serve (sv) [--listen <addr>] [--name <n>] / serve --stop / serve --status
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
| `create` | `mcsup server create [<name>] [--version <ver>] [--variant <v>] [--workspace <名或路径>] [--workspace-name <名>] [--workspace-dir <路径>] [--origin <名字>]` | 建站并下载核心 JAR；`--origin <远端>` 在对端只做登记（核心需在对端 `server install`），本地缺省行为不变（变体从可用能力列选，缺省 vanilla；版本交互从列表选，也接受 `latest` / `lts` / `latest-snapshot` 别名，`lts` 等价 `latest`）。`--workspace` 命中已有工作区直接使用，未命中时配合 `--workspace-name` / `--workspace-dir` 新建；名称与路径不一致 → 退出码 2。TTY 缺名/缺版本进向导（名称重输直到唯一，步骤回显 `[n/3]`，参数给定则跳过对应步）；无工作区且可交互时先复用 `workspace init` 向导再继续；非 TTY 缺 `--version` 或缺名 → 退出码 2；显式变体未知/未支持 → 2 并列出可用变体；下载失败保留已注册结果，提示 `mcsup server install <name>` 重试。`--variant forge` 走「下载安装器 + 运行 `--installServer`」两步；`neoforge` 同理 |
| `install` | `mcsup server install [<name>] [--version <ver>]` | 给既有服务安装/更换服务端核心（JAR），校验后落盘；交互无 `--version` 从版本列表选择（Release/Snapshot/旧版标注），非 TTY 缺省 `latest` 解析为 manifest 当前稳定版；服务器不存在 → 退出码 3。变体为 `forge` / `neoforge` 时要先运行安装器，缺可用 Java → 退出码 3 并提示先装 Java |
| `start` | `mcsup server start [<name>] [--accept-eula]` | 经后台 supervisor 拉起 JVM（`server set java-version` 绑定了大版本则用该版本，否则取已注册 Java 最高大版本；核心 JAR 取已安装项；堆上限 `server set ram` 优先、缺省 2048 MiB），返回 supervisor PID；EULA 未同意时 TTY 询问，非交互缺 `--accept-eula` → 退出码 2；已在运行 → 4；无核心 / 无 Java / 绑定大版本无运行时 → 3；spawn / IPC 失败 → 5 |
| `stop` | `mcsup server stop [<name>] [--grace <sec>] [--force]` | 先优雅停机（默认宽限 30s），宽限后 `--force` 强杀；未在运行幂等成功（exit 0）；超时未 `--force` → 退出码 1 并提示 `--force` |
| `restart` | `mcsup server restart [<name>] [--accept-eula] [--grace <sec>] [--force]` | 先停后启：停机规格与 EULA 同 start / stop，停机超时中止不拉起 |
| `status` | `mcsup server status [<name>]` | 注册版本 / 变体 + 实时状态、进程、运行时长、退出码；未在运行显示 `stopped`（exit 0） |
| `console` | `mcsup server console [<name>]` | 交互附加控制台；附加时先回放 `console.log` 末尾 30 行历史，再持续输出新增；`Ctrl+D` 只分离不断开进程；Windows 无 EOF 键，`Ctrl+D` / `Ctrl+Z` 随行到达、回车生效，Unix 行首 `Ctrl+D` 立即生效；键位表统一在 `keymap`，未绑定的控制字符按文本注入；非 TTY → 退出码 2 |
| `send` | `mcsup server send [<name>] ["<命令>"]` | 单条命令注入服务端 stdin；未在运行 → 退出码 4 |
| `logs` | `mcsup server logs [<name>] [--tail <n>] [--follow]` | 查看日志（`--tail` 默认 30），`--follow` 持续跟踪（JSON 模式 JSONL，一行一个对象） |
| `props` | `mcsup server props [<name>] [key=value ...]` | 无参列出 `server.properties`（TTY 下随后进入交互：选键改值、逐项写入、Esc 退出）；`key=value` 直接写入并提醒重启生效；单独 `key` 缺值时 TTY 交互输入、非 TTY → 退出码 2；键 / 值非法 → 2 |
| `set` | `mcsup server set [<name>] [<key>] [<value>]` | 设置服务属性，见下表；TTY 下缺 key 从键列表选、缺 value 按类型补齐（java-version 从已注册大版本选、ram 输入、autostart 二选一）；非 TTY 缺参 → 退出码 2；`java-version` 绑定无运行时的大版本 → 3 |
| `link` | `mcsup server link <path> [--name <name>] [--workspace <ws>]` | 注册服务器目录：外部目录建符号链接进工作区（目标不能在任一工作区 `servers/` 内）；目标是某工作区 `servers/` 的直接子目录时不建链接、就地注册（恢复注册丢失的服务器，名称必须与目录名一致，`--workspace` 必须与目录归属一致）；带 `mcsup.json` 时复用其身份 |
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
| `list` | `mcsup server backup list [<name>] [--page <n>] [--page-size <n>]` | 分页历史表格：ID、时间、状态、文件数、大小、标签（缺省 20 条/页） |
| `restore` | `mcsup server backup restore [<name>] [<id>] [--to <新名>]` | 恢复。缺 `id` 时交互选择。**原地恢复要求服务器已停止**（运行中返回退出码 4），恢复前自动生成安全快照；`--to` 恢复到新服务，原服务不动 |
| `delete` | `mcsup server backup delete [<name>] [<id>]` | 删除某个快照记录；内容对象不即时删除，交由 `prune` 回收 |
| `verify` | `mcsup server backup verify [<name>] [<id>]` | 校验快照引用的所有对象存在且哈希匹配；有损坏时返回退出码 1 |
| `prune` | `mcsup server backup prune [<name>] [--keep <n> \| --older-than <dur>] [--dry-run]` | 按保留数或年龄删除快照，并回收整个工作区中不再被任何快照引用的 CAS 对象。`--dry-run` 只预览 |

备份范围也可由服务属性 `backup-scope` 设定为仅备份个别顶层目录（见 `mcsup server set`）。

## mcsup server mod（Mod，属于服务端）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `list` | `mcsup server mod list <name>` | Mod 列表与版本、分组状态 |
| `install` | `mcsup server mod install <name> <spec>` | `spec` 支持 ID/URL/`@search 关键词`；先显示依赖与冲突再确认 |
| `remove` | `mcsup server mod remove <name> <mod>` | 卸载单个 Mod |
| `update` | `mcsup server mod update <name> [<mod>]` | 更新指定或全部（先 dry-run 列出变更） |
| `outdated` | `mcsup server mod outdated <name>` | 可更新清单 |

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
搜索 ──Esc──→ 取消整个建站
  ↓ 回车（关键词发给上游，offset=0）
结果 ──Esc──→ 搜索（改关键词即重搜）
  ↓ 选中          （「下一页 / 上一页」翻页）
详情 + 版本 ──Esc──→ 结果
  ↓ 选中版本（即开始创建）
读 `.mrpack` 清单 → 装加载器核心 → 铺 `overrides/` → 下载清单文件
```

- 搜索是**真实的服务端搜索**：关键词直接作为 `ModpackQuery::keyword` 发给
  Modrinth，不是先拉一页再本地过滤，因此能搜到第一页之外的结果。
- 结果按**下载量**排序（上游 `index=downloads`），每项先显示 ID（灰色）再显示标题。
- 每页 20 条；结果页底部按需出现「下一页 / 上一页」。Esc 回搜索即重搜。
- 搜索返回 0 条也留在结果页：标题说明没有匹配，列表为空时按 Esc 回搜索改
  关键词，而不是直接结束整个建站流程。
- 搜索结果只包含支持服务端的整合包（`server_side` facet 过滤纯客户端整合包）。
- 详情与版本列表在同一屏：详情按行输出，紧接着就是版本选择；选中版本后直接
  开始创建，不再有单独的确认步骤。Esc 回到上一步，Ctrl+C 取消整个命令。

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

清单里 `server: unsupported` 的文件不会装到服务端；`optional` 视为需要安装。
`.mrpack` 由内置 zip 解析直接解开，不调用系统 `unzip`，归档内的 UTF-8
文件名按原样落盘。

建好的整合包服务与 `server create` 建的服务是同一种对象，后续运维都走
`mcsup server ...`。

## mcsup java（机器级 JVM 运行时）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `list` | `mcsup java list [--origin <all\|local\|remote\|名字>]` | 已注册运行时；未扫描过则自动扫描本机；`--origin` 按来源过滤（缺省 `all`） |
| `scan` | `mcsup java scan [--origin <名字>]` | 扫描本机 Java 并登记；`--origin <远端>` 让对端扫描并返回其运行时列表 |
| `install` | `mcsup java install [<version>] [--jre] [--vendor <name>]` | 从远端下载安装 JDK（默认）或 JRE；缺省版本时交互选择 |
| `uninstall` | `mcsup java uninstall [<version或路径>] [--dir <路径>]` | 卸载；系统 Java 仅移除注册记录，文件保留。缺目标时 TTY 下从已注册运行时中交互选择（无注册运行时提示先 `java scan`）；同一 major 多个运行时且在 TTY 下交互选择；非 TTY 须用 `--dir` 显式指定路径 |

JVM 安装在平台数据目录 `{data_dir}/mcsup/java/`，不属于任何工作目录。
不同服务器通过 `server set java-version <major>` 绑定各自的大版本，不设全局 default。
下载源按启动时探测延迟排序（foojay / adoptium / azul），单源失败自动切换下一个。

## mcsup remote（远端执行位置 = Backend）

接入只需两侧各一条命令：被连接端 `mcsup remote serve`（前台进程，系统服务
注册未实现），连接端 `mcsup remote connect <host[:port]> --code <配对码>`。
之后 `workspace` / `server` / `java` / `backup` 命令不变，远端资源以
`ORIGIN` 列出现在原有列表中。接入有两条路：`user@host` 走 SSH 引导（经
SSH 起远端 agent + 本地 `ssh -L` 隧道，每条命令临时建立），
`host[:port]` 直连（远端 `remote serve` 常驻）。无参数局域网发现**尚未
实现**。完整设计见 `docs/remote.md`。

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `serve` | `mcsup remote serve [--listen <addr>] [--name <n>]` | 本机作为被连接端：前台运行 agent，打印名称 / 地址 / 指纹 / 配对码（码过期自动轮换） |
| `serve --stop` | `mcsup remote serve --stop` | 按 `{data_dir}/remote/agent.pid` 终止前台进程 |
| `serve --status` | `mcsup remote serve --status` | 离线读本机身份、指纹、pid 与已配对客户端 |
| `connect` | `mcsup remote connect <host[:port]> [--code <码>] [-y]` | 直连配对：probe 捕获指纹 → 展示确认（非交互需 `-y`）→ `Enroll` → 写配置与凭证 |
| `reconnect` | `mcsup remote reconnect <name>` | 复用记录重新认证；指纹变化要求重新 connect |
| `list` | `mcsup remote list` | 已配对远端、连接状态（`ok` / `unreachable` / `unauthorized`）、角色、agent 版本 |
| `status` | `mcsup remote status [<name>]` | 单个远端详情（缺名等价 list） |
| `ping` | `mcsup remote ping <name>` | 连通性 / 版本 / 能力，供脚本使用 |
| `disconnect` | `mcsup remote disconnect <name>` | 删除本地配对与凭证，远端不受影响 |
| `revoke` | `mcsup remote revoke <client>` | 在 serve 端离线删除客户端记录，token 立即失效 |

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
| `update` | `mcsup update [--version <v>]` | 升级 mcsup 自身 |
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

写入走「临时文件 → fsync → rename」，写入失败时旧配置原样保留；校验在落地前完成，
取值非法时文件不会被创建或改动。