pub mod config;
pub mod listener_stats;
pub mod resolver;
pub mod manager;
pub mod runner;
pub mod idle_tracker;
pub mod admin_server;
pub mod controller;
pub mod tls_header;
pub mod active_tracker;
pub mod request_id;
pub mod extensible;
pub mod hostutil;
pub mod ifutil;
extern crate rocket;
use std::error::Error;
use config::Config;
use log::{info, error};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::load_file("config.yaml").await.unwrap();
    config.init_logging();
    admin_server::init(&config).await;
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
                    },
                    Err(inner_start_err) => {
                        info!("start listner {name} error: {inner_start_err}");
                    }
                }
            }
        },
        Err(cause) => {
            error!("failed to start all listeners: {cause}");
        }
    }
    let _ = admin_server::run_rocket().await?;
    Ok(())
}
