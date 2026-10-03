# MCSUP 代码设计规范

来自 `AGENTS.md`。

设计模块接口、引入依赖、处理平台差异时查阅。

---

# 避免 God Object / God Trait

禁止一个 `ServerManager` 同时负责 Server、Java、Backup、File、Mod、Database、
Network、Auth；应拆成 ServerService、Runtime、Backup、File、Mod、Storage、Auth
等，Service 只编排，不吞掉所有职责。

不要创建 `trait EverythingManager`；Trait 应描述稳定的能力边界，如 Runtime、
Provider、Storage、Backend、FileStore。

# 依赖管理

增加依赖前先问为什么需要，并检查：现有依赖能否完成、是否真的降低复杂度、是否
增加编译时间与安全风险、是否维护活跃、是否跨平台。禁止为几十行代码引入大型
依赖。

# 跨平台

目标平台是 Windows、macOS、Linux，尤其关注 Path、Process、Permission、Signal、
Shell、Java、Filesystem、Terminal。禁止默认假设 Unix / Linux / bash / systemd；
Windows 必须是一等公民。
