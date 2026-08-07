use crate::management_control;
use crate::management_direct;
use crate::management_instance::AgentInstanceId;
use crate::management_jobs::ManagementJobRegistry;
use crate::management_protocol::{
    MANAGEMENT_CONTROL_PORT, MANAGEMENT_DISCOVERY_PORT, MANAGEMENT_PROTOCOL_VERSION,
};
use std::collections::BTreeMap;
use std::env;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::process;
use std::ptr;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, GetUnicastIpAddressTable, MIB_IF_ROW2, MIB_IF_TABLE2,
    MIB_UNICASTIPADDRESS_ROW, MIB_UNICASTIPADDRESS_TABLE,
};
use windows_sys::Win32::Networking::WinSock::AF_INET;

const DISCOVERY_MAGIC: [u8; 4] = *b"NMD1";
const DISCOVERY_VERSION: u8 = 1;

const DISCOVERY_HEADER_BYTES: usize = 24;
const MAX_HOSTNAME_BYTES: usize = 255;

const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1_500);

const DISCOVERY_POLL_TIMEOUT: Duration = Duration::from_millis(100);

const DISCOVERY_RETRY_DELAY: Duration = Duration::from_millis(300);

const DISCOVERY_ATTEMPTS: usize = 3;

const NO_ERROR: u32 = 0;

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_TUNNEL: u32 = 131;

const IF_OPER_STATUS_UP: i32 = 1;
const MEDIA_CONNECT_STATE_CONNECTED: i32 = 1;

const IF_FLAG_HARDWARE_INTERFACE: u8 = 1 << 0;
const IF_FLAG_FILTER_INTERFACE: u8 = 1 << 1;
const IF_FLAG_CONNECTOR_PRESENT: u8 = 1 << 2;
const IF_FLAG_ENDPOINT_INTERFACE: u8 = 1 << 7;

const MIN_LOCAL_AFFINITY_PREFIX_BITS: u32 = 8;

const WSAEADDRNOTAVAIL: i32 = 10049;
const WSAENETDOWN: i32 = 10050;
const WSAENETUNREACH: i32 = 10051;
const WSAEHOSTDOWN: i32 = 10064;
const WSAEHOSTUNREACH: i32 = 10065;

static NEXT_NONCE: AtomicU64 = AtomicU64::new(1);

struct UnicastAddressTable(*mut MIB_UNICASTIPADDRESS_TABLE);

impl Drop for UnicastAddressTable {
    fn drop(&mut self) {
        unsafe {
            FreeMibTable(self.0.cast());
        }
    }
}

struct InterfaceTable(*mut MIB_IF_TABLE2);

impl Drop for InterfaceTable {
    fn drop(&mut self) {
        unsafe {
            FreeMibTable(self.0.cast());
        }
    }
}

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
    const DESKTOP_LAYOUT_BIT: u8 = 0x04;

    const KNOWN_BITS: u8 = Self::SEND_BIT | Self::RECEIVE_BIT | Self::DESKTOP_LAYOUT_BIT;

    pub const SENDER: Self = Self {
        bits: Self::SEND_BIT,
    };

    pub const RECEIVER: Self = Self {
        bits: Self::RECEIVE_BIT,
    };

    pub const SEND_RECEIVE: Self = Self {
        bits: Self::SEND_BIT | Self::RECEIVE_BIT,
    };

    pub const SEND_RECEIVE_DESKTOP_LAYOUT: Self = Self {
        bits: Self::SEND_BIT | Self::RECEIVE_BIT | Self::DESKTOP_LAYOUT_BIT,
    };

    pub const fn can_send(self) -> bool {
        self.bits & Self::SEND_BIT != 0
    }

    pub const fn can_receive(self) -> bool {
        self.bits & Self::RECEIVE_BIT != 0
    }

    pub const fn supports_desktop_layout(self) -> bool {
        self.bits & Self::DESKTOP_LAYOUT_BIT != 0
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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum DiscoveredAgentIdentity {
    Instance(AgentInstanceId),

    Endpoint {
        hostname: String,

        endpoint: SocketAddr,
    },
}

#[derive(Clone, Debug)]
struct ResolvedDiscoveredAgent {
    agent: DiscoveredAgent,

    identity: DiscoveredAgentIdentity,
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

            capabilities: AgentCapabilities::SEND_RECEIVE_DESKTOP_LAYOUT,
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
    let targets = discovery_targets()?;

    let agents = discover_targets(&targets, DISCOVERY_TIMEOUT)?;

    deduplicate_agent_instances(agents)
}

fn discovery_targets() -> io::Result<Vec<SocketAddr>> {
    let mut targets = ipv4_directed_broadcast_targets()?;

    // Retain the original limited broadcast as a fallback for unusual
    // interface configurations. Directed subnet broadcasts are sent first so
    // Windows can route each probe through its matching adapter.
    let limited_broadcast = SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::BROADCAST,
        MANAGEMENT_DISCOVERY_PORT,
    ));

    if !targets.contains(&limited_broadcast) {
        targets.push(limited_broadcast);
    }

    Ok(targets)
}

