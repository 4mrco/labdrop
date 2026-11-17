use std::path::PathBuf;
use thiserror::Error;

/// Core error type for LabDrop operations.
#[derive(Debug, Error)]
pub enum LabDropError {
    #[error("I/O error on '{path}': {source}")]
    IoPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Selected path does not exist: '{0}'")]
    PathNotFound(PathBuf),

    #[error("No files or folders selected for staging")]
    EmptySelection,

    #[error(
        "Insufficient disk space in staging directory '{staging_dir}': required {required_bytes} bytes, available {available_bytes} bytes"
    )]
    InsufficientDiskSpace {
        staging_dir: PathBuf,
        required_bytes: u64,
        available_bytes: u64,
    },

    #[error("Staging operation was cancelled by the user")]
    StagingCancelled,

    #[error("Network error: {0}")]
    Network(String),

    #[error("SMB sharing error: {0}")]
    Smb(String),

    #[error("Privilege elevation required: {0}")]
    ElevationRequired(String),

    #[error("Artifact verification failed for '{artifact}': {reason}")]
    ArtifactVerification { artifact: String, reason: String },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, LabDropError>;
