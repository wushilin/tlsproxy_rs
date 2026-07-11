#![allow(clippy::needless_return)]
#![allow(clippy::option_map_unit_fn)]
#![allow(clippy::too_many_arguments)]

pub mod active_tracker;
pub mod admin_server;
pub mod ca;
pub mod certificate;
pub mod config;
pub mod controller;
pub mod extensible;
pub mod forward;
pub mod hello_cache;
pub mod hostutil;
pub mod idle_tracker;
pub mod listener_stats;
pub mod manager;
pub mod request_id;
pub mod resolver;
pub mod runner;
pub mod tls_header;
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use config::Config;
use config::{AdminServerConfig, CaConfig, LocalCaConfig};
use log::{error, info};
use std::path::PathBuf;
use tokio::{fs::OpenOptions, io::AsyncWriteExt};

const DEFAULT_CONFIG_FILE: &str = "config.yaml";

#[derive(Debug, Parser)]
#[command(
    name = "tlsproxy",
    version,
    about = "TLS passthrough and termination proxy with an embedded admin UI",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the proxy and admin UI.
    Run(ConfigPath),
    /// Generate a starter configuration file.
    Genconfig(ConfigPath),
    /// Validate that a configuration file can be loaded.
    Validate(ConfigPath),
}

#[derive(Debug, Args)]
struct ConfigPath {
    /// Configuration file path.
    #[arg(short = 'c', long = "config", default_value = DEFAULT_CONFIG_FILE)]
    config: PathBuf,
}

/// Logs go to stdout so the supervising process manager can capture them.
/// `RUST_LOG` overrides the default `info` filter.
fn init_logging() {
    use std::io::IsTerminal;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run(ConfigPath {
        config: PathBuf::from(DEFAULT_CONFIG_FILE),
    })) {
        Command::Run(args) => run(args.config).await,
        Command::Genconfig(args) => genconfig(args.config).await,
        Command::Validate(args) => validate(args.config).await,
    }
}

async fn run(config_path: PathBuf) -> Result<()> {
    // Select a rustls crypto provider before any TLS component is constructed.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();
    let config = load_config(&config_path).await?;
    admin_server::init(&config_path, &config)
        .await
        .map_err(|cause| anyhow::anyhow!("{cause}"))?;
    let admin_enabled = admin_server::enabled().await;
    let start_result = manager::start(config).await;
    match start_result {
        Ok(result) => {
            for (name, inner_result) in result {
                match inner_result {
                    Ok(inner_start_result) => {
                        if inner_start_result.running {
                            info!("started listener {name}");
                        } else {
                            info!("started listener {name} (false)");
                        }
                    }
                    Err(inner_start_err) => {
                        info!("start listner {name} error: {inner_start_err}");
                    }
                }
            }
        }
        Err(cause) => {
            error!("failed to start all listeners: {cause}");
            if !admin_enabled {
                return Err(cause.context("admin server is disabled, so the configuration cannot be repaired through the UI"));
            }
        }
    }
    if admin_enabled {
        admin_server::run()
            .await
            .map_err(|cause| anyhow::anyhow!("{cause}"))?;
    } else {
        info!("admin server disabled; waiting for Ctrl-C");
        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for Ctrl-C")?;
        info!("Ctrl-C received; stopping listeners");
        manager::stop().await;
    }
    Ok(())
}

async fn genconfig(config_path: PathBuf) -> Result<()> {
    let config = starter_config();
    let yaml = serde_yaml_ng::to_string(&config).context("failed to serialize starter config")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_path)
        .await
        .with_context(|| {
            format!(
                "failed to create config file `{}`; refusing to overwrite an existing file",
                config_path.display()
            )
        })?;
    file.write_all(yaml.as_bytes())
        .await
        .with_context(|| format!("failed to write config file `{}`", config_path.display()))?;
    println!("generated {}", config_path.display());
    Ok(())
}

async fn validate(config_path: PathBuf) -> Result<()> {
    load_config(&config_path).await?;
    println!("{} is valid", config_path.display());
    Ok(())
}

async fn load_config(config_path: &PathBuf) -> Result<Config> {
    Config::load_file(config_path).await.map_err(|cause| {
        anyhow::anyhow!(
            "failed to load config file `{}`: {cause}",
            config_path.display()
        )
    })
}

fn starter_config() -> Config {
    Config {
        ca: Some(CaConfig {
            localca: Some(LocalCaConfig::default()),
        }),
        admin_server: Some(AdminServerConfig::default()),
        ..Default::default()
    }
}
