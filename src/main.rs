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
pub mod hello_cache;
pub mod hostutil;
pub mod idle_tracker;
pub mod listener_stats;
pub mod manager;
pub mod request_id;
pub mod resolver;
pub mod runner;
pub mod tls_header;
use config::Config;
use log::{error, info};
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Select a rustls crypto provider before any TLS component is constructed.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();
    let config = Config::load_file("config.yaml").await.unwrap();
    config.init_logging();
    admin_server::init(&config).await?;
    let start_result = manager::start(config).await;
    match start_result {
        Ok(result) => {
            for (name, inner_result) in result {
                match inner_result {
                    Ok(inner_start_result) => {
                        if inner_start_result {
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
        }
    }
    admin_server::run().await?;
    Ok(())
}
