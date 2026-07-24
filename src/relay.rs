//! Shared connection admission, target selection, and bidirectional relay.
//! Listener implementations depend on this layer rather than the legacy
//! listener lifecycle in `Runner`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_speed_limit::Limiter;
use log::{info, warn};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

use crate::accounting::{ConnStatus};
use crate::active_tracker;
use crate::config::Listener;
use crate::controller::Controller;
use crate::idle_tracker::IdleTracker;
use crate::listener_stats::ListenerStats;
use crate::request_id::RequestId;

pub async fn reject_obvious_self_connect(listener: &Listener, resolved: &str, id: &RequestId) -> Result<()> {
    let own_bind = crate::bindaddr::resolve_bind_addr(&listener.bind)
        .ok()
        .and_then(|value| value.parse::<SocketAddr>().ok());
    let Ok(upstream_addrs) = tokio::net::lookup_host(resolved).await else { return Ok(()); };
    let endpoints = crate::runtime_live::self_endpoints_snapshot();
    for upstream in upstream_addrs {
        // Own-bind check retained separately: it works even before the
        // runtime registers its bound sockets (bootstrap, tests).
        if own_bind.is_some_and(|bind| upstream_lands_on(upstream, bind, &endpoints.local_ips, &endpoints.self_ips)) {
            warn!("{id} upstream target {resolved} points back to this listener; closing loop");
            return Err(anyhow!("detected self-connection loop"));
        }
        if endpoints.bound.iter().any(|bound| upstream_lands_on(upstream, *bound, &endpoints.local_ips, &endpoints.self_ips)) {
            warn!("{id} upstream target {resolved} points back to another listener of this proxy; closing loop");
            return Err(anyhow!("detected self-connection loop"));
        }
    }
    Ok(())
}

/// Whether connecting to `upstream` would land on the listener socket bound
/// at `bound`. An unspecified bind (`0.0.0.0`/`::`) receives loopback, any
/// local interface address, and the configured public self IPs on its port;
/// a specific bind receives its own address, plus the public self IPs when it
/// is not loopback-only (NAT hairpinning a public address to a bound
/// interface). Matching per bound socket rather than per bare port keeps a
/// different process legitimately serving the same port number on an address
/// we are not bound to reachable.
fn upstream_lands_on(
    upstream: SocketAddr,
    bound: SocketAddr,
    local_ips: &[std::net::IpAddr],
    self_ips: &[std::net::IpAddr],
) -> bool {
    if upstream.port() != bound.port() {
        return false;
    }
    if bound.ip().is_unspecified() {
        upstream.ip().is_loopback()
            || local_ips.contains(&upstream.ip())
            || self_ips.contains(&upstream.ip())
    } else {
        upstream.ip() == bound.ip()
            || (!bound.ip().is_loopback() && self_ips.contains(&upstream.ip()))
    }
}

pub async fn check_acl(listener: &Listener, host: &str, id: &RequestId) -> Result<()> {
    if listener.is_allowed(host) { info!("{id} {host} allowed by ACL"); return Ok(()); }
    info!("{id} {host} denied by ACL");
    active_tracker::set_status(id, ConnStatus::Denied);
    Err(anyhow!("{host} denied by ACL"))
}

pub async fn resolve_target(listener: &Listener, sni: &str, upstream_tls: bool, tls_name: &str, id: &RequestId) -> Result<crate::forward::SelectedTarget> {
    check_acl(listener, sni, id).await?;
    let selected = crate::forward::select_runtime_target(sni, listener.target_port, upstream_tls, tls_name).await?;
    info!("{id} final target: {}", selected.endpoint);
    Ok(selected)
}

