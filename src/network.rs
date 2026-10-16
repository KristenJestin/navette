//! Whether the remote machine is on the desk's network, so that the agent never dials the desk's
//! address from another one (an office LAN, a train's Wi-Fi), where the same private address
//! may be anybody's. A subnet says nothing: the same private subnet exists on many networks.
//! The desk's network is known by the hardware address of its router, the remote's gateway on the
//! way to the desk, recorded at pairing.
//!
//! The answer comes from the kernel's routing and ARP tables, read from `/proc`: nothing is
//! sent, not even an ARP request.

use std::cmp::Reverse;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use crate::store::Store;

/// How often an agent away from the desk looks again. Reading two files sends nothing, and a
/// laptop back to the desk links within this.
pub const RECHECK: Duration = Duration::from_secs(10);

/// A hardware (MAC) address.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Mac([u8; 6]);

impl Mac {
    /// `aa:bb:cc:dd:ee:ff`, either case.
    pub fn parse(text: &str) -> Option<Self> {
        let mut bytes = [0u8; 6];
        let mut parts = text.trim().split(':');
        for byte in &mut bytes {
            let part = parts.next().filter(|part| part.len() == 2)?;
            *byte = u8::from_str_radix(part, 16).ok()?;
        }
        parts.next().is_none().then_some(Self(bytes))
    }
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

impl fmt::Debug for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mac({self})")
    }
}

/// The kernel's routing and ARP tables, as `/proc/net/route` and `/proc/net/arp` show them.
pub trait Tables {
    fn route(&self) -> io::Result<String>;
    fn arp(&self) -> io::Result<String>;
}

/// The real tables.
pub struct ProcNet;

impl Tables for ProcNet {
    fn route(&self) -> io::Result<String> {
        std::fs::read_to_string("/proc/net/route")
    }

    fn arp(&self) -> io::Result<String> {
        std::fs::read_to_string("/proc/net/arp")
    }
}

/// The hardware addresses of the routers on the way to the desk at `desk`, the preferred
/// route's first: the gateways of the routes through the interface whose directly connected
/// subnet holds the desk's address, as the ARP table knows them on it. None when no such subnet
/// is connected, whatever the default route: a tethered phone's, or a full-tunnel VPN's, says
/// nothing of the desk's network. Unreadable tables, or an IPv6 desk (`/proc/net/route` has
/// only IPv4), are no router.
pub fn routers(tables: &impl Tables, desk: IpAddr) -> Vec<Mac> {
    let IpAddr::V4(desk) = desk else {
        return Vec::new();
    };
    let (Ok(route), Ok(arp)) = (tables.route(), tables.arp()) else {
        return Vec::new();
    };
    let mut routes = routes(&route);
    let holds_desk = |route: &Route| {
        let mask = u32::from(route.mask);
        u32::from(desk) & mask == u32::from(route.destination) & mask
    };
    // The kernel's own choice among connected subnets: the narrowest, then the lowest metric.
    let Some(lan) = routes
        .iter()
        .filter(|route| route.flags & RTF_GATEWAY == 0 && holds_desk(route))
        .max_by_key(|route| (u32::from(route.mask).count_ones(), Reverse(route.metric)))
        .map(|route| route.device)
    else {
        return Vec::new();
    };
    routes.sort_by_key(|route| route.metric);
    let mut routers = Vec::new();
    // The default route's router, or under a full-tunnel VPN the one its server's host route
    // goes through: the same box.
    for route in routes
        .iter()
        .filter(|route| route.device == lan && route.flags & RTF_GATEWAY != 0)
    {
        if let Some(mac) = resolved(&arp, lan, route.gateway)
            && !routers.contains(&mac)
        {
            routers.push(mac);
        }
    }
    routers
}

/// `RTF_UP`: the route is in use.
const RTF_UP: u16 = 0x1;
/// `RTF_GATEWAY`: the route goes through a router; without it, the subnet is directly
/// connected.
const RTF_GATEWAY: u16 = 0x2;
/// `ATF_COM`: the ARP entry has a hardware address.
const ATF_COM: u16 = 0x2;

/// A line of `/proc/net/route`.
struct Route<'a> {
    device: &'a str,
    destination: Ipv4Addr,
    gateway: Ipv4Addr,
    flags: u16,
    metric: u32,
    mask: Ipv4Addr,
}

