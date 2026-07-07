use crate::cli::{Cli, Commands};
use crate::error::AppError;
use crate::platform::render::{command_to_request, print_event, render_response};
use std::sync::Arc;

pub async fn run(cli: Cli) -> Result<(), AppError> {
    match cli.command {
        Commands::Guestd(cmd) => crate::guestd::serve(&cmd).await,
        Commands::InternalExec(cmd) => {
            let store = crate::containers::ContainerStore::new(&cmd.data_root, &cmd.store_dir)
                .map_err(AppError::Message)?;
            let result = store
                .run_managed(&cmd.container_id)
                .await
                .map_err(AppError::Message)?;
            if !result.exit_status.success() {
                std::process::exit(result.exit_status.code().unwrap_or(1));
            }
            Ok(())
        }
        other => {
            let request = command_to_request(other)?;
            let emit = Arc::new(print_event);
            let response = crate::services::execute(request.clone(), emit).await?;
            render_response(&request, response)
        }
    }
}
