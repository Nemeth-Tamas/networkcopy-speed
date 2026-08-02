use crate::console_progress::ProgressCounter;
use crate::direct_address::{self, DIRECT_TRANSFER_PORT};
use crate::direct_discovery_v4;
use crate::direct_link;
use crate::direct_route;
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::process;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const DISCOVERY_PORT: u16 = 7336;

const DISCOVERY_MAGIC: [u8; 4] = *b"NCD1";
const DISCOVERY_VERSION: u8 = 1;
const DISCOVERY_PACKET_BYTES: usize = 20;

const DISCOVERY_ATTEMPTS: usize = 6;
const DISCOVERY_REPLY_TIMEOUT: Duration = Duration::from_millis(500);

const DISCOVERY_POLL_TIMEOUT: Duration = Duration::from_millis(100);

const DISCOVERY_GROUP: Ipv6Addr = Ipv6Addr::new(0xFF12, 0, 0, 0, 0, 0, 0x4E43, 0x5350);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiscoveredPath {
    pub(crate) interface_index: u32,
    pub(crate) local_endpoint: SocketAddr,
    pub(crate) endpoint: SocketAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReceivedPath {
    pub(crate) interface_index: u32,
    pub(crate) local_endpoint: SocketAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiscoveryKind {
    Probe = 1,
    Offer = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiscoveryPacket {
    pub(crate) kind: DiscoveryKind,
    pub(crate) nonce: u64,
    pub(crate) transfer_port: u16,
}

impl DiscoveryPacket {
    pub(crate) fn probe(nonce: u64) -> Self {
        Self {
            kind: DiscoveryKind::Probe,
            nonce,
            transfer_port: 0,
        }
    }

    pub(crate) fn offer(nonce: u64, transfer_port: u16) -> Self {
        Self {
            kind: DiscoveryKind::Offer,
            nonce,
            transfer_port,
        }
    }

    pub(crate) fn encode(self) -> [u8; DISCOVERY_PACKET_BYTES] {
        let mut bytes = [0_u8; DISCOVERY_PACKET_BYTES];

        bytes[..4].copy_from_slice(&DISCOVERY_MAGIC);

        bytes[4] = DISCOVERY_VERSION;
        bytes[5] = self.kind as u8;

        bytes[8..16].copy_from_slice(&self.nonce.to_be_bytes());

        bytes[16..18].copy_from_slice(&self.transfer_port.to_be_bytes());

        bytes
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != DISCOVERY_PACKET_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "discovery packet has {} bytes, expected {}",
                    bytes.len(),
                    DISCOVERY_PACKET_BYTES,
                ),
            ));
        }

        if bytes[..4] != DISCOVERY_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "discovery packet has invalid magic",
            ));
        }

        if bytes[4] != DISCOVERY_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported discovery protocol version {}", bytes[4],),
            ));
        }

        let kind = match bytes[5] {
            1 => DiscoveryKind::Probe,
            2 => DiscoveryKind::Offer,

            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown discovery packet kind {unknown}",),
                ));
            }
        };

        let nonce = u64::from_be_bytes(
            bytes[8..16]
                .try_into()
                .map_err(|_| io::Error::other("discovery nonce slice has incorrect length"))?,
        );

        let transfer_port = u16::from_be_bytes(
            bytes[16..18]
                .try_into()
                .map_err(|_| io::Error::other("discovery port slice has incorrect length"))?,
        );

        Ok(Self {
            kind,
            nonce,
            transfer_port,
        })
    }
}

pub(crate) fn receive(interface_index: u32) -> io::Result<()> {
    let local_endpoint = direct_address::link_local_endpoint(interface_index, DISCOVERY_PORT)?;

    let bind_endpoint = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, DISCOVERY_PORT, 0, 0);

    let socket = UdpSocket::bind(bind_endpoint)?;

    socket.join_multicast_v6(&DISCOVERY_GROUP, interface_index)?;

    socket.set_multicast_loop_v6(false)?;

    println!("NetworkCopy Speed Edition direct-link discovery receiver");

    println!("  Interface index: {}", interface_index,);

    println!(
        "  Local IPv6:      {}%{}",
        local_endpoint.ip(),
        interface_index,
    );

    println!(
        "  Multicast group: [{}%{}]:{}",
        DISCOVERY_GROUP, interface_index, DISCOVERY_PORT,
    );

    println!("  Transfer port:   {}", DIRECT_TRANSFER_PORT,);

    println!();
    println!("Waiting for direct-link probes...");

    let mut buffer = [0_u8; 256];

    loop {
        let (received, source) = socket.recv_from(&mut buffer)?;

        let SocketAddr::V6(source) = source else {
            continue;
        };

        let Ok(packet) = DiscoveryPacket::decode(&buffer[..received]) else {
            continue;
        };

        if packet.kind != DiscoveryKind::Probe {
            continue;
        }

        let Some(reply_target) = scoped_link_local(source, interface_index) else {
            continue;
        };

        let offer = DiscoveryPacket::offer(packet.nonce, DIRECT_TRANSFER_PORT);

        socket.send_to(&offer.encode(), reply_target)?;

        println!(
            "  Replied to {} through interface {}",
            reply_target, interface_index,
        );
    }
}