/// The routes in use, from `/proc/net/route`.
fn routes(text: &str) -> Vec<Route<'_>> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let &[device, destination, gateway, flags, _, _, metric, mask, ..] = fields.as_slice()
            else {
                return None;
            };
            // The kernel prints an address's four bytes as one number in its own byte order.
            let ip = |text: &str| {
                u32::from_str_radix(text, 16)
                    .ok()
                    .map(|n| Ipv4Addr::from(n.to_ne_bytes()))
            };
            let flags = u16::from_str_radix(flags, 16).ok()?;
            (flags & RTF_UP != 0).then_some(Route {
                device,
                destination: ip(destination)?,
                gateway: ip(gateway)?,
                flags,
                metric: metric.parse().ok()?,
                mask: ip(mask)?,
            })
        })
        .collect()
}

/// The hardware address `/proc/net/arp` gives `ip` on `device`, once resolved.
fn resolved(arp: &str, device: &str, ip: Ipv4Addr) -> Option<Mac> {
    arp.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let &[address, _, flags, hardware, _, on] = fields.as_slice() else {
            return None;
        };
        let flags = u16::from_str_radix(flags.trim_start_matches("0x"), 16).ok()?;
        (address.parse() == Ok(ip) && on == device && flags & ATF_COM != 0)
            .then(|| Mac::parse(hardware))
            .flatten()
            .filter(|mac| mac.0 != [0; 6])
    })
}

/// The agent's gate: it dials only on the desk's network.
pub struct HomeNetwork<T> {
    tables: T,
    store: Store,
    /// The desk's address, from the pairing.
    desk: Option<IpAddr>,
    /// The desk's gateway, as recorded in the pairing; none for a pairing older than this check.
    gateway: Option<Mac>,
}

impl<T: Tables> HomeNetwork<T> {
    /// The gate of the pairing in `store`.
    pub fn new(tables: T, store: Store) -> io::Result<Self> {
        let pairing = store.pairing()?;
        Ok(Self {
            tables,
            desk: pairing.as_ref().map(|pairing| pairing.address.ip()),
            gateway: pairing.and_then(|pairing| pairing.gateway),
            store,
        })
    }

    /// Whether to dial the desk: one of this machine's default gateways is the desk's. A pairing that
    /// does not know the desk's yet dials, as it did before.
    pub fn at_home(&self) -> bool {
        match self.gateway {
            // Any router on the way to the desk, not only the preferred route's.
            Some(home) => self
                .desk
                .is_some_and(|desk| routers(&self.tables, desk).contains(&home)),
            None => true,
        }
    }

