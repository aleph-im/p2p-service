use std::collections::HashMap;
use std::net::Ipv4Addr;

use libp2p::multiaddr::Protocol;
use libp2p::Multiaddr;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Reject,
}

/// Subnet a connection is counted against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Subnet {
    /// First three octets of an IPv4 address (/24).
    V4([u8; 3]),
    /// First three 16-bit segments of an IPv6 address (/48, the usual
    /// per-site allocation; a /64 is a single host network and any host can
    /// rotate through billions of them).
    V6([u16; 3]),
}

/// Caps the number of connections per subnet: IPv4 /24 or IPv6 /48
/// (IPv4-mapped IPv6 addresses count against their IPv4 /24). Exempt peers
/// (preferred, bootstrap) are counted but never rejected. Non-IP addresses
/// are always allowed.
/// Note: DNS-dialed connections (e.g. /dns/ bootstrap multiaddrs) carry
/// no ip4/ip6 component post-dial and are neither counted nor capped.
#[derive(Debug)]
pub struct SubnetLimits {
    cap: usize,
    counts: HashMap<Subnet, usize>,
}

impl SubnetLimits {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            counts: HashMap::new(),
        }
    }

    fn subnet(addr: &Multiaddr) -> Option<Subnet> {
        let v4 = |ip: Ipv4Addr| {
            let octets = ip.octets();
            Subnet::V4([octets[0], octets[1], octets[2]])
        };
        addr.iter().find_map(|p| match p {
            Protocol::Ip4(ip) => Some(v4(ip)),
            Protocol::Ip6(ip) => Some(match ip.to_ipv4_mapped() {
                Some(mapped) => v4(mapped),
                None => {
                    let segments = ip.segments();
                    Subnet::V6([segments[0], segments[1], segments[2]])
                }
            }),
            _ => None,
        })
    }

    pub fn on_connection(&mut self, addr: &Multiaddr, exempt: bool) -> Verdict {
        let Some(subnet) = Self::subnet(addr) else {
            return Verdict::Allow;
        };
        let count = self.counts.entry(subnet).or_insert(0);
        let verdict = if !exempt && *count >= self.cap {
            Verdict::Reject
        } else {
            Verdict::Allow
        };
        // Always increment: the connection exists until the disconnect fires,
        // so on_disconnection will be called even for rejected connections.
        *count += 1;
        verdict
    }

    pub fn on_disconnection(&mut self, addr: &Multiaddr) {
        if let Some(subnet) = Self::subnet(addr) {
            if let Some(count) = self.counts.get_mut(&subnet) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.counts.remove(&subnet);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip4_addr(a: u8, b: u8, c: u8, d: u8) -> Multiaddr {
        format!("/ip4/{}.{}.{}.{}/tcp/4025", a, b, c, d)
            .parse()
            .unwrap()
    }

    fn ip6_addr(s: &str) -> Multiaddr {
        format!("/ip6/{}/tcp/4025", s).parse().unwrap()
    }

    fn dns_addr() -> Multiaddr {
        "/dns/example.com/tcp/4025".parse().unwrap()
    }

    /// Test 1: connections below cap are allowed (cap 2, two different IPs in 10.0.0.0/24).
    #[test]
    fn below_cap_allowed() {
        let mut limits = SubnetLimits::new(2);
        let addr1 = ip4_addr(10, 0, 0, 1);
        let addr2 = ip4_addr(10, 0, 0, 2);
        assert_eq!(limits.on_connection(&addr1, false), Verdict::Allow);
        assert_eq!(limits.on_connection(&addr2, false), Verdict::Allow);
    }

    /// Test 2: third connection in same /24 is rejected (over cap).
    #[test]
    fn over_cap_rejected() {
        let mut limits = SubnetLimits::new(2);
        let addr1 = ip4_addr(10, 0, 0, 1);
        let addr2 = ip4_addr(10, 0, 0, 2);
        let addr3 = ip4_addr(10, 0, 0, 3);
        assert_eq!(limits.on_connection(&addr1, false), Verdict::Allow);
        assert_eq!(limits.on_connection(&addr2, false), Verdict::Allow);
        assert_eq!(limits.on_connection(&addr3, false), Verdict::Reject);
    }

    /// Test 3: different subnets have separate counts (cap 1).
    #[test]
    fn different_subnets_separate_counts() {
        let mut limits = SubnetLimits::new(1);
        let addr1 = ip4_addr(10, 0, 0, 1);
        let addr2 = ip4_addr(10, 0, 1, 1);
        assert_eq!(limits.on_connection(&addr1, false), Verdict::Allow);
        assert_eq!(limits.on_connection(&addr2, false), Verdict::Allow);
    }

    /// Test 4: exempt bypasses cap but still counts.
    /// cap 1: normal conn (allow, count=1), exempt conn (allow despite count>=cap),
    /// then a 3rd normal is rejected.
    #[test]
    fn exempt_bypasses_cap_but_still_counts() {
        let mut limits = SubnetLimits::new(1);
        let addr1 = ip4_addr(192, 168, 1, 1);
        let addr_exempt = ip4_addr(192, 168, 1, 2);
        let addr3 = ip4_addr(192, 168, 1, 3);
        assert_eq!(limits.on_connection(&addr1, false), Verdict::Allow);
        assert_eq!(limits.on_connection(&addr_exempt, true), Verdict::Allow);
        assert_eq!(limits.on_connection(&addr3, false), Verdict::Reject);
    }

    /// Test 5: disconnection frees a slot.
    #[test]
    fn disconnection_frees_slot() {
        let mut limits = SubnetLimits::new(1);
        let addr1 = ip4_addr(10, 1, 0, 1);
        let addr2 = ip4_addr(10, 1, 0, 2);
        assert_eq!(limits.on_connection(&addr1, false), Verdict::Allow);
        assert_eq!(limits.on_connection(&addr2, false), Verdict::Reject);
        // Disconnect addr2 (rejected but still counted).
        limits.on_disconnection(&addr2);
        // Now disconnect addr1 to free the real slot.
        limits.on_disconnection(&addr1);
        // A new connection should now be allowed.
        let addr3 = ip4_addr(10, 1, 0, 3);
        assert_eq!(limits.on_connection(&addr3, false), Verdict::Allow);
    }

    /// Test 6: non-IPv4 (DNS multiaddr) always allowed even with cap 0.
    #[test]
    fn non_ipv4_always_allowed() {
        let mut limits = SubnetLimits::new(0);
        let addr = dns_addr();
        assert_eq!(limits.on_connection(&addr, false), Verdict::Allow);
    }

    /// Test 7: increment-on-reject keeps counts balanced.
    /// cap 1: connect A (allow, count=1), connect B (reject, count=2),
    /// disconnect B (count=1), connect C must still be REJECTED because A holds the slot.
    #[test]
    fn reject_increments_count_keeps_balance() {
        let mut limits = SubnetLimits::new(1);
        let addr_a = ip4_addr(172, 16, 0, 1);
        let addr_b = ip4_addr(172, 16, 0, 2);
        let addr_c = ip4_addr(172, 16, 0, 3);

        assert_eq!(limits.on_connection(&addr_a, false), Verdict::Allow); // count=1
        assert_eq!(limits.on_connection(&addr_b, false), Verdict::Reject); // count=2 (rejected but counted)
        limits.on_disconnection(&addr_b); // count=1
                                          // A still holds the slot: count(1) >= cap(1), so C must be rejected.
        assert_eq!(limits.on_connection(&addr_c, false), Verdict::Reject);
    }

    /// IPv6: four peers in one /48 fill a cap of 4 (even from different /64s,
    /// since a /48 is one site), a fifth from the same /48 is rejected, and a
    /// peer from another /48 is allowed.
    #[test]
    fn ipv6_cap_is_per_48() {
        let mut limits = SubnetLimits::new(4);
        for addr in [
            "2001:db8:1:1::1",
            "2001:db8:1:2::1",
            "2001:db8:1:ffff::2",
            "2001:db8:1:1::3",
        ] {
            assert_eq!(
                limits.on_connection(&ip6_addr(addr), false),
                Verdict::Allow,
                "{addr} should be allowed below the cap"
            );
        }
        assert_eq!(
            limits.on_connection(&ip6_addr("2001:db8:1:42::9"), false),
            Verdict::Reject
        );
        assert_eq!(
            limits.on_connection(&ip6_addr("2001:db8:2::1"), false),
            Verdict::Allow
        );
    }

    /// IPv6: disconnection decrements the /48 count and frees a slot.
    #[test]
    fn ipv6_disconnection_frees_slot() {
        let mut limits = SubnetLimits::new(1);
        let addr1 = ip6_addr("2001:db8:5::1");
        let addr2 = ip6_addr("2001:db8:5::2");
        assert_eq!(limits.on_connection(&addr1, false), Verdict::Allow);
        assert_eq!(limits.on_connection(&addr2, false), Verdict::Reject);
        limits.on_disconnection(&addr2);
        // addr1 still holds the slot.
        assert_eq!(
            limits.on_connection(&ip6_addr("2001:db8:5::3"), false),
            Verdict::Reject
        );
        limits.on_disconnection(&ip6_addr("2001:db8:5::3"));
        limits.on_disconnection(&addr1);
        assert_eq!(
            limits.on_connection(&ip6_addr("2001:db8:5::4"), false),
            Verdict::Allow
        );
    }

    /// IPv6: exempt peers are counted but never rejected.
    #[test]
    fn ipv6_exempt_bypasses_cap_but_still_counts() {
        let mut limits = SubnetLimits::new(1);
        assert_eq!(
            limits.on_connection(&ip6_addr("2001:db8:7::1"), false),
            Verdict::Allow
        );
        assert_eq!(
            limits.on_connection(&ip6_addr("2001:db8:7::2"), true),
            Verdict::Allow
        );
        assert_eq!(
            limits.on_connection(&ip6_addr("2001:db8:7::3"), false),
            Verdict::Reject
        );
    }

    /// IPv4 and IPv6 subnets never share a counter, even when the leading
    /// bytes coincide.
    #[test]
    fn ipv4_and_ipv6_counted_separately() {
        let mut limits = SubnetLimits::new(1);
        assert_eq!(
            limits.on_connection(&ip4_addr(32, 1, 13, 1), false),
            Verdict::Allow
        );
        // 2001:0db8:... starts with bytes 32, 1, 13, 184.
        assert_eq!(
            limits.on_connection(&ip6_addr("2001:db8::1"), false),
            Verdict::Allow
        );
    }

    /// An IPv4-mapped IPv6 address is the IPv4 host: it shares the /24 counter
    /// so the cap cannot be sidestepped by writing the address as /ip6/.
    #[test]
    fn ipv4_mapped_ipv6_counts_as_ipv4() {
        let mut limits = SubnetLimits::new(1);
        assert_eq!(
            limits.on_connection(&ip4_addr(198, 51, 100, 1), false),
            Verdict::Allow
        );
        assert_eq!(
            limits.on_connection(&ip6_addr("::ffff:198.51.100.2"), false),
            Verdict::Reject
        );
    }
}
