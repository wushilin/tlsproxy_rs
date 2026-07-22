//! Raw layer-4 forwarding handler for non-system listeners.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::info;
use tokio::net::TcpStream;
use tokio::sync::RwLock;

use crate::accounting::ConnStatus;
use crate::active_tracker;
use crate::config::Listener;
use crate::controller::Controller;
use crate::extensible::Extensible;
use crate::listener_stats::ListenerStats;
use crate::request_id::RequestId;
use crate::upstream_tls::connect_trust_all_tls;

pub(crate) async fn run(
    name: Arc<String>,
    client: Extensible<TcpStream>,
    listener_config: Arc<Listener>,
    context: Arc<ListenerStats>,
    controller: Arc<RwLock<Controller>>,
) -> Result<()> {
    let conn_id = client.get_extension::<RequestId>().await.unwrap();
    info!("{conn_id} {name} forward worker started");
    let resolved = crate::forward::choose_online(&name)
        .await
        .ok_or_else(|| anyhow!("no online forward backends"))?;
    active_tracker::set_target(&conn_id, &resolved.tls_server_name, &resolved.endpoint).await;
    let upstream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&resolved.endpoint),
    )
    .await??;
    info!("{conn_id} connected to forward upstream {}", resolved.endpoint);
    active_tracker::set_status(&conn_id, ConnStatus::Ok).await;
    let (client_read, client_write) = tokio::io::split(client);
    if listener_config.upstream_tls {
        let upstream = connect_trust_all_tls(upstream, &resolved.tls_server_name).await?;
        info!(
            "{conn_id} wrapped forward upstream {} in trust-all TLS",
            resolved.endpoint
        );
        let (upstream_read, upstream_write) = tokio::io::split(upstream);
        crate::relay::relay(
            conn_id,
            client_read,
            client_write,
            upstream_read,
            upstream_write,
            listener_config,
            context,
            controller,
            0,
        )
        .await
    } else {
        let (upstream_read, upstream_write) = tokio::io::split(upstream);
        crate::relay::relay(
            conn_id,
            client_read,
            client_write,
            upstream_read,
            upstream_write,
            listener_config,
            context,
            controller,
            0,
        )
        .await
    }
}
