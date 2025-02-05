use crate::active_tracker;
use crate::controller::Controller;
use crate::idle_tracker::IdleTracker;
use anyhow::anyhow;
use anyhow::Result;
use async_speed_limit::Limiter;
use lazy_static::lazy_static;
use log::{info, warn, error};
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::net::lookup_host;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use regex::Regex;

use tokio::{
    io::{ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock},
    task::JoinHandle,
    time::sleep,
};

use crate::{
    config::{Config, Listener},
    listener_stats::ListenerStats,
    resolver,
    tls_header,
};
use crate::extensible::Extensible;
use crate::request_id::RequestId;

lazy_static! {
    static ref COUNTER: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    static ref HOST_PORT_REGEX: Arc<Regex> = Arc::new(Regex::new(r"(?i)^\s*host\s*=\s*(\S+)\s*,\s*port\s*=\s*(\d+)\s*$").unwrap());
    static ref HOST_REGEX: Arc<Regex> = Arc::new(Regex::new(r"(?i)^\s*host\s*=\s*(\S+)\s*$").unwrap());
}
pub struct Runner {
    pub name: String,
    pub listener: Listener,
    pub config: Arc<RwLock<Config>>,
    pub controller: Arc<RwLock<Controller>>,
    pub self_addresses: Arc<Vec<SocketAddr>>
}

impl Runner {
    pub fn new(
        name: String,
        listener: Listener,
        config: Arc<RwLock<Config>>,
        root_context: Arc<RwLock<Controller>>,
        self_addresses: Arc<Vec<SocketAddr>>,
    ) -> Runner {
        Runner {
            name,
            listener,
            config,
            controller: root_context,
            self_addresses
        }
    }

    pub async fn start(self) -> Result<Arc<ListenerStats>> {
        let bind = self.listener.bind.clone();
        let name = self.name.clone();
        let listener_config = self.listener;
        let listener_config = Arc::new(listener_config);
        let idle_timeout_ms = listener_config.max_idle_time_ms();
        let stats = ListenerStats::new(&self.name, idle_timeout_ms);
        let stats = Arc::new(stats);
        let root_context_clone = Arc::clone(&self.controller);
        let controller_clone = Arc::clone(&self.controller);
        let stats_clone = Arc::clone(&stats);
        let (tx, mut rx) = mpsc::channel(1);
        let self_addresses = Arc::clone(&self.self_addresses);
        let name_clone = name.clone();
        let _ = root_context_clone
            .write()
            .await
            .spawn(async move {
                let mut listener = TcpListener::bind(&bind).await;
                let max_retry = 3;
                for i in 1..max_retry + 1 {
                    if listener.is_ok() {
                        break;
                    }
                    warn!("Listener: `{}` unable to bind to `{}` yet. retrying({i} of {max_retry})", &name_clone, &bind);
                    sleep(Duration::from_millis(100)).await;
                    listener = TcpListener::bind(&bind).await;
                }
                match listener {
                    Ok(inner_listener) => {
                        let _ = tx.send(None).await; // tell listener started successfully
                        let name_clone_result = name_clone.clone();
                        let result = Self::run_listener(
                            name_clone,
                            inner_listener,
                            listener_config,
                            stats_clone,
                            controller_clone,
                            self_addresses
                        ).await;
                        match result {
                            Err(cause) => {
                                error!("listener {name_clone_result} failed with {cause}");
                            },
                            _ => {
                                // it was ok
                            }
                        }
                    }
                    Err(cause) => {
                        let _ = tx.send(Some(format!("{cause}"))).await; // tell listener stopped successfully
                    }
                }
            })
            .await;
        let fail_reason = tokio::select! {
            _ = sleep(Duration::from_secs(1)) => {
                // trade off. typically if it fails we won't receive confirmation in rx
                // within 1 second (e.g. a bind typically fails within 1 sec)
                None
            },
            what = rx.recv() => {
                what
            }
        };
        let name = name.clone();
        return match fail_reason {
            None => {
                info!("listener {name} start cancelled");
                Ok(stats)
            }
            Some(fail_reason) => match fail_reason {
                None => {
                    warn!("listener {name} started without error");
                    Ok(stats)
                }
                Some(cause) => {
                    info!("listener {name} start with error {cause}");
                    Err(anyhow!("{}", cause))
                }
            },
        }
    }