fn ipv4_directed_broadcast_targets() -> io::Result<Vec<SocketAddr>> {
    let mut raw_table = ptr::null_mut();

    let status = unsafe { GetUnicastIpAddressTable(AF_INET, &mut raw_table) };

    if status != NO_ERROR {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    if raw_table.is_null() {
        return Err(io::Error::other(
            "GetUnicastIpAddressTable returned a null IPv4 table",
        ));
    }

    let table = UnicastAddressTable(raw_table);

    let entry_count = unsafe { (*table.0).NumEntries as usize };

    let first_entry = unsafe { (*table.0).Table.as_ptr() };

    let rows = unsafe { slice::from_raw_parts(first_entry, entry_count) };

    let mut targets = rows
        .iter()
        .filter_map(|row| {
            let address = ipv4_address(row)?;

            let broadcast = directed_broadcast_address(address, row.OnLinkPrefixLength)?;

            Some(SocketAddr::V4(SocketAddrV4::new(
                broadcast,
                MANAGEMENT_DISCOVERY_PORT,
            )))
        })
        .collect::<Vec<_>>();

    targets.sort_by_key(|target| target.to_string());
    targets.dedup();

    Ok(targets)
}

fn ipv4_address(row: &MIB_UNICASTIPADDRESS_ROW) -> Option<Ipv4Addr> {
    let family = unsafe { row.Address.si_family };

    if family != AF_INET {
        return None;
    }

    let socket_address = unsafe { row.Address.Ipv4 };

    let address_bytes = unsafe { socket_address.sin_addr.S_un.S_addr }.to_ne_bytes();

    Some(Ipv4Addr::from(address_bytes))
}

fn local_ipv4_interface_ranks() -> io::Result<BTreeMap<Ipv4Addr, u8>> {
    let interface_ranks = automatic_lan_interface_ranks()?;

    let mut raw_table = ptr::null_mut();

    let status = unsafe { GetUnicastIpAddressTable(AF_INET, &mut raw_table) };

    if status != NO_ERROR {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    if raw_table.is_null() {
        return Err(io::Error::other(
            "GetUnicastIpAddressTable returned a null IPv4 table",
        ));
    }

    let table = UnicastAddressTable(raw_table);

    let entry_count = unsafe { (*table.0).NumEntries as usize };

    let first_entry = unsafe { (*table.0).Table.as_ptr() };

    let rows = unsafe { slice::from_raw_parts(first_entry, entry_count) };

    let mut ranks = BTreeMap::<Ipv4Addr, u8>::new();

    for row in rows {
        let Some(address) = ipv4_address(row) else {
            continue;
        };

        let rank = interface_ranks
            .get(&row.InterfaceIndex)
            .copied()
            .unwrap_or_default();

        ranks
            .entry(address)
            .and_modify(|existing| {
                *existing = (*existing).max(rank);
            })
            .or_insert(rank);
    }

    Ok(ranks)
}

fn automatic_lan_interface_ranks() -> io::Result<BTreeMap<u32, u8>> {
    let mut raw_table = ptr::null_mut();

    let status = unsafe { GetIfTable2(&mut raw_table) };

    if status != NO_ERROR {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    if raw_table.is_null() {
        return Err(io::Error::other(
            "GetIfTable2 returned a null interface table",
        ));
    }

    let table = InterfaceTable(raw_table);

    let entry_count = unsafe { (*table.0).NumEntries as usize };

    let first_entry = unsafe { (*table.0).Table.as_ptr() };

    let rows = unsafe { slice::from_raw_parts(first_entry, entry_count) };

    Ok(rows
        .iter()
        .map(|row| (row.InterfaceIndex, automatic_lan_interface_rank(row)))
        .collect())
}

fn automatic_lan_interface_rank(row: &MIB_IF_ROW2) -> u8 {
    let flags = row.InterfaceAndOperStatusFlags._bitfield;

    if flags & IF_FLAG_FILTER_INTERFACE != 0
        || flags & IF_FLAG_ENDPOINT_INTERFACE != 0
        || row.Type == IF_TYPE_SOFTWARE_LOOPBACK
        || row.Type == IF_TYPE_TUNNEL
        || row.OperStatus != IF_OPER_STATUS_UP
        || row.MediaConnectState != MEDIA_CONNECT_STATE_CONNECTED
    {
        return 0;
    }

    let hardware = flags & IF_FLAG_HARDWARE_INTERFACE != 0;

    let connector = flags & IF_FLAG_CONNECTOR_PRESENT != 0;

    match (hardware, connector) {
        (true, true) => 3,

        (true, false) => 2,

        (false, _) => 1,
    }
}

fn directed_broadcast_address(address: Ipv4Addr, prefix_length: u8) -> Option<Ipv4Addr> {
    if prefix_length == 0 || prefix_length > 30 {
        return None;
    }

    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address == Ipv4Addr::BROADCAST
    {
        return None;
    }

    let address_bits = u32::from_be_bytes(address.octets());

    let host_mask = u32::MAX >> u32::from(prefix_length);

    let broadcast_bits = address_bits | host_mask;

    let broadcast = Ipv4Addr::from(broadcast_bits.to_be_bytes());

    if broadcast == address {
        return None;
    }

    Some(broadcast)
}

fn is_skippable_discovery_send_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(WSAEADDRNOTAVAIL | WSAENETDOWN | WSAENETUNREACH | WSAEHOSTDOWN | WSAEHOSTUNREACH)
    )
}

#[cfg(test)]
fn discover_target(target: SocketAddr, timeout: Duration) -> io::Result<Vec<DiscoveredAgent>> {
    discover_targets(&[target], timeout)
}

fn discover_targets(targets: &[SocketAddr], timeout: Duration) -> io::Result<Vec<DiscoveredAgent>> {
    if targets.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "management discovery requires at least one target",
        ));
    }

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
            let mut sent_to_any_target = false;

            let mut last_skipped_error = None;

            for target in targets {
                match socket.send_to(&probe, target) {
                    Ok(_) => {
                        sent_to_any_target = true;
                    }

                    Err(error) if is_skippable_discovery_send_error(&error) => {
                        last_skipped_error = Some(error);
                    }

                    Err(error) => {
                        return Err(error);
                    }
                }
            }

            if !sent_to_any_target {
                return Err(last_skipped_error.unwrap_or_else(|| {
                    io::Error::other("management discovery could not send to any target")
                }));
            }

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

