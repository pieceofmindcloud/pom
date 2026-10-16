//! Addresses at which this machine can be found.
//!
//! Kept apart from `route.rs` because it is the only place with per-platform
//! code. The filtering, which is the part that fails silently, lives outside
//! the `cfg` and has a test that runs on any system.

/// Cap per family. See the test `at_most_two_candidates_per_family_survive`.
const MAX_PER_FAMILY: usize = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct LocalAddr {
    pub ip: String,
    /// Interface that owns the address, when the operating system reports it.
    /// Synthetic callers may leave it empty.
    pub interface: Option<String>,
    /// Address the system marked as end-of-life. macOS keeps these around
    /// for a while after rotating the privacy IPv6.
    pub deprecated: bool,
    /// Privacy IPv6, which rotates on its own. Usable, but loses to the `secured` one.
    pub temporary: bool,
}

fn is_virtual_interface(interface: Option<&str>) -> bool {
    let Some(interface) = interface else {
        return false;
    };
    let interface = interface.split('@').next().unwrap_or(interface);
    interface == "docker0"
        || interface.starts_with("br-")
        || interface.starts_with("docker")
        || interface.starts_with("veth")
        || interface.starts_with("virbr")
        || interface.starts_with("podman")
        || interface.starts_with("cni")
        || interface.starts_with("flannel")
        || interface.starts_with("cali")
        || interface.starts_with("weave")
}

fn e_ipv6(ip: &str) -> bool {
    ip.contains(':')
}

fn without_brackets(ip: &str) -> &str {
    ip.trim_start_matches('[').trim_end_matches(']')
}

fn disposable(ip: &str) -> bool {
    ip == "127.0.0.1"
        || ip == "::1"
        || ip.starts_with("fe80:")
        || ip.starts_with("169.254.")
        || ip.starts_with("0.0.0.0")
}

/// Whether `ip` could never be dialed from outside this machine's own
/// network — everything `disposable` already excludes (loopback,
/// link-local, the `0.0.0.0` sentinel), plus RFC 1918 (`10/8`, `172.16/12`,
/// `192.168/16`), RFC 6598 shared address space (`100.64/10`) and IPv6
/// unique-local (`fc00::/7`).
///
/// Used to decide whether a peer-observed address (`Network::set_observed_addr`,
/// from libp2p `identify`) is real new information about this node's public
/// IP, or just the peer telling us about a private range we already know
/// isn't reachable from the internet — e.g. a peer on the same LAN, or a NAT
/// that rewrote to another private range instead of a routable one.
///
/// **`100.64/10` is not an optional extra.** `Ipv4Addr::is_private` answers
/// RFC 1918 only, and RFC 6598 is what a CGNAT hands out - and what Tailscale
/// hands out too. Measured on this MacBook on 2026-08-16: the very first
/// `identify` answer came from a peer reached over the tailnet and reported
/// `100.122.132.97`, which the node then adopted as its *public* address and
/// announced to the whole cluster. A tailnet address proves the peer is on the
/// same tailnet, exactly the way a `192.168.` address proves it is on the same
/// LAN; a peer outside it cannot dial either.
pub fn is_private_or_local(ip: &str) -> bool {
    if disposable(ip) {
        return true;
    }
    if let Ok(v4) = ip.parse::<std::net::Ipv4Addr>() {
        let [a, b, ..] = v4.octets();
        // RFC 6598 shared address space, 100.64.0.0/10.
        let carrier_grade_nat = a == 100 && (64..128).contains(&b);
        return v4.is_private()
            || v4.is_loopback()
            || v4.is_link_local()
            // 0.0.0.0/8, "this network" - `is_unspecified` is only 0.0.0.0.
            || a == 0
            || v4.is_multicast()
            || v4.is_broadcast()
            || carrier_grade_nat;
    }
    let bare = ip.trim_start_matches('[').trim_end_matches(']');
    if let Ok(v6) = bare.parse::<std::net::Ipv6Addr>() {
        let first = v6.segments()[0];
        // fc00::/7: top 7 bits are 1111 110. fe80::/10: top 10 are 1111 1110 10.
        return (first & 0xfe00) == 0xfc00
            || (first & 0xffc0) == 0xfe80
            || v6.is_loopback()
            || v6.is_unspecified()
            || v6.is_multicast();
    }
    false
}

