/// EULA 文件名（约定在服务器目录下）
pub const EULA_FILE: &str = "eula.txt";

/// 判定 `eula.txt` 内容是否已同意
///
/// 只认 `eula=true`（忽略空白与大小写差异由文件格式保证：Mojang 格式为
/// `eula=true` / `eula=false`）；缺失、`false`、乱码一律视为未同意。
#[must_use]
pub fn eula_accepted(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .any(|line| line.eq_ignore_ascii_case("eula=true"))
}

/// 渲染 `eula.txt` 内容
#[must_use]
pub fn render_eula(accepted: bool) -> String {
    format!("eula={accepted}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_true_and_rejects_false_or_garbage() {
        assert!(eula_accepted(&render_eula(true)));
        assert!(!eula_accepted(&render_eula(false)));
        assert!(!eula_accepted(""));
        assert!(!eula_accepted("random text"));
        assert!(!eula_accepted("eula=yes"));
    }

    #[test]
    fn accepts_true_amid_comments_and_whitespace() {
        let text = "# https://aka.ms/MinecraftEULA\n  eula=true \n";
        assert!(eula_accepted(text));
        let mixed = "EULA=true\n";
        assert!(eula_accepted(mixed));
    }

    #[test]
    fn render_roundtrips() {
        assert!(eula_accepted(&render_eula(true)));
        assert!(!eula_accepted(&render_eula(false)));
        assert_eq!(render_eula(true), "eula=true\n");
    }
}
