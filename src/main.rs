use clap::Parser;
use std::path::PathBuf;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod config;
mod error;
mod proxy;
mod tls;

#[derive(Parser, Debug)]
#[command(
    name = "mixer",
    version,
    about = "HTTP proxy mixer: routes traffic directly or via an upstream proxy based on IP ranges",
    long_about = None
)]
struct Cli {
    /// Path to the JSON configuration file
    #[arg(short, long, default_value = "config.json")]
    config: PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("mixer=info")),
        )
        .init();

    let cli = Cli::parse();

    info!("Loading configuration from {}", cli.config.display());

    let cfg = match config::load(&cli.config).await {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    if cfg.services.is_empty() {
        error!("No services defined in configuration");
        std::process::exit(1);
    }

    info!("Starting {} proxy service(s)", cfg.services.len());

    let tls_config = cfg.tls;

    let handles: Vec<_> = cfg
        .services
        .into_iter()
        .map(|svc| {
            let tls = tls_config.clone();
            tokio::spawn(proxy::run_service(svc, tls))
        })
        .collect();

    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!("Service exited with error: {}", e),
            Err(e) => error!("Service task panicked: {}", e),
        }
    }
}