    async fn handle_new_socket(
        name:Arc<String>,
        ext:Extensible<TcpStream>,
        remote_address:SocketAddr,
        listener_config: Arc<Listener>,
        stats: Arc<ListenerStats>,
        controller: Arc<RwLock<Controller>>,
        self_addresses: Arc<Vec<SocketAddr>>
    ) -> JoinHandle<Option<()>> {
        let mut controller_inner = controller.write().await;
        let controller_clone_inner = Arc::clone(&controller);
        let name = Arc::clone(&name);
        let jh = controller_inner.spawn(async move {
            let stats_local = Arc::clone(&stats);
            let request_id = ext.get_extension::<RequestId>().await.unwrap();
            let mut new_active = stats_local.increase_conn_count();
            let mut new_total = stats_local.total_count();
            info!("{request_id} ({name}) new connection from {remote_address} active {new_active} total {new_total}");
            let self_addresses_clone = Arc::clone(&self_addresses);
            let is_remote_local = Self::is_local(&request_id, &remote_address, self_addresses_clone);
            let start = Instant::now();
            if is_remote_local {
                info!("{request_id} pre-connection check: connection is from this machine. closing {remote_address}");
            } else {
                active_tracker::put(&request_id, remote_address).await;
                let stats_local_clone = Arc::clone(&stats_local);
                let rr = Self::worker(name, ext, listener_config, stats_local_clone, controller_clone_inner, self_addresses).await;
                if rr.is_err() {
                    let err = rr.err().unwrap();
                    warn!("{request_id} connection error: {err}");
                }
                active_tracker::remove(&request_id).await;
            }
            new_active = stats_local.decrease_conn_count();
            new_total = stats_local.total_count();
            let elapsed = start.elapsed();
            info!("{request_id} closing connection: active {new_active} total {new_total} duration {elapsed:?}");
        }).await;

        return jh;
    }

    async fn run_listener(
        name: String,
        listener: TcpListener,
        listener_config: Arc<Listener>,
        stats: Arc<ListenerStats>,
        controller: Arc<RwLock<Controller>>,
        self_addresses: Arc<Vec<SocketAddr>>
    ) -> Result<()> {
        let name = Arc::new(name);
        loop {
            let self_addresses = Arc::clone(&self_addresses);
            let accept_result = listener.accept().await;
            match accept_result {
                Ok((socket, addr)) => {
                    let conn_id = RequestId::new();
                    let ext = Extensible::of(socket);
                    ext.extend(conn_id).await;
                    let join_handle = Self::handle_new_socket(
                        Arc::clone(&name),
                        ext,
                        addr, 
                        Arc::clone(&listener_config),
                        Arc::clone(&stats),
                        Arc::clone(&controller),
                        Arc::clone(&self_addresses)
                    ).await;
                    // We don't nee to wait it to complete
                    drop(join_handle);
                },
                Err(cause) => {
                    warn!("listener {name} accept error: {cause}");
                    // continue processing
                    continue;
                }
            }
            
        }
    }

    async fn read_header_with_timeout(socket:&mut TcpStream, timeout:Duration, buffer:&mut[u8]) -> Option<usize> {
        let read_future = Self::must_read_header(socket, buffer);
        let result = tokio::time::timeout(timeout, read_future).await;
        match result {
            Ok(inner) => {
                inner
            },
            Err(_) => {
                None
            }
        }
    }

    async fn must_read_header(socket:&mut TcpStream, buffer:&mut[u8]) -> Option<usize> {
        let mut read_count: usize = 0;
        loop {
            let read_result = socket.read(&mut buffer[read_count..]).await;
            match read_result {
                Ok(n_read) => {
                    if n_read == 0 {
                        info!("alert! zero byte read! the tls header is probably longer than the buffer allocated!");
                        return None;
                    }
                    // info!("Read {n_read} bytes in header!");
                    read_count += n_read;
                    if tls_header::pre_check(&buffer[..read_count]) {
                        return Some(read_count);
                    }
                },
                Err(what) => {
                    info!("Something happened {what}");
                    return None;
                }
            }
        }
    }

    fn is_local(request_id: &RequestId, addr:&SocketAddr, local_addresses: Arc<Vec<SocketAddr>>) -> bool{
        info!("{request_id} checking if {addr} is a local address");
        match addr {
            SocketAddr::V4(v4a) => {
                let ip = v4a.ip();
                
                match ip.octets() {
                    [127, _, _, _] =>  {
                        info!("{request_id} {v4a} is v4, and starts with 127, it is local.");
                        return true
                    },
                    _ => {
                    },
                }
            },
            SocketAddr::V6(v6a) => {
                let ip = v6a.ip();
                if ip.is_loopback() {
                    info!("{request_id} {v6a} is v6 and is loopback, it is local");
                    return true
                }
            }
        }
        for next_self_address in local_addresses.iter() {
            if addr.ip() == next_self_address.ip() {
                info!("{request_id} {addr} matches self ip {next_self_address}, it is local");
                return true
            }
        }

        info!("{request_id} {addr} it is not local");
        false
    }

