use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("Invalid configuration: {0}")]
    Config(String),
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Upstream proxy error: {0}")]
    UpstreamProxy(String),
}

pub type Result<T> = std::result::Result<T, ProxyError>;