fn automatic_direct_candidates() -> io::Result<direct_route::RouteCandidates> {
    let strict_candidates = direct_link::strict_candidate_indices()?;

    let candidates = direct_route::classify_candidates(&strict_candidates)?;

    if candidates.direct.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no dedicated Ethernet interface without a default route was found",
        ));
    }

    Ok(candidates)
}

pub(crate) fn receive_all() -> io::Result<()> {
    receive_automatically(false, None, DIRECT_TRANSFER_PORT).map(|_| ())
}

pub(crate) fn receive_all_on_port(offer_port: u16) -> io::Result<()> {
    receive_automatically(false, None, offer_port).map(|_| ())
}

pub(crate) fn receive_one() -> io::Result<ReceivedPath> {
    receive_automatically(true, None, DIRECT_TRANSFER_PORT)
}

pub(crate) fn receive_one_with_progress(progress: &ProgressCounter) -> io::Result<ReceivedPath> {
    receive_automatically(true, Some(progress), DIRECT_TRANSFER_PORT)
}

fn receive_automatically(
    stop_after_first: bool,
    progress: Option<&ProgressCounter>,
    offer_port: u16,
) -> io::Result<ReceivedPath> {
    if offer_port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "direct discovery offer port must not be zero",
        ));
    }
    check_cancelled(progress)?;

    let candidates = automatic_direct_candidates()?;

    let interface_indices = candidates.direct;

    let routed_interface_indices = candidates.routed;

    let mut local_endpoints = Vec::new();

    for interface_index in &interface_indices {
        match direct_address::link_local_endpoint(*interface_index, DISCOVERY_PORT) {
            Ok(endpoint) => {
                local_endpoints.push((*interface_index, endpoint));
            }

            Err(error) if error.kind() == io::ErrorKind::AddrNotAvailable => {}

            Err(error) => {
                return Err(error);
            }
        }
    }

    let (ipv4_receiver, ipv4_warning) =
        match direct_discovery_v4::receiver(&interface_indices, offer_port) {
            Ok(receiver) => (receiver, None),

            Err(error) if !local_endpoints.is_empty() => (None, Some(error.to_string())),

            Err(error) => {
                return Err(error);
            }
        };

    if local_endpoints.is_empty() && ipv4_receiver.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no direct candidate has a usable IPv6 link-local or IPv4 APIPA address",
        ));
    }

    let ipv6_socket = if local_endpoints.is_empty() {
        None
    } else {
        let socket = bind_ipv6_receiver()?;

        socket.set_multicast_loop_v6(false)?;

        for (interface_index, _) in &local_endpoints {
            socket.join_multicast_v6(&DISCOVERY_GROUP, *interface_index)?;
        }

        Some(socket)
    };

    println!("NetworkCopy Speed Edition automatic direct-link discovery receiver");

    println!("  Direct candidates:    {}", interface_indices.len(),);

    for interface_index in &routed_interface_indices {
        println!("  Rejected routed:      {}", interface_index,);
    }

    for (interface_index, endpoint) in &local_endpoints {
        println!();

        println!("  Listening interface: {}", interface_index,);

        println!(
            "  IPv6 link-local:     {}%{}",
            endpoint.ip(),
            interface_index,
        );

        println!(
            "  IPv6 multicast:     [{}%{}]:{}",
            DISCOVERY_GROUP, interface_index, DISCOVERY_PORT,
        );
    }

    if let Some(receiver) = &ipv4_receiver {
        println!();

        println!("  IPv4 fallback:       {}", receiver.local_endpoint(),);
    }

    if let Some(warning) = ipv4_warning {
        println!();

        println!("  IPv4 fallback unavailable: {}", warning,);
    }

    println!();

    println!("  Advertised port:     {}", offer_port,);

    println!();
    println!("Waiting for direct-link probes...");

    loop {
        check_cancelled(progress)?;

        if let Some(socket) = &ipv6_socket
            && let Some(path) = receive_ipv6_probe(socket, &interface_indices, offer_port)?
            && stop_after_first
        {
            return Ok(path);
        }

        check_cancelled(progress)?;

        if let Some(receiver) = &ipv4_receiver
            && let Some(path) = receiver.poll()?
        {
            let path = ReceivedPath {
                interface_index: path.interface_index,

                local_endpoint: SocketAddr::V4(path.local_endpoint),
            };

            if stop_after_first {
                return Ok(path);
            }
        }
    }
}

