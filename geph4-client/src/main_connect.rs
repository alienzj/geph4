use crate::stats::GLOBAL_LOGGER;
use crate::{cache::ClientCache, kalive::Keepalive, stats::StatCollector, AuthOpt, CommonOpt};
use chrono::prelude::*;
use scopeguard::defer;
use smol::prelude::*;
use smol_timeout::TimeoutExt;
use std::{net::Ipv4Addr, net::SocketAddr, net::SocketAddrV4, sync::Arc, time::Duration};
use structopt::StructOpt;

#[derive(Debug, StructOpt, Clone)]
pub struct ConnectOpt {
    #[structopt(flatten)]
    common: CommonOpt,

    #[structopt(flatten)]
    auth: AuthOpt,

    #[structopt(long)]
    /// whether or not to use bridges
    use_bridges: bool,

    #[structopt(long, default_value = "127.0.0.1:9909")]
    /// where to listen for SOCKS5 connections
    socks5_listen: SocketAddr,

    #[structopt(long, default_value = "127.0.0.1:9910")]
    /// where to listen for HTTP proxy connections
    http_listen: SocketAddr,

    #[structopt(long, default_value = "127.0.0.1:9809")]
    /// where to listen for REST-based local connections
    stats_listen: SocketAddr,

    #[structopt(long)]
    /// where to listen for proxied DNS requests. Optional.
    dns_listen: Option<SocketAddr>,

    #[structopt(long, default_value = "sg-sgp-test-01.exits.geph.io")]
    /// which exit server to connect to. If there isn't an exact match, the exit server with the most similar hostname is picked.
    exit_server: String,

    #[structopt(long)]
    /// whether or not to collect detailed profiling statistics
    pprof: bool,
}

pub async fn main_connect(opt: ConnectOpt) -> anyhow::Result<()> {
    log::info!("connect mode started");
    let stat_collector = Arc::new(StatCollector::default());
    // create a db directory if doesn't exist
    let client_cache = ClientCache::from_opts(&opt.common, &opt.auth)?;
    // create a kalive
    let keepalive = Keepalive::new(
        stat_collector.clone(),
        &opt.exit_server,
        opt.use_bridges,
        Arc::new(client_cache),
    );
    // enter the socks5 loop
    let socks5_listener = smol::net::TcpListener::bind(opt.socks5_listen).await?;
    let stat_listener = smol::net::TcpListener::bind(opt.stats_listen).await?;
    let http_listener = smol::net::TcpListener::bind(opt.http_listen).await?;
    let scollect = stat_collector.clone();
    // scope
    let scope = smol::Executor::new();
    if let Some(dns_listen) = opt.dns_listen {
        scope.spawn(dns_loop(dns_listen, &keepalive)).detach();
    }
    let _stat: smol::Task<anyhow::Result<()>> = scope.spawn(async {
        let my_scope = smol::Executor::new();
        my_scope
            .run(async {
                loop {
                    let (stat_client, _) = stat_listener.accept().await?;
                    let scollect = scollect.clone();
                    let keepalive = &keepalive;
                    my_scope
                        .spawn(async move {
                            drop(
                                async_h1::accept(stat_client, |req| {
                                    handle_stats(scollect.clone(), keepalive, req)
                                })
                                .await,
                            );
                        })
                        .detach();
                }
            })
            .await
    });
    let _http: smol::Task<anyhow::Result<()>> = scope.spawn(async {
        let my_scope = smol::Executor::new();
        my_scope
            .run(async {
                loop {
                    let (http_client, _) = http_listener.accept().await?;
                    my_scope
                        .spawn(handle_http(stat_collector.clone(), http_client, &keepalive))
                        .detach();
                }
            })
            .await
    });
    scope
        .run(async {
            loop {
                let (s5client, _) = socks5_listener.accept().await?;
                scope
                    .spawn(handle_socks5(stat_collector.clone(), s5client, &keepalive))
                    .detach()
            }
        })
        .await
}

use std::io::prelude::*;

/// Handle a request for stats
async fn handle_stats(
    stats: Arc<StatCollector>,
    kalive: &Keepalive,
    _req: http_types::Request,
) -> http_types::Result<http_types::Response> {
    let mut res = http_types::Response::new(http_types::StatusCode::Ok);
    match _req.url().path() {
        "/debugpack" => {
            // create logs and sosistab buffers
            let mut logs_buffer = Vec::new();
            {
                let noo = GLOBAL_LOGGER.read();
                for line in noo.iter() {
                    writeln!(logs_buffer, "{}", line)?;
                }
            }
            let mut sosistab_buf = Vec::new();
            writeln!(sosistab_buf, "time,seqno")?;
            let detail = kalive.get_stats().await?;
            if let Some((first_time, _)) = detail.recent_seqnos.first() {
                for (time, seqno) in detail.recent_seqnos.iter() {
                    writeln!(
                        sosistab_buf,
                        "{},{}",
                        time.saturating_duration_since(*first_time).as_secs_f64(),
                        seqno
                    )?;
                }
            }
            // form a tar
            let tar_buffer = Vec::new();
            let mut tar_build = tar::Builder::new(tar_buffer);
            let mut sosis_header = tar::Header::new_gnu();
            sosis_header.set_mode(0o666);
            sosis_header.set_size(sosistab_buf.len() as u64);
            tar_build.append_data(
                &mut sosis_header,
                "sosistab-trace.csv",
                sosistab_buf.as_slice(),
            )?;
            let mut logs_header = tar::Header::new_gnu();
            logs_header.set_mode(0o666);
            logs_header.set_size(logs_buffer.len() as u64);
            tar_build.append_data(&mut logs_header, "logs.txt", logs_buffer.as_slice())?;
            let result = tar_build.into_inner()?;
            res.insert_header("content-type", "application/tar");
            res.insert_header(
                "content-disposition",
                format!(
                    "attachment; filename=\"geph4-debug-{}.tar\"",
                    Local::now().to_rfc3339()
                ),
            );
            res.set_body(result);
            Ok(res)
        }
        "/proxy.pac" => {
            res.set_body("function FindProxyForURL(url, host){return 'PROXY 127.0.0.1:9910';}");
            Ok(res)
        }
        "/kill" => std::process::exit(0),
        _ => {
            let jstats = serde_json::to_string(&stats)?;
            res.set_body(jstats);
            res.insert_header("Content-Type", "application/json");
            Ok(res)
        }
    }
}

