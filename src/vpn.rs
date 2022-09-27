use anyhow::Context;
use bytes::Bytes;

use cidr_utils::cidr::Ipv4Cidr;
use futures_util::TryFutureExt;
use libc::{c_void, fcntl, F_GETFL, F_SETFL, O_NONBLOCK, SOL_IP, SO_ORIGINAL_DST};

use moka::sync::Cache;

use once_cell::sync::Lazy;
use os_socketaddr::OsSocketAddr;
use parking_lot::Mutex;
use pnet_packet::{
    ip::IpNextHeaderProtocols, ipv4::Ipv4Packet, tcp::TcpPacket, udp::UdpPacket, Packet,
};
use rand::prelude::*;
use smol::channel::Sender;
use sosistab::{Buff, BuffMut};

use geph4_protocol::VpnMessage;
use std::{
    collections::HashSet,
    io::{Read},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::{Deref, DerefMut},
    os::unix::prelude::{AsRawFd, FromRawFd},
    sync::{atomic::Ordering, Arc},
};
use tundevice::TunDevice;

use crate::{
    connect::proxy_loop,
    listen::RootCtx,
    ratelimit::{RateLimiter, STAT_LIMITER, TOTAL_BW_COUNT},
};

/// Runs the transparent proxy helper
pub async fn transparent_proxy_helper(ctx: Arc<RootCtx>) -> anyhow::Result<()> {
    if ctx.config.nat_external_iface().is_none() {
        return Ok(());
    }
    // always run on port 10000
    // TODO this should bind dynamically
    let listen_addr: SocketAddr = "0.0.0.0:10000".parse().unwrap();
    let listener = smol::Async::<std::net::TcpListener>::bind(listen_addr).unwrap();

    loop {
        let (client, _) = listener.accept().await.unwrap();
        let ctx = ctx.clone();
        let rate_limit = Arc::new(RateLimiter::unlimited());
        let conn_task = smolscale::spawn(
            async move {
                static CLIENT_ID_CACHE: Lazy<Cache<IpAddr, u64>> =
                    Lazy::new(|| Cache::new(1_000_000));
                let peer_addr = client.as_ref().peer_addr().context("no peer addr")?.ip();
                let client_id = CLIENT_ID_CACHE.get_with(peer_addr, || rand::thread_rng().gen());
                let client_fd = client.as_raw_fd();
                let addr = unsafe {
                    let raw_addr = OsSocketAddr::new();
                    if libc::getsockopt(
                        client_fd,
                        SOL_IP,
                        SO_ORIGINAL_DST,
                        raw_addr.as_ptr() as *mut c_void,
                        (&mut std::mem::size_of::<libc::sockaddr>()) as *mut usize as *mut u32,
                    ) != 0
                    {
                        anyhow::bail!("cannot get SO_ORIGINAL_DST, aborting");
                    };
                    let lala = raw_addr.into_addr();
                    if let Some(lala) = lala {
                        lala
                    } else {
                        anyhow::bail!("SO_ORIGINAL_DST is not an IP address, aborting");
                    }
                };
                let client = async_dup::Arc::new(client);
                client
                    .get_ref()
                    .set_nodelay(true)
                    .context("cannot set nodelay")?;
                proxy_loop(ctx, rate_limit, client, client_id, addr.to_string(), false).await
            }
            .map_err(|e| log::debug!("vpn conn closed: {:?}", e)),
        );
        conn_task.detach();
    }
}

