//! Raw layer-4 forwarding handler for non-system listeners.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::info;
use tokio::net::TcpStream;

use crate::accounting::ConnStatus;
use crate::active_tracker;
use crate::dataplane::RelayPolicy;
use crate::conn_stream::ConnStream;
use crate::upstream_tls::connect_trust_all_tls;

pub(crate) async fn run(
    ctx: crate::dataplane::ConnCtx,
    policy: Arc<RelayPolicy>,
    client: ConnStream<TcpStream>,
    load_balancing: crate::runtime_config::HttpLoadBalancing,
) -> Result<()> {
    let crate::dataplane::ConnCtx { name, stats, controller, remote } = ctx;
    let client_ip = remote.ip();
    let conn_id = client.request_id();
    info!("{conn_id} {name} forward worker started");
    let resolved = crate::forward::choose_online(&name, client_ip, load_balancing)
        .await
        .ok_or_else(|| anyhow!("no online forward backends"))?;
    active_tracker::set_target(&conn_id, &resolved.tls_server_name, &resolved.endpoint);
    // A raw forward listener connects upstream before reading any client
    // bytes, so a self-pointing target amplifies connections unboundedly
    // (accept -> connect -> accept -> ...). Loop markers cannot exist in an
    // opaque byte stream, so the address check is the only guard here.
    crate::relay::reject_obvious_self_connect(&policy, &resolved.endpoint, &conn_id).await?;
    let upstream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&resolved.endpoint),
    )
    .await??;
    info!("{conn_id} connected to forward upstream {}", resolved.endpoint);
    active_tracker::set_status(&conn_id, ConnStatus::Ok);
    let (client_read, client_write) = tokio::io::split(client);
    if policy.upstream_tls {
        let upstream = tokio::time::timeout(std::time::Duration::from_secs(10), connect_trust_all_tls(upstream, &resolved.tls_server_name)).await.map_err(|_| anyhow!("upstream TLS handshake timed out"))??;
        info!(
            "{conn_id} wrapped forward upstream {} in trust-all TLS",
            resolved.endpoint
        );
        let (upstream_read, upstream_write) = tokio::io::split(upstream);
        crate::relay::relay(
            crate::relay::RelayContext { id: conn_id, policy: policy, stats, controller, initial_uploaded: 0 },
            client_read,
            client_write,
            upstream_read,
            upstream_write,
        )
        .await
    } else {
        let (upstream_read, upstream_write) = tokio::io::split(upstream);
        crate::relay::relay(
            crate::relay::RelayContext { id: conn_id, policy: policy, stats, controller, initial_uploaded: 0 },
            client_read,
            client_write,
            upstream_read,
            upstream_write,
        )
        .await
    }
}