fn deduplicate_agent_instances(agents: Vec<DiscoveredAgent>) -> io::Result<Vec<DiscoveredAgent>> {
    let mut hostname_counts = BTreeMap::<String, usize>::new();

    for agent in &agents {
        *hostname_counts.entry(agent.hostname.clone()).or_default() += 1;
    }

    let mut resolved = Vec::with_capacity(agents.len());

    let mut workers = Vec::new();

    for agent in agents {
        let hostname_count = hostname_counts
            .get(&agent.hostname)
            .copied()
            .unwrap_or_default();

        if hostname_count < 2 {
            let identity = DiscoveredAgentIdentity::Endpoint {
                hostname: agent.hostname.clone(),

                endpoint: agent.endpoint,
            };

            resolved.push(ResolvedDiscoveredAgent { agent, identity });

            continue;
        }

        let worker = thread::Builder::new()
            .name("networkcopy-management-discovery-identity".to_string())
            .spawn(move || resolve_discovered_agent_identity(agent))?;

        workers.push(worker);
    }

    for worker in workers {
        let resolved_agent = worker
            .join()
            .map_err(|_| io::Error::other("management discovery identity worker panicked"))?;

        resolved.push(resolved_agent);
    }

    Ok(deduplicate_resolved_agents(resolved))
}