    /// After a link has been up: a pairing without the desk's gateway learns it now, since this
    /// machine just reached the desk, and keeps it.
    pub fn linked(&mut self) -> io::Result<()> {
        if self.gateway.is_some() {
            return Ok(());
        }
        // Not known yet (no traffic through the router since it came up): a later link
        // learns it.
        let Some(gateway) = self
            .desk
            .and_then(|desk| routers(&self.tables, desk).first().copied())
        else {
            return Ok(());
        };
        let Some(mut pairing) = self.store.pairing()? else {
            return Ok(());
        };
        pairing.gateway = Some(gateway);
        self.store.save_pairing(&pairing).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("saving the desk's router in paired.conf: {e}"),
            )
        })?;
        self.gateway = Some(gateway);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::identity::Identity;
    use crate::store::{Pairing, Role};

    pub(crate) const HOME_ROUTER: &str = "02:00:00:00:00:01";
    pub(crate) const OTHER_ROUTER: &str = "02:00:00:00:00:99";

    const ROUTE_HEADER: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n";
    const ARP_HEADER: &str =
        "IP address       HW type     Flags       HW address            Mask     Device\n";

    /// Tables a test changes as the laptop moves.
    #[derive(Clone, Default)]
    pub(crate) struct FakeTables(Arc<Mutex<(String, String)>>);

    impl FakeTables {
        /// A laptop on 192.0.2.0/24, the desk's network or one like it, whose router
        /// (192.0.2.1, on `wlan0`) has `router`.
        pub(crate) fn behind(router: &str) -> Self {
            let tables = Self::default();
            tables.move_behind(router);
            tables
        }

        pub(crate) fn move_behind(&self, router: &str) {
            self.set(
                // As the kernel prints them: 192.0.2.0/24 on `wlan0`, the default route through
                // 192.0.2.1.
                &format!(
                    "{ROUTE_HEADER}wlan0\t00000000\t010200C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
                     wlan0\t000200C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n"
                ),
                &format!("{ARP_HEADER}192.0.2.1        0x1         0x2         {router}     *        wlan0\n"),
            );
        }

        pub(crate) fn set(&self, route: &str, arp: &str) {
            *self.0.lock().unwrap() = (route.into(), arp.into());
        }
    }

    impl Tables for FakeTables {
        fn route(&self) -> io::Result<String> {
            Ok(self.0.lock().unwrap().0.clone())
        }

        fn arp(&self) -> io::Result<String> {
            Ok(self.0.lock().unwrap().1.clone())
        }
    }

    struct Unreadable;

    impl Tables for Unreadable {
        fn route(&self) -> io::Result<String> {
            Err(io::ErrorKind::PermissionDenied.into())
        }

        fn arp(&self) -> io::Result<String> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }

    pub(crate) fn mac(text: &str) -> Mac {
        Mac::parse(text).unwrap()
    }

    /// A remote's store paired with the desk, which knows the desk's router as `gateway`.
    pub(crate) fn paired_remote(gateway: Option<&str>) -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        store
            .save_pairing(&Pairing {
                role: Role::Remote,
                peer: Identity::generate().unwrap().fingerprint(),
                address: "192.0.2.10:7420".parse().unwrap(),
                gateway: gateway.map(mac),
            })
            .unwrap();
        (tmp, store)
    }

    #[test]
    fn a_mac_reads_and_prints_back() {
        assert_eq!(mac("02:00:00:AB:cd:01").to_string(), "02:00:00:ab:cd:01");
        assert_eq!(Mac::parse("02:00:00:00:00"), None);
        assert_eq!(Mac::parse("02:00:00:00:00:01:02"), None);
        assert_eq!(Mac::parse("02:00:00:00:00:zz"), None);
        assert_eq!(Mac::parse("2:0:0:0:0:1"), None);
    }

    const DESK: &str = "192.0.2.10";

    /// One line of `/proc/net/route`, its addresses printed the kernel's way.
    fn route(device: &str, destination: &str, gateway: &str, mask: &str, metric: u32) -> String {
        let hex = |ip: &str| {
            format!(
                "{:08X}",
                u32::from_ne_bytes(ip.parse::<Ipv4Addr>().unwrap().octets())
            )
        };
        // RTF_UP, and RTF_GATEWAY through a router.
        let flags = if gateway == "0.0.0.0" { 1 } else { 3 };
        format!(
            "{device}\t{}\t{}\t{flags:04X}\t0\t0\t{metric}\t{}\t0\t0\t0\n",
            hex(destination),
            hex(gateway),
            hex(mask)
        )
    }

    /// One resolved line of `/proc/net/arp`.
    fn arp(ip: &str, hardware: &str, device: &str) -> String {
        format!("{ip:<16} 0x1         0x2         {hardware}     *        {device}\n")
    }

    fn tables(routes: &[String], arps: &[String]) -> FakeTables {
        let tables = FakeTables::default();
        tables.set(
            &format!("{ROUTE_HEADER}{}", routes.concat()),
            &format!("{ARP_HEADER}{}", arps.concat()),
        );
        tables
    }

    fn desk_routers(tables: &impl Tables) -> Vec<Mac> {
        routers(tables, DESK.parse().unwrap())
    }

    #[test]
    fn the_router_is_found_through_the_arp_table() {
        assert_eq!(
            desk_routers(&FakeTables::behind(HOME_ROUTER)),
            vec![mac(HOME_ROUTER)]
        );
    }

    #[test]
    fn a_desk_address_in_no_connected_subnet_is_away() {
        // The same router, but the desk's address is not on any network this laptop is on.
        let tables = tables(
            &[
                route("wlan0", "198.51.100.0", "0.0.0.0", "255.255.255.0", 600),
                route("wlan0", "0.0.0.0", "198.51.100.1", "0.0.0.0", 600),
            ],
            &[arp("198.51.100.1", HOME_ROUTER, "wlan0")],
        );
        assert_eq!(desk_routers(&tables), vec![]);
    }

    #[test]
    fn no_route_is_no_router() {
        assert_eq!(desk_routers(&tables(&[], &[])), vec![]);
    }

    #[test]
    fn a_router_missing_from_the_arp_table_is_no_router() {
        let tables = FakeTables::behind(HOME_ROUTER);
        tables.set(&tables.route().unwrap(), ARP_HEADER);
        assert_eq!(desk_routers(&tables), vec![]);
    }

    #[test]
    fn an_unresolved_arp_entry_is_no_router() {
        let tables = FakeTables::behind(HOME_ROUTER);
        tables.set(
            &tables.route().unwrap(),
            &format!("{ARP_HEADER}192.0.2.1        0x1         0x0         00:00:00:00:00:00     *        wlan0\n"),
        );
        assert_eq!(desk_routers(&tables), vec![]);
    }

    #[test]
    fn an_arp_entry_on_another_interface_is_not_the_router() {
        let tables = FakeTables::behind(HOME_ROUTER);
        tables.set(
            &tables.route().unwrap(),
            &format!("{ARP_HEADER}{}", arp("192.0.2.1", HOME_ROUTER, "eth0")),
        );
        assert_eq!(desk_routers(&tables), vec![]);
    }

    #[test]
    fn a_policy_routing_vpn_does_not_hide_the_router() {
        // The VPN's tunnel takes the default route with a lower metric, through no router; the
        // LAN's default route stays.
        let tables = tables(
            &[
                route("tun0", "0.0.0.0", "0.0.0.0", "0.0.0.0", 50),
                route("wlan0", "0.0.0.0", "192.0.2.1", "0.0.0.0", 600),
                route("wlan0", "192.0.2.0", "0.0.0.0", "255.255.255.0", 600),
            ],
            &[arp("192.0.2.1", HOME_ROUTER, "wlan0")],
        );
        assert_eq!(desk_routers(&tables), vec![mac(HOME_ROUTER)]);
    }

    #[test]
    fn a_full_tunnel_vpn_still_finds_the_router_on_the_way_to_the_desk() {
        // The VPN replaces the default route: only its server's host route goes through the
        // LAN's router, and the LAN stays directly connected.
        let tables = tables(
            &[
                route("tun0", "0.0.0.0", "10.8.0.1", "0.0.0.0", 0),
                route("tun0", "10.8.0.0", "0.0.0.0", "255.255.255.0", 0),
                route("wlan0", "203.0.113.5", "192.0.2.1", "255.255.255.255", 0),
                route("wlan0", "192.0.2.0", "0.0.0.0", "255.255.255.0", 600),
            ],
            &[arp("192.0.2.1", HOME_ROUTER, "wlan0")],
        );
        assert_eq!(desk_routers(&tables), vec![mac(HOME_ROUTER)]);
    }

    #[test]
    fn a_tethered_phone_with_a_lower_metric_is_not_the_router() {
        let tables = tables(
            &[
                route("usb0", "0.0.0.0", "198.51.100.1", "0.0.0.0", 100),
                route("usb0", "198.51.100.0", "0.0.0.0", "255.255.255.0", 100),
                route("wlan0", "0.0.0.0", "192.0.2.1", "0.0.0.0", 600),
                route("wlan0", "192.0.2.0", "0.0.0.0", "255.255.255.0", 600),
            ],
            &[
                arp("198.51.100.1", OTHER_ROUTER, "usb0"),
                arp("192.0.2.1", HOME_ROUTER, "wlan0"),
            ],
        );
        assert_eq!(desk_routers(&tables), vec![mac(HOME_ROUTER)]);
    }

    #[test]
    fn wi_fi_and_ethernet_reach_the_same_router() {
        let on = |device: &str| {
            tables(
                &[
                    route(device, "0.0.0.0", "192.0.2.1", "0.0.0.0", 100),
                    route(device, "192.0.2.0", "0.0.0.0", "255.255.255.0", 100),
                ],
                &[arp("192.0.2.1", HOME_ROUTER, device)],
            )
        };
        assert_eq!(desk_routers(&on("wlan0")), vec![mac(HOME_ROUTER)]);
        assert_eq!(desk_routers(&on("eth0")), vec![mac(HOME_ROUTER)]);
    }

    #[test]
    fn the_narrowest_connected_subnet_holding_the_desk_wins() {
        // A VPN's wide private subnet also holds the desk's address; the LAN's is narrower.
        let tables = tables(
            &[
                route("tun0", "192.0.0.0", "0.0.0.0", "255.0.0.0", 0),
                route("tun0", "0.0.0.0", "192.0.0.1", "0.0.0.0", 0),
                route("wlan0", "192.0.2.0", "0.0.0.0", "255.255.255.0", 600),
                route("wlan0", "0.0.0.0", "192.0.2.1", "0.0.0.0", 600),
            ],
            &[
                arp("192.0.0.1", OTHER_ROUTER, "tun0"),
                arp("192.0.2.1", HOME_ROUTER, "wlan0"),
            ],
        );
        assert_eq!(desk_routers(&tables), vec![mac(HOME_ROUTER)]);
    }

    #[test]
    fn an_ipv6_desk_is_no_router() {
        assert_eq!(
            routers(
                &FakeTables::behind(HOME_ROUTER),
                "2001:db8::10".parse().unwrap()
            ),
            vec![]
        );
    }

    #[test]
    fn unreadable_tables_are_no_router() {
        assert_eq!(desk_routers(&Unreadable), vec![]);
    }

    #[test]
    fn behind_a_tethered_phone_an_old_pairing_learns_the_lan_s_router() {
        let (_tmp, store) = paired_remote(None);
        let tables = tables(
            &[
                route("usb0", "0.0.0.0", "198.51.100.1", "0.0.0.0", 100),
                route("usb0", "198.51.100.0", "0.0.0.0", "255.255.255.0", 100),
                route("wlan0", "0.0.0.0", "192.0.2.1", "0.0.0.0", 600),
                route("wlan0", "192.0.2.0", "0.0.0.0", "255.255.255.0", 600),
            ],
            &[
                arp("198.51.100.1", OTHER_ROUTER, "usb0"),
                arp("192.0.2.1", HOME_ROUTER, "wlan0"),
            ],
        );
        let mut network = HomeNetwork::new(tables, store.clone()).unwrap();
        network.linked().unwrap();
        assert_eq!(
            store.pairing().unwrap().unwrap().gateway,
            Some(mac(HOME_ROUTER))
        );
    }

    #[test]
    fn a_failed_save_of_the_learned_router_names_paired_conf() {
        let (tmp, store) = paired_remote(None);
        // The atomic write's temporary file cannot be made.
        std::fs::create_dir(tmp.path().join(".paired.conf.tmp")).unwrap();
        let mut network = HomeNetwork::new(FakeTables::behind(HOME_ROUTER), store).unwrap();
        let e = network.linked().unwrap_err();
        assert!(e.to_string().contains("paired.conf"), "{e}");
    }

    #[test]
    fn the_matching_gateway_is_home() {
        let (_tmp, store) = paired_remote(Some(HOME_ROUTER));
        let network = HomeNetwork::new(FakeTables::behind(HOME_ROUTER), store).unwrap();
        assert!(network.at_home());
    }

    #[test]
    fn another_gateway_is_not_home() {
        let (_tmp, store) = paired_remote(Some(HOME_ROUTER));
        let network = HomeNetwork::new(FakeTables::behind(OTHER_ROUTER), store).unwrap();
        assert!(!network.at_home());
    }

    #[test]
    fn no_gateway_is_not_home() {
        let (_tmp, store) = paired_remote(Some(HOME_ROUTER));
        let network = HomeNetwork::new(FakeTables::default(), store).unwrap();
        assert!(!network.at_home());
    }

    #[test]
    fn an_old_pairing_dials_until_it_learns_home_s_gateway() {
        let (_tmp, store) = paired_remote(None);
        let network = HomeNetwork::new(FakeTables::behind(OTHER_ROUTER), store).unwrap();
        assert!(network.at_home());
    }

    #[test]
    fn an_old_pairing_learns_home_s_gateway_at_its_first_link_and_keeps_it() {
        let (_tmp, store) = paired_remote(None);
        let tables = FakeTables::behind(HOME_ROUTER);
        let mut network = HomeNetwork::new(tables.clone(), store.clone()).unwrap();
        network.linked().unwrap();
        assert_eq!(
            store.pairing().unwrap().unwrap().gateway,
            Some(mac(HOME_ROUTER))
        );
        tables.move_behind(OTHER_ROUTER);
        assert!(!network.at_home());
        // A restarted agent reads it back.
        assert!(!HomeNetwork::new(tables, store).unwrap().at_home());
    }

    #[test]
    fn a_link_never_replaces_the_recorded_gateway() {
        let (_tmp, store) = paired_remote(Some(HOME_ROUTER));
        let mut network =
            HomeNetwork::new(FakeTables::behind(OTHER_ROUTER), store.clone()).unwrap();
        network.linked().unwrap();
        assert_eq!(
            store.pairing().unwrap().unwrap().gateway,
            Some(mac(HOME_ROUTER))
        );
    }

    #[test]
    fn an_old_pairing_linked_without_a_known_gateway_learns_at_a_later_link() {
        let (_tmp, store) = paired_remote(None);
        let tables = FakeTables::default();
        let mut network = HomeNetwork::new(tables.clone(), store.clone()).unwrap();
        network.linked().unwrap();
        assert_eq!(store.pairing().unwrap().unwrap().gateway, None);
        assert!(network.at_home());
        tables.move_behind(HOME_ROUTER);
        network.linked().unwrap();
        assert_eq!(
            store.pairing().unwrap().unwrap().gateway,
            Some(mac(HOME_ROUTER))
        );
    }
}
