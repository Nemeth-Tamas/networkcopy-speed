use crate::console_progress::ProgressCounter;
use crate::direct_address;
use crate::direct_discovery::{DISCOVERY_PORT, DiscoveryKind, DiscoveryPacket};
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

const DISCOVERY_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 78, 67);

const DISCOVERY_ATTEMPTS: usize = 6;

const DISCOVERY_REPLY_TIMEOUT: Duration = Duration::from_millis(500);

const DISCOVERY_POLL_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4DiscoveredPath {
    pub(crate) interface_index: u32,
    pub(crate) local_endpoint: SocketAddrV4,
    pub(crate) endpoint: SocketAddrV4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4ReceivedPath {
    pub(crate) interface_index: u32,
    pub(crate) local_endpoint: SocketAddrV4,
}

pub(crate) struct Ipv4Receiver {
    interface_index: u32,

    local_endpoint: SocketAddrV4,

    offer_port: u16,

    socket: UdpSocket,
}

impl Ipv4Receiver {
    pub(crate) fn local_endpoint(&self) -> SocketAddrV4 {
        SocketAddrV4::new(*self.local_endpoint.ip(), self.offer_port)
    }

    pub(crate) fn poll(&self) -> io::Result<Option<Ipv4ReceivedPath>> {
        let mut buffer = [0_u8; 256];

        let (received, source) = match self.socket.recv_from(&mut buffer) {
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

        let SocketAddr::V4(source) = source else {
            return Ok(None);
        };

        if !is_apipa(*source.ip()) {
            return Ok(None);
        }

        let Ok(packet) = DiscoveryPacket::decode(&buffer[..received]) else {
            return Ok(None);
        };

        if packet.kind != DiscoveryKind::Probe {
            return Ok(None);
        }

        let offer = DiscoveryPacket::offer(packet.nonce, self.offer_port);

        self.socket.send_to(&offer.encode(), source)?;

        println!(
            "  Replied to {} through interface {} using IPv4 APIPA",
            source, self.interface_index,
        );

        Ok(Some(Ipv4ReceivedPath {
            interface_index: self.interface_index,

            local_endpoint: self.local_endpoint(),
        }))
    }
}

pub(crate) fn receiver(
    interface_indices: &[u32],
    offer_port: u16,
) -> io::Result<Option<Ipv4Receiver>> {
    if offer_port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "direct discovery offer port must not be zero",
        ));
    }
    let mut endpoints = Vec::new();

    for interface_index in interface_indices {
        match direct_address::apipa_endpoint(*interface_index, DISCOVERY_PORT) {
            Ok(endpoint) => {
                endpoints.push((*interface_index, endpoint));
            }

            Err(error) if error.kind() == io::ErrorKind::AddrNotAvailable => {}

            Err(error) => {
                return Err(error);
            }
        }
    }

    if endpoints.is_empty() {
        return Ok(None);
    }

    if endpoints.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!(
                "IPv4 APIPA fallback requires exactly one direct candidate, but {} APIPA interfaces were found",
                endpoints.len(),
            ),
        ));
    }

    let (interface_index, local_endpoint) = endpoints.remove(0);

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_reuse_address(true)?;

    let bind_endpoint = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT));

    socket.bind(&bind_endpoint.into())?;

    socket.set_multicast_loop_v4(false)?;

    socket.join_multicast_v4(&DISCOVERY_GROUP, local_endpoint.ip())?;

    let socket: UdpSocket = socket.into();

    socket.set_read_timeout(Some(DISCOVERY_POLL_TIMEOUT))?;

    Ok(Some(Ipv4Receiver {
        interface_index,

        local_endpoint,

        offer_port,

        socket,
    }))
}

pub(crate) fn discover_on_interface(
    interface_index: u32,
    verbose: bool,
) -> io::Result<Ipv4DiscoveredPath> {
    discover_on_interface_with_progress(interface_index, verbose, None)
}

pub(crate) fn discover_on_interface_with_progress(
    interface_index: u32,
    verbose: bool,
    progress: Option<&ProgressCounter>,
) -> io::Result<Ipv4DiscoveredPath> {
    check_cancelled(progress)?;

    let local_endpoint = direct_address::apipa_endpoint(interface_index, 0)?;

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_multicast_if_v4(local_endpoint.ip())?;

    socket.set_multicast_loop_v4(false)?;

    socket.set_multicast_ttl_v4(1)?;

    let bind_endpoint = SocketAddr::V4(local_endpoint);

    socket.bind(&bind_endpoint.into())?;

    let socket: UdpSocket = socket.into();

    socket.set_read_timeout(Some(DISCOVERY_POLL_TIMEOUT))?;

    let multicast_endpoint = SocketAddrV4::new(DISCOVERY_GROUP, DISCOVERY_PORT);

    let nonce = super::direct_discovery::create_nonce()?;

    let probe = DiscoveryPacket::probe(nonce);

    let encoded_probe = probe.encode();

    let mut buffer = [0_u8; 256];

    if verbose {
        println!("NetworkCopy Speed Edition direct-link IPv4 discovery sender");

        println!("  Interface index: {}", interface_index,);

        println!("  Local APIPA:     {}", local_endpoint.ip(),);

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
                    let SocketAddr::V4(source) = source else {
                        continue;
                    };

                    if !is_apipa(*source.ip()) {
                        continue;
                    }

                    let Ok(packet) = DiscoveryPacket::decode(&buffer[..received]) else {
                        continue;
                    };

                    if packet.kind != DiscoveryKind::Offer
                        || packet.nonce != nonce
                        || packet.transfer_port == 0
                    {
                        continue;
                    }

                    let endpoint = SocketAddrV4::new(*source.ip(), packet.transfer_port);

                    if verbose {
                        println!();

                        println!("Direct-link IPv4 receiver discovered");

                        println!("  Peer APIPA:      {}", endpoint.ip(),);

                        println!("  TCP endpoint:    {}", endpoint,);

                        println!("  Local interface: {}", interface_index,);
                    }

                    return Ok(Ipv4DiscoveredPath {
                        interface_index,
                        local_endpoint,
                        endpoint,
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
        format!("no NetworkCopy IPv4 receiver replied through interface {interface_index}",),
    ))
}

fn check_cancelled(progress: Option<&ProgressCounter>) -> io::Result<()> {
    if let Some(progress) = progress {
        progress.check_cancelled()?;
    }

    Ok(())
}

fn is_apipa(address: Ipv4Addr) -> bool {
    let octets = address.octets();

    octets[0] == 169 && octets[1] == 254
}
