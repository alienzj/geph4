use crate::cache::ClientCache;
use crate::stats::StatCollector;
use anyhow::Context;
use smol::channel::{Receiver, Sender};
use smol::prelude::*;
use smol_timeout::TimeoutExt;
use std::time::Duration;
use std::{sync::Arc, time::Instant};

/// An "actor" that keeps a client session alive.
pub struct Keepalive {
    open_socks5_conn: Sender<(String, Sender<sosistab::mux::RelConn>)>,
    get_stats: Sender<Sender<sosistab::SessionStats>>,
    _task: smol::Task<anyhow::Result<()>>,
}

impl Keepalive {
    /// Creates a new keepalive.
    pub fn new(
        stats: Arc<StatCollector>,
        exit_host: &str,
        use_bridges: bool,
        ccache: Arc<ClientCache>,
    ) -> Self {
        let (send, recv) = smol::channel::unbounded();
        let (send_stats, recv_stats) = smol::channel::unbounded();
        Keepalive {
            open_socks5_conn: send,
            get_stats: send_stats,
            _task: smolscale::spawn(keepalive_actor(
                stats,
                exit_host.to_string(),
                use_bridges,
                ccache,
                recv,
                recv_stats,
            )),
        }
    }

    /// Opens a connection
    pub async fn connect(&self, remote: &str) -> anyhow::Result<sosistab::mux::RelConn> {
        let (send, recv) = smol::channel::bounded(1);
        self.open_socks5_conn
            .send((remote.to_string(), send))
            .await?;
        Ok(recv.recv().await?)
    }

    /// Gets session statistics
    pub async fn get_stats(&self) -> anyhow::Result<sosistab::SessionStats> {
        let (send, recv) = smol::channel::bounded(1);
        self.get_stats.send(send).await?;
        Ok(recv.recv().await?)
    }
}

async fn keepalive_actor(
    stats: Arc<StatCollector>,
    exit_host: String,
    use_bridges: bool,
    ccache: Arc<ClientCache>,
    recv_socks5_conn: Receiver<(String, Sender<sosistab::mux::RelConn>)>,
    recv_get_stats: Receiver<Sender<sosistab::SessionStats>>,
) -> anyhow::Result<()> {
    loop {
        if let Err(err) = keepalive_actor_once(
            stats.clone(),
            exit_host.clone(),
            use_bridges,
            ccache.clone(),
            recv_socks5_conn.clone(),
            recv_get_stats.clone(),
        )
        .await
        {
            log::warn!("keepalive_actor restarting: {}", err);
            smol::Timer::after(Duration::from_secs(1)).await;
        }
    }
}

