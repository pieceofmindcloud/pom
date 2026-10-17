//! Select a free port set for a fresh node before the native UI or service starts.
//!
//! The persisted configuration remains authoritative after first boot. This
//! module only probes candidates when the operator left the default port(s)
//! implicit and there is no `pom.json` yet.

use anyhow::{anyhow, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{Ipv6Addr, SocketAddr, UdpSocket};

pub(crate) const DEFAULT_GATEWAY_PORT: u16 = 8080;
const INFERENCE_HTTP_OFFSET: u16 = crate::config::DEFAULT_INFERENCE_PORT - DEFAULT_GATEWAY_PORT;
const P2P_PORT_OFFSET: u16 = crate::p2p::P2P_PORT_OFFSET;
const MAX_GATEWAY_PORT: u16 = u16::MAX - INFERENCE_HTTP_OFFSET;
const MAX_P2P_PORT: u16 = u16::MAX - P2P_PORT_OFFSET;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InitialPorts {
    pub gateway: u16,
    pub p2p: u16,
}

/// Keep operator-provided values fixed and search only the defaults they left
/// implicit. The HTTP inference port follows the gateway by its normal offset;
/// the QUIC listeners, the p2p endpoint and its relay follow the P2P port.
pub(crate) fn select(
    gateway: u16,
    p2p: u16,
    gateway_is_default: bool,
    p2p_is_default: bool,
) -> Result<InitialPorts> {
    select_with_probes(
        gateway,
        p2p,
        gateway_is_default,
        p2p_is_default,
        gateway_ports_available,
        p2p_ports_available,
        complete_port_plan_available,
    )
    .ok_or_else(|| {
        anyhow!(
            "no free initial POM port set is available; choose --gateway-port/--port or set POM_GATEWAY_PORT/POM_PORT"
        )
    })
}

fn select_with_probes<G, P, C>(
    gateway: u16,
    p2p: u16,
    gateway_is_default: bool,
    p2p_is_default: bool,
    mut gateway_available: G,
    mut p2p_available: P,
    mut complete_plan_available: C,
) -> Option<InitialPorts>
where
    G: FnMut(u16) -> bool,
    P: FnMut(u16) -> bool,
    C: FnMut(u16, u16) -> bool,
{
    if !gateway_is_default && !p2p_is_default {
        return Some(InitialPorts { gateway, p2p });
    }

    let mut selected_gateway = next_candidate(
        gateway,
        gateway_is_default,
        MAX_GATEWAY_PORT,
        &mut gateway_available,
    )?;
    let mut selected_p2p = next_candidate(p2p, p2p_is_default, MAX_P2P_PORT, &mut p2p_available)?;
    let first_gateway = selected_gateway;

    loop {
        if complete_plan_available(selected_gateway, selected_p2p) {
            return Some(InitialPorts {
                gateway: selected_gateway,
                p2p: selected_p2p,
            });
        }

        // A TCP collision between the gateway/inference listeners and
        // the relay server's TCP listener is the only overlap between otherwise-free
        // individual port groups. Prefer moving the gateway; if that search
        // runs out, move P2P and retry from the first free gateway candidate.
        if gateway_is_default {
            if let Some(next) = next_candidate(
                selected_gateway.checked_add(1)?,
                true,
                MAX_GATEWAY_PORT,
                &mut gateway_available,
            ) {
                selected_gateway = next;
                continue;
            }
        }

        if p2p_is_default {
            selected_p2p = next_candidate(
                selected_p2p.checked_add(1)?,
                true,
                MAX_P2P_PORT,
                &mut p2p_available,
            )?;
            selected_gateway = first_gateway;
            continue;
        }

        return None;
    }
}

fn next_candidate<F>(start: u16, automatic: bool, max: u16, mut available: F) -> Option<u16>
where
    F: FnMut(u16) -> bool,
{
    if start == 0 || start > max {
        return None;
    }
    if !automatic {
        return available(start).then_some(start);
    }
    (start..=max).find(|port| available(*port))
}

fn inference_http_port(gateway: u16) -> Option<u16> {
    gateway.checked_add(INFERENCE_HTTP_OFFSET)
}

fn p2p_derived_ports(p2p: u16) -> Option<(u16, u16, u16)> {
    Some((
        crate::quic::inference_quic_port(p2p)?,
        crate::quic::gateway_quic_port(p2p)?,
        p2p.checked_add(P2P_PORT_OFFSET)?,
    ))
}

fn gateway_ports_available(gateway: u16) -> bool {
    let Some(inference) = inference_http_port(gateway) else {
        return false;
    };
    let Ok(gateway_listener) = crate::infer_server::listener_dual_stack(gateway) else {
        return false;
    };
    let Ok(inference_listener) = crate::infer_server::listener_dual_stack(inference) else {
        return false;
    };
    let _reservations = (gateway_listener, inference_listener);
    true
}

fn p2p_ports_available(p2p: u16) -> bool {
    let Some((inference_quic, gateway_quic, endpoint)) = p2p_derived_ports(p2p) else {
        return false;
    };
    let mut udp_reservations = Vec::with_capacity(4);
    for port in [p2p, inference_quic, gateway_quic, endpoint] {
        let Ok(socket) = bind_udp_dual_stack(port) else {
            return false;
        };
        udp_reservations.push(socket);
    }
    let Ok(relay_tcp) = crate::infer_server::listener_dual_stack(endpoint) else {
        return false;
    };
    let _reservations = (udp_reservations, relay_tcp);
    true
}

/// Probe all listeners together as well as individually, so collisions inside
/// the selected set and races between probes are caught before startup.
fn complete_port_plan_available(gateway: u16, p2p: u16) -> bool {
    let Some(inference_http) = inference_http_port(gateway) else {
        return false;
    };
    let Some((inference_quic, gateway_quic, endpoint)) = p2p_derived_ports(p2p) else {
        return false;
    };
    let tcp_ports = [gateway, inference_http, endpoint];
    let udp_ports = [p2p, inference_quic, gateway_quic, endpoint];
    if has_duplicate_ports(&tcp_ports) || has_duplicate_ports(&udp_ports) {
        return false;
    }

    let mut tcp_reservations = Vec::with_capacity(tcp_ports.len());
    for port in tcp_ports {
        let Ok(listener) = crate::infer_server::listener_dual_stack(port) else {
            return false;
        };
        tcp_reservations.push(listener);
    }

    let mut udp_reservations = Vec::with_capacity(udp_ports.len());
    for port in udp_ports {
        let Ok(socket) = bind_udp_dual_stack(port) else {
            return false;
        };
        udp_reservations.push(socket);
    }

    let _reservations = (tcp_reservations, udp_reservations);
    true
}

fn has_duplicate_ports(ports: &[u16]) -> bool {
    ports
        .iter()
        .enumerate()
        .any(|(index, port)| ports[index + 1..].contains(port))
}

fn bind_udp_dual_stack(port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(false)?;
    socket.set_broadcast(true)?;
    socket.bind(&SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port).into())?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn first_boot_skips_occupied_default_gateway_and_p2p_ports() {
        let selected = select_with_probes(
            DEFAULT_GATEWAY_PORT,
            crate::network::DISCOVERY_PORT,
            true,
            true,
            |port| port != DEFAULT_GATEWAY_PORT,
            |port| port != crate::network::DISCOVERY_PORT,
            |gateway, p2p| {
                gateway == DEFAULT_GATEWAY_PORT + 1 && p2p == crate::network::DISCOVERY_PORT + 1
            },
        )
        .expect("an adjacent free pair exists");

        assert_eq!(selected.gateway, DEFAULT_GATEWAY_PORT + 1);
        assert_eq!(selected.p2p, crate::network::DISCOVERY_PORT + 1);
    }

    #[test]
    fn first_boot_keeps_a_free_default_port_unchanged() {
        let selected = select_with_probes(
            DEFAULT_GATEWAY_PORT,
            crate::network::DISCOVERY_PORT,
            true,
            true,
            |_| true,
            |_| true,
            |gateway, p2p| gateway == DEFAULT_GATEWAY_PORT && p2p == crate::network::DISCOVERY_PORT,
        )
        .expect("the defaults are free");

        assert_eq!(
            selected,
            InitialPorts {
                gateway: DEFAULT_GATEWAY_PORT,
                p2p: crate::network::DISCOVERY_PORT,
            }
        );
    }

    #[test]
    fn explicitly_selected_ports_are_never_rewritten() {
        let selected = select_with_probes(
            9000,
            43000,
            false,
            false,
            |_| false,
            |_| false,
            |_, _| false,
        )
        .expect("explicit values are returned without probing");

        assert_eq!(
            selected,
            InitialPorts {
                gateway: 9000,
                p2p: 43000
            }
        );
    }

    #[test]
    fn gateway_and_inference_http_ports_must_both_be_free() {
        let busy = HashSet::from([DEFAULT_GATEWAY_PORT + INFERENCE_HTTP_OFFSET]);
        let selected = select_with_probes(
            DEFAULT_GATEWAY_PORT,
            crate::network::DISCOVERY_PORT,
            true,
            false,
            |port| {
                !busy.contains(&port)
                    && inference_http_port(port).is_some_and(|inference| !busy.contains(&inference))
            },
            |_| true,
            |gateway, _| {
                !busy.contains(&gateway)
                    && inference_http_port(gateway)
                        .is_some_and(|inference| !busy.contains(&inference))
            },
        )
        .expect("a gateway with a free derived inference port exists");

        assert_eq!(selected.gateway, DEFAULT_GATEWAY_PORT + 1);
    }
}