fn resolve_discovered_agent_identity(agent: DiscoveredAgent) -> ResolvedDiscoveredAgent {
    let endpoint = agent.endpoint;

    let identity = management_control::agent_snapshot(endpoint)
        .map(|snapshot| DiscoveredAgentIdentity::Instance(snapshot.agent_instance_id))
        .unwrap_or_else(|_| DiscoveredAgentIdentity::Endpoint {
            hostname: agent.hostname.clone(),

            endpoint,
        });

    ResolvedDiscoveredAgent { agent, identity }
}

fn deduplicate_resolved_agents(agents: Vec<ResolvedDiscoveredAgent>) -> Vec<DiscoveredAgent> {
    let local_interface_ranks = local_ipv4_interface_ranks().unwrap_or_default();

    deduplicate_resolved_agents_with_local_interface_ranks(agents, &local_interface_ranks)
}

fn deduplicate_resolved_agents_with_local_interface_ranks(
    agents: Vec<ResolvedDiscoveredAgent>,
    local_interface_ranks: &BTreeMap<Ipv4Addr, u8>,
) -> Vec<DiscoveredAgent> {
    let route_catalog = agents
        .iter()
        .map(|resolved| (resolved.identity.clone(), resolved.agent.endpoint))
        .collect::<Vec<_>>();

    let mut groups = BTreeMap::<DiscoveredAgentIdentity, Vec<ResolvedDiscoveredAgent>>::new();

    for resolved in agents {
        groups
            .entry(resolved.identity.clone())
            .or_default()
            .push(resolved);
    }

    let mut deduplicated = Vec::with_capacity(groups.len());

    for (identity, mut candidates) in groups {
        candidates.sort_by_key(|candidate| candidate.agent.endpoint.to_string());

        let preferred_index = preferred_candidate_index(
            &identity,
            &candidates,
            &route_catalog,
            local_interface_ranks,
        );

        deduplicated.push(candidates.swap_remove(preferred_index).agent);
    }

    deduplicated.sort_by(|left, right| {
        left.hostname
            .cmp(&right.hostname)
            .then_with(|| left.endpoint.to_string().cmp(&right.endpoint.to_string()))
    });

    deduplicated
}

fn preferred_candidate_index(
    identity: &DiscoveredAgentIdentity,
    candidates: &[ResolvedDiscoveredAgent],
    route_catalog: &[(DiscoveredAgentIdentity, SocketAddr)],
    local_interface_ranks: &BTreeMap<Ipv4Addr, u8>,
) -> usize {
    debug_assert!(!candidates.is_empty());

    let mut preferred_index = 0_usize;

    let mut preferred_score = candidate_preference_score(
        identity,
        candidates[0].agent.endpoint,
        route_catalog,
        local_interface_ranks,
    );

    for (index, candidate) in candidates.iter().enumerate().skip(1) {
        let score = candidate_preference_score(
            identity,
            candidate.agent.endpoint,
            route_catalog,
            local_interface_ranks,
        );

        if score > preferred_score {
            preferred_index = index;

            preferred_score = score;
        }
    }

    preferred_index
}

fn candidate_preference_score(
    identity: &DiscoveredAgentIdentity,
    endpoint: SocketAddr,
    route_catalog: &[(DiscoveredAgentIdentity, SocketAddr)],
    local_interface_ranks: &BTreeMap<Ipv4Addr, u8>,
) -> (u8, u8, u32, u32, u32) {
    let SocketAddr::V4(endpoint) = endpoint else {
        return (0, 0, 0, 0, 0);
    };

    let address = *endpoint.ip();

    let address_rank = automatic_lan_address_rank(address);

    let (local_interface_rank, local_prefix) =
        local_network_affinity_score(address, local_interface_ranks);

    let (remote_maximum_prefix, remote_total_prefix) =
        route_affinity_score(identity, SocketAddr::V4(endpoint), route_catalog);

    (
        address_rank,
        local_interface_rank,
        local_prefix,
        remote_maximum_prefix,
        remote_total_prefix,
    )
}

fn automatic_lan_address_rank(address: Ipv4Addr) -> u8 {
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address == Ipv4Addr::BROADCAST
    {
        return 0;
    }

    if address.is_link_local() { 1 } else { 2 }
}