async fn keepalive_actor_once(
    stats: Arc<StatCollector>,
    exit_host: String,
    use_bridges: bool,
    ccache: Arc<ClientCache>,
    recv_socks5_conn: Receiver<(String, Sender<sosistab::mux::RelConn>)>,
    recv_get_stats: Receiver<Sender<sosistab::SessionStats>>,
) -> anyhow::Result<()> {
    stats.set_exit_descriptor(None);

    // find the exit
    let mut exits = ccache.get_exits().await.context("can't get exits")?;
    if exits.is_empty() {
        anyhow::bail!("no exits found")
    }
    exits.sort_by(|a, b| {
        strsim::damerau_levenshtein(&a.hostname, &exit_host)
            .cmp(&strsim::damerau_levenshtein(&b.hostname, &exit_host))
    });
    let exit_host = exits[0].hostname.clone();

    let bridge_sess_async = async {
        let bridges = ccache
            .get_bridges(&exit_host)
            .await
            .context("can't get bridges")?;
        log::debug!("got {} bridges", bridges.len());
        if bridges.is_empty() {
            anyhow::bail!("absolutely no bridges found")
        }
        // spawn a task for *every* bridge
        let (send, recv) = smol::channel::unbounded();
        let _tasks: Vec<_> = bridges
            .into_iter()
            .map(|desc| {
                let send = send.clone();
                smolscale::spawn(async move {
                    log::debug!("connecting through {}...", desc.endpoint);
                    drop(
                        send.send((
                            desc.endpoint,
                            sosistab::connect(desc.endpoint, desc.sosistab_key).await,
                        ))
                        .await,
                    )
                })
            })
            .collect();
        // wait for a successful result
        loop {
            let (saddr, res) = recv.recv().await.context("ran out of bridges")?;
            if let Ok(res) = res {
                log::info!("{} is our fastest bridge", saddr);
                break Ok(res);
            }
        }
    };
    let exit_info = exits.iter().find(|v| v.hostname == exit_host).unwrap();
    let connected_sess_async = async {
        if use_bridges {
            bridge_sess_async.await
        } else {
            async {
                Ok(infal(
                    sosistab::connect(
                        smol::net::resolve(format!("{}:19831", exit_info.hostname))
                            .await
                            .context("can't resolve hostname of exit")?[0],
                        exit_info.sosistab_key,
                    )
                    .await,
                )
                .await)
            }
            .or(async {
                smol::Timer::after(Duration::from_secs(5)).await;
                log::warn!("turning on bridges because we couldn't get a direct connection");
                bridge_sess_async.await
            })
            .await
        }
    };
    let session: anyhow::Result<sosistab::Session> = connected_sess_async
        .or(async {
            smol::Timer::after(Duration::from_secs(10)).await;
            anyhow::bail!("initial connection timeout after 10");
        })
        .await;
    let session = session?;
    let mux = sosistab::mux::Multiplex::new(session);
    let (send_stop, recv_stop) = smol::channel::unbounded();
    let scope = smol::Executor::new();
    // now let's authenticate
    let token = ccache.get_auth_token().await?;
    authenticate_session(&mux, &token)
        .timeout(Duration::from_secs(5))
        .await
        .ok_or_else(|| anyhow::anyhow!("authentication timed out"))??;
    // TODO actually authenticate
    log::info!(
        "KEEPALIVE MAIN LOOP for exit_host={}, use_bridges={}",
        exit_host,
        use_bridges
    );
    stats.set_exit_descriptor(Some(exits[0].clone()));
    scope
        .spawn(async {
            loop {
                smol::Timer::after(Duration::from_secs(200)).await;
                if mux
                    .open_conn(None)
                    .timeout(Duration::from_secs(15))
                    .await
                    .is_none()
                {
                    let _ = send_stop.send(anyhow::anyhow!("watchdog timed out")).await;
                    return;
                }
            }
        })
        .detach();
    scope
        .run(
            async {
                loop {
                    let (conn_host, conn_reply) = recv_socks5_conn
                        .recv()
                        .await
                        .context("cannot get socks5 connect request")?;
                    let mux = &mux;
                    let stats = stats.clone();
                    let send_stop = send_stop.clone();
                    scope
                        .spawn(async move {
                            let start = Instant::now();
                            let remote = (&mux)
                                .open_conn(Some(conn_host))
                                .timeout(Duration::from_secs(15))
                                .await;
                            if let Some(remote) = remote {
                                let remote = remote.ok()?;
                                log::debug!(
                                    "opened connection in {} ms",
                                    start.elapsed().as_millis()
                                );
                                stats.set_latency(start.elapsed().as_secs_f64() * 1000.0);
                                conn_reply.send(remote).await.ok()?;
                                Some(())
                            } else {
                                send_stop
                                    .try_send(anyhow::anyhow!("normal connection timed out"))
                                    .unwrap();
                                Some(())
                            }
                        })
                        .detach();
                }
            }
            .or(async { Err(recv_stop.recv().await?) })
            .or(async {
                loop {
                    let stat_send = recv_get_stats.recv().await?;
                    let stats = mux.get_session().get_stats().await;
                    stat_send.send(stats).await?;
                }
            }),
        )
        .await
}

async fn infal<T, E>(v: Result<T, E>) -> T {
    if let Ok(v) = v {
        v
    } else {
        smol::future::pending().await
    }
}

/// authenticates a muxed session
async fn authenticate_session(
    session: &sosistab::mux::Multiplex,
    token: &crate::cache::Token,
) -> anyhow::Result<()> {
    let mut auth_conn = session.open_conn(None).await?;
    log::debug!("sending auth info...");
    aioutils::write_pascalish(
        &mut auth_conn,
        &(
            &token.unblinded_digest,
            &token.unblinded_signature,
            &token.level,
        ),
    )
    .await?;
    let _: u8 = aioutils::read_pascalish(&mut auth_conn).await?;
    Ok(())
}
