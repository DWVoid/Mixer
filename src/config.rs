use ipnet::IpNet;
use serde::Deserialize;
use std::path::Path;
use tokio::fs;
use crate::error::ProxyError;

#[derive(Debug, Deserialize, Clone)]
pub struct ServiceConfig {
    pub listen: String,
    pub local_ranges: Vec<IpNet>,
    pub upstream_proxy: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub tls_exclude: Vec<IpNet>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct TlsConfig {
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub tls: Option<TlsConfig>,
    pub services: Vec<ServiceConfig>,
}

pub async fn load(path: &Path) -> crate::error::Result<Config> {
    let content = fs::read_to_string(path).await
        .map_err(|e| ProxyError::Config(format!("Cannot read config file {}: {}", path.display(), e)))?;
    serde_json::from_str(&content)
        .map_err(|e| ProxyError::Config(format!("Invalid JSON in config file: {}", e)))
}