fn local_network_affinity_score(
    candidate: Ipv4Addr,
    local_interface_ranks: &BTreeMap<Ipv4Addr, u8>,
) -> (u8, u32) {
    let mut best_prefix = 0_u32;

    let mut best_interface_rank = 0_u8;

    for (local_address, interface_rank) in local_interface_ranks {
        let prefix = common_ipv4_prefix_bits(candidate, *local_address);

        if prefix > best_prefix || (prefix == best_prefix && *interface_rank > best_interface_rank)
        {
            best_prefix = prefix;

            best_interface_rank = *interface_rank;
        }
    }

    if best_prefix < MIN_LOCAL_AFFINITY_PREFIX_BITS {
        return (0, 0);
    }

    (best_interface_rank, best_prefix)
}

fn route_affinity_score(
    identity: &DiscoveredAgentIdentity,
    endpoint: SocketAddr,
    route_catalog: &[(DiscoveredAgentIdentity, SocketAddr)],
) -> (u32, u32) {
    let SocketAddr::V4(candidate_endpoint) = endpoint else {
        return (0, 0);
    };

    let mut maximum_prefix = 0_u32;

    let mut total_prefix = 0_u32;

    for (other_identity, other_endpoint) in route_catalog {
        if other_identity == identity {
            continue;
        }

        let SocketAddr::V4(other_endpoint) = *other_endpoint else {
            continue;
        };

        let prefix = common_ipv4_prefix_bits(*candidate_endpoint.ip(), *other_endpoint.ip());

        maximum_prefix = maximum_prefix.max(prefix);

        total_prefix = total_prefix.saturating_add(prefix);
    }

    (maximum_prefix, total_prefix)
}

