use crate::management_control;
use crate::management_direct;
use crate::management_jobs::ManagementJobRegistry;
use crate::management_protocol::{
    MANAGEMENT_CONTROL_PORT, MANAGEMENT_DISCOVERY_PORT, MANAGEMENT_PROTOCOL_VERSION,
};
use std::env;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DISCOVERY_MAGIC: [u8; 4] = *b"NMD1";
const DISCOVERY_VERSION: u8 = 1;

const DISCOVERY_HEADER_BYTES: usize = 24;
const MAX_HOSTNAME_BYTES: usize = 255;

const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1_500);

const DISCOVERY_POLL_TIMEOUT: Duration = Duration::from_millis(100);

const DISCOVERY_RETRY_DELAY: Duration = Duration::from_millis(300);

const DISCOVERY_ATTEMPTS: usize = 3;

static NEXT_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AgentState {
    Idle = 0,
    Busy = 1,
}

impl AgentState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
        }
    }
}

impl TryFrom<u8> for AgentState {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Idle),
            1 => Ok(Self::Busy),

            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown management agent state {unknown}"),
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentCapabilities {
    bits: u8,
}

impl AgentCapabilities {
    const SEND_BIT: u8 = 0x01;
    const RECEIVE_BIT: u8 = 0x02;
    const KNOWN_BITS: u8 = Self::SEND_BIT | Self::RECEIVE_BIT;

    pub const SENDER: Self = Self {
        bits: Self::SEND_BIT,
    };

    pub const RECEIVER: Self = Self {
        bits: Self::RECEIVE_BIT,
    };

    pub const SEND_RECEIVE: Self = Self {
        bits: Self::KNOWN_BITS,
    };

    pub const fn can_send(self) -> bool {
        self.bits & Self::SEND_BIT != 0
    }

    pub const fn can_receive(self) -> bool {
        self.bits & Self::RECEIVE_BIT != 0
    }

    pub(crate) const fn bits(self) -> u8 {
        self.bits
    }

