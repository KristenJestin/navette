//! Whether the remote machine is on home's network, so that the agent never dials home's
//! address from another one (an office LAN, a train's Wi-Fi), where the same private address
//! may be anybody's. A subnet says nothing: the same private subnet exists on many networks.
//! Home's network is known by the hardware address of its router, the remote's default gateway,
//! recorded at pairing.
//!
//! The answer comes from the kernel's routing and ARP tables, read from `/proc`: nothing is
//! sent, not even an ARP request.

use std::fmt;
use std::io;
use std::net::Ipv4Addr;
use std::time::Duration;

use crate::store::Store;

/// How often an agent away from home looks again. Reading two files sends nothing, and a
/// laptop back home links within this.
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

/// The hardware addresses of this machine's default gateways that the ARP table knows, the
/// preferred route's first. Unreadable tables are no gateway.
pub fn gateways(tables: &impl Tables) -> Vec<Mac> {
    let (Ok(route), Ok(arp)) = (tables.route(), tables.arp()) else {
        return Vec::new();
    };
    let mut routes = default_gateways(&route);
    routes.sort_by_key(|&(_, _, metric)| metric);
    routes
        .into_iter()
        .filter_map(|(device, ip, _)| resolved(&arp, device, ip))
        .collect()
}

/// `RTF_GATEWAY`: the route goes through a router. A tunnel's default route has none.
const RTF_GATEWAY: u16 = 0x2;
/// `ATF_COM`: the ARP entry has a hardware address.
const ATF_COM: u16 = 0x2;

