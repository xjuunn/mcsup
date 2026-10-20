//! 归档解压：系统自带工具链（`tar` / `unzip` / `ditto` / PowerShell）

use port::{ArchiveExtractor, ExtractError};
use std::ffi::{OsStr, OsString};
use std::path::Path;

/// 用系统自带工具链解压归档
///
/// 各平台自带的解压工具不同（Windows 无 `unzip`，Linux 无 `ditto`），
/// zip 按平台给出候选依次尝试；`.tar.gz` 统一走 `tar`
/// （macOS / Linux 常驻，Windows 10 1803+ 内置）。
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandExtractor;

impl CommandExtractor {
    /// 构建解压器
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ArchiveExtractor for CommandExtractor {
    fn is_extractable(&self, archive: &Path) -> bool {
        detect_format(archive).is_some()
    }

    fn extract(&self, archive: &Path, dest: &Path) -> Result<(), ExtractError> {
        match detect_format(archive) {
            Some(ArchiveFormat::Gzip) => untar_gz(archive, dest),
            Some(ArchiveFormat::Zip) => unzip(archive, dest),
            // is_extractable 挡住的格式（dmg / msi 等）由调用方按目录内容处理，
            // 静默 no-op 会把「没解压」误当「解压完成」，按误用上报
            None => Err(ExtractError(format!(
                "unsupported archive format: {}",
                archive.display()
            ))),
        }
    }
}

/// 归档格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFormat {
    /// gzip 容器（`.tar.gz` / `.tgz`）
    Gzip,
    /// zip
    Zip,
}

/// 识别归档格式：先按扩展名，识别不出再按文件头魔数
///
/// 下载文件名来自 URL 末段，可能没有扩展名（如 foojay 的重定向地址
/// `…/packages/{id}/redirect` 落盘为 `redirect`），只按后缀判定会把真正的
/// 归档当安装包跳过解压。魔数与内容一致，不受命名影响。
fn detect_format(archive: &Path) -> Option<ArchiveFormat> {
    if is_gzip(archive) {
        return Some(ArchiveFormat::Gzip);
    }
    if has_ext(archive, "zip") {
        return Some(ArchiveFormat::Zip);
    }
    sniff_magic(archive)
}

/// 读文件头魔数判定格式；文件不存在或读失败时返回 `None`
fn sniff_magic(archive: &Path) -> Option<ArchiveFormat> {
    use std::io::Read;
    let mut header = [0u8; 4];
    let mut file = std::fs::File::open(archive).ok()?;
    let read = file.read(&mut header).ok()?;
    if read >= 2 && header.starts_with(b"PK") {
        return Some(ArchiveFormat::Zip);
    }
    if read >= 2 && header[..2] == [0x1f, 0x8b] {
        return Some(ArchiveFormat::Gzip);
    }
    None
}

fn has_ext(path: &Path, expected: &str) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(expected))
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_gzip(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy().to_ascii_lowercase();
    name.ends_with(".tar.gz") || name.ends_with(".tgz")
}

/// `.tar.gz` 解压：系统自带 `tar`
fn untar_gz(archive: &Path, into: &Path) -> Result<(), ExtractError> {
    let extractor = Extractor::new(
        OsStr::new("tar"),
        &[
            OsStr::new("-xzf"),
            archive.as_os_str(),
            OsStr::new("-C"),
            into.as_os_str(),
        ],
    );
    match run_extractor(&extractor) {
        Ok(()) => Ok(()),
        Err(ExtractAttempt::Missing) => Err(ExtractError(
            "tar not found; install tar to extract .tar.gz".to_owned(),
        )),
        Err(ExtractAttempt::Failed(message)) => Err(ExtractError(message)),
    }
}