/// Applies the spec's rules: drops what is useless, prefers the stable one, and
/// limits how many candidates each family contributes.
pub fn filter_candidates(raw: Vec<LocalAddr>, pinned: Option<&str>) -> Vec<String> {
    let mut useful: Vec<LocalAddr> = raw
        .into_iter()
        .filter(|a| {
            !a.deprecated && !disposable(&a.ip) && !is_virtual_interface(a.interface.as_deref())
        })
        .collect();
    // Stable before temporary, with lexicographic order as the tiebreaker: the
    // announced set must not change just because the enumeration came in another order.
    useful.sort_by(|a, b| a.temporary.cmp(&b.temporary).then_with(|| a.ip.cmp(&b.ip)));

    let mut output: Vec<String> = Vec::new();
    if let Some(f) = pinned {
        // Does not count toward the cap: it is the operator's explicit intent.
        output.push(f.to_string());
    }
    let (mut v4, mut v6) = (0usize, 0usize);
    for a in useful {
        // Compared without brackets on both sides: `pinned` comes from
        // `effective_host`, which brackets the IPv6, and `a.ip` is the raw address.
        // Comparing the two forms never matched, and the pinned address went out
        // duplicated in the announcement - in a datagram whose budget is the MTU.
        if output
            .iter()
            .any(|s| without_brackets(s) == without_brackets(&a.ip))
        {
            continue;
        }
        let count = if e_ipv6(&a.ip) { &mut v6 } else { &mut v4 };
        if *count >= MAX_PER_FAMILY {
            continue;
        }
        *count += 1;
        output.push(a.ip);
    }
    output
}

