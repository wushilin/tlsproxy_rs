//! TLS passthrough data-plane handler.
//!
//! This layer relays an already-classified ordinary TLS connection. It does
//! not know about the control hostname or ACME ALPN.

use std::sync::Arc;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::{info, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::RwLock;

use crate::accounting::ConnStatus;
use crate::active_tracker;
use crate::config::Listener;
use crate::controller::Controller;
use crate::extensible::Extensible;
use crate::hello_cache;
use crate::listener_stats::ListenerStats;
use crate::tls_header::{self, ClientHello};

pub(crate) async fn run(
    name: Arc<String>,
    mut client: Extensible<TcpStream>,
    listener_config: Arc<Listener>,
    context: Arc<ListenerStats>,
    controller: Arc<RwLock<Controller>>,
    inspected: Option<ClientHello>,
    route_target: Option<(Option<String>, u16, crate::runtime_config::HttpLoadBalancing, IpAddr)>,
) -> Result<()> {
    let conn_id = client.request_id();
    info!("{conn_id} {name} passthrough worker started");
    let client_hello = match inspected {
        Some(client_hello) => client_hello,
        None => {
            tls_header::read_client_hello(
                &mut client,
                Duration::from_secs(3),
                tls_header::DEFAULT_MAX_CLIENT_HELLO_SIZE,
            )
            .await?
        }
    };
    if hello_cache::is_looped(&client_hello.random) {
        warn!("{conn_id} inbound ClientHello was recently forwarded by this proxy; closing self-connection loop");
        return Err(anyhow!("detected self-connection loop"));
    }
    let header_len = client_hello.buffered.len();
    let sni_target = client_hello.sni_host;
    info!("{conn_id} sni target is {sni_target}");
    active_tracker::set_sni(&conn_id, &sni_target);
    context.increase_uploaded_bytes(header_len);
    active_tracker::add_uploaded(&conn_id, header_len as u64);
    let selected = match route_target {
        Some((target, target_port, load_balancing, client_ip)) => {
            crate::forward::select_routed_pool(
                &name,
                &sni_target,
                target.as_deref(),
                target_port,
                true,
                client_ip,
                load_balancing,
            )
            .await?
        }
        None => {
            crate::relay::resolve_target(
                &listener_config,
                &sni_target,
                true,
                &sni_target,
                &conn_id,
            )
            .await?
        }
    };
    active_tracker::set_target(&conn_id, &selected.tls_server_name, &selected.endpoint);
    hello_cache::insert(client_hello.random);
    let upstream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&selected.endpoint),
    )
    .await??;
    info!("{conn_id} connected to TLS upstream {}", selected.endpoint);
    active_tracker::set_status(&conn_id, ConnStatus::Ok);
    let (client_read, client_write) = tokio::io::split(client);
    let (upstream_read, mut upstream_write) = tokio::io::split(upstream);
    upstream_write.write_all(&client_hello.buffered).await?;
    crate::relay::relay(
        conn_id,
        client_read,
        client_write,
        upstream_read,
        upstream_write,
        listener_config,
        context,
        controller,
        header_len as u64,
    )
    .await
}
