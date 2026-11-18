use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("config: {0}")]
    Config(String),

    #[error("adb: {0}")]
    Adb(String),

    #[error("provision: {0}")]
    Provision(String),

    #[error("emulator: {0}")]
    Emulator(String),

    #[error("apk: {0}")]
    Apk(String),

    #[error("state transition {from} -> {to} is not allowed")]
    BadTransition { from: String, to: String },

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("timeout: {0}")]
    Timeout(String),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}