/// The default routes through a router, from `/proc/net/route`: (device, gateway, metric).
fn default_gateways(route: &str) -> Vec<(&str, Ipv4Addr, u32)> {
    route
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let &[device, destination, gateway, flags, _, _, metric, mask, ..] = fields.as_slice()
            else {
                return None;
            };
            let hex = |text: &str| u32::from_str_radix(text, 16).ok();
            let flags = u16::from_str_radix(flags, 16).ok()?;
            (hex(destination)? == 0 && hex(mask)? == 0 && flags & RTF_GATEWAY != 0).then_some(())?;
            // The kernel prints the address's four bytes as one number in its own byte order.
            let gateway = Ipv4Addr::from(hex(gateway)?.to_ne_bytes());
            Some((device, gateway, metric.parse().ok()?))
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

/// The agent's gate: it dials only on home's network.
pub struct HomeNetwork<T> {
    tables: T,
    store: Store,
    /// Home's gateway, as recorded in the pairing; none for a pairing older than this check.
    gateway: Option<Mac>,
}

impl<T: Tables> HomeNetwork<T> {
    /// The gate of the pairing in `store`.
    pub fn new(tables: T, store: Store) -> io::Result<Self> {
        let gateway = store.pairing()?.and_then(|pairing| pairing.gateway);
        Ok(Self {
            tables,
            store,
            gateway,
        })
    }

    /// Whether to dial home: one of this machine's default gateways is home's. A pairing that
    /// does not know home's yet dials, as it did before.
    pub fn at_home(&self) -> bool {
        match self.gateway {
            // Any default route's, not only the preferred one: a VPN's tunnel takes that.
            Some(home) => gateways(&self.tables).contains(&home),
            None => true,
        }
    }

    /// After a link has been up: a pairing without home's gateway learns it now, since this
    /// machine just reached home, and keeps it.
    pub fn linked(&mut self) -> io::Result<()> {
        if self.gateway.is_some() {
            return Ok(());
        }
        // Not known yet (no traffic through the router since it came up): a later link
        // learns it.
        let Some(&gateway) = gateways(&self.tables).first() else {
            return Ok(());
        };
        let Some(mut pairing) = self.store.pairing()? else {
            return Ok(());
        };
        pairing.gateway = Some(gateway);
        self.store.save_pairing(&pairing)?;
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
        /// A laptop on a network whose router (192.0.2.1, on `wlan0`) has `router`.
        pub(crate) fn behind(router: &str) -> Self {
            let tables = Self::default();
            tables.move_behind(router);
            tables
        }

        pub(crate) fn move_behind(&self, router: &str) {
            self.set(
                &format!("{ROUTE_HEADER}wlan0\t00000000\t010200C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n"),
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

    /// A remote's store paired with home, which knows home's router as `gateway`.
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

    #[test]
    fn the_default_gateway_is_found_through_the_arp_table() {
        assert_eq!(
            gateways(&FakeTables::behind(HOME_ROUTER)),
            vec![mac(HOME_ROUTER)]
        );
    }

    #[test]
    fn no_default_route_is_no_gateway() {
        let tables = FakeTables::default();
        tables.set(
            &format!("{ROUTE_HEADER}wlan0\t000200C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n"),
            &format!("{ARP_HEADER}192.0.2.1        0x1         0x2         {HOME_ROUTER}     *        wlan0\n"),
        );
        assert_eq!(gateways(&tables), vec![]);
    }

    #[test]
    fn a_gateway_missing_from_the_arp_table_is_no_gateway() {
        let tables = FakeTables::default();
        tables.set(
            &format!(
                "{ROUTE_HEADER}wlan0\t00000000\t010200C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n"
            ),
            ARP_HEADER,
        );
        assert_eq!(gateways(&tables), vec![]);
    }

    #[test]
    fn an_unresolved_arp_entry_is_no_gateway() {
        let tables = FakeTables::default();
        tables.set(
            &format!("{ROUTE_HEADER}wlan0\t00000000\t010200C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n"),
            &format!("{ARP_HEADER}192.0.2.1        0x1         0x0         00:00:00:00:00:00     *        wlan0\n"),
        );
        assert_eq!(gateways(&tables), vec![]);
    }

    #[test]
    fn an_arp_entry_on_another_interface_is_not_the_gateway() {
        let tables = FakeTables::default();
        tables.set(
            &format!("{ROUTE_HEADER}wlan0\t00000000\t010200C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n"),
            &format!("{ARP_HEADER}192.0.2.1        0x1         0x2         {HOME_ROUTER}     *        eth0\n"),
        );
        assert_eq!(gateways(&tables), vec![]);
    }

    #[test]
    fn a_vpn_default_route_does_not_hide_the_router() {
        // A corporate VPN's tunnel takes the default route with a lower metric, and has no
        // gateway: the router is still the gateway of the other default route.
        let tables = FakeTables::default();
        tables.set(
            &format!(
                "{ROUTE_HEADER}tun0\t00000000\t00000000\t0001\t0\t0\t50\t00000000\t0\t0\t0\n\
                 wlan0\t00000000\t010200C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n"
            ),
            &format!("{ARP_HEADER}192.0.2.1        0x1         0x2         {HOME_ROUTER}     *        wlan0\n"),
        );
        assert_eq!(gateways(&tables), vec![mac(HOME_ROUTER)]);
    }

    #[test]
    fn the_preferred_default_route_s_gateway_comes_first() {
        let tables = FakeTables::default();
        tables.set(
            &format!(
                "{ROUTE_HEADER}wlan0\t00000000\t010200C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
                 eth0\t00000000\t016433C6\t0003\t0\t0\t100\t00000000\t0\t0\t0\n"
            ),
            &format!(
                "{ARP_HEADER}192.0.2.1        0x1         0x2         {OTHER_ROUTER}     *        wlan0\n\
                 198.51.100.1     0x1         0x2         {HOME_ROUTER}     *        eth0\n"
            ),
        );
        assert_eq!(gateways(&tables), vec![mac(HOME_ROUTER), mac(OTHER_ROUTER)]);
    }

    #[test]
    fn unreadable_tables_are_no_gateway() {
        assert_eq!(gateways(&Unreadable), vec![]);
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
