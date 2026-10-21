# CLI 子命令设计

`mcsup` 命令采用统一的 `<对象> <动作> <名字> ...` 结构，关键约定：

- **动作总在名字前**：`mcsup server start <name>`、`mcsup server backup create <name>`。
- **建站只有两个入口**：`server create <name>`（指定 version/variant，自配 Mod）；
  `modpack install <pack>`（整包一键建站，version/variant/loader/Mod 全部来自整包）。
- **凡属某个服务器的内容，挂在 `server` 下**：备份（`backup`）、Mod（`mod`）。
- **服务属性统一 `server set`**：JVM 绑定、内存、自启等一律
  `mcsup server set <name> <key> <value>`，不为一类属性单开子命令。
- **机器级命令**：`java`（本机 JVM 运行时）、`remote`（远端执行位置）。
- **平台级命令直接动作**：`doctor` / `config` / `update` 等；工作目录经
  `workspace` 子命令管理。
- **服务器只能建在已注册工作目录下**：`--workspace <名>` 按名选择，
  缺省用当前工作区（`--workspace` > 环境变量 `MCSUP_WORKSPACE` > 当前工作区）；
  目录规范与 init 行为见 `docs/workspace.md`。

命令层级：

```
mcsup
├── workspace                 # 工作区：创建 / 注册表 / 当前切换
│   ├── init [--dir <path>] [--name <name>]
│   ├── list / use [<name>] / remove <name> [--purge] / doctor
├── server                    # 单台服务：生命周期 + 属主内容
│   ├── list / create / link / install / start / stop / restart
│   ├── status / console / send / logs / props / set / remove / version
│   ├── backup    create · list · restore · verify · prune
│   └── mod       list · install · remove · update · outdated
├── modpack                   # 整合包：一键建整包服务器
│   ├── install <pack> [--name <name>] [--workspace <ws>]
│   └── update <name>
├── java                      # 机器级 JVM 运行时
│   └── list / install / uninstall / default
├── remote                    # 远端执行位置（Backend）
│   └── list / deploy / ping / remove
└── doctor / config / update / completions / version / help
```

CLI 是 Thin Entry Point，不承载业务逻辑；退出码、stdout/stderr、JSON 等
机器契约见 `docs/embedding.md` 的 CLI 契约一节。

## mcsup server（服务端生命周期）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `list` | `mcsup server list` | 列出所有服务与状态 |
| `create` | `mcsup server create <name> --version <ver> [--variant <v>] [--workspace <ws>]` | 按指定版本/变体建站，落在所选工作目录；`--variant` 取 `vanilla/paper/fabric/forge/neoforge` |
| `install` | `mcsup server install <name> [--version <ver>]` | 给既有服务安装/更换服务端核心（JAR），校验后落盘 |
| `start` | `mcsup server start <name>` | 按已配置的 JVM 与内存启动，返回 PID |
| `stop` | `mcsup server stop <name> [--grace <sec>] [--force]` | 先优雅停机，宽限后 `--force` 强杀 |
| `restart` | `mcsup server restart <name>` | 组合 stop/start |
| `status` | `mcsup server status <name>` | 状态、进程、版本、占用 |
| `console` | `mcsup server console <name>` | 交互附加控制台；`Ctrl+D` 只分离不断开进程 |
| `send` | `mcsup server send <name> "<命令>"` | 单条命令注入（RCON/控制台） |
| `logs` | `mcsup server logs <name> [--tail <n>] [--follow]` | 查看日志，`--follow` 持续跟踪 |
| `props` | `mcsup server props <name> [key=value ...]` | 无参列出 `server.properties`；带参写入，改完提醒重启生效 |
| `set` | `mcsup server set <name> <key> <value>` | 设置服务属性，见下表 |
| `link` | `mcsup server link <path> [--name <name>] [--workspace <ws>]` | 把外部服务器目录链接进工作区并注册；目标不能在任一工作区 `servers/` 内；带 `mcsup.json` 时复用其身份 |
| `remove` | `mcsup server remove <name> [--purge]` | 默认注销保留磁盘；`--purge` 连目录一起删（链接服务器拒绝 `--purge`，仅移除链接） |
| `version` | `mcsup server version <name>` | 当前安装的版本、校验和、来源 |

### server set（服务属性）

属性键统一 kebab-case，随领域扩展追加；设置会校验并持久化，非法值报用法错误。

| 键 | 值 | 说明 |
| --- | --- | --- |
| `java-version` | 运行时版本 | 绑定本服务的 JVM 大版本（从 `mcsup java` 已注册运行时中按 major 匹配） |
| `ram` | 如 `2G` / `1024M` | 启动内存上限 |
| `autostart` | `true` / `false` | 机器启动时自动拉起 |

