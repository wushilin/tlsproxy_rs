pub mod statistics;
pub mod tlsheader;
pub mod errors;
pub mod rules;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use std::time::{Instant};
use log::{error, info, LevelFilter, Record};

use std::io::Write;
use std::thread;
use std::net::{Ipv4Addr, Ipv6Addr, IpAddr, ToSocketAddrs};
use chrono::prelude::*;
use env_logger::fmt::Formatter;
use env_logger::Builder;
use std::error::Error;
use clap::Parser;
use std::sync::Arc;
use statistics::{GlobalStats, ConnStats};
use std::time::{Duration};
use bytesize::ByteSize;
use tlsheader::{parse};


#[derive(Parser, Debug, Clone)]
pub struct CliArg {
    #[arg(short, long, help="forward config `bind_ip:bind_port:forward_port` format (repeat for multiple)")]
    pub bind: Vec<String>,
    #[arg(short, long, default_value_t=30000, help="stats report interval in ms")]
    pub ri: i32,
    #[arg(long, default_value_t=String::from("INFO"), help="log level argument (ERROR INFO WARN DEBUG)")]
    pub log_level: String,
    #[arg(long, help="self IP address to reject (repeat for multiple)")]
    pub self_ip: Vec<String>,
    #[arg(long, default_value_t=String::from(""), help="acl files. see `rules.json`")]
    pub acl:String,
}

pub fn setup_logger(log_thread: bool, rust_log: Option<&str>) {
    let output_format = move |formatter: &mut Formatter, record: &Record| {
        let thread_name = if log_thread {
            format!("(t: {}) ", thread::current().name().unwrap_or("unknown"))
        } else {
            "".to_string()
        };
        let local_time: DateTime<Local> = Local::now();
        let time_str = local_time.to_rfc3339_opts(SecondsFormat::Millis, true);
        write!(
            formatter,
            "{} {}{: >5} - {}\n",
            time_str,
            thread_name,
            record.level(),
            record.args()
        )
    };

    let mut builder = Builder::new();
    builder
        .format(output_format)
        .filter(None, LevelFilter::Info);

    rust_log.map(|conf| builder.parse_filters(conf));

    builder.init();
}

fn is_address_in(ip_addr:IpAddr, target:&Arc<Vec<IpAddr>>)->bool{
    for i in target.as_ref() {
        if ip_addr == *i {
            return true;
        }
    }
    return false;
}

