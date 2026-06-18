use std::collections::HashMap;

use libp2p::multiaddr::Protocol;
use libp2p::Multiaddr;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Reject,
}

/// Caps the number of connections per IPv4 /24 subnet. Exempt peers
/// (preferred, bootstrap) are counted but never rejected. Non-IPv4
/// addresses are always allowed (the service only listens on IPv4).
/// Note: DNS-dialed connections (e.g. /dns/ bootstrap multiaddrs) carry
/// no ip4 component post-dial and are neither counted nor capped.
#[derive(Debug)]
pub struct SubnetLimits {
    cap: usize,
    counts: HashMap<[u8; 3], usize>,
}

impl SubnetLimits {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            counts: HashMap::new(),
        }
    }

    fn subnet(addr: &Multiaddr) -> Option<[u8; 3]> {
        addr.iter().find_map(|p| match p {
            Protocol::Ip4(ip) => {
                let octets = ip.octets();
                Some([octets[0], octets[1], octets[2]])
            }
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
}
