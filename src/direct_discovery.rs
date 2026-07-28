use crate::direct_address::{self, DIRECT_TRANSFER_PORT};
use crate::direct_link;
use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::process;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const DISCOVERY_PORT: u16 = 7336;

const DISCOVERY_MAGIC: [u8; 4] = *b"NCD1";
const DISCOVERY_VERSION: u8 = 1;
const DISCOVERY_PACKET_BYTES: usize = 20;

const DISCOVERY_ATTEMPTS: usize = 6;
const DISCOVERY_REPLY_TIMEOUT: Duration = Duration::from_millis(500);

const DISCOVERY_GROUP: Ipv6Addr = Ipv6Addr::new(0xFF12, 0, 0, 0, 0, 0, 0x4E43, 0x5350);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiscoveredPath {
    pub(crate) interface_index: u32,
    pub(crate) endpoint: SocketAddrV6,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiscoveryKind {
    Probe = 1,
    Offer = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DiscoveryPacket {
    kind: DiscoveryKind,
    nonce: u64,
    transfer_port: u16,
}

impl DiscoveryPacket {
    fn probe(nonce: u64) -> Self {
        Self {
            kind: DiscoveryKind::Probe,
            nonce,
            transfer_port: 0,
        }
    }

    fn offer(nonce: u64, transfer_port: u16) -> Self {
        Self {
            kind: DiscoveryKind::Offer,
            nonce,
            transfer_port,
        }
    }

    fn encode(self) -> [u8; DISCOVERY_PACKET_BYTES] {
        let mut bytes = [0_u8; DISCOVERY_PACKET_BYTES];

        bytes[..4].copy_from_slice(&DISCOVERY_MAGIC);

        bytes[4] = DISCOVERY_VERSION;
        bytes[5] = self.kind as u8;

        bytes[8..16].copy_from_slice(&self.nonce.to_be_bytes());

        bytes[16..18].copy_from_slice(&self.transfer_port.to_be_bytes());

        bytes
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
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

pub(crate) fn discover(interface_index: u32) -> io::Result<SocketAddrV6> {
    discover_on_interface(interface_index, true)
}

pub(crate) fn discover_all() -> io::Result<Vec<DiscoveredPath>> {
    let interface_indices = direct_link::strict_candidate_indices()?;

    println!("NetworkCopy Speed Edition automatic direct-link discovery");

    println!("  Candidate interfaces: {}", interface_indices.len(),);

    for interface_index in &interface_indices {
        println!("  Probing interface:    {}", interface_index,);
    }

    println!();

    let (mut discovered, failures) = thread::scope(|scope| {
        let handles = interface_indices
            .iter()
            .copied()
            .map(|interface_index| {
                (
                    interface_index,
                    scope.spawn(move || discover_on_interface(interface_index, false)),
                )
            })
            .collect::<Vec<_>>();

        let mut discovered = Vec::new();
        let mut failures = Vec::new();

        for (interface_index, handle) in handles {
            match handle.join() {
                Ok(Ok(endpoint)) => {
                    discovered.push(DiscoveredPath {
                        interface_index,
                        endpoint,
                    });
                }

                Ok(Err(error)) => {
                    failures.push(format!("interface {interface_index}: {error}"));
                }

                Err(_) => {
                    failures.push(format!(
                        "interface {interface_index}: discovery thread panicked"
                    ));
                }
            }
        }

        (discovered, failures)
    });

    discovered.sort_by_key(|path| path.interface_index);

    if discovered.is_empty() {
        let details = if failures.is_empty() {
            "no candidate returned a diagnostic".to_string()
        } else {
            failures.join("; ")
        };

        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "no NetworkCopy receiver was discovered through any strict Ethernet interface: {details}"
            ),
        ));
    }

    println!(
        "Direct-link receiver path{} discovered",
        if discovered.len() == 1 { "" } else { "s" },
    );

    for path in &discovered {
        println!();

        println!("  Local interface: {}", path.interface_index,);

        println!(
            "  Peer IPv6:       {}%{}",
            path.endpoint.ip(),
            path.interface_index,
        );

        println!("  TCP endpoint:    {}", path.endpoint,);
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

fn discover_on_interface(interface_index: u32, verbose: bool) -> io::Result<SocketAddrV6> {
    let local_endpoint = direct_address::link_local_endpoint(interface_index, 0)?;

    let socket = UdpSocket::bind(local_endpoint)?;

    socket.set_multicast_loop_v6(false)?;

    socket.set_read_timeout(Some(DISCOVERY_REPLY_TIMEOUT))?;

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
        socket.send_to(&encoded_probe, multicast_endpoint)?;

        if verbose {
            println!("  Probe {attempt}/{DISCOVERY_ATTEMPTS} sent");
        }

        loop {
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

                    return Ok(endpoint);
                }

                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }

                Err(error) => {
                    return Err(error);
                }
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no NetworkCopy receiver replied through interface {interface_index}"),
    ))
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

fn create_nonce() -> io::Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            io::Error::other(format!("system clock is before Unix epoch: {error}",))
        })?;

    Ok(elapsed.as_nanos() as u64 ^ (u64::from(process::id()) << 32))
}

#[cfg(test)]
mod tests {
    use super::{DISCOVERY_PACKET_BYTES, DiscoveryKind, DiscoveryPacket};

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
}