/// Handles a VPN session
pub async fn handle_vpn_session(
    ctx: Arc<RootCtx>,
    mux: Arc<sosistab::Multiplex>,
    rate_limit: Arc<RateLimiter>,
    on_activity: impl Fn(),
) -> anyhow::Result<()> {
    if ctx.config.nat_external_iface().is_none() {
        log::warn!("disabling VPN mode since external interface is not specified!");
        return smol::future::pending().await;
    }
    Lazy::force(&INCOMING_PKT_HANDLER);
    log::trace!("handle_vpn_session entered");
    scopeguard::defer!(log::trace!("handle_vpn_session exited"));

    // set up IP address allocation
    let assigned_ip: Lazy<AssignedIpv4Addr> = Lazy::new(|| IpAddrAssigner::global().assign());
    let addr = assigned_ip.addr();
    scopeguard::defer!({
        INCOMING_MAP.invalidate(&addr);
    });
    let stat_key = format!(
        "exit_usage.{}",
        ctx.config
            .official()
            .as_ref()
            .map(|official| official.exit_hostname().to_string())
            .unwrap_or_default()
            .replace('.', "-")
    );

    let (send_down, recv_down) = smol::channel::bounded(if rate_limit.is_unlimited() {
        65536
    } else {
        (rate_limit.limit() / 4) as usize
    });
    INCOMING_MAP.insert(addr, send_down);
    let _down_task: smol::Task<anyhow::Result<()>> = {
        let stat_key = stat_key.clone();
        let ctx = ctx.clone();
        let mux = mux.clone();
        smolscale::spawn(async move {
            loop {
                let bts = recv_down.recv().await?;
                if let Some(stat_client) = ctx.stat_client.as_ref() {
                    let n = bts.len();
                    TOTAL_BW_COUNT.fetch_add(n as u64, Ordering::Relaxed);
                    if fastrand::f64() < 0.01 && STAT_LIMITER.check().is_ok() {
                        stat_client
                            .count(&stat_key, TOTAL_BW_COUNT.swap(0, Ordering::Relaxed) as f64)
                    }
                }
                rate_limit.wait(bts.len()).await;
                let pkt = Ipv4Packet::new(&bts).expect("don't send me invalid IPv4 packets!");
                assert_eq!(pkt.get_destination(), addr);
                let msg = VpnMessage::Payload(Bytes::copy_from_slice(&bts));
                let mut to_send = BuffMut::new();
                bincode::serialize_into(to_send.deref_mut(), &msg).unwrap();
                let _ = mux.send_urel(to_send).await;
            }
        })
    };
    let mut stat_count = 0u64;
    loop {
        let bts = mux.recv_urel().await?;
        on_activity();
        let msg: VpnMessage = bincode::deserialize(&bts)?;
        match msg {
            VpnMessage::ClientHello { .. } => {
                mux.send_urel(
                    bincode::serialize(&VpnMessage::ServerHello {
                        client_ip: *assigned_ip.clone(),
                        gateway: "100.64.0.1".parse().unwrap(),
                    })
                    .unwrap()
                    .as_slice(),
                )
                .await?;
            }
            VpnMessage::Payload(bts) => {
                if let Some(stat_client) = ctx.stat_client.as_ref() {
                    stat_count += bts.len() as u64;
                    if fastrand::f64() < 0.01 && STAT_LIMITER.check().is_ok() {
                        stat_client.count(&stat_key, stat_count as f64);
                        stat_count = 0;
                    }
                }
                let pkt = Ipv4Packet::new(&bts);
                if let Some(pkt) = pkt {
                    // source must be correct and destination must not be banned
                    if pkt.get_source() != assigned_ip.addr()
                        || pkt.get_destination().is_loopback()
                        || pkt.get_destination().is_private()
                        || pkt.get_destination().is_unspecified()
                        || pkt.get_destination().is_broadcast()
                    {
                        continue;
                    }
                    // must not be blacklisted
                    let port = {
                        match pkt.get_next_level_protocol() {
                            IpNextHeaderProtocols::Tcp => {
                                TcpPacket::new(pkt.payload()).map(|v| v.get_destination())
                            }
                            IpNextHeaderProtocols::Udp => {
                                UdpPacket::new(pkt.payload()).map(|v| v.get_destination())
                            }
                            _ => None,
                        }
                    };
                    if let Some(port) = port {
                        // Block QUIC due to it performing badly over sosistab etc
                        if pkt.get_next_level_protocol() == IpNextHeaderProtocols::Udp
                            && port == 443
                        {
                            continue;
                        }
                        if crate::lists::BLACK_PORTS.contains(&port) {
                            continue;
                        }
                        if ctx.config.port_whitelist() && !crate::lists::WHITE_PORTS.contains(&port)
                        {
                            continue;
                        }
                    }
                    RAW_TUN.write_raw(&bts).await;
                }
            }
            _ => anyhow::bail!("message in invalid context"),
        }
    }
}

/// Mapping for incoming packets
#[allow(clippy::type_complexity)]
static INCOMING_MAP: Lazy<Cache<Ipv4Addr, Sender<Buff>>> =
    Lazy::new(|| Cache::builder().max_capacity(1_000_000).build());

