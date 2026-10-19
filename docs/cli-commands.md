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
  缺省用默认工作目录；目录规范与 init 行为见 `docs/workspace.md`。

命令层级：

```
mcsup
├── workspace                 # 工作目录：创建 / 注册表 / 默认切换
│   ├── init [--dir <path>] [--name <name>]
│   ├── list / use <name> / remove <name> [--purge] / info
├── server                    # 单台服务：生命周期 + 属主内容
│   ├── list / create / install / start / stop / restart
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
| `remove` | `mcsup server remove <name> [--purge]` | 停止并删除；`--purge` 连数据目录一起删 |
| `version` | `mcsup server version <name>` | 当前安装的版本、校验和、来源 |

### server set（服务属性）

属性键统一 kebab-case，随领域扩展追加；设置会校验并持久化，非法值报用法错误。

| 键 | 值 | 说明 |
| --- | --- | --- |
| `java-version` | 运行时版本 | 绑定本服务的 JVM（来自 `mcsup java` 运行时集，默认跟随 `java default`） |
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
| `list` | `mcsup workspace list` | 已注册工作目录：名称、路径、服务器数、是否默认 |
| `use` | `mcsup workspace use <name>` | 切换默认工作目录；只影响 `create` 落点，不隐藏其他目录的服务器 |
| `remove` | `mcsup workspace remove <name> [--purge]` | 注销工作目录；`--purge` 才删磁盘数据 |
| `info` | `mcsup workspace info` | 默认工作目录、平台数据目录与结构说明 |

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
| `list` | `mcsup java list` | 已安装运行时与位置 |
| `install` | `mcsup java install <version> [--vendor <name>]` | 下载并安装 JVM |
| `uninstall` | `mcsup java uninstall <version>` | 移除运行时 |
| `default` | `mcsup java default [<version>]` | 查询/设置默认运行时 |

JVM 安装在平台数据目录 `{data_dir}/mcsup/java/`，不属于任何工作目录。

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