pub async fn relay<CR, CW, UR, UW>(id: Arc<RequestId>, client_read: CR, client_write: CW, upstream_read: UR, upstream_write: UW, listener: Arc<Listener>, stats: Arc<ListenerStats>, controller: Arc<RwLock<Controller>>, initial_uploaded: u64) -> Result<()>
where CR: AsyncRead + Unpin + Send + 'static, CW: AsyncWrite + Unpin + Send + 'static, UR: AsyncRead + Unpin + Send + 'static, UW: AsyncWrite + Unpin + Send + 'static {
    let idle = Arc::new(Mutex::new(IdleTracker::new(stats.idle_timeout_ms())));
    let uploaded = Arc::new(AtomicU64::new(initial_uploaded));
    let downloaded = Arc::new(AtomicU64::new(0));
    let limiter = Limiter::new(listener.speed_limit());
    let upload = pipe(id.clone(), client_read, upstream_write, stats.clone(), idle.clone(), true, uploaded.clone(), controller.clone(), limiter.clone()).await;
    let download = pipe(id.clone(), upstream_read, client_write, stats, idle.clone(), false, downloaded.clone(), controller.clone(), limiter).await;
    // After the request side has finished, a response that makes no progress
    // for this long is torn down even when the listener idle timeout is
    // disabled. A client FIN is indistinguishable from a full close, so this
    // bounds how long a vanished client can hold sockets and tasks while
    // still letting an actively-sending response drain in full (comparable
    // to HAProxy's `timeout client-fin`).
    const HALF_CLOSED_DRAIN_LIMIT: Duration = Duration::from_secs(30);
    let monitor = controller.write().await.spawn(async move {
        loop {
            // A client may half-close its request side immediately after the
            // request body. That completes `upload`, but the upstream response
            // may still be in flight and must be drained in full. Conversely,
            // once `download` is complete there is nothing left to send to the
            // client, so the request side can be stopped.
            let stalled_after_half_close =
                upload.is_finished() && idle.lock().await.idled_for() > HALF_CLOSED_DRAIN_LIMIT;
            if download.is_finished() || idle.lock().await.is_expired() || stalled_after_half_close {
                if !upload.is_finished() { upload.abort(); }
                if !download.is_finished() { download.abort(); }
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    let _ = monitor.await;
    info!("{id} end uploaded {} downloaded {}", uploaded.load(Ordering::SeqCst), downloaded.load(Ordering::SeqCst));
    Ok(())
}

async fn pipe<R, W>(id: Arc<RequestId>, mut reader: R, mut writer: W, stats: Arc<ListenerStats>, idle: Arc<Mutex<IdleTracker>>, upload: bool, counter: Arc<AtomicU64>, controller: Arc<RwLock<Controller>>, limiter: Limiter) -> JoinHandle<Option<()>>
where R: AsyncRead + Send + Unpin + 'static, W: AsyncWrite + Send + Unpin + 'static {
    controller.write().await.spawn(async move {
        let mut buffer = vec![0; 4096];
        loop {
            let Ok(count) = reader.read(&mut buffer).await else { break; };
            if count == 0 { break; }
            counter.fetch_add(count as u64, Ordering::SeqCst);
            if upload { stats.increase_uploaded_bytes(count); active_tracker::add_uploaded(&id, count as u64); }
            else { stats.increase_downloaded_bytes(count); active_tracker::add_downloaded(&id, count as u64); }
            limiter.consume(count).await;
            if writer.write_all(&buffer[..count]).await.is_err() { break; }
            idle.lock().await.mark();
        }
        // Propagate a directional EOF without tearing down the opposite
        // direction. In particular, an upload EOF becomes a TCP/TLS
        // half-close while the response continues to drain.
        let _ = writer.shutdown().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ListenerMode, Policy, Rules};

    #[test]
    fn upstream_lands_on_matches_self_addresses_only_on_reachable_binds() {
        let locals: Vec<std::net::IpAddr> = vec!["192.168.1.5".parse().unwrap(), "127.0.0.1".parse().unwrap()];
        let selfs: Vec<std::net::IpAddr> = vec!["203.0.113.9".parse().unwrap()];
        let addr = |value: &str| value.parse::<SocketAddr>().unwrap();

        // unspecified bind receives loopback, local interface, and public self IPs on its port
        let all = addr("0.0.0.0:443");
        assert!(upstream_lands_on(addr("127.0.0.1:443"), all, &locals, &selfs));
        assert!(upstream_lands_on(addr("192.168.1.5:443"), all, &locals, &selfs));
        assert!(upstream_lands_on(addr("203.0.113.9:443"), all, &locals, &selfs));
        assert!(!upstream_lands_on(addr("198.51.100.7:443"), all, &locals, &selfs), "foreign IP is not self");
        assert!(!upstream_lands_on(addr("127.0.0.1:8443"), all, &locals, &selfs), "different port is not managed");

        // specific bind receives its own address, and public self IPs via NAT hairpin
        let eth = addr("192.168.1.5:9000");
        assert!(upstream_lands_on(addr("192.168.1.5:9000"), eth, &locals, &selfs));
        assert!(upstream_lands_on(addr("203.0.113.9:9000"), eth, &locals, &selfs), "NAT hairpin to a bound interface");
        assert!(!upstream_lands_on(addr("127.0.0.1:9000"), eth, &locals, &selfs), "loopback does not reach an eth-bound socket");

        // loopback-only bind: another process may serve the same port on other addresses
        let local_only = addr("127.0.0.1:8080");
        assert!(upstream_lands_on(addr("127.0.0.1:8080"), local_only, &locals, &selfs));
        assert!(!upstream_lands_on(addr("192.168.1.5:8080"), local_only, &locals, &selfs), "same port on an unbound address stays reachable");
        assert!(!upstream_lands_on(addr("203.0.113.9:8080"), local_only, &locals, &selfs), "public IP cannot hairpin to loopback-only");
    }

    #[tokio::test]
    async fn client_request_half_close_does_not_truncate_slow_response() {
        let (client, proxy_client) = tokio::io::duplex(128 * 1024);
        let (proxy_upstream, upstream) = tokio::io::duplex(128 * 1024);
        let (client_read, mut client_write) = tokio::io::split(client);
        let (proxy_client_read, proxy_client_write) = tokio::io::split(proxy_client);
        let (proxy_upstream_read, proxy_upstream_write) = tokio::io::split(proxy_upstream);
        let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
        let listener = Arc::new(Listener {
            bind: "127.0.0.1:443".into(),
            target: None,
            target_port: 80,
            policy: Policy::ALLOW,
            rules: Rules { static_hosts: Vec::new(), patterns: Vec::new() },
            max_idle_time_ms: None,
            speed_limit: None,
            mode: ListenerMode::Http,
            upstream_tls: false,
        });
        let stats = Arc::new(ListenerStats::new("test", 5_000));
        let controller = Arc::new(RwLock::new(Controller::new()));
        let relay_task = tokio::spawn(relay(
            Arc::new(RequestId::new()),
            proxy_client_read,
            proxy_client_write,
            proxy_upstream_read,
            proxy_upstream_write,
            listener,
            stats,
            controller,
            0,
        ));

        client_write.write_all(b"GET / HTTP/1.1\r\nHost: example\r\n\r\n").await.unwrap();
        client_write.shutdown().await.unwrap();
        let upstream_task = tokio::spawn(async move {
            let mut request = Vec::new();
            upstream_read.read_to_end(&mut request).await.unwrap();
            assert!(request.starts_with(b"GET / HTTP/1.1"));
            let body = vec![b'x'; 32 * 1024];
            upstream_write.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            for chunk in body.chunks(1024) {
                upstream_write.write_all(chunk).await.unwrap();
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            upstream_write.shutdown().await.unwrap();
        });
        let mut response = Vec::new();
        let mut client_read = client_read;
        client_read.read_to_end(&mut response).await.unwrap();
        upstream_task.await.unwrap();
        relay_task.await.unwrap().unwrap();

        let split = response.windows(4).position(|value| value == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(response.len() - split, 32 * 1024);
    }
}