fn bind_ipv6_receiver() -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_only_v6(true)?;

    let bind_endpoint = SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::UNSPECIFIED,
        DISCOVERY_PORT,
        0,
        0,
    ));

    socket.bind(&bind_endpoint.into())?;

    let socket: UdpSocket = socket.into();

    socket.set_read_timeout(Some(DISCOVERY_POLL_TIMEOUT))?;

    Ok(socket)
}

fn receive_ipv6_probe(
    socket: &UdpSocket,
    interface_indices: &[u32],
    offer_port: u16,
) -> io::Result<Option<ReceivedPath>> {
    let mut buffer = [0_u8; 256];

    let (received, source) = match socket.recv_from(&mut buffer) {
        Ok(value) => value,

        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            return Ok(None);
        }

        Err(error) => {
            return Err(error);
        }
    };

    let SocketAddr::V6(source) = source else {
        return Ok(None);
    };

    let interface_index = source.scope_id();

    if interface_index == 0 || !interface_indices.contains(&interface_index) {
        return Ok(None);
    }

    let Ok(packet) = DiscoveryPacket::decode(&buffer[..received]) else {
        return Ok(None);
    };

    if packet.kind != DiscoveryKind::Probe {
        return Ok(None);
    }

    let Some(reply_target) = scoped_link_local(source, interface_index) else {
        return Ok(None);
    };

    let offer = DiscoveryPacket::offer(packet.nonce, offer_port);

    socket.send_to(&offer.encode(), reply_target)?;

    println!(
        "  Replied to {} through interface {} using IPv6",
        reply_target, interface_index,
    );

    let local_endpoint = direct_address::link_local_endpoint(interface_index, offer_port)?;

    Ok(Some(ReceivedPath {
        interface_index,
        local_endpoint: SocketAddr::V6(local_endpoint),
    }))
}

pub(crate) fn discover(interface_index: u32) -> io::Result<SocketAddr> {
    match discover_on_interface(interface_index, true) {
        Ok(path) => Ok(path.endpoint),

        Err(ipv6_error) => {
            println!();

            println!("IPv6 discovery failed: {}", ipv6_error,);

            println!("Trying IPv4 APIPA fallback...");

            direct_discovery_v4::discover_on_interface(
                interface_index,
                true,
            )
            .map(|path| {
                SocketAddr::V4(
                    path.endpoint,
                )
            })
            .map_err(|ipv4_error| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "IPv6 discovery failed: {ipv6_error}; IPv4 discovery failed: {ipv4_error}",
                    ),
                )
            })
        }
    }
}

pub(crate) fn discover_all() -> io::Result<Vec<DiscoveredPath>> {
    discover_all_configured(None)
}

fn discover_all_configured(progress: Option<&ProgressCounter>) -> io::Result<Vec<DiscoveredPath>> {
    check_cancelled(progress)?;

    let candidates = automatic_direct_candidates()?;

    let interface_indices = candidates.direct;

    let routed_interface_indices = candidates.routed;

    println!("NetworkCopy Speed Edition automatic direct-link discovery");

    println!("  Direct candidates:    {}", interface_indices.len(),);

    for interface_index in &routed_interface_indices {
        println!("  Rejected routed:      {}", interface_index,);
    }

    for interface_index in &interface_indices {
        println!("  Probing interface:    {}", interface_index,);
    }

    println!();

    let (mut discovered, mut failures) = discover_ipv6_paths(&interface_indices, progress);

    check_cancelled(progress)?;

    if discovered.is_empty() {
        let ipv6_details = failure_details(&failures);

        println!("No IPv6 receiver replied; trying IPv4 APIPA fallback...");

        let (ipv4_discovered, ipv4_failures) = discover_ipv4_paths(&interface_indices, progress);

        check_cancelled(progress)?;

        discovered = ipv4_discovered;

        failures = ipv4_failures;

        if discovered.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "IPv6 discovery failed: {ipv6_details}; IPv4 discovery failed: {}",
                    failure_details(&failures,),
                ),
            ));
        }
    }

    discovered.sort_by_key(|path| (path.interface_index, path.endpoint.is_ipv4()));

    println!(
        "Direct-link receiver path{} discovered",
        if discovered.len() == 1 { "" } else { "s" },
    );

    for path in &discovered {
        println!();

        println!("  Local interface: {}", path.interface_index,);

        println!(
            "  Protocol:        {}",
            if path.endpoint.is_ipv6() {
                "IPv6 link-local"
            } else {
                "IPv4 APIPA"
            },
        );

        println!("  Local binding:   {}", path.local_endpoint,);

        println!("  Peer endpoint:   {}", path.endpoint,);
    }

    if !failures.is_empty() {
        println!();

        println!("Interfaces without a valid reply:");

        for failure in failures {
            println!("  {failure}");
        }
    }

    Ok(discovered)
}

