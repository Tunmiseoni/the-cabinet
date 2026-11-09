use serde::Serializer;

pub type Result<T> = std::result::Result<T, CabinetError>;
pub type CommandResult<T> = std::result::Result<T, CommandError>;

#[derive(Debug, thiserror::Error)]
pub enum CabinetError {
    #[error("{0}")]
    Message(String),
    #[error("{0}")]
    Tailscale(String),
    #[error("{0}")]
    Launch(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Tauri(#[from] tauri::Error),
    #[error("{0}")]
    Opener(#[from] tauri_plugin_opener::Error),
    #[error("{0}")]
    Cartridge(#[from] cabinet_cartridge::Error),
}

impl From<String> for CabinetError {
    fn from(value: String) -> Self {
        CabinetError::Message(value)
    }
}

impl From<&str> for CabinetError {
    fn from(value: &str) -> Self {
        CabinetError::Message(value.to_string())
    }
}

#[derive(Debug)]
pub struct CommandError(pub CabinetError);

impl From<CabinetError> for CommandError {
    fn from(value: CabinetError) -> Self {
        CommandError(value)
    }
}

impl From<String> for CommandError {
    fn from(value: String) -> Self {
        CommandError(CabinetError::Message(value))
    }
}

impl From<&str> for CommandError {
    fn from(value: &str) -> Self {
        CommandError(CabinetError::Message(value.to_string()))
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl serde::Serialize for CommandError {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}