    pub(crate) fn from_bits(bits: u8) -> io::Result<Self> {
        if bits == 0 || bits & !Self::KNOWN_BITS != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid management agent capability bits 0x{bits:02X}"),
            ));
        }

        Ok(Self { bits })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredAgent {
    pub hostname: String,

    pub endpoint: SocketAddr,

    pub protocol_version: u16,

    pub state: AgentState,

    pub capabilities: AgentCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalAgentDescriptor {
    hostname: String,

    control_port: u16,

    capabilities: AgentCapabilities,
}

impl LocalAgentDescriptor {
    fn local() -> io::Result<Self> {
        Ok(Self {
            hostname: local_hostname()?,

            control_port: MANAGEMENT_CONTROL_PORT,

            capabilities: AgentCapabilities::SEND_RECEIVE,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DiscoveryPacket {
    Probe {
        nonce: u64,
    },

    Offer {
        nonce: u64,

        control_port: u16,

        state: AgentState,

        capabilities: AgentCapabilities,

        hostname: String,
    },
}

impl DiscoveryPacket {
    fn encode(&self) -> io::Result<Vec<u8>> {
        let mut bytes = match self {
            Self::Probe { .. } => {
                vec![0_u8; DISCOVERY_HEADER_BYTES]
            }

            Self::Offer { hostname, .. } => {
                validate_hostname(hostname)?;

                let hostname_length = hostname.len();

                let total_length = DISCOVERY_HEADER_BYTES
                    .checked_add(hostname_length)
                    .ok_or_else(|| {
                        io::Error::other("management discovery packet length overflowed")
                    })?;

                vec![0_u8; total_length]
            }
        };

        bytes[0..4].copy_from_slice(&DISCOVERY_MAGIC);

        bytes[4] = DISCOVERY_VERSION;

        match self {
            Self::Probe { nonce } => {
                bytes[5] = 1;

                bytes[8..16].copy_from_slice(&nonce.to_le_bytes());
            }

            Self::Offer {
                nonce,
                control_port,
                state,
                capabilities,
                hostname,
            } => {
                if *control_port == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "management control port must not be zero",
                    ));
                }

                bytes[5] = 2;

                bytes[8..16].copy_from_slice(&nonce.to_le_bytes());

                bytes[16..18].copy_from_slice(&control_port.to_le_bytes());

                bytes[18] = *state as u8;
                bytes[19] = capabilities.bits();

                let hostname_length = u16::try_from(hostname.len()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "management hostname length cannot be represented",
                    )
                })?;

                bytes[20..22].copy_from_slice(&hostname_length.to_le_bytes());

                bytes[DISCOVERY_HEADER_BYTES..].copy_from_slice(hostname.as_bytes());
            }
        }

        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < DISCOVERY_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "management discovery packet has {} bytes, expected at least {}",
                    bytes.len(),
                    DISCOVERY_HEADER_BYTES,
                ),
            ));
        }

        if bytes[0..4] != DISCOVERY_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "management discovery packet used invalid magic",
            ));
        }

        if bytes[4] != DISCOVERY_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported management discovery version {}", bytes[4],),
            ));
        }

        if bytes[6] != 0 || bytes[7] != 0 || bytes[22] != 0 || bytes[23] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "management discovery reserved bytes were not zero",
            ));
        }

        let nonce = u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management discovery nonce was malformed",
            )
        })?);

        let control_port = u16::from_le_bytes(bytes[16..18].try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "management discovery control port was malformed",
            )
        })?);

        let hostname_length = usize::from(u16::from_le_bytes(bytes[20..22].try_into().map_err(
            |_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "management discovery hostname length was malformed",
                )
            },
        )?));

        let expected_length = DISCOVERY_HEADER_BYTES
            .checked_add(hostname_length)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "management discovery packet length overflowed",
                )
            })?;

        if bytes.len() != expected_length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "management discovery packet has {} bytes, expected {expected_length}",
                    bytes.len(),
                ),
            ));
        }

        match bytes[5] {
            1 => {
                if control_port != 0 || bytes[18] != 0 || bytes[19] != 0 || hostname_length != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "management discovery probe contained offer fields",
                    ));
                }

                Ok(Self::Probe { nonce })
            }

            2 => {
                if control_port == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "management discovery offer stored zero control port",
                    ));
                }

                let state = AgentState::try_from(bytes[18])?;

                let capabilities = AgentCapabilities::from_bits(bytes[19])?;

                let hostname = std::str::from_utf8(&bytes[DISCOVERY_HEADER_BYTES..])
                    .map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("management hostname was not valid UTF-8: {error}"),
                        )
                    })?
                    .to_owned();

                validate_hostname(&hostname).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                })?;

                Ok(Self::Offer {
                    nonce,
                    control_port,
                    state,
                    capabilities,
                    hostname,
                })
            }

            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown management discovery packet kind {unknown}"),
            )),
        }
    }
}

pub fn run_agent() -> io::Result<()> {
    let descriptor = LocalAgentDescriptor::local()?;

    let jobs = Arc::new(ManagementJobRegistry::new()?);

    let socket = UdpSocket::bind(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        MANAGEMENT_DISCOVERY_PORT,
    ))?;

    management_control::spawn(
        descriptor.hostname.clone(),
        descriptor.capabilities,
        Arc::clone(&jobs),
    )?;

    let direct_interface_count = management_direct::spawn_responder()?;

    println!("NetworkCopy Speed Edition management agent",);

    println!("  Computer:       {}", descriptor.hostname,);

    println!("  Instance:       {}", jobs.instance_id(),);

    println!("  Discovery UDP:  0.0.0.0:{}", MANAGEMENT_DISCOVERY_PORT,);

    println!("  Control TCP v4: 0.0.0.0:{}", descriptor.control_port,);

    println!("  Control TCP v6: [::]:{}", descriptor.control_port,);

    if direct_interface_count == 0 {
        println!("  Direct Link:    inactive — no gateway-free Ethernet cable",);
    } else {
        println!(
            "  Direct Link:    listening on {} interface{}",
            direct_interface_count,
            if direct_interface_count == 1 { "" } else { "s" },
        );
    }

    println!("  Protocol:       {}", MANAGEMENT_PROTOCOL_VERSION,);

    println!("  Capabilities:   sender, receiver");

    println!(
        "  State:          {}",
        if jobs.is_busy()? {
            AgentState::Busy
        } else {
            AgentState::Idle
        }
        .label(),
    );

    println!();

    println!("WARNING: management mode is unauthenticated.");

    println!("Use it only on a known, trusted local network.");

    println!();

    println!("Waiting for management discovery probes...");

    loop {
        if let Some(source) = respond_once(&socket, &descriptor, &jobs)? {
            println!("  Advertised to {source}");
        }
    }
}