// Try to read up to min bytes for TLS header
// Returns error if it failed to do so
async fn read_header_with_timeout(stream:&TcpStream, buffer:&mut [u8], min:usize, timeout:Duration) -> Result<usize, Box<dyn Error>> {
    let start = Instant::now();
    let mut read_count:usize = 0;
    loop {
        if start.elapsed() > timeout {
            return Err(errors::PipeError::wrap_box(format!("tls header timeout. received {read_count} bytes < {min} bytes after {timeout:#?}")));
        }
        // Check if the stream is ready to be read

        let read_result = stream.try_read(&mut buffer[read_count..]);
        match read_result {
            Ok(0) => {
                if read_count >= min {
                    return Ok(read_count);
                }
                return Err(errors::PipeError::wrap_box(format!("eof before complete header. received {read_count} bytes < {min} bytes")));
            },
            Ok(n) => {
                read_count = read_count + n;
                if read_count >= min {
                    return Ok(read_count);
                } 
            },
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            },
            Err(e) => {
                return Err(e.into());
            }
        }

    }
}
async fn handle_socket_inner(acl:Arc<Option<rules::RuleSet>>, socket:TcpStream, rport: i32, conn_stats:Arc<ConnStats>, blacklist:Arc<Vec<IpAddr>>
) -> Result<(), Box<dyn Error>> {
    let conn_id = conn_stats.id_str();
    let mut client_hello_buf = vec![0;1024];
    let read_result = read_header_with_timeout(&socket, &mut client_hello_buf, 32,  Duration::from_secs(5));

    let tls_client_hello_size = read_result.await?;
    if tls_client_hello_size == 0 {
        return Err(errors::PipeError::wrap_box(format!("tls client hello read error")));
    }

    let tls_header_parsed = parse(&client_hello_buf[0..tls_client_hello_size])?;
    let tlshost = tls_header_parsed.sni_host;
    match acl.as_ref() {
        Some(acl_inner) => {
            let check_result = acl_inner.check_access(&tlshost);
            if ! check_result {
                return Err(errors::PipeError::wrap_box(format!("rejected by ACL")))
            } else {
                info!("{conn_id} acl pass: [{tlshost}]");
            }
        },
        _ => {

        }
    }
    let raddr = format!("{tlshost}:{rport}");
    info!("{conn_id} connecting to {raddr}...");
    let raddr_list = raddr.to_socket_addrs()?;
    for next_addr in raddr_list {
        let next_addr_ip = next_addr.ip();
        if is_address_in(next_addr_ip, &blacklist) {
            return Err(errors::PipeError::wrap_box(format!("rejected self connection")));
        }
    }

    let r_stream = TcpStream::connect(raddr).await?;
    info!("{conn_id} connected.");
    let (mut lr, mut lw) = tokio::io::split(socket);
    let (mut rr, mut rw) = tokio::io::split(r_stream);

    // write the header
    rw.write_all(&client_hello_buf[0..tls_client_hello_size]).await?;
    conn_stats.add_uploaded_bytes(tls_client_hello_size);
    let conn_stats1 = Arc::clone(&conn_stats);
    let conn_stats2 = Arc::clone(&conn_stats);
    // L -> R path
    let jh_lr = tokio::spawn( async move {
        let direction = ">>>";
        let mut buf = vec![0; 4096];
        let conn_id = conn_stats1.id_str();
        loop {
            let nr = lr
                .read(&mut buf)
                .await;
            match nr {
                Err(cause) => {
                    error!("{conn_id} {direction} failed to read data from socket: {cause}");
                    return;
                },
                _ =>{}
            }
    
            let n = nr.unwrap();
            if n == 0 {
                return;
            }
    
            let write_result = rw
                .write_all(&buf[0..n])
                .await;
            match write_result {
                Err(cause) => {
                    error!("{conn_id} {direction} failed to write data to socket: {cause}");
                    break;
                },
                Ok(_) => {
                    conn_stats1.add_uploaded_bytes(n);
                }
            }
        }
    });

    // R -> L path
    let jh_rl = tokio::spawn(async move {
        let direction = "<<<";
        let conn_id = conn_stats2.id_str();
        let mut buf = vec![0;4096];
        loop {
            let nr = rr
                .read(&mut buf)
                .await;
    
            match nr {
                Err(cause) => {
                    error!("{conn_id} {direction} failed to read data from socket: {cause}");
                    return;
                },
                _ =>{}
            }
            let n = nr.unwrap();
            if n == 0 {
                return;
            }
    
            let write_result = lw
                .write_all(&buf[0..n])
                .await;
            match write_result {
                Err(cause) => {
                    error!("{conn_id} {direction} failed to write data to socket: {cause}");
                    break;
                },
                Ok(_) => {
                    conn_stats2.add_downloaded_bytes(n);
                }
            }
        }

    });
    jh_lr.await?;
    jh_rl.await?;
    return Ok(());
}
async fn run_pair(acl_in:Arc<Option<rules::RuleSet>>, bind:String, rport:i32, g_stats:Arc<GlobalStats>, self_addresses:Arc<Vec<IpAddr>>) -> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind(&bind).await?;
    info!("Listening on: {}", &bind);

    loop {
        // Asynchronously wait for an inbound socket.
        let (socket, _) = listener.accept().await?;
        let local_gstats = Arc::clone(&g_stats);
        let laddr = bind.clone();
        let local_blacklist = Arc::clone(&self_addresses);
        let acl = Arc::clone(&acl_in);
        tokio::spawn(async move {
            //handle_incoming(socket);
            let _ = handle_socket(acl, socket, laddr, rport, local_gstats, local_blacklist).await;
        });
    }
}

