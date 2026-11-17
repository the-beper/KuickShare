//! Connecting to a LAN peer over the interface that holds its subnet.
//!
//! The default route is not enough: a machine with a VPN, a tunnel or a second
//! NIC can send a LAN transfer out of the wrong interface, where the peer is
//! unreachable — the send then fails in a way that looks like "the device is not
//! reachable" and nothing in the logs says why. After crQuickShare's
//! `ConnectTcpChannel` (BSD-3-Clause, The Chromium Authors): pick the interface
//! whose subnet contains the peer, pin the socket to that device with
//! `SO_BINDTODEVICE` and bind the source address on it. crQuickShare pins with
//! `IP_UNICAST_IF`, which Linux TCP ignores (read in the kernel source:
//! `tcp_v4_connect` routes with `sk_bound_dev_if`, and only UDP, raw and ping sockets
//! read `uc_index`), so that pin would do nothing here. `SO_BINDTODEVICE` needs no
//! privilege on a socket not yet bound to a device since Linux 5.7; where it is
//! refused, the connect falls back to the default route. Point-to-point interfaces (WireGuard,
//! OpenVPN `tun`, Tailscale) never win the pick, and a physical interface (a backing
//! device under `/sys/class/net`) wins over a bridge, bond or VLAN on the same
//! subnet. Unlike crQuickShare, a peer that no interface holds (a routed subnet)
//! takes the default route instead of being refused: that is the only route to
//! it anyway.
//!
//! IPv4 only: the mDNS discovery publishes IPv4 addresses only, and any other
//! peer takes the plain connect.
//!
//! Log lines carry the interface and the outcome — never an address — the same
//! rule the app applies in its route choice.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;

use if_addrs::{IfAddr, IfOperStatus, Ifv4Addr, Interface};
use tokio::net::{TcpSocket, TcpStream};

/// The directly attached route to a peer.
#[derive(Debug, PartialEq)]
struct Route {
    /// The interface's own address: the connect's source.
    local_ip: Ipv4Addr,
    /// Tells a second address of one interface from another interface.
    ifindex: u32,
    /// Device for `SO_BINDTODEVICE`.
    name: String,
}

/// Connects to a LAN peer over the interface that holds its subnet; see the
/// module docs for the pick and for the peers that keep the plain connect.
pub async fn lan_connect(peer: SocketAddr) -> io::Result<TcpStream> {
    let SocketAddr::V4(v4) = peer else {
        return TcpStream::connect(peer).await;
    };
    if v4.ip().is_loopback() {
        return TcpStream::connect(peer).await;
    }
    let Some(route) = pick_route(*v4.ip()) else {
        debug!("LAN connect: no interface holds the peer's subnet, using the default route");
        return TcpStream::connect(peer).await;
    };
    debug!("LAN connect over interface {}", route.name);
    let socket = TcpSocket::new_v4()?;
    let pinned = bind_to_device(socket.as_fd(), device_name(&route.name))
        .and_then(|()| socket.bind(SocketAddr::new(IpAddr::V4(route.local_ip), 0)));
    if let Err(e) = pinned {
        // A sandbox that refuses the option must not cost the LAN: the plain connect
        // is what worked before.
        debug!(
            "LAN connect: cannot bind to {} ({e}), using the default route",
            route.name
        );
        return TcpStream::connect(peer).await;
    }
    socket.connect(peer).await
}

/// `None` when no interface holds the peer's subnet, or when they cannot be listed.
fn pick_route(peer: Ipv4Addr) -> Option<Route> {
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces,
        Err(e) => {
            debug!("LAN connect: cannot list the interfaces: {e}");
            return None;
        }
    };
    pick(peer, &interfaces, physical_interface)
}

