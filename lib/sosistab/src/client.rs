use crate::*;
use bytes::Bytes;
use smol::channel::{Receiver, Sender};
use smol::prelude::*;
use std::net::SocketAddr;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// Connects to a remote server.
pub async fn connect(
    server_addr: SocketAddr,
    pubkey: x25519_dalek::PublicKey,
) -> std::io::Result<Session> {
    connect_custom(server_addr, pubkey, || {
        let val = "0.0.0.0:0".parse::<SocketAddr>().unwrap();
        Ok(val)
    })
    .await
}

/// Connects to a remote server, given a closure that generates socket addresses.
pub async fn connect_custom(
    server_addr: SocketAddr,
    pubkey: x25519_dalek::PublicKey,
    laddr_gen: impl Fn() -> std::io::Result<SocketAddr> + Send + Sync + 'static,
) -> std::io::Result<Session> {
    let udp_socket = runtime::new_udp_socket_bind(laddr_gen()?).await?;
    let my_long_sk = x25519_dalek::StaticSecret::new(&mut rand::thread_rng());
    let my_eph_sk = x25519_dalek::StaticSecret::new(&mut rand::thread_rng());
    // do the handshake
    let cookie = crypt::Cookie::new(pubkey);
    let init_hello = msg::HandshakeFrame::ClientHello {
        long_pk: (&my_long_sk).into(),
        eph_pk: (&my_eph_sk).into(),
        version: 1,
    };
    let mut buf = [0u8; 2048];
    for timeout_factor in (0u32..).map(|x| 2u64.pow(x)) {
        // send hello
        let init_hello = crypt::StdAEAD::new(&cookie.generate_c2s().next().unwrap())
            .pad_encrypt(&init_hello, 1000);
        udp_socket.send_to(&init_hello, server_addr).await?;
        log::trace!("sent client hello");
        // wait for response
        let res = udp_socket
            .recv_from(&mut buf)
            .or(async {
                smol::Timer::after(Duration::from_secs(timeout_factor)).await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out",
                ))
            })
            .await;
        match res {
            Ok((n, _)) => {
                let buf = &buf[..n];
                for possible_key in cookie.generate_s2c() {
                    let decrypter = crypt::StdAEAD::new(&possible_key);
                    let response: Option<msg::HandshakeFrame> = decrypter.pad_decrypt(buf);
                    if let Some(msg::HandshakeFrame::ServerHello {
                        long_pk,
                        eph_pk,
                        resume_token,
                    }) = response
                    {
                        log::trace!("obtained response from server");
                        if long_pk.as_bytes() != pubkey.as_bytes() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::ConnectionRefused,
                                "bad pubkey",
                            ));
                        }
                        let shared_sec =
                            crypt::triple_ecdh(&my_long_sk, &my_eph_sk, &long_pk, &eph_pk);
                        return init_session(
                            cookie,
                            resume_token,
                            shared_sec,
                            server_addr,
                            Arc::new(laddr_gen),
                        )
                        .await;
                    }
                }
            }
            Err(err) => {
                if err.kind() == std::io::ErrorKind::TimedOut {
                    log::trace!(
                        "timed out to {} with {}s timeout; trying again",
                        server_addr,
                        timeout_factor
                    );
                    continue;
                }
                return Err(err);
            }
        }
    }
    unimplemented!()
}

const SHARDS: u8 = 2;
const RESET_MILLIS: u128 = 5000;

async fn init_session(
    cookie: crypt::Cookie,
    resume_token: Bytes,
    shared_sec: blake3::Hash,
    remote_addr: SocketAddr,
    laddr_gen: Arc<impl Fn() -> std::io::Result<SocketAddr> + Send + Sync + 'static>,
) -> std::io::Result<Session> {
    let (send_frame_out, recv_frame_out) = smol::channel::bounded::<msg::DataFrame>(1000);
    let (send_frame_in, recv_frame_in) = smol::channel::bounded::<msg::DataFrame>(1000);
    let backhaul_tasks: Vec<_> = (0..SHARDS)
        .map(|i| {
            runtime::spawn(client_backhaul_once(
                cookie.clone(),
                resume_token.clone(),
                send_frame_in.clone(),
                recv_frame_out.clone(),
                i,
                remote_addr,
                shared_sec,
                laddr_gen.clone(),
            ))
        })
        .collect();
    let mut session = Session::new(SessionConfig {
        latency: std::time::Duration::from_millis(1),
        target_loss: 0.05,
        send_frame: send_frame_out,
        recv_frame: recv_frame_in,
    });
    session.on_drop(move || {
        drop(backhaul_tasks);
    });
    Ok(session)
}