/// `.zip` 解压：按平台候选依次尝试
///
/// 只调用其中一个必然在某个平台报 `program not found`。
fn unzip(archive: &Path, into: &Path) -> Result<(), ExtractError> {
    let candidates = zip_extractors(archive, into);
    let mut failure: Option<String> = None;
    for extractor in &candidates {
        match run_extractor(extractor) {
            Ok(()) => return Ok(()),
            // 本机没有这个程序：换下一个候选
            Err(ExtractAttempt::Missing) => {}
            Err(ExtractAttempt::Failed(message)) => {
                if failure.is_none() {
                    // 保留首选工具的失败原因，后续候选只用于覆盖「工具缺失」
                    failure = Some(message);
                }
            }
        }
    }
    let source = failure.unwrap_or_else(|| {
        let tried = candidates
            .iter()
            .map(|extractor| extractor.program.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        format!("no zip extractor available (tried: {tried})")
    });
    Err(ExtractError(source))
}

/// zip 的候选解压命令，按平台排列本机最可能可用的工具
fn zip_extractors(archive: &Path, into: &Path) -> Vec<Extractor> {
    let archive = archive.as_os_str();
    let into = into.as_os_str();

    #[cfg(target_os = "windows")]
    {
        vec![
            // Windows 10 1803+ 自带 bsdtar，直接解 zip
            Extractor::new(
                OsStr::new("tar"),
                &[OsStr::new("-xf"), archive, OsStr::new("-C"), into],
            ),
            // 老系统回退 PowerShell 的 Expand-Archive；路径走环境变量，避开引号转义
            Extractor {
                program: OsString::from("powershell"),
                args: vec![
                    OsString::from("-NoProfile"),
                    OsString::from("-NonInteractive"),
                    OsString::from("-Command"),
                    OsString::from(POWERSHELL_EXPAND_ARCHIVE),
                ],
                envs: vec![
                    (OsString::from("MCSUP_ZIP_SRC"), archive.to_os_string()),
                    (OsString::from("MCSUP_ZIP_DST"), into.to_os_string()),
                ],
            },
        ]
    }

    #[cfg(target_os = "macos")]
    {
        vec![
            Extractor::new(
                OsStr::new("unzip"),
                &[OsStr::new("-q"), archive, OsStr::new("-d"), into],
            ),
            // macOS 系统自带，能处理带资源分叉的 zip
            Extractor::new(
                OsStr::new("ditto"),
                &[OsStr::new("-x"), OsStr::new("-k"), archive, into],
            ),
            Extractor::new(
                OsStr::new("tar"),
                &[OsStr::new("-xf"), archive, OsStr::new("-C"), into],
            ),
        ]
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        vec![
            Extractor::new(
                OsStr::new("unzip"),
                &[OsStr::new("-q"), archive, OsStr::new("-d"), into],
            ),
            Extractor::new(
                OsStr::new("tar"),
                &[OsStr::new("-xf"), archive, OsStr::new("-C"), into],
            ),
        ]
    }
}

/// `PowerShell` 解压脚本：路径经环境变量传入，不进命令行
///
/// 只有 Windows 的候选解压链用它，其他目标下不跟着引用方一起裁掉就是死代码
#[cfg(target_os = "windows")]
const POWERSHELL_EXPAND_ARCHIVE: &str =
    "Expand-Archive -LiteralPath $env:MCSUP_ZIP_SRC -DestinationPath $env:MCSUP_ZIP_DST -Force";

/// 外部解压命令
struct Extractor {
    /// 程序名
    program: OsString,
    /// 参数
    args: Vec<OsString>,
    /// 环境变量
    envs: Vec<(OsString, OsString)>,
}

impl Extractor {
    /// 组装无环境变量的命令
    fn new(program: &OsStr, args: &[&OsStr]) -> Self {
        Self {
            program: program.to_os_string(),
            args: args.iter().map(|arg| (*arg).to_os_string()).collect(),
            envs: Vec::new(),
        }
    }
}

/// [`run_extractor`] 的一次尝试结果
#[derive(Debug)]
enum ExtractAttempt {
    /// 本机没有该程序，可换下一个候选
    Missing,
    /// 程序存在但执行失败
    Failed(String),
}

/// 执行解压命令；程序不存在与执行失败分开上报，前者驱动候选回退
fn run_extractor(extractor: &Extractor) -> Result<(), ExtractAttempt> {
    let mut command = std::process::Command::new(&extractor.program);
    command.args(&extractor.args);
    for (key, value) in &extractor.envs {
        command.env(key, value);
    }
    let status = command.status().map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => ExtractAttempt::Missing,
        _ => ExtractAttempt::Failed(format!("{}: {source}", extractor.program.to_string_lossy())),
    })?;
    if !status.success() {
        return Err(ExtractAttempt::Failed(format!(
            "{} exited with {status}",
            extractor.program.to_string_lossy()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_extractors_start_with_platform_tool() {
        let candidates = zip_extractors(Path::new("a.zip"), Path::new("out"));
        assert!(!candidates.is_empty());
        #[cfg(target_os = "windows")]
        assert_eq!(candidates[0].program, OsString::from("tar"));
        #[cfg(not(target_os = "windows"))]
        assert_eq!(candidates[0].program, OsString::from("unzip"));
    }

    #[test]
    fn is_extractable_recognizes_archives_only() {
        let extractor = CommandExtractor::new();
        assert!(extractor.is_extractable(Path::new("/x/a.tar.gz")));
        assert!(extractor.is_extractable(Path::new("/x/a.tgz")));
        assert!(extractor.is_extractable(Path::new("/x/a.zip")));
        assert!(extractor.is_extractable(Path::new("/x/A.ZIP")));
        assert!(!extractor.is_extractable(Path::new("/x/a.dmg")));
        assert!(!extractor.is_extractable(Path::new("/x/java-bin")));
    }

    /// 下载文件名来自 URL 末段（如 foojay 重定向地址落盘为 `redirect`），
    /// 无扩展名时必须按文件头魔数识别，否则真正的归档被当安装包跳过解压
    #[test]
    fn detects_format_by_magic_when_extension_missing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let no_ext = tmp.path().join("redirect");

        std::fs::write(&no_ext, b"PK\x03\x04rest of zip").expect("zip magic");
        assert_eq!(detect_format(&no_ext), Some(ArchiveFormat::Zip));

        std::fs::write(&no_ext, [0x1f, 0x8b, 0x08, 0x00]).expect("gzip magic");
        assert_eq!(detect_format(&no_ext), Some(ArchiveFormat::Gzip));

        std::fs::write(&no_ext, b"MZ\x90\x00").expect("exe magic");
        assert_eq!(detect_format(&no_ext), None);
    }

    /// 无扩展名的真 zip：魔数识别后能真正解压出内容
    #[cfg(target_os = "windows")]
    #[test]
    fn extracts_extensionless_zip_roundtrip() {
        let tmp = tempfile::tempdir().expect("tmp");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("bin")).expect("mkdir");
        std::fs::write(src.join("bin").join("java.exe"), b"stub").expect("write");
        // Windows 10 1803+ 自带 bsdtar，用它造 zip（与解压候选同源）
        let archive = tmp.path().join("payload.zip");
        let status = std::process::Command::new("tar")
            .arg("-a")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg("bin")
            .status()
            .expect("tar create");
        assert!(status.success());
        // 去掉扩展名，模拟 URL 末段落盘的文件名
        let extensionless = tmp.path().join("redirect");
        std::fs::rename(&archive, &extensionless).expect("rename");

        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).expect("out");
        CommandExtractor::new()
            .extract(&extensionless, &dest)
            .expect("extract");
        let got = std::fs::read(dest.join("bin").join("java.exe")).expect("read back");
        assert_eq!(got, b"stub");
    }

    #[cfg(unix)]
    #[test]
    fn extracts_tar_gz_roundtrip() {
        let tmp = tempfile::tempdir().expect("tmp");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("bin")).expect("mkdir");
        std::fs::write(src.join("bin").join("java"), b"stub").expect("write");
        let archive = tmp.path().join("payload.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg("bin")
            .status()
            .expect("tar create");
        assert!(status.success());

        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).expect("out");
        CommandExtractor::new()
            .extract(&archive, &dest)
            .expect("extract");
        let got = std::fs::read(dest.join("bin").join("java")).expect("read back");
        assert_eq!(got, b"stub");
    }

    #[test]
    fn missing_program_is_reported_for_fallback() {
        let extractor = Extractor::new(OsStr::new("mcsup-no-such-extractor"), &[]);
        assert!(matches!(
            run_extractor(&extractor),
            Err(ExtractAttempt::Missing)
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn failing_program_is_reported_as_failure() {
        let extractor =
            Extractor::new(OsStr::new("cmd"), &[OsStr::new("/C"), OsStr::new("exit 3")]);
        assert!(matches!(
            run_extractor(&extractor),
            Err(ExtractAttempt::Failed(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn failing_program_is_reported_as_failure() {
        let extractor = Extractor::new(OsStr::new("sh"), &[OsStr::new("-c"), OsStr::new("exit 3")]);
        assert!(matches!(
            run_extractor(&extractor),
            Err(ExtractAttempt::Failed(_))
        ));
    }
}
