#[cfg(target_os = "linux")]
mod linux_local;
#[cfg(target_os = "macos")]
mod macos_host;
mod render;

use crate::cli::Cli;
use crate::error::AppError;

pub async fn run(cli: Cli) -> Result<(), AppError> {
    #[cfg(target_os = "linux")]
    {
        return linux_local::run(cli).await;
    }

    #[cfg(target_os = "macos")]
    {
        return macos_host::run(cli).await;
    }

    #[allow(unreachable_code)]
    Err(AppError::Message(format!(
        "qbuild does not support this platform: {}",
        std::env::consts::OS
    )))
}
