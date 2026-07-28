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
    let Ok(resolved_bind) = crate::bindaddr::resolve_bind_addr(&listener.bind) else { return Ok(()); };
    let Ok(bind_addr) = resolved_bind.parse::<SocketAddr>() else { return Ok(()); };
    let Ok(upstream_addrs) = tokio::net::lookup_host(resolved).await else { return Ok(()); };
    for upstream in upstream_addrs {
        if upstream.port() == bind_addr.port() && (upstream.ip() == bind_addr.ip() || (bind_addr.ip().is_unspecified() && upstream.ip().is_loopback())) {
            warn!("{id} upstream target {resolved} points back to this listener; closing loop");
            return Err(anyhow!("detected self-connection loop"));
        }
    }
    Ok(())
}

pub async fn check_acl(listener: &Listener, host: &str, id: &RequestId) -> Result<()> {
    if listener.is_allowed(host) { info!("{id} {host} allowed by ACL"); return Ok(()); }
    info!("{id} {host} denied by ACL");
    active_tracker::set_status(id, ConnStatus::Denied).await;
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
    let idle = Arc::new(Mutex::new(IdleTracker::new(stats.idle_timeout_ms)));
    let uploaded = Arc::new(AtomicU64::new(initial_uploaded));
    let downloaded = Arc::new(AtomicU64::new(0));
    let limiter = Limiter::new(listener.speed_limit());
    let upload = pipe(id.clone(), client_read, upstream_write, stats.clone(), idle.clone(), true, uploaded.clone(), controller.clone(), limiter.clone()).await;
    let download = pipe(id.clone(), upstream_read, client_write, stats, idle.clone(), false, downloaded.clone(), controller.clone(), limiter).await;
    let monitor = controller.write().await.spawn(async move {
        loop {
            if upload.is_finished() || download.is_finished() || idle.lock().await.is_expired() {
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
            if upload { stats.increase_uploaded_bytes(count); active_tracker::add_uploaded(&id, count as u64).await; }
            else { stats.increase_downloaded_bytes(count); active_tracker::add_downloaded(&id, count as u64).await; }
            limiter.consume(count).await;
            if writer.write_all(&buffer[..count]).await.is_err() { break; }
            idle.lock().await.mark();
        }
    })
}
