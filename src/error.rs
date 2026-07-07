use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Image(#[from] crate::image::ImageError),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Registry(#[from] crate::registry::RegistryError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