/// Picks the initial address the wizard suggests for peers.
///
/// The UDP route may pick the temporary privacy IPv6 on macOS. The interface
/// enumeration knows that distinction, so the suggestion must go through the
/// same filtering used by the announcement and apply the product preference:
/// stable global IPv6, then any usable IPv4. The operator can still edit the
/// value, and the node's announcement keeps re-detecting the current
/// addresses.
pub fn preferred_advertise_ip(raw: Vec<LocalAddr>) -> Option<String> {
    let candidates = filter_candidates(raw.clone(), None);
    let stable = |ip: &str| {
        raw.iter().any(|source| {
            !source.deprecated
                && !source.temporary
                && without_brackets(&source.ip) == without_brackets(ip)
        })
    };
    candidates
        .iter()
        .find(|ip| {
            // `filter_candidates` orders stable addresses first but returns
            // strings, so retain the explicit stability check here instead
            // of treating that order as metadata.
            stable(ip)
                && ip
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::Ipv6Addr>()
                    .is_ok_and(|value| !is_private_or_local(&value.to_string()))
        })
        .cloned()
        .or_else(|| {
            candidates
                .into_iter()
                .find(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
        })
}

/// Returns the preference computed for this machine's current interfaces.
pub fn preferred_local_advertise_ip() -> Option<String> {
    preferred_advertise_ip(local_addrs())
}

/// Addresses of this machine's interfaces.
///
/// No new dependency: reads the output of `ifconfig` (macOS) or `ip -o addr`
/// (Linux). Windows gets an empty list for now and falls back to the address
/// observed by the peer - which is exactly today's behavior, not a regression.
pub fn local_addrs() -> Vec<LocalAddr> {
    #[cfg(target_os = "macos")]
    {
        // macOS keeps system administration tools under /sbin, which is not
        // guaranteed to be present in a GUI-launched process's PATH.
        parse_ifconfig(&run("/sbin/ifconfig", &[]))
    }
    #[cfg(target_os = "linux")]
    {
        parse_ip_addr(&run("ip", &["-o", "addr"]))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run(program: &str, args: &[&str]) -> String {
    match std::process::Command::new(program).args(args).output() {
        Ok(o) => String::from_utf8(o.stdout).unwrap_or_default(),
        Err(error) => {
            // Without the program, the node knows no address of its own and
            // announces no candidate at all - not even the public IPv6. LAN peers
            // keep the packet's private source and relay it in the `PeerList`
            // to remote peers, who dial an IP they cannot reach. It happened in the
            // Docker image without `iproute2`. Called on every announcement: warns once.
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    "could not run `{program}` to list the interfaces ({error}); this node will not announce its own addresses"
                );
            }
            String::new()
        }
    }
}

#[cfg(target_os = "macos")]
fn parse_ifconfig(output: &str) -> Vec<LocalAddr> {
    let mut out = Vec::new();
    let mut interface = None;
    for line in output.lines() {
        if !line.starts_with(char::is_whitespace) {
            interface = line.split(':').next().map(str::trim).map(str::to_string);
        }
        let l = line.trim();
        let fields: Vec<&str> = l.split_whitespace().collect();
        if fields.len() < 2 || (fields[0] != "inet" && fields[0] != "inet6") {
            continue;
        }
        // "inet6 fe80::1%en0" - the scope is not part of the address.
        let ip = fields[1].split('%').next().unwrap_or(fields[1]).to_string();
        out.push(LocalAddr {
            ip,
            interface: interface.clone(),
            deprecated: l.contains("deprecated"),
            temporary: l.contains("temporary"),
        });
    }
    out
}

#[cfg(target_os = "linux")]
fn parse_ip_addr(output: &str) -> Vec<LocalAddr> {
    let mut out = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(pos) = fields.iter().position(|c| *c == "inet" || *c == "inet6") else {
            continue;
        };
        let Some(cidr) = fields.get(pos + 1) else {
            continue;
        };
        let ip = cidr.split('/').next().unwrap_or(cidr).to_string();
        let interface = fields
            .get(1)
            .and_then(|name| name.split('@').next())
            .map(str::to_string);
        out.push(LocalAddr {
            ip,
            interface,
            deprecated: line.contains("deprecated"),
            temporary: line.contains("temporary"),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(ip: &str) -> LocalAddr {
        LocalAddr {
            ip: ip.into(),
            interface: None,
            deprecated: false,
            temporary: false,
        }
    }

    fn on(ip: &str, interface: &str) -> LocalAddr {
        LocalAddr {
            ip: ip.into(),
            interface: Some(interface.into()),
            deprecated: false,
            temporary: false,
        }
    }

    #[test]
    fn deprecated_and_link_local_and_loopback_are_dropped() {
        let raw = vec![
            a("192.168.100.240"),
            LocalAddr {
                ip: "2804:d55::9a0d".into(),
                interface: None,
                deprecated: true,
                temporary: true,
            },
            a("fe80::1"),
            a("127.0.0.1"),
            a("::1"),
        ];
        assert_eq!(filter_candidates(raw, None), vec!["192.168.100.240"]);
    }

    /// macOS creates several IPv6 addresses on its own - five on this machine on 2026-08-13.
    /// PeerList grows ~530 bytes per peer and is already sliced by MTU, so the cap
    /// is not cosmetic: without it, discovery turns into fragmentation.
    #[test]
    fn at_most_two_candidates_per_family_survive() {
        let raw = vec![
            a("192.168.1.1"),
            a("192.168.1.2"),
            a("192.168.1.3"),
            a("2804:d55::1"),
            a("2804:d55::2"),
            a("2804:d55::3"),
        ];
        let output = filter_candidates(raw, None);
        assert_eq!(output.iter().filter(|s| s.contains(':')).count(), 2);
        assert_eq!(output.iter().filter(|s| !s.contains(':')).count(), 2);
    }

    /// The address pinned by the operator is explicit intent: it comes first and
    /// does not count toward the cap, otherwise the filtering could drop it.
    #[test]
    fn a_pinned_address_comes_first_and_is_never_dropped() {
        let raw = vec![a("192.168.1.1"), a("192.168.1.2"), a("192.168.1.3")];
        let output = filter_candidates(raw, Some("2804:d55::99"));
        assert_eq!(output[0], "2804:d55::99");
        assert!(output.contains(&"192.168.1.1".to_string()));
    }

    #[test]
    fn a_secured_address_wins_over_a_temporary_one() {
        let raw = vec![
            LocalAddr {
                ip: "2804:d55::temp1".into(),
                interface: None,
                deprecated: false,
                temporary: true,
            },
            LocalAddr {
                ip: "2804:d55::sec".into(),
                interface: None,
                deprecated: false,
                temporary: false,
            },
            LocalAddr {
                ip: "2804:d55::temp2".into(),
                interface: None,
                deprecated: false,
                temporary: true,
            },
        ];
        assert_eq!(filter_candidates(raw, None)[0], "2804:d55::sec");
    }

    #[test]
    fn preferred_address_chooses_stable_global_ipv6_before_ipv4() {
        let raw = vec![
            LocalAddr {
                ip: "2804:d55::100".into(),
                interface: Some("en0".into()),
                deprecated: false,
                temporary: true,
            },
            LocalAddr {
                ip: "192.168.1.8".into(),
                interface: Some("en0".into()),
                deprecated: false,
                temporary: false,
            },
            LocalAddr {
                ip: "2804:d55::200".into(),
                interface: Some("en0".into()),
                deprecated: false,
                temporary: false,
            },
        ];
        assert_eq!(
            preferred_advertise_ip(raw),
            Some("2804:d55::200".to_string())
        );
    }

    #[test]
    fn preferred_address_falls_back_to_ipv4_when_ipv6_is_private_or_temporary() {
        let raw = vec![
            LocalAddr {
                ip: "2804:d55::100".into(),
                interface: Some("en0".into()),
                deprecated: false,
                temporary: true,
            },
            LocalAddr {
                ip: "fd12:3456::1".into(),
                interface: Some("en0".into()),
                deprecated: false,
                temporary: false,
            },
            LocalAddr {
                ip: "192.168.1.8".into(),
                interface: Some("en0".into()),
                deprecated: false,
                temporary: false,
            },
        ];
        assert_eq!(preferred_advertise_ip(raw), Some("192.168.1.8".to_string()));
    }

    #[test]
    fn duplicates_are_collapsed() {
        let raw = vec![a("192.168.1.1"), a("192.168.1.1")];
        assert_eq!(filter_candidates(raw, None).len(), 1);
    }

    /// The pinned address arrives bracketed (it comes from `effective_host`) and the
    /// local one comes raw. Measured on the cluster on 2026-08-16: the node's
    /// announcement went out with the same IPv6 twice, in a datagram whose cap is the MTU.
    #[test]
    fn a_pinned_ipv6_is_not_repeated_by_the_local_enumeration() {
        let output = filter_candidates(
            vec![a("2804:d55::9a0d"), a("192.168.1.1")],
            Some("[2804:d55::9a0d]"),
        );
        assert_eq!(output[0], "[2804:d55::9a0d]");
        assert_eq!(
            output
                .iter()
                .filter(|s| s.contains("2804:d55::9a0d"))
                .count(),
            1,
            "{output:?}"
        );
    }

    #[test]
    fn rfc1918_and_ula_ranges_are_private() {
        for ip in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "fc00::1",
            "fd12:3456::1",
            "[fd12:3456::1]",
        ] {
            assert!(is_private_or_local(ip), "{ip} should be private");
        }
    }

    #[test]
    fn a_routable_address_is_not_private() {
        for ip in ["1.2.3.4", "172.32.0.1", "8.8.8.8", "2804:d55::1"] {
            assert!(!is_private_or_local(ip), "{ip} should be routable");
        }
    }

    /// The CGNAT range (RFC 6598) is the one Tailscale uses. Measured on
    /// 2026-08-16 on this machine: the first `identify` came from a peer
    /// reached over the tailnet and reported `100.122.132.97`, which the node
    /// adopted as its *public* address. `Ipv4Addr::is_private` answers only RFC 1918
    /// and lets this range through.
    #[test]
    fn carrier_grade_nat_is_not_a_public_address() {
        for ip in ["100.64.0.1", "100.122.132.97", "100.127.255.255"] {
            assert!(is_private_or_local(ip), "{ip} e CGNAT/tailnet");
        }
        // The edges just outside the /10 stay public.
        for ip in ["100.63.255.255", "100.128.0.1"] {
            assert!(!is_private_or_local(ip), "{ip} is outside 100.64/10");
        }
    }

    #[test]
    fn docker_bridge_addresses_are_not_announced_but_lan_addresses_are() {
        let found = filter_candidates(
            vec![on("172.17.0.1", "docker0"), on("192.168.100.138", "eth0")],
            None,
        );

        assert_eq!(found, vec!["192.168.100.138"]);
    }

    /// Loopback and link-local are ranges, not just the two addresses that
    /// `disposable` names.
    #[test]
    fn the_whole_loopback_and_link_local_ranges_are_local() {
        for ip in [
            "127.0.0.2",
            "127.255.255.254",
            "0.0.0.1",
            "169.254.10.1",
            "224.0.0.1",
            "255.255.255.255",
            "febf::1",
            "::",
            "ff02::1",
        ] {
            assert!(is_private_or_local(ip), "{ip} is not a public address");
        }
    }

    #[test]
    fn already_discarded_ranges_stay_private() {
        for ip in ["127.0.0.1", "::1", "fe80::1", "169.254.1.1", "0.0.0.0"] {
            assert!(is_private_or_local(ip));
        }
    }

    /// The development machine has real addresses, and the filtering must
    /// never return anything undialable from them.
    #[test]
    fn the_real_machine_yields_only_usable_addresses() {
        let found = filter_candidates(local_addrs(), None);
        // Without this line the test would pass vacuously on a machine where the
        // enumeration did not work, which is exactly what it should catch.
        assert!(!found.is_empty(), "the enumeration found no address at all");
        assert!(found.len() <= 4, "cap of 2 per family: {found:?}");
        for ip in found {
            assert!(!disposable(&ip), "{ip} should not have passed");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn docker_bridge_addresses_are_not_announced() {
        let found = filter_candidates(
            parse_ip_addr(
                r#"2: docker0: <BROADCAST,UP> mtu 1500 state UP
    2: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0
    3: eth0: <BROADCAST,UP> mtu 1500 state UP
    3: eth0    inet 192.168.100.138/24 brd 192.168.100.255 scope global eth0"#,
            ),
            None,
        );

        assert_eq!(found, vec!["192.168.100.138"]);
    }
}

/// Local IP this machine uses to reach the internet: a UDP socket "connected"
/// to `8.8.8.8:80` picks the route without sending anything. Behind NAT it is a
/// private IP; it is only a last resort for the node to dial itself, and it is
/// never published as an identity.
pub fn outbound_local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}