fn pick<F>(peer: Ipv4Addr, interfaces: &[Interface], physical: F) -> Option<Route>
where
    F: Fn(&str) -> bool,
{
    let mut chosen: Option<(Route, bool)> = None;
    for iface in interfaces {
        if iface.is_loopback() || iface.is_p2p {
            continue;
        }
        // Unknown stays: a netlink that cannot report the state must not make every
        // interface look down.
        if !matches!(iface.oper_status, IfOperStatus::Up | IfOperStatus::Unknown) {
            continue;
        }
        let (Some(ifindex), IfAddr::V4(addr)) = (iface.index, &iface.addr) else {
            continue;
        };
        if !covers(addr, peer) {
            continue;
        }
        let route = Route {
            local_ip: addr.ip,
            ifindex,
            name: iface.name.clone(),
        };
        let is_physical = physical(&iface.name);
        match &chosen {
            None => chosen = Some((route, is_physical)),
            // A second address of the same interface: the same route.
            Some((previous, _)) if previous.ifindex == ifindex => {}
            Some((previous, was_physical)) => {
                let replace = is_physical && !was_physical;
                debug!(
                    "LAN connect: {} and {} both hold the peer's subnet, using {}",
                    previous.name,
                    route.name,
                    if replace { &route.name } else { &previous.name }
                );
                if replace {
                    chosen = Some((route, is_physical));
                }
            }
        }
    }
    chosen.map(|(route, _)| route)
}

/// Whether the interface's subnet contains the peer. A zero netmask would make
/// every address "covered", so it never matches.
fn covers(addr: &Ifv4Addr, peer: Ipv4Addr) -> bool {
    let mask = u32::from(addr.netmask);
    mask != 0 && (u32::from(peer) & mask) == (u32::from(addr.ip) & mask)
}

/// A physical interface has a backing device: `/sys/class/net/<name>/device`.
/// Where sysfs cannot be read (some containers) nothing counts as physical, and
/// the pick takes the first interface that holds the subnet.
fn physical_interface(name: &str) -> bool {
    Path::new("/sys/class/net")
        .join(name)
        .join("device")
        .try_exists()
        .unwrap_or(false)
}

/// `SO_BINDTODEVICE`: the socket's traffic uses this device regardless of what the
/// routing table would pick. Linux allows it without `CAP_NET_RAW` on a socket that
/// is not bound to a device yet.
/// The device of an interface name: an IPv4 alias is listed as `eth0:1`, and
/// `SO_BINDTODEVICE` takes the device, not the label.
fn device_name(name: &str) -> &str {
    name.split_once(':').map_or(name, |(device, _)| device)
}