    // Resolver may return host=<host|ipv4|ipv6>,port=<u16> format
    // Resolver can also return <host|ipv4|ipv6> format, where port is the original port
    
    fn parse_host_port(input: &str) -> Result<(String, Option<u16>)> {
        if let Some(caps) = HOST_PORT_REGEX.captures(input) {
            let host = caps[1].to_string();
            let port = caps[2].parse::<u16>()?; // Convert port to u16 safely
            return Ok((host, Some(port)));
        }
        if let Some(caps) = HOST_REGEX.captures(input) {
            return Ok((caps[1].to_string(), None));
        }
        return Ok((input.to_string(), None))
    }

    async fn worker(
        name: Arc<String>,
        ext: Extensible<TcpStream>,
        listener_config: Arc<Listener>,
        context: Arc<ListenerStats>,
        controller: Arc<RwLock<Controller>>,
        self_addresses: Arc<Vec<SocketAddr>>,
    ) -> Result<()> {
        let conn_id = ext.get_extension::<RequestId>().await.unwrap();
        info!("{conn_id} {name} worker started");
        let mut ext = ext;
        let mut tls_header_buffer = vec![0u8; 4096];
        let timeout = Duration::from_secs(3);
        let header_len = Self::read_header_with_timeout(&mut ext, timeout, &mut tls_header_buffer).await;
        match header_len {
            None => {
                info!("{conn_id} tls header timed out after {timeout:?}");
                return Ok(())
            },
            Some(inner) => {
                info!("{conn_id} tls header read {inner} bytes")
            }
        }

        let header_len = header_len.unwrap();

        let sni_host_result = tls_header::parse(&tls_header_buffer[..header_len]);
        match sni_host_result {
            Err(cause) => {
                info!("{conn_id} tls header error: {cause}");
                return Ok(());
            },
            _ => {
                info!("{conn_id} tls header parse OK")
            }
        }
        let client_hello = sni_host_result.unwrap();
        let sni_target = client_hello.sni_host;
        info!("{conn_id} sni target is {sni_target}");
        let check_result = listener_config.is_allowed(&sni_target);
        match check_result {
            true => {
                info!("{conn_id} {sni_target} allowed by ACL");
            },
            false => {
                info!("{conn_id} {sni_target} denied by ACL");
                return Ok(());
            }
        }
        let (resolved, did_hit_resolver) = resolver::resolve(&sni_target).await;
        if did_hit_resolver {
            info!("{conn_id} resolved {sni_target} to {resolved} (hit)");
        } else {
            info!("{conn_id} resolved {sni_target} to {resolved} (no hit)")
        }

        let host_and_port = Self::parse_host_port(&resolved);
        let actual_host: String;
        let actual_port: u16;

        match host_and_port {
            Err(cause) => {
                info!("{conn_id} {resolved} is invalid: {cause}");
                return Ok(())
            }, 
            Ok((host, port)) => {
                actual_host = host;
                match port {
                    Some(inner) => {
                        actual_port = inner;
                    },
                    None => {
                        actual_port = listener_config.target_port;
                    }
                }
            }
        }

        let resolved = format!("{actual_host}:{actual_port}");
        info!("{conn_id} final target: {resolved}");
        if !did_hit_resolver {
            // check self connection
            let dns_result = lookup_host(&resolved).await;
            match dns_result {
                Err(cause) => {
                    warn!("{conn_id} dns error: {cause}");
                    return Ok(());
                },
                Ok(addresses) => {
                    for next_address in addresses {
                        let self_addresses_clone = Arc::clone(&self_addresses);
                        let is_local = Self::is_local(&conn_id, &next_address, self_addresses_clone);
                        if is_local {
                            warn!("{conn_id} rejected self connection: {}", next_address.ip());
                            return Ok(());
                        }
                    }
                }
            }
        } else {
            info!("{conn_id} skiped target check as resolver did hit");
        }
        let connect_future = TcpStream::connect(&resolved);
        let r_stream = tokio::time::timeout(Duration::from_secs(5), connect_future).await??;
        let local_addr = r_stream.local_addr()?;
        info!("{conn_id} connected to {resolved} via {local_addr:?}");
        let (lr, lw) = tokio::io::split(ext);

        let (rr, mut rw) = tokio::io::split(r_stream);

        let idle_tracker = Arc::new(Mutex::new(IdleTracker::new(context.idle_timeout_ms)));
        let context_clone = Arc::clone(&context);
        let uploaded = Arc::new(AtomicU64::new(0));
        let downloaded = Arc::new(AtomicU64::new(0));
        let header_write_result = rw.write_all(&tls_header_buffer[..header_len]).await;
        match header_write_result {
            Err(cause) => {
                warn!("{conn_id} tls header write error: {cause}");
                return Ok(());
            },
            _ =>{
                context.increase_uploaded_bytes(header_len);
                uploaded.fetch_add(header_len as u64, Ordering::SeqCst);
            }
        }
        let controller_clone = Arc::clone(&controller);
        let limiter = <Limiter>::new(listener_config.speed_limit());
        let limiter1 = limiter.clone();
        let limiter2 = limiter.clone();
        let conn_id1 = Arc::clone(&conn_id);
        let conn_id2 = Arc::clone(&conn_id);
        let conn_id3 = Arc::clone(&conn_id);

        let jh1 = Self::pipe(
            conn_id1,
            lr,
            rw,
            context_clone,
            Arc::clone(&idle_tracker),
            true,
            Arc::clone(&uploaded),
            controller_clone,
            limiter1,
        )
        .await;
        let context_clone = Arc::clone(&context);
        let controller_clone = Arc::clone(&controller);
        let jh2 = Self::pipe(
            conn_id2,
            rr,
            lw,
            context_clone,
            Arc::clone(&idle_tracker),
            false,
            Arc::clone(&downloaded),
            controller_clone,
            limiter2
        )
        .await;

        let controller_clone = Arc::clone(&controller);
        let jh = Self::run_idle_tracker(
            conn_id3,
            jh1,
            jh2,
            Arc::clone(&idle_tracker),
            controller_clone,
        )
        .await;
        let _ = jh.await;
        let uploaded_total = uploaded.load(Ordering::SeqCst);
        let downloaded_total = downloaded.load(Ordering::SeqCst);
        info!("{conn_id} end uploaded {uploaded_total} downloaded {downloaded_total}");
        Ok(())
    }
    async fn run_idle_tracker(
        conn_id: Arc<RequestId>,
        jh1: JoinHandle<Option<()>>,
        jh2: JoinHandle<Option<()>>,
        idle_tracker: Arc<Mutex<IdleTracker>>,
        root_context: Arc<RwLock<Controller>>,
    ) -> JoinHandle<Option<()>> {
        root_context
            .write()
            .await
            .spawn(async move {
                loop {
                    if jh1.is_finished() || jh2.is_finished() {
                        if !jh1.is_finished() {
                            info!("{conn_id} abort upload as download stopped");
                            jh1.abort();
                        }
                        if !jh2.is_finished() {
                            info!("{conn_id} abort download as upload stopped");
                            jh2.abort();
                        }
                        break;
                    }
                    if idle_tracker.lock().await.is_expired() {
                        info!("{conn_id} idle time out. aborting.");
                        if !jh1.is_finished() {
                            jh1.abort();
                        }
                        if !jh2.is_finished() {
                            jh2.abort();
                        }
                        break;
                    }
                    sleep(Duration::from_millis(500)).await;
                }
            })
            .await
    }
    async fn pipe<S,T>(
        request_id: Arc<RequestId>,
        reader_i: ReadHalf<T>,
        writer_i: WriteHalf<S>,
        context: Arc<ListenerStats>,
        idle_tracker: Arc<Mutex<IdleTracker>>,
        is_upload: bool,
        counter: Arc<AtomicU64>,
        controller: Arc<RwLock<Controller>>,
        limiter: Limiter,
    ) -> JoinHandle<Option<()>> 
    where 
        T: AsyncRead + AsyncWrite + Sync + Send + Unpin + 'static, 
        S: AsyncRead + AsyncWrite + Sync + Send + Unpin + 'static {
        let mut reader = reader_i;
        let mut writer = writer_i;
        let direction = match is_upload {
            true => "upload",
            false => "download",
        };
        controller
            .write()
            .await
            .spawn(async move {
                let mut buf = vec![0; 4096];

                loop {
                    let nr = reader.read(&mut buf).await;
                    match nr {
                        Err(_) => {
                            break;
                        }
                        _ => {}
                    }

                    let n = nr.unwrap();
                    if n == 0 {
                        break;
                    }

                    limiter.consume(n).await;
                    let write_result = writer.write_all(&buf[0..n]).await;
                    match write_result {
                        Err(_) => {
                            break;
                        }
                        Ok(_) => {
                            counter.fetch_add(n as u64, Ordering::SeqCst);
                            if is_upload {
                                context.increase_uploaded_bytes(n);
                            } else {
                                context.increase_downloaded_bytes(n);
                            }
                            idle_tracker.lock().await.mark();
                        }
                    }
                }
                info!("{request_id} {direction} ended");
            })
            .await
    }
}