#[allow(clippy::all)]
async fn client_backhaul_once(
    cookie: crypt::Cookie,
    resume_token: Bytes,
    send_frame_in: Sender<msg::DataFrame>,
    recv_frame_out: Receiver<msg::DataFrame>,
    shard_id: u8,
    remote_addr: SocketAddr,
    shared_sec: blake3::Hash,
    laddr_gen: Arc<impl Fn() -> std::io::Result<SocketAddr> + Send + Sync + 'static>,
) -> Option<()> {
    let up_key = blake3::keyed_hash(crypt::UP_KEY, shared_sec.as_bytes());
    let dn_key = blake3::keyed_hash(crypt::DN_KEY, shared_sec.as_bytes());
    let dn_crypter = Arc::new(crypt::StdAEAD::new(dn_key.as_bytes()));
    let up_crypter = Arc::new(crypt::StdAEAD::new(up_key.as_bytes()));
    let mut buf = [0u8; 2048];

    let mut last_resume = Instant::now();
    let mut updated = false;
    let mut socket = runtime::new_udp_socket_bind(laddr_gen().ok()?).await.ok()?;
    // let mut _old_cleanup: Option<smol::Task<Option<()>>> = None;

    #[derive(Debug)]
    enum Evt {
        Incoming(msg::DataFrame),
        Outgoing(Bytes),
    };

    loop {
        let down_socket = socket.clone();
        let down = {
            let dn_crypter = dn_crypter.clone();
            async move {
                let (n, addr) = down_socket.recv_from(&mut buf).await.ok()?;
                if let Some(plain) = dn_crypter.pad_decrypt::<msg::DataFrame>(&buf[..n]) {
                    log::trace!("shard {} decrypted UDP message with len {}", shard_id, n);
                    Some(Evt::Incoming(plain))
                } else {
                    log::warn!("anomalous UDP packet of len {} from {}", n, addr);
                    smol::future::pending().await
                }
            }
        };
        let up_crypter = up_crypter.clone();
        let up = async {
            let df = recv_frame_out.recv().await.ok()?;
            let encrypted = up_crypter.pad_encrypt(df, 1000);
            Some(Evt::Outgoing(encrypted))
        };
        match smol::future::race(down, up).await {
            Some(Evt::Incoming(df)) => {
                send_frame_in.send(df).await.ok()?;
            }
            Some(Evt::Outgoing(bts)) => {
                let now = Instant::now();
                if now.saturating_duration_since(last_resume).as_millis() > RESET_MILLIS || !updated
                {
                    updated = true;
                    last_resume = Instant::now();
                    let g_encrypt = crypt::StdAEAD::new(&cookie.generate_c2s().next().unwrap());
                    // also replace the UDP socket!
                    let old_socket = socket.clone();
                    let dn_crypter = dn_crypter.clone();
                    let send_frame_in = send_frame_in.clone();
                    // spawn a task to clean up the UDP socket
                    let tata: smol::Task<Option<()>> = runtime::spawn(
                        async move {
                            loop {
                                let (n, _) = old_socket.recv_from(&mut buf).await.ok()?;
                                if let Some(plain) =
                                    dn_crypter.pad_decrypt::<msg::DataFrame>(&buf[..n])
                                {
                                    log::trace!(
                                        "shard {} decrypted UDP message with len {}",
                                        shard_id,
                                        n
                                    );
                                    drop(send_frame_in.send(plain).await)
                                }
                            }
                        }
                        .or(async {
                            smol::Timer::after(Duration::from_secs(5)).await;
                            None
                        }),
                    );
                    tata.detach();
                    socket = loop {
                        match runtime::new_udp_socket_bind(laddr_gen().ok()?).await {
                            Ok(sock) => break sock,
                            Err(err) => {
                                log::warn!("error rebinding: {}", err);
                                smol::Timer::after(Duration::from_secs(1)).await;
                            }
                        }
                    };
                    log::trace!(
                        "resending resume token {} to {} from {}...",
                        shard_id,
                        remote_addr,
                        socket.local_addr().unwrap()
                    );
                    drop(
                        socket
                            .send_to(
                                &g_encrypt.pad_encrypt(
                                    msg::HandshakeFrame::ClientResume {
                                        resume_token: resume_token.clone(),
                                        shard_id,
                                    },
                                    1000,
                                ),
                                remote_addr,
                            )
                            .await,
                    );
                }
                drop(socket.send_to(&bts, remote_addr).await);
            }
            None => return None,
        }
    }
}