fn bind_to_device(fd: BorrowedFd<'_>, name: &str) -> io::Result<()> {
    // SAFETY: `fd` is a socket borrowed for the duration of the call, and the option
    // value points to `name`'s bytes, which outlive the call, with their length
    // passed; the kernel only reads them.
    let outcome = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast::<libc::c_void>(),
            name.len() as libc::socklen_t,
        )
    };
    if outcome == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, index: u32, ip: Ipv4Addr, netmask: Ipv4Addr) -> Interface {
        Interface {
            name: name.to_owned(),
            addr: IfAddr::V4(Ifv4Addr {
                ip,
                netmask,
                prefixlen: u32::from(netmask).count_ones() as u8,
                broadcast: None,
            }),
            index: Some(index),
            oper_status: IfOperStatus::Up,
            is_p2p: false,
        }
    }

    /// A point-to-point variant (a tunnel), same addresses otherwise.
    fn p2p(name: &str, index: u32, ip: Ipv4Addr, netmask: Ipv4Addr) -> Interface {
        let mut iface = iface(name, index, ip, netmask);
        iface.is_p2p = true;
        iface
    }

    const EVERYTHING: Ipv4Addr = Ipv4Addr::new(0, 0, 0, 0);
    const HOME_MASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
    const HOME: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);
    const PEER: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 33);

    /// What sysfs would say: NICs and Wi-Fi have a device, bridges and tunnels not.
    fn physical(name: &str) -> bool {
        name.starts_with("wl") || name.starts_with("en")
    }

    #[test]
    fn picks_the_interface_whose_subnet_contains_the_peer() {
        // Wi-Fi and a VPN: only the Wi-Fi interface may serve a LAN peer. The
        // tunnel's zero netmask would swallow every peer, so it must be out.
        let interfaces = [
            iface("wlan0", 2, HOME, HOME_MASK),
            p2p("wg0", 3, Ipv4Addr::new(10, 8, 0, 2), EVERYTHING),
        ];
        let route = pick(PEER, &interfaces, physical).expect("wlan0 holds the peer's subnet");
        assert_eq!(
            route,
            Route {
                local_ip: HOME,
                ifindex: 2,
                name: "wlan0".into()
            }
        );
    }

    #[test]
    fn a_tunnel_never_serves_a_lan_peer() {
        // A VPN that pushes the LAN's own subnet: still not the LAN.
        let interfaces = [p2p("tun0", 3, HOME, HOME_MASK)];
        assert_eq!(pick(PEER, &interfaces, physical), None);
    }

    #[test]
    fn a_bridge_serves_its_subnet() {
        // The NIC is enslaved to a bridge (VMs on the desktop): the bridge holds the
        // address and must serve the LAN.
        let interfaces = [iface("br0", 4, HOME, HOME_MASK)];
        assert_eq!(pick(PEER, &interfaces, physical).unwrap().name, "br0");
    }

    #[test]
    fn a_physical_interface_wins_over_a_virtual_one_on_the_same_subnet() {
        let interfaces = [
            iface("tap0", 5, Ipv4Addr::new(192, 168, 1, 99), HOME_MASK),
            iface("enp42s0", 2, HOME, HOME_MASK),
        ];
        assert_eq!(pick(PEER, &interfaces, physical).unwrap().name, "enp42s0");
    }

    #[test]
    fn the_first_of_two_physical_interfaces_on_the_same_subnet_stays() {
        let interfaces = [
            iface("enp42s0", 2, HOME, HOME_MASK),
            iface("wlp4s0", 3, Ipv4Addr::new(192, 168, 1, 21), HOME_MASK),
        ];
        assert_eq!(pick(PEER, &interfaces, physical).unwrap().name, "enp42s0");
    }

    #[test]
    fn a_peer_outside_every_subnet_has_no_route() {
        let interfaces = [iface("wlan0", 2, HOME, HOME_MASK)];
        // A neighbouring subnet with the same prefix, then TEST-NET-3.
        assert_eq!(
            pick(Ipv4Addr::new(192, 168, 2, 33), &interfaces, physical),
            None
        );
        assert_eq!(
            pick(Ipv4Addr::new(203, 0, 113, 7), &interfaces, physical),
            None
        );
    }

    #[test]
    fn a_zero_netmask_never_covers_a_peer() {
        let interfaces = [iface("eth0", 2, HOME, EVERYTHING)];
        assert_eq!(pick(PEER, &interfaces, physical), None);
    }

    #[test]
    fn down_and_indexless_interfaces_are_skipped() {
        let mut down = iface("wlan0", 2, HOME, HOME_MASK);
        down.oper_status = IfOperStatus::Down;
        let mut indexless = iface("wlan1", 3, HOME, HOME_MASK);
        indexless.index = None;
        assert_eq!(pick(PEER, &[down, indexless], physical), None);
    }

    #[tokio::test]
    async fn loopback_peers_keep_the_plain_connect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        lan_connect(addr)
            .await
            .expect("a loopback peer needs no interface");
    }

    /// The option is set (read back from the socket) and the connect goes through it. The
    /// check runs as a normal user, so it also shows the option needs no privilege there.
    #[tokio::test]
    async fn a_socket_is_bound_to_its_device() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let socket = TcpSocket::new_v4().unwrap();
        bind_to_device(socket.as_fd(), "lo").expect("SO_BINDTODEVICE refused");
        assert_eq!(bound_device(socket.as_fd()), "lo");
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        socket.connect(addr).await.expect("connect over lo");
    }

    #[test]
    fn an_alias_label_is_not_a_device() {
        assert_eq!(device_name("eth0:1"), "eth0");
        assert_eq!(device_name("wlp4s0"), "wlp4s0");
    }

    fn bound_device(fd: BorrowedFd<'_>) -> String {
        let mut name = [0u8; libc::IFNAMSIZ];
        let mut len = name.len() as libc::socklen_t;
        // SAFETY: `fd` is a live socket; the buffer and its length are local and the kernel
        // writes at most `len` bytes into it.
        let outcome = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                name.as_mut_ptr().cast::<libc::c_void>(),
                &mut len,
            )
        };
        assert_eq!(outcome, 0, "{}", io::Error::last_os_error());
        let end = name.iter().position(|&b| b == 0).unwrap_or(len as usize);
        String::from_utf8_lossy(&name[..end]).into_owned()
    }
}
