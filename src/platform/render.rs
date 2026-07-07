use crate::cli::Commands;
use crate::error::AppError;
use crate::protocol::{CommandRequest, GuestEvent, GuestResponse};

pub fn print_event(event: GuestEvent) {
    match event {
        GuestEvent::Status(message) => eprintln!("{}", message),
    }
}

pub fn render_response(request: &CommandRequest, response: GuestResponse) -> Result<(), AppError> {
    match (request, response) {
        (CommandRequest::Ping, GuestResponse::Pong) => {
            println!("pong");
        }
        (CommandRequest::Build(cmd), GuestResponse::Build(result)) => {
            if cmd.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Built {}", result.image_reference);
                println!("Manifest: {}", result.manifest_digest);
                println!("Config: {}", result.config_digest);
                if let Some(store_dir) = &cmd.store_dir {
                    println!("Store: {}", store_dir.display());
                }
                println!("Size: {} bytes", result.size_bytes);
            }
        }
        (CommandRequest::Pull(cmd), GuestResponse::Pull(result)) => {
            if cmd.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Pulled {}", result.reference);
                println!("Manifest: {}", result.manifest_digest);
                println!("Config: {}", result.config_digest);
                println!("Layers: {}", result.layers);
                if let Some(store_dir) = &cmd.store_dir {
                    println!("Store: {}", store_dir.display());
                }
            }
        }
        (CommandRequest::Push(cmd), GuestResponse::Push(result)) => {
            if cmd.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Pushed {}", result.reference);
                println!("Manifest: {}", result.manifest_digest);
            }
        }
        (CommandRequest::Run(_), GuestResponse::Run(result)) => {
            if result.exit_code != 0 {
                std::process::exit(result.exit_code);
            }
        }
        (CommandRequest::Create(_), GuestResponse::Create(result)) => {
            println!("{}", result.id);
        }
        (CommandRequest::Start(_), GuestResponse::Start(result)) => {
            println!("{} {}", result.id, result.pid);
        }
        (CommandRequest::Stop(_), GuestResponse::Stop(result)) => {
            println!("{}", result.id);
        }
        (CommandRequest::Rm(_), GuestResponse::Removed(_)) => {}
        (CommandRequest::Ps(cmd), GuestResponse::Ps(result)) => {
            if cmd.json {
                println!("{}", serde_json::to_string_pretty(&result.containers)?);
            } else {
                for container in result.containers {
                    println!(
                        "{}\t{}\t{:?}\t{}",
                        container.id,
                        container.image_reference,
                        container.state,
                        container.pid.map(|pid| pid.to_string()).unwrap_or_default()
                    );
                }
            }
        }
        (CommandRequest::Logs(_), GuestResponse::Logs(result)) => {
            print!("{}", result.contents);
        }
        (CommandRequest::Inspect(cmd), GuestResponse::Inspect(result)) => {
            if cmd.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Reference: {}", result.reference);
                println!("Manifest: {}", result.manifest_digest);
                println!("Config: {}", result.config_digest);
                println!("Platform: {}/{}", result.os, result.architecture);
                println!("Layers: {}", result.layers);
                println!("Size: {} bytes", result.size_bytes);
            }
        }
        (CommandRequest::List(cmd), GuestResponse::List(result)) => {
            if cmd.json {
                println!("{}", serde_json::to_string_pretty(&result.images)?);
            } else if result.images.is_empty() {
                println!("No local images");
            } else {
                for image in result.images {
                    println!("{}\t{}", image.reference, image.manifest_digest);
                }
            }
        }
        (_, other) => {
            return Err(AppError::Message(format!(
                "guest returned an unexpected response for request: {:?}",
                response_name(&other)
            )));
        }
    }
    Ok(())
}

pub fn command_to_request(command: Commands) -> Result<CommandRequest, AppError> {
    CommandRequest::try_from(command).map_err(AppError::Message)
}

fn response_name(response: &GuestResponse) -> &'static str {
    match response {
        GuestResponse::Pong => "pong",
        GuestResponse::Build(_) => "build",
        GuestResponse::Pull(_) => "pull",
        GuestResponse::Push(_) => "push",
        GuestResponse::Run(_) => "run",
        GuestResponse::Create(_) => "create",
        GuestResponse::Start(_) => "start",
        GuestResponse::Stop(_) => "stop",
        GuestResponse::Removed(_) => "rm",
        GuestResponse::Ps(_) => "ps",
        GuestResponse::Logs(_) => "logs",
        GuestResponse::Inspect(_) => "inspect",
        GuestResponse::List(_) => "list",
    }
}