fn common_ipv4_prefix_bits(left: Ipv4Addr, right: Ipv4Addr) -> u32 {
    let left_bits = u32::from_be_bytes(left.octets());

    let right_bits = u32::from_be_bytes(right.octets());

    (left_bits ^ right_bits).leading_zeros()
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
        AgentCapabilities, AgentState, DiscoveredAgent, DiscoveredAgentIdentity, DiscoveryPacket,
        LocalAgentDescriptor, ResolvedDiscoveredAgent, WSAENETUNREACH,
        deduplicate_resolved_agents_with_local_interface_ranks, directed_broadcast_address,
        discover_target, is_skippable_discovery_send_error, respond_once,
    };

    #[test]
    fn unreachable_discovery_target_is_skippable() {
        let error = io::Error::from_raw_os_error(WSAENETUNREACH);

        assert!(is_skippable_discovery_send_error(&error));
    }

    #[test]
    fn unexpected_discovery_send_error_is_not_skipped() {
        let error = io::Error::from_raw_os_error(5);

        assert!(!is_skippable_discovery_send_error(&error));
    }

    #[test]
    fn slash_24_directed_broadcast_is_calculated() {
        assert_eq!(
            directed_broadcast_address(Ipv4Addr::new(192, 168, 2, 200), 24,),
            Some(Ipv4Addr::new(192, 168, 2, 255)),
        );
    }

    #[test]
    fn slash_16_directed_broadcast_is_calculated() {
        assert_eq!(
            directed_broadcast_address(Ipv4Addr::new(172, 20, 224, 1), 16,),
            Some(Ipv4Addr::new(172, 20, 255, 255)),
        );
    }

    #[test]
    fn point_to_point_prefix_has_no_broadcast() {
        assert_eq!(
            directed_broadcast_address(Ipv4Addr::new(192, 168, 2, 200), 31,),
            None,
        );
    }

    #[test]
    fn loopback_address_is_not_probed() {
        assert_eq!(directed_broadcast_address(Ipv4Addr::LOCALHOST, 8,), None,);
    }
    use crate::management_instance::AgentInstanceId;
    use crate::management_jobs::ManagementJobRegistry;
    use crate::management_protocol::{MANAGEMENT_CONTROL_PORT, MANAGEMENT_PROTOCOL_VERSION};
    use std::collections::BTreeMap;
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
    fn duplicate_instance_prefers_shared_lan_route() {
        let local_instance = AgentInstanceId::from_raw(1).unwrap();

        let remote_instance = AgentInstanceId::from_raw(2).unwrap();

        let actual = deduplicate_resolved_agents_with_local_interface_ranks(
            vec![
                resolved_test_agent("LOCAL-PC", Ipv4Addr::new(172, 20, 224, 1), local_instance),
                resolved_test_agent("LOCAL-PC", Ipv4Addr::new(192, 168, 124, 1), local_instance),
                resolved_test_agent("LOCAL-PC", Ipv4Addr::new(192, 168, 2, 200), local_instance),
                resolved_test_agent(
                    "REMOTE-PC",
                    Ipv4Addr::new(192, 168, 2, 103),
                    remote_instance,
                ),
            ],
            &BTreeMap::new(),
        );

        assert_eq!(actual.len(), 2);

        let local = actual
            .iter()
            .find(|agent| agent.hostname == "LOCAL-PC")
            .unwrap();

        assert_eq!(
            local.endpoint,
            SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(192, 168, 2, 200),
                MANAGEMENT_CONTROL_PORT,
            )),
        );
    }

    #[test]
    fn duplicate_instance_prefers_physical_lan_over_apipa_and_virtual() {
        let instance = AgentInstanceId::from_raw(1).unwrap();

        let apipa = Ipv4Addr::new(169, 254, 21, 253);

        let virtual_adapter = Ipv4Addr::new(172, 20, 96, 1);

        let physical_lan = Ipv4Addr::new(192, 168, 1, 2);

        let local_interface_ranks =
            BTreeMap::from([(apipa, 3), (virtual_adapter, 1), (physical_lan, 3)]);

        let actual = deduplicate_resolved_agents_with_local_interface_ranks(
            vec![
                resolved_test_agent("DESKTOP-05LE4I", apipa, instance),
                resolved_test_agent("DESKTOP-05LE4I", virtual_adapter, instance),
                resolved_test_agent("DESKTOP-05LE4I", physical_lan, instance),
            ],
            &local_interface_ranks,
        );

        assert_eq!(actual.len(), 1);

        assert_eq!(
            actual[0].endpoint,
            SocketAddr::V4(SocketAddrV4::new(physical_lan, MANAGEMENT_CONTROL_PORT,),),
        );
    }

    #[test]
    fn duplicate_remote_instance_prefers_subnet_reached_through_physical_interface() {
        let remote_instance = AgentInstanceId::from_raw(2).unwrap();

        let local_interface_ranks = BTreeMap::from([
            (Ipv4Addr::new(172, 20, 96, 1), 1),
            (Ipv4Addr::new(192, 168, 1, 2), 3),
        ]);

        let actual = deduplicate_resolved_agents_with_local_interface_ranks(
            vec![
                resolved_test_agent("REMOTE-PC", Ipv4Addr::new(172, 20, 96, 50), remote_instance),
                resolved_test_agent("REMOTE-PC", Ipv4Addr::new(192, 168, 1, 50), remote_instance),
            ],
            &local_interface_ranks,
        );

        assert_eq!(actual.len(), 1);

        assert_eq!(
            actual[0].endpoint,
            SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(192, 168, 1, 50,),
                MANAGEMENT_CONTROL_PORT,
            ),),
        );
    }

    #[test]
    fn identical_hostnames_with_distinct_instances_remain() {
        let actual = deduplicate_resolved_agents_with_local_interface_ranks(
            vec![
                resolved_test_agent(
                    "SAME-NAME",
                    Ipv4Addr::new(192, 168, 2, 10),
                    AgentInstanceId::from_raw(10).unwrap(),
                ),
                resolved_test_agent(
                    "SAME-NAME",
                    Ipv4Addr::new(192, 168, 2, 20),
                    AgentInstanceId::from_raw(20).unwrap(),
                ),
            ],
            &BTreeMap::new(),
        );

        assert_eq!(actual.len(), 2);
    }

    fn resolved_test_agent(
        hostname: &str,
        address: Ipv4Addr,
        instance_id: AgentInstanceId,
    ) -> ResolvedDiscoveredAgent {
        ResolvedDiscoveredAgent {
            agent: DiscoveredAgent {
                hostname: hostname.to_string(),

                endpoint: SocketAddr::V4(SocketAddrV4::new(address, MANAGEMENT_CONTROL_PORT)),

                protocol_version: MANAGEMENT_PROTOCOL_VERSION,

                state: AgentState::Idle,

                capabilities: AgentCapabilities::SEND_RECEIVE,
            },

            identity: DiscoveredAgentIdentity::Instance(instance_id),
        }
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
