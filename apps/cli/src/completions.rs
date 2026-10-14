use anyhow::{Ok, Result};
use clap::CommandFactory;
use clap_complete::Shell;

/// 生命 shell 补全脚本到 stdout
pub(crate) fn generate(shell: Shell) -> Result<()> {
    let mut cmd = crate::cli::Cli::command();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    clap_complete::generate(shell, &mut cmd, "mcsup", &mut writer);
    Ok(())
}
