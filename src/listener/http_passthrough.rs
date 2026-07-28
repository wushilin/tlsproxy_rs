//! Plain HTTP host-routing handler for non-system listeners.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::info;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::RwLock;

use crate::accounting::ConnStatus;
use crate::active_tracker;
use crate::config::Listener;
use crate::controller::Controller;
use crate::extensible::Extensible;
use crate::http_header;
use crate::listener_stats::ListenerStats;
use crate::request_id::RequestId;
use crate::upstream_tls::connect_trust_all_tls;

pub(crate) async fn run(
    name: Arc<String>,
    mut client: Extensible<TcpStream>,
    remote_address: SocketAddr,
    listener_config: Arc<Listener>,
    context: Arc<ListenerStats>,
    controller: Arc<RwLock<Controller>>,
    inspected: Option<http_header::HttpHead>,
    route: Option<(Option<String>, u16, bool)>,
) -> Result<()> {
    let conn_id = client.get_extension::<RequestId>().await.unwrap();
    info!("{conn_id} {name} http worker started");
    let head = match inspected {
        Some(head) => head,
        None => http_header::read_http_head(
            &mut client,
            Duration::from_secs(10),
            http_header::DEFAULT_MAX_HTTP_HEADER_SIZE,
        )
        .await?,
    };
    let header_len = head.buffered.len();
    info!("{conn_id} http host is {}", head.host_raw);
    active_tracker::set_sni(&conn_id, &head.host).await;
    context.increase_uploaded_bytes(header_len);
    active_tracker::add_uploaded(&conn_id, header_len as u64).await;
    if route.is_none() {
        crate::relay::check_acl(&listener_config, &head.host, &conn_id).await?;
    }
    let upstream_tls = route
        .as_ref()
        .map_or(listener_config.upstream_tls, |(_, _, tls)| *tls);
    let selected = if let Some((target, target_port, _)) = &route {
        crate::forward::select_routed_target(
            &head.host,
            target.as_deref(),
            *target_port,
            upstream_tls,
        )
        .await?
    } else if crate::forward::http_listener_targets(&listener_config).is_some() {
        crate::forward::choose_online(&name)
            .await
            .ok_or_else(|| anyhow!("no online http backends"))?
    } else {
        let port = head.port.unwrap_or(listener_config.target_port);
        crate::forward::select_runtime_target(&head.host, port, false, &head.host).await?
    };
    active_tracker::set_target(&conn_id, &selected.tls_server_name, &selected.endpoint).await;
    crate::relay::reject_obvious_self_connect(&listener_config, &selected.endpoint, &conn_id).await?;
    let upstream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&selected.endpoint),
    )
    .await??;
    info!("{conn_id} connected to http upstream {}", selected.endpoint);
    active_tracker::set_status(&conn_id, ConnStatus::Ok).await;
    let (client_read, client_write) = tokio::io::split(client);
    let rewritten = head.with_forwarded_headers(remote_address.ip());
    if upstream_tls {
        let upstream = connect_trust_all_tls(upstream, &selected.tls_server_name).await?;
        let (upstream_read, mut upstream_write) = tokio::io::split(upstream);
        upstream_write.write_all(&rewritten).await?;
        crate::relay::relay(conn_id, client_read, client_write, upstream_read, upstream_write, listener_config, context, controller, header_len as u64).await
    } else {
        let (upstream_read, mut upstream_write) = tokio::io::split(upstream);
        upstream_write.write_all(&rewritten).await?;
        crate::relay::relay(conn_id, client_read, client_write, upstream_read, upstream_write, listener_config, context, controller, header_len as u64).await
    }
}