pub fn discover() -> io::Result<Vec<DiscoveredAgent>> {
    let target = SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::BROADCAST,
        MANAGEMENT_DISCOVERY_PORT,
    ));

    discover_target(target, DISCOVERY_TIMEOUT)
}

fn discover_target(target: SocketAddr, timeout: Duration) -> io::Result<Vec<DiscoveredAgent>> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "management discovery timeout must not be zero",
        ));
    }

    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;

    socket.set_broadcast(true)?;

    socket.set_read_timeout(Some(DISCOVERY_POLL_TIMEOUT))?;

    let nonce = create_nonce();

    let probe = DiscoveryPacket::Probe { nonce }.encode()?;

    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::other("management discovery deadline overflowed"))?;

    let mut attempts = 0_usize;

    let mut next_probe = Instant::now();

    let mut agents = Vec::<DiscoveredAgent>::new();

    loop {
        let now = Instant::now();

        if now >= deadline {
            break;
        }

        if attempts < DISCOVERY_ATTEMPTS && now >= next_probe {
            socket.send_to(&probe, target)?;

            attempts += 1;

            next_probe = now.checked_add(DISCOVERY_RETRY_DELAY).ok_or_else(|| {
                io::Error::other("management discovery retry deadline overflowed")
            })?;
        }

        let mut buffer = [0_u8; 512];

        let (received, source) = match socket.recv_from(&mut buffer) {
            Ok(value) => value,

            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::ConnectionReset
                ) =>
            {
                continue;
            }

            Err(error) => {
                return Err(error);
            }
        };

        let Ok(packet) = DiscoveryPacket::decode(&buffer[..received]) else {
            continue;
        };

        let DiscoveryPacket::Offer {
            nonce: offer_nonce,
            control_port,
            state,
            capabilities,
            hostname,
        } = packet
        else {
            continue;
        };

        if offer_nonce != nonce {
            continue;
        }

        let endpoint = SocketAddr::new(source.ip(), control_port);

        let agent = DiscoveredAgent {
            hostname,
            endpoint,
            protocol_version: MANAGEMENT_PROTOCOL_VERSION,
            state,
            capabilities,
        };

        if agents.iter().any(|existing| {
            existing.hostname == agent.hostname && existing.endpoint == agent.endpoint
        }) {
            continue;
        }

        agents.push(agent);
    }

    agents.sort_by(|left, right| {
        left.hostname
            .cmp(&right.hostname)
            .then_with(|| left.endpoint.to_string().cmp(&right.endpoint.to_string()))
    });

    Ok(agents)
}

fn respond_once(
    socket: &UdpSocket,
    descriptor: &LocalAgentDescriptor,
    jobs: &ManagementJobRegistry,
) -> io::Result<Option<SocketAddr>> {
    let mut buffer = [0_u8; 512];

    let (received, source) = socket.recv_from(&mut buffer)?;

    let Ok(packet) = DiscoveryPacket::decode(&buffer[..received]) else {
        return Ok(None);
    };

    let DiscoveryPacket::Probe { nonce } = packet else {
        return Ok(None);
    };

    let offer = DiscoveryPacket::Offer {
        nonce,

        control_port: descriptor.control_port,

        state: if jobs.is_busy()? {
            AgentState::Busy
        } else {
            AgentState::Idle
        },

        capabilities: descriptor.capabilities,

        hostname: descriptor.hostname.clone(),
    }
    .encode()?;

    socket.send_to(&offer, source)?;

    Ok(Some(source))
}