/// Handle DNS requests from localhost
async fn dns_loop(addr: SocketAddr, keepalive: &Keepalive) -> anyhow::Result<()> {
    let socket = smol::net::UdpSocket::bind(addr).await?;
    let mut buf = [0; 2048];
    let (send_conn, recv_conn) = smol::channel::unbounded();
    let scope = smol::Executor::new();
    let dns_timeout = Duration::from_secs(1);
    scope
        .run(async {
            loop {
                let (n, c_addr) = socket.recv_from(&mut buf).await?;
                let buff = buf[..n].to_vec();
                let socket = &socket;
                let recv_conn = &recv_conn;
                let send_conn = &send_conn;
                scope
                    .spawn(async move {
                        let fut = || async {
                            let mut conn = {
                                let lala = recv_conn.try_recv();
                                match lala {
                                    Ok(v) => v,
                                    _ => keepalive
                                        .connect("ordns.he.net:53")
                                        .timeout(dns_timeout)
                                        .await?
                                        .ok()?,
                                }
                            };
                            conn.write_all(&(buff.len() as u16).to_be_bytes())
                                .timeout(dns_timeout)
                                .await?
                                .ok()?;
                            conn.write_all(&buff).timeout(dns_timeout).await?.ok()?;
                            conn.flush().timeout(dns_timeout).await?.ok()?;
                            let mut n_buf = [0; 2];
                            conn.read_exact(&mut n_buf)
                                .timeout(dns_timeout)
                                .await?
                                .ok()?;
                            let mut true_buf = vec![0u8; u16::from_be_bytes(n_buf) as usize];
                            conn.read_exact(&mut true_buf)
                                .timeout(dns_timeout)
                                .await?
                                .ok()?;
                            socket.send_to(&true_buf, c_addr).await.ok()?;
                            send_conn.send(conn).await.ok()?;
                            Some(())
                        };
                        for i in 0u32..5 {
                            if fut().await.is_some() {
                                log::debug!("DNS request succeeded on try {}", i);
                                return;
                            }
                        }
                    })
                    .detach();
            }
        })
        .await
}

/// Handle a socks5 client from localhost.
async fn handle_socks5(
    stats: Arc<StatCollector>,
    s5client: smol::net::TcpStream,
    keepalive: &Keepalive,
) -> anyhow::Result<()> {
    let s5client = debuffer(s5client);
    stats.incr_open_conns();
    defer!(stats.decr_open_conns());
    use socksv5::v5::*;
    let _handshake = read_handshake(s5client.clone()).await?;
    write_auth_method(s5client.clone(), SocksV5AuthMethod::Noauth).await?;
    let request = read_request(s5client.clone()).await?;
    let port = request.port;
    let addr: String = match &request.host {
        SocksV5Host::Domain(dom) => format!("{}:{}", String::from_utf8_lossy(&dom), request.port),
        SocksV5Host::Ipv4(v4) => SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(v4[0], v4[1], v4[2], v4[3]),
            request.port,
        ))
        .to_string(),
        _ => anyhow::bail!("not supported"),
    };
    write_request_status(
        s5client.clone(),
        SocksV5RequestStatus::Success,
        request.host,
        port,
    )
    .await?;
    let conn = keepalive.connect(&addr).await?;
    smol::future::race(
        aioutils::copy_with_stats(conn.clone(), s5client.clone(), |n| {
            stats.incr_total_rx(n as u64)
        }),
        aioutils::copy_with_stats(s5client, conn, |n| stats.incr_total_tx(n as u64)),
    )
    .await?;
    Ok(())
}

/// Handle a HTTP client from localhost.
async fn handle_http(
    stats: Arc<StatCollector>,
    hclient: smol::net::TcpStream,
    keepalive: &Keepalive,
) -> anyhow::Result<()> {
    let hclient = debuffer(hclient);
    stats.incr_open_conns();
    defer!(stats.decr_open_conns());
    // Rely on "squid" remotely
    let conn = keepalive.connect("127.0.0.1:3128").await?;
    smol::future::race(
        aioutils::copy_with_stats(conn.clone(), hclient.clone(), |n| {
            stats.incr_total_rx(n as u64)
        }),
        aioutils::copy_with_stats(hclient, conn, |n| stats.incr_total_tx(n as u64)),
    )
    .await?;
    Ok(())
}

/// Smallify the buffers for a TCP connection
fn debuffer(conn: async_net::TcpStream) -> async_net::TcpStream {
    let conn: Arc<smol::Async<std::net::TcpStream>> = conn.into();
    let conn: std::net::TcpStream = conn.get_ref().try_clone().unwrap();
    let conn: socket2::Socket = conn.into();
    conn.set_nodelay(true).unwrap();
    conn.set_recv_buffer_size(163840).unwrap();
    conn.set_send_buffer_size(163840).unwrap();
    smol::Async::new(conn.into_tcp_stream()).unwrap().into()
}
