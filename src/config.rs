use ipnet::IpNet;
use serde::Deserialize;
use std::path::Path;
use tokio::fs;
use crate::error::ProxyError;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub services: Vec<ServiceConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServiceConfig {
    /// Address to listen on, e.g. "0.0.0.0:8080"
    pub listen: String,
    /// CIDR ranges that should be proxied directly (locally)
    pub local_ranges: Vec<IpNet>,
    /// URL of the upstream HTTP proxy for traffic outside local_ranges, e.g. "http://proxy.corp:3128"
    pub upstream_proxy: String,
}

pub async fn load(path: &Path) -> crate::error::Result<Config> {
    let content = fs::read_to_string(path).await
        .map_err(|e| ProxyError::Config(format!("Cannot read config file {}: {}", path.display(), e)))?;
    serde_json::from_str(&content)
        .map_err(|e| ProxyError::Config(format!("Invalid JSON in config file: {}", e)))
}