fn local_hostname() -> io::Result<String> {
    let hostname = env::var_os("COMPUTERNAME")
        .or_else(|| env::var_os("HOSTNAME"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Windows computer name is unavailable",
            )
        })?
        .to_string_lossy()
        .trim()
        .to_owned();

    validate_hostname(&hostname)?;

    Ok(hostname)
}

fn validate_hostname(hostname: &str) -> io::Result<()> {
    if hostname.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "management hostname must not be empty",
        ));
    }

    if hostname.len() > MAX_HOSTNAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "management hostname contains {} bytes, exceeding the {MAX_HOSTNAME_BYTES} byte limit",
                hostname.len(),
            ),
        ));
    }

    Ok(())
}

fn create_nonce() -> u64 {
    let sequence = NEXT_NONCE.fetch_add(1, Ordering::Relaxed);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);

    timestamp ^ (u64::from(process::id()) << 32) ^ sequence
}

#[cfg(test)]
mod tests {
    use super::{
        AgentCapabilities, AgentState, DiscoveryPacket, LocalAgentDescriptor, discover_target,
        respond_once,
    };
    use crate::management_jobs::ManagementJobRegistry;
    use crate::management_protocol::{MANAGEMENT_CONTROL_PORT, MANAGEMENT_PROTOCOL_VERSION};
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn offer_packet_round_trips() {
        let expected = DiscoveryPacket::Offer {
            nonce: 0x1234_5678_90AB_CDEF,

            control_port: MANAGEMENT_CONTROL_PORT,

            state: AgentState::Busy,

            capabilities: AgentCapabilities::SEND_RECEIVE,

            hostname: "TEST-PC".to_string(),
        };

        let encoded = expected.encode().unwrap();

        let actual = DiscoveryPacket::decode(&encoded).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn probe_packet_round_trips() {
        let expected = DiscoveryPacket::Probe { nonce: 42 };

        let encoded = expected.encode().unwrap();

        let actual = DiscoveryPacket::decode(&encoded).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn packet_rejects_invalid_magic() {
        let mut encoded = DiscoveryPacket::Probe { nonce: 42 }.encode().unwrap();

        encoded[0..4].copy_from_slice(b"NOPE");

        let error = DiscoveryPacket::decode(&encoded).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn packet_rejects_unknown_capabilities() {
        let mut encoded = DiscoveryPacket::Offer {
            nonce: 42,

            control_port: MANAGEMENT_CONTROL_PORT,

            state: AgentState::Idle,

            capabilities: AgentCapabilities::SEND_RECEIVE,

            hostname: "TEST-PC".to_string(),
        }
        .encode()
        .unwrap();

        encoded[19] = 0x80;

        let error = DiscoveryPacket::decode(&encoded).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);
    }

    #[test]
    fn loopback_discovers_agent() {
        let agent_socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();

        agent_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let target = agent_socket.local_addr().unwrap();

        let descriptor = LocalAgentDescriptor {
            hostname: "LOOPBACK-PC".to_string(),

            control_port: MANAGEMENT_CONTROL_PORT,

            capabilities: AgentCapabilities::SEND_RECEIVE,
        };

        let jobs = Arc::new(ManagementJobRegistry::new().unwrap());

        let server_jobs = Arc::clone(&jobs);

        let server = thread::spawn(move || {
            respond_once(&agent_socket, &descriptor, &server_jobs).unwrap();
        });

        let agents = discover_target(target, Duration::from_millis(500)).unwrap();

        server.join().unwrap();

        assert_eq!(agents.len(), 1);

        assert_eq!(agents[0].hostname, "LOOPBACK-PC",);

        assert_eq!(
            agents[0].endpoint,
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), MANAGEMENT_CONTROL_PORT,),
        );

        assert_eq!(agents[0].protocol_version, MANAGEMENT_PROTOCOL_VERSION,);

        assert!(agents[0].capabilities.can_send(),);

        assert!(agents[0].capabilities.can_receive(),);

        assert_eq!(agents[0].state, AgentState::Idle,);
    }
}