fn discover_ipv6_paths(
    interface_indices: &[u32],
    progress: Option<&ProgressCounter>,
) -> (Vec<DiscoveredPath>, Vec<String>) {
    thread::scope(|scope| {
        let handles = interface_indices
            .iter()
            .copied()
            .map(|interface_index| {
                (
                    interface_index,
                    scope.spawn(move || {
                        discover_on_interface_with_progress(interface_index, false, progress)
                    }),
                )
            })
            .collect::<Vec<_>>();

        let mut discovered = Vec::new();

        let mut failures = Vec::new();

        for (interface_index, handle) in handles {
            match handle.join() {
                Ok(Ok(path)) => {
                    discovered.push(path);
                }

                Ok(Err(error)) => {
                    failures.push(format!("interface {interface_index}: {error}",));
                }

                Err(_) => {
                    failures.push(format!(
                        "interface {interface_index}: IPv6 discovery thread panicked",
                    ));
                }
            }
        }

        (discovered, failures)
    })
}

fn discover_ipv4_paths(
    interface_indices: &[u32],
    progress: Option<&ProgressCounter>,
) -> (Vec<DiscoveredPath>, Vec<String>) {
    thread::scope(|scope| {
        let handles = interface_indices
            .iter()
            .copied()
            .map(|interface_index| {
                (
                    interface_index,
                    scope.spawn(move || {
                        direct_discovery_v4::discover_on_interface_with_progress(
                            interface_index,
                            false,
                            progress,
                        )
                    }),
                )
            })
            .collect::<Vec<_>>();

        let mut discovered = Vec::new();

        let mut failures = Vec::new();

        for (interface_index, handle) in handles {
            match handle.join() {
                Ok(Ok(path)) => {
                    discovered.push(DiscoveredPath {
                        interface_index: path.interface_index,

                        local_endpoint: SocketAddr::V4(path.local_endpoint),

                        endpoint: SocketAddr::V4(path.endpoint),
                    });
                }

                Ok(Err(error)) => {
                    failures.push(format!("interface {interface_index}: {error}",));
                }

                Err(_) => {
                    failures.push(format!(
                        "interface {interface_index}: IPv4 discovery thread panicked",
                    ));
                }
            }
        }

        (discovered, failures)
    })
}

fn failure_details(failures: &[String]) -> String {
    if failures.is_empty() {
        "no candidate returned a diagnostic".to_string()
    } else {
        failures.join("; ")
    }
}

pub(crate) fn discover_one() -> io::Result<DiscoveredPath> {
    discover_one_configured(None)
}

pub(crate) fn discover_one_with_progress(progress: &ProgressCounter) -> io::Result<DiscoveredPath> {
    discover_one_configured(Some(progress))
}

fn discover_one_configured(progress: Option<&ProgressCounter>) -> io::Result<DiscoveredPath> {
    let mut paths = discover_all_configured(progress)?;

    check_cancelled(progress)?;

    if paths.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!(
                "automatic direct-link connection requires exactly one discovered path, but {} paths replied",
                paths.len(),
            ),
        ));
    }

    Ok(paths.remove(0))
}

fn discover_on_interface(interface_index: u32, verbose: bool) -> io::Result<DiscoveredPath> {
    discover_on_interface_with_progress(interface_index, verbose, None)
}