/// Incoming packet handler
static INCOMING_PKT_HANDLER: Lazy<std::thread::JoinHandle<()>> = Lazy::new(|| {
    std::thread::Builder::new()
        .name("tun-reader".into())
        .spawn(|| {
            let mut buf = [0; 2048];
            let fd = RAW_TUN.dup_rawfd();
            // set into BLOCKING mode
            unsafe {
                let mut flags = libc::fcntl(fd, F_GETFL);
                flags &= !O_NONBLOCK;
                fcntl(fd, F_SETFL, flags);
            }
            let mut reader = unsafe { std::fs::File::from_raw_fd(fd) };
            // let mut bufs = vec![[0u8; 2048]; 128];
            // loop {
            //     let result = {
            //         let mut mmsg_buffers = bufs
            //             .iter_mut()
            //             .map(|b| [IoSliceMut::new(b)])
            //             .collect::<Vec<_>>();
            //         let mut mmsg_buffers = mmsg_buffers
            //             .iter_mut()
            //             .map(|b| RecvMmsgData {
            //                 iov: b,
            //                 cmsg_buffer: None,
            //             })
            //             .collect::<Vec<_>>();
            //         let mmsg_buffers = mmsg_buffers.iter_mut().collect::<Vec<_>>();
            //         recvmmsg::<_, SockaddrStorage>(fd, mmsg_buffers, MsgFlags::empty(), None)
            //             .expect("recvmmsg failed")
            //             .into_iter()
            //             .map(|s| s.bytes)
            //             .collect::<Vec<_>>()
            //     };
            //     log::debug!("tun got {} mmsg", result.len());
            //     for (n, buf) in result.into_iter().zip(bufs.iter()) {
            //         let pkt = &buf[..n];
            //         let dest =
            //             Ipv4Packet::new(pkt).map(|pkt| INCOMING_MAP.get(&pkt.get_destination()));
            //         if let Some(Some(dest)) = dest {
            //             if let Err(err) = dest.try_send(pkt.into()) {
            //                 log::trace!("error forwarding packet obtained from tun: {:?}", err);
            //             }
            //         }
            //     }
            // }
            loop {
                let n = reader.read(&mut buf).expect("cannot read from tun device");
                let pkt = &buf[..n];
                let dest = Ipv4Packet::new(pkt).map(|pkt| INCOMING_MAP.get(&pkt.get_destination()));
                if let Some(Some(dest)) = dest {
                    if let Err(err) = dest.try_send(pkt.into()) {
                        log::trace!("error forwarding packet obtained from tun: {:?}", err);
                    }
                }
            }
        })
        .unwrap()
});

/// The raw TUN device.
static RAW_TUN: Lazy<TunDevice> = Lazy::new(|| {
    log::info!("initializing tun-geph");
    let dev =
        TunDevice::new_from_os("tun-geph").expect("could not initiate 'tun-geph' tun device!");
    dev.assign_ip("100.64.0.1/10");
    smol::future::block_on(dev.write_raw(b"hello world"));
    dev
});

/// Global IpAddr assigner
static CGNAT_IPASSIGN: Lazy<IpAddrAssigner> =
    Lazy::new(|| IpAddrAssigner::new("100.64.0.0/10".parse().unwrap()));

/// An IP address assigner
pub struct IpAddrAssigner {
    cidr: Ipv4Cidr,
    table: Arc<Mutex<HashSet<Ipv4Addr>>>,
}

impl IpAddrAssigner {
    /// Creates a new address assigner.
    pub fn new(cidr: Ipv4Cidr) -> Self {
        Self {
            cidr,
            table: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Get the global CGNAT instance.
    pub fn global() -> &'static Self {
        &CGNAT_IPASSIGN
    }

    /// Assigns a new IP address.
    pub fn assign(&self) -> AssignedIpv4Addr {
        let first = self.cidr.first();
        let last = self.cidr.last();
        loop {
            let candidate = rand::thread_rng().gen_range(first + 16, last - 16);
            let candidate = Ipv4Addr::from(candidate);
            let mut tab = self.table.lock();
            if !tab.contains(&candidate) {
                tab.insert(candidate);
                log::trace!("assigned {}", candidate);
                return AssignedIpv4Addr::new(self.table.clone(), candidate);
            }
        }
    }
}

/// An assigned IP address. Derefs to std::net::Ipv4Addr and acts as a smart-pointer that deassigns the IP address when no longer needed.
#[derive(Clone, Debug)]
pub struct AssignedIpv4Addr {
    inner: Arc<AssignedIpv4AddrInner>,
}

impl AssignedIpv4Addr {
    fn new(table: Arc<Mutex<HashSet<Ipv4Addr>>>, addr: Ipv4Addr) -> Self {
        Self {
            inner: Arc::new(AssignedIpv4AddrInner { addr, table }),
        }
    }
    pub fn addr(&self) -> Ipv4Addr {
        self.inner.addr
    }
}

impl PartialEq for AssignedIpv4Addr {
    fn eq(&self, other: &Self) -> bool {
        self.inner.addr.eq(&other.inner.addr)
    }
}

impl Eq for AssignedIpv4Addr {}

impl PartialOrd for AssignedIpv4Addr {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.inner.addr.partial_cmp(&other.inner.addr)
    }
}

impl Ord for AssignedIpv4Addr {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.inner.addr.cmp(&other.inner.addr)
    }
}

impl Deref for AssignedIpv4Addr {
    type Target = Ipv4Addr;

    fn deref(&self) -> &Self::Target {
        &self.inner.addr
    }
}

#[derive(Debug)]
struct AssignedIpv4AddrInner {
    addr: Ipv4Addr,
    table: Arc<Mutex<HashSet<Ipv4Addr>>>,
}

impl Drop for AssignedIpv4AddrInner {
    fn drop(&mut self) {
        log::trace!("dropped {}", self.addr);
        if !self.table.lock().remove(&self.addr) {
            panic!("AssignedIpv4Addr double free?! {}", self.addr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgnat() {
        let assigner = IpAddrAssigner::new("100.64.0.0/10".parse().unwrap());
        let mut assigned = Vec::new();
        for _ in 0..2 {
            assigned.push(assigner.assign());
        }
        dbg!(assigned);
    }
}