## mcsup server backup（备份，属于服务端）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `create` | `mcsup server backup create <name> [--label <text>]` | 先触发 `save-all`、锁世界，再快照；返回快照 ID |
| `list` | `mcsup server backup list <name>` | 快照列表：ID、时间、大小、完整性 |
| `restore` | `mcsup server backup restore <name> <id> [--to <新名>]` | 恢复；`--to` 恢复到新服务，不动原服务 |
| `verify` | `mcsup server backup verify <name> <id>` | 校验快照完整性（CAS/checksum） |
| `prune` | `mcsup server backup prune <name> --keep <n> \| --older-than <dur>` | 按保留数或年龄清理 |

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
| `init` | `mcsup workspace init [--dir <path>] [--name <name>]` | 创建并注册工作目录；无 `--dir` 进入交互（默认当前目录），目录必须为空 |
| `list` | `mcsup workspace list` | 已注册工作目录：名称、路径、服务器数、是否当前 |
| `use` | `mcsup workspace use [<name>]` | 切换当前工作区；不带名字在 TTY 下交互选择；只影响 `create` / `link` 缺省落点，不隐藏其他目录的服务器 |
| `remove` | `mcsup workspace remove <name> [--purge]` | 注销工作目录，同时清除该工作目录下的服务器登记；交互下询问是否删除磁盘数据（默认不删），同意后红色警示并要求输入工作区名称确认，`--purge` 跳过询问直接删除；删除失败（目录被占用）保留注册，退出占用后重试即可；删除当前工作区时当前标记移交给剩余中按名称排序的第一个 |
| `doctor` | `mcsup workspace doctor` | 检查工作区与注册表一致性：工作目录缺失、悬空链接、磁盘/登记漂移、无当前工作区；发现错误级问题非零退出 |

## mcsup modpack（整合包：一键建整包服务器）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `install` | `mcsup modpack install <pack> [--name <name>] [--workspace <ws>]` | 一键建整包服务器；缺省 `--name` 时用整包名 |
| `update` | `mcsup modpack update <name>` | 把已安装整包升级到新版本（先 dry-run 列出变更） |

建好的整包服务与 `server create` 建的服务是同一种对象，后续运维都走
`mcsup server ...`；`modpack` 只负责「按整包生根」与「整包升级」。

## mcsup java（机器级 JVM 运行时）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `list` | `mcsup java list` | 已注册运行时；未扫描过则自动扫描本机 |
| `scan` | `mcsup java scan` | 重新扫描本机已安装的 Java |
| `install` | `mcsup java install [<version>] [--jre] [--vendor <name>]` | 从远端下载安装 JDK（默认）或 JRE；缺省版本时交互选择 |
| `uninstall` | `mcsup java uninstall <version或路径>` 或 `--dir <路径>` | 卸载；系统 Java 仅移除注册记录，文件保留。同一 major 多个运行时且在 TTY 下交互选择；非 TTY 须用 `--dir` 显式指定路径 |

JVM 安装在平台数据目录 `{data_dir}/mcsup/java/`，不属于任何工作目录。
不同服务器通过 `server set java-version <major>` 绑定各自的大版本，不设全局 default。
下载源按启动时探测延迟排序（foojay / adoptium / azul），单源失败自动切换下一个。

## mcsup remote（远端执行位置 = Backend）

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `list` | `mcsup remote list` | 已注册远端与连接状态 |
| `deploy` | `mcsup remote deploy <name> <host> [--token <t>]` | 在远端安装/注册代理 |
| `ping` | `mcsup remote ping <name>` | 连通性与版本握手 |
| `remove` | `mcsup remote remove <name>` | 注销远端（远端进程需另停） |

远端与本地共享同一套 `server` 命令，差异只体现在 Backend 执行位置上。

## 平台级

| 子命令 | 用法 | 说明 |
| --- | --- | --- |
| `doctor` | `mcsup doctor` | 自检：路径权限、Java、provider 连通、远端 |
| `config` | `mcsup config show \| set <key> <value> \| edit` | 查看/修改/编辑平台配置，`edit` 打开 `$EDITOR` |
| `update` | `mcsup update [--version <v>]` | 升级 mcsup 自身 |
| `completions` | `mcsup completions <zsh\|bash\|fish>` | 生成 shell 补全脚本 |
| `version` | `mcsup version` | 版本、构建与协议信息 |
| `help` | `mcsup help [<group>]` | 分组帮助 |