async fn handle_socket(acl:Arc<Option<rules::RuleSet>>, socket:TcpStream, laddr:String, rport:i32, gstat:Arc<GlobalStats>, blacklist:Arc<Vec<IpAddr>>) {
    let cstat = Arc::new(ConnStats::new(Arc::clone(&gstat)));
    let conn_id = cstat.id_str();
    let remote_addr = socket.peer_addr();
    if remote_addr.is_err() {
        error!("{conn_id} has no remote peer info. closed");
        return;
    } 
    let remote_addr = remote_addr.unwrap();
    info!("{conn_id} started: from {remote_addr} via {laddr}");
    let cstat_clone = Arc::clone(&cstat);
    let result = handle_socket_inner(acl, socket, rport, cstat_clone, blacklist).await;
    let up_bytes = cstat.uploaded_bytes();
    let down_bytes = cstat.downloaded_bytes();
    let up_bytes_str = ByteSize(up_bytes as u64);
    let down_bytes_str = ByteSize(down_bytes as u64);
    let elapsed = cstat.elapsed();
    match result {
        Err(cause) => {
            error!("{conn_id} failed. cause: {cause}");
        },
        Ok(_) => {

        }
    }
    info!("{conn_id} stopped: up {up_bytes_str} down {down_bytes_str} uptime {elapsed:#?}");
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = CliArg::parse();
    let log_level = args.log_level;
    setup_logger(false, Some(&log_level));

    if args.bind.len() == 0 {
        error!("no binding arguments provided. please provide via `-b` or `--bind`");
        return Ok(());
    }
    for i in &args.bind {
        info!("Binding config: {i}");
    }

    let acl_file = args.acl;
    let mut acl_o = None;
    if acl_file.len() > 0 {
        acl_o = Some(rules::parse(&acl_file).unwrap())
    }
    let acl = Arc::new(acl_o);
    let mut self_addresses= Vec::<IpAddr>::new();
    for i in &args.self_ip {
        let ipv4 = i.parse::<Ipv4Addr>();
        let ipv6 = i.parse::<Ipv6Addr>();
        if ipv4.is_ok() {
            self_addresses.push(IpAddr::V4(ipv4.unwrap()));
        }
        if ipv6.is_ok() {
            self_addresses.push(IpAddr::V6(ipv6.unwrap()));
        }
    }
    for addr in &self_addresses {
        info!("Added self address: {addr}");
    }
    let self_addresses_arc = Arc::new(self_addresses);
    let mut futures = Vec::new();
    let global_stats = statistics::GlobalStats::new();
    let g_stats = Arc::new(global_stats);
    for next_bind in args.bind {
        let tokens = next_bind.split(":").collect::<Vec<&str>>();
        if tokens.len() != 3 {
            error!("invalid specification {next_bind}");
            continue;
        }
        let self_addresses_clone = Arc::clone(&self_addresses_arc);
        let bind_addr = tokens[0];
        let bind_port = tokens[1];
        let target_port = tokens[2];
        let bind_addr = format!("{bind_addr}:{bind_port}");
        let target_port:i32 = target_port.parse().unwrap();
        let new_g_stats = Arc::clone(&g_stats);
        let acl_c = Arc::clone(&acl);
        let jh = tokio::spawn(async move {
            let bind_c = next_bind.clone();
            let result = run_pair(acl_c, bind_addr, target_port, new_g_stats, self_addresses_clone).await;
            if let Err(cause) = result {
                error!("error running tlsproxy for {bind_c} caused by {cause}");
            }
        });
        futures.push(jh);
    }

    let g_stats = Arc::clone(&g_stats);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(args.ri as u64)).await;
            let active = g_stats.active_conn_count();
            let downloaded = ByteSize(g_stats.total_downloaded_bytes() as u64);
            let uploaded = ByteSize(g_stats.total_uploaded_bytes() as u64);
            let total_conn_count = g_stats.conn_count();
            info!("**  Stats: active: {active} total: {total_conn_count} up: {uploaded} down: {downloaded} **");
        }
    });
    for next_future in futures {
        let _ = next_future.await;
    }
    return Ok(());
}