fn discover_on_interface_with_progress(
    interface_index: u32,
    verbose: bool,
    progress: Option<&ProgressCounter>,
) -> io::Result<DiscoveredPath> {
    check_cancelled(progress)?;

    let local_endpoint = direct_address::link_local_endpoint(interface_index, 0)?;

    let socket = UdpSocket::bind(local_endpoint)?;

    socket.set_multicast_loop_v6(false)?;

    socket.set_read_timeout(Some(DISCOVERY_POLL_TIMEOUT))?;

    let multicast_endpoint = SocketAddrV6::new(DISCOVERY_GROUP, DISCOVERY_PORT, 0, interface_index);

    let nonce = create_nonce()?;

    let probe = DiscoveryPacket::probe(nonce);

    let encoded_probe = probe.encode();

    let mut buffer = [0_u8; 256];

    if verbose {
        println!("NetworkCopy Speed Edition direct-link discovery sender");

        println!("  Interface index: {}", interface_index,);

        println!(
            "  Local IPv6:      {}%{}",
            local_endpoint.ip(),
            interface_index,
        );

        println!("  Multicast probe: {}", multicast_endpoint,);

        println!();
    }

    for attempt in 1..=DISCOVERY_ATTEMPTS {
        check_cancelled(progress)?;

        socket.send_to(&encoded_probe, multicast_endpoint)?;

        if verbose {
            println!("  Probe {attempt}/{DISCOVERY_ATTEMPTS} sent");
        }

        let deadline = Instant::now() + DISCOVERY_REPLY_TIMEOUT;

        loop {
            check_cancelled(progress)?;

            if Instant::now() >= deadline {
                break;
            }

            match socket.recv_from(&mut buffer) {
                Ok((received, source)) => {
                    let SocketAddr::V6(source) = source else {
                        continue;
                    };

                    let Ok(packet) = DiscoveryPacket::decode(&buffer[..received]) else {
                        continue;
                    };

                    if packet.kind != DiscoveryKind::Offer
                        || packet.nonce != nonce
                        || packet.transfer_port == 0
                    {
                        continue;
                    }

                    let Some(source) = scoped_link_local(source, interface_index) else {
                        continue;
                    };

                    let endpoint =
                        SocketAddrV6::new(*source.ip(), packet.transfer_port, 0, interface_index);

                    if verbose {
                        println!();

                        println!("Direct-link receiver discovered");

                        println!("  Peer IPv6:      {}%{}", endpoint.ip(), interface_index,);

                        println!("  TCP endpoint:   {}", endpoint,);

                        println!("  Local interface: {}", interface_index,);
                    }

                    return Ok(DiscoveredPath {
                        interface_index,

                        local_endpoint: SocketAddr::V6(local_endpoint),

                        endpoint: SocketAddr::V6(endpoint),
                    });
                }

                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }

                Err(error) => {
                    return Err(error);
                }
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no NetworkCopy receiver replied through interface {interface_index}",),
    ))
}

fn check_cancelled(progress: Option<&ProgressCounter>) -> io::Result<()> {
    if let Some(progress) = progress {
        progress.check_cancelled()?;
    }

    Ok(())
}

fn scoped_link_local(mut address: SocketAddrV6, interface_index: u32) -> Option<SocketAddrV6> {
    if !is_link_local(*address.ip()) {
        return None;
    }

    address.set_scope_id(interface_index);

    Some(address)
}

fn is_link_local(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xFFC0 == 0xFE80
}

pub(crate) fn create_nonce() -> io::Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            io::Error::other(format!("system clock is before Unix epoch: {error}",))
        })?;

    Ok(elapsed.as_nanos() as u64 ^ (u64::from(process::id()) << 32))
}

#[cfg(test)]
mod tests {
    use super::{DISCOVERY_PACKET_BYTES, DiscoveryKind, DiscoveryPacket, check_cancelled};
    use crate::console_progress::ProgressCounter;

    #[test]
    fn probe_packet_round_trips() {
        let expected = DiscoveryPacket::probe(0x1234_5678_9ABC_DEF0);

        let encoded = expected.encode();

        let actual = DiscoveryPacket::decode(&encoded).unwrap();

        assert_eq!(actual, expected,);
    }

    #[test]
    fn offer_packet_round_trips() {
        let expected = DiscoveryPacket::offer(0x0FED_CBA9_8765_4321, 7337);

        let encoded = expected.encode();

        let actual = DiscoveryPacket::decode(&encoded).unwrap();

        assert_eq!(actual, expected,);
    }

    #[test]
    fn invalid_magic_is_rejected() {
        let mut encoded = [0_u8; DISCOVERY_PACKET_BYTES];

        encoded[..4].copy_from_slice(b"NOPE");

        let error = DiscoveryPacket::decode(&encoded).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
    }

    #[test]
    fn invalid_packet_kind_is_rejected() {
        let mut encoded = DiscoveryPacket {
            kind: DiscoveryKind::Probe,
            nonce: 7,
            transfer_port: 0,
        }
        .encode();

        encoded[5] = 0xFF;

        let error = DiscoveryPacket::decode(&encoded).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
    }

    #[test]
    fn cancelled_discovery_returns_interrupted() {
        let progress = ProgressCounter::new("test discovery", 0);

        progress.cancel();

        let error = check_cancelled(Some(&progress)).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted,);
    }
}
