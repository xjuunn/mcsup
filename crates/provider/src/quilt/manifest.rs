//! Quilt 官方 Meta 的响应模型
//!
//! Quilt 的元数据服务与 Fabric 同源同形（Quilt 的 Meta 服务是从 Fabric 的
//! 实现 fork 出来的），但**端点路径与字段不完全相同**，且本项目用的是「安装器
//! 形态」而非 Fabric 的「服务端 JAR 直下」端点，因此单独建模。
//!
//! 端点（均已实测可达）：
//!
//! ```text
//! GET {meta}/v3/versions/loader   → 全部 loader 版本
//! GET {meta}/v3/versions/game     → 全部 MC 版本（含 stable 标记）
//! ```
//!
//! # 顺序
//!
//! 两个列表都是「新在前」，**首项最新**，与 Forge / `NeoForge` 的「旧在前」相反。
//!
//! # 安装器
//!
//! Quilt 的服务端 JAR 端点（`/v3/versions/loader/{game}/{loader}/{installer}/server/jar`）
//! 实测不稳定，本项目改用官方**安装器**：
//!
//! ```text
//! GET https://quiltmc.org/api/v1/download-latest-installer/java-universal
//! ```
//!
//! 它是一个通用的「最新安装器」地址，不带版本号；`InstallKind::Installer`，
//! 由 service 执行后产出参数文件。

use serde::Deserialize;

/// `GET /v3/versions/loader` 的一条 loader
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LoaderVersion {
    /// loader 版本号（如 `0.31.0-beta.4`）
    #[serde(default)]
    pub(crate) version: String,
}

impl LoaderVersion {
    /// 是否为稳定版（不带 `-beta` / `-alpha` / `-pre` / `-rc` 后缀）
    ///
    /// Quilt Meta **不提供**稳定标记，只能按版本串后缀判断。这是惯例而非契约：
    /// 上游若出版本前缀，此判定会把它当稳定版——但比起「永不选稳定版」，
    /// 这是更可用的默认。
    #[must_use]
    pub(crate) fn is_stable(&self) -> bool {
        !self.version.contains('-')
    }
}

/// `GET /v3/versions/game` 的一条 MC 版本
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GameVersion {
    /// MC 版本号
    #[serde(default)]
    pub(crate) version: String,
    /// 是否为正式版（Quilt Meta 给出的真实标记）
    #[serde(default)]
    pub(crate) stable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOADER: &str = include_str!("../../tests/fixtures/quilt_loader.json");
    const GAME: &str = include_str!("../../tests/fixtures/quilt_game.json");

    fn loaders() -> Vec<LoaderVersion> {
        serde_json::from_str(LOADER).expect("quilt_loader fixture")
    }

    fn games() -> Vec<GameVersion> {
        serde_json::from_str(GAME).expect("quilt_game fixture")
    }

    #[test]
    fn loader_list_deserializes() {
        let loaders = loaders();
        assert!(!loaders.is_empty());
        assert_eq!(loaders[0].version, "0.31.0-beta.4");
        assert!(loaders.iter().any(|l| l.version == "0.30.1"));
    }

    /// 稳定版判定：有无 `-` 后缀
    #[test]
    fn stability_is_inferred_from_suffix() {
        let stable = LoaderVersion {
            version: "0.30.1".to_owned(),
        };
        assert!(stable.is_stable());
        let beta = LoaderVersion {
            version: "0.31.0-beta.4".to_owned(),
        };
        assert!(!beta.is_stable());
        let rc = LoaderVersion {
            version: "0.31.0-rc.1".to_owned(),
        };
        assert!(!rc.is_stable());
    }

    #[test]
    fn game_list_carries_stable_flag() {
        let games = games();
        assert_eq!(games[0].version, "26.3");
        assert!(games[0].stable);
        assert!(!games[1].stable);
        assert!(games.iter().any(|g| g.version == "26.2" && g.stable));
    }

    #[test]
    fn missing_fields_are_defaulted() {
        let game: GameVersion = serde_json::from_str("{}").expect("parse");
        assert_eq!(game.version, "");
        assert!(!game.stable);
        let loader: LoaderVersion = serde_json::from_str("{}").expect("parse");
        assert!(loader.is_stable());
    }
}
