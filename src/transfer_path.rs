use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::ptr;
use std::slice;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, GetUnicastIpAddressTable, MIB_IF_ROW2, MIB_IF_TABLE2,
    MIB_UNICASTIPADDRESS_ROW, MIB_UNICASTIPADDRESS_TABLE,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

const NO_ERROR: u32 = 0;

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_PPP: u32 = 23;
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_PROP_VIRTUAL: u32 = 53;
const IF_TYPE_IEEE80211: u32 = 71;
const IF_TYPE_TUNNEL: u32 = 131;

const NDIS_MEDIUM_TUNNEL: i32 = 15;
const NDIS_MEDIUM_NATIVE_802_11: i32 = 16;
const NDIS_MEDIUM_LOOPBACK: i32 = 17;

const NDIS_PHYSICAL_MEDIUM_WIRELESS_LAN: i32 = 1;
const NDIS_PHYSICAL_MEDIUM_NATIVE_802_11: i32 = 9;
const NDIS_PHYSICAL_MEDIUM_802_3: i32 = 14;

const IF_FLAG_HARDWARE_INTERFACE: u8 = 1 << 0;
const IF_FLAG_FILTER_INTERFACE: u8 = 1 << 1;
const IF_FLAG_CONNECTOR_PRESENT: u8 = 1 << 2;
const IF_FLAG_ENDPOINT_INTERFACE: u8 = 1 << 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferPathKind {
    PhysicalEthernet,
    Wifi,
    Tunnel,
    Virtual,
    Loopback,
    Unknown,
}

impl fmt::Display for TransferPathKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PhysicalEthernet => "physical Ethernet",
            Self::Wifi => "Wi-Fi",
            Self::Tunnel => "VPN/tunnel",
            Self::Virtual => "virtual",
            Self::Loopback => "loopback",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferPath {
    pub kind: TransferPathKind,
    pub local_address: Option<IpAddr>,
    pub interface_index: Option<u32>,
    pub interface_alias: Option<String>,
    pub mtu: Option<u32>,
    pub transmit_link_speed_bps: Option<u64>,
    pub receive_link_speed_bps: Option<u64>,
}

impl Default for TransferPath {
    fn default() -> Self {
        Self {
            kind: TransferPathKind::Unknown,
            local_address: None,
            interface_index: None,
            interface_alias: None,
            mtu: None,
            transmit_link_speed_bps: None,
            receive_link_speed_bps: None,
        }
    }
}

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
struct InterfaceFacts {
    interface_type: u32,
    media_type: i32,
    physical_medium_type: i32,
    flags: u8,
}

pub fn classify_tcp_stream(stream: &TcpStream) -> TransferPath {
    inspect_tcp_stream(stream).unwrap_or_default()
}

pub fn inspect_tcp_stream(stream: &TcpStream) -> io::Result<TransferPath> {
    let local = stream.local_addr()?;
    let local_address = local.ip();

    if local_address.is_loopback() {
        return Ok(TransferPath {
            kind: TransferPathKind::Loopback,
            local_address: Some(local_address),
            ..TransferPath::default()
        });
    }

    let Some(interface_index) = interface_index_for_local_address(local)? else {
        return Ok(TransferPath {
            local_address: Some(local_address),
            ..TransferPath::default()
        });
    };

    let Some(mut path) = path_for_interface(interface_index)? else {
        return Ok(TransferPath {
            local_address: Some(local_address),
            interface_index: Some(interface_index),
            ..TransferPath::default()
        });
    };

    path.local_address = Some(local_address);

    Ok(path)
}

fn interface_index_for_local_address(local: SocketAddr) -> io::Result<Option<u32>> {
    if let SocketAddr::V6(address) = local
        && address.scope_id() != 0
    {
        return Ok(Some(address.scope_id()));
    }

    let family = match local {
        SocketAddr::V4(_) => AF_INET,
        SocketAddr::V6(_) => AF_INET6,
    };

    let mut raw_table = ptr::null_mut();

    let status = unsafe { GetUnicastIpAddressTable(family, &mut raw_table) };

    if status != NO_ERROR {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    if raw_table.is_null() {
        return Err(io::Error::other(
            "GetUnicastIpAddressTable returned a null table",
        ));
    }

    let table = UnicastAddressTable(raw_table);

    let entry_count = unsafe { (*table.0).NumEntries as usize };

    let first_entry = unsafe { (*table.0).Table.as_ptr() };

    let rows = unsafe { slice::from_raw_parts(first_entry, entry_count) };

    let mut interface_index = None;

    for row in rows {
        let matches = match local {
            SocketAddr::V4(address) => ipv4_address(row) == Some(*address.ip()),
            SocketAddr::V6(address) => ipv6_address(row) == Some(*address.ip()),
        };

        if !matches {
            continue;
        }

        match interface_index {
            Some(existing) if existing != row.InterfaceIndex => {
                return Ok(None);
            }
            Some(_) => {}
            None => {
                interface_index = Some(row.InterfaceIndex);
            }
        }
    }

    Ok(interface_index)
}

fn path_for_interface(interface_index: u32) -> io::Result<Option<TransferPath>> {
    let mut raw_table = ptr::null_mut();

    let status = unsafe { GetIfTable2(&mut raw_table) };

    if status != NO_ERROR {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    if raw_table.is_null() {
        return Err(io::Error::other("GetIfTable2 returned a null table"));
    }

    let table = InterfaceTable(raw_table);

    let entry_count = unsafe { (*table.0).NumEntries as usize };

    let first_entry = unsafe { (*table.0).Table.as_ptr() };

    let rows = unsafe { slice::from_raw_parts(first_entry, entry_count) };

    let Some(row) = rows
        .iter()
        .find(|row| row.InterfaceIndex == interface_index)
    else {
        return Ok(None);
    };

    let facts = InterfaceFacts {
        interface_type: row.Type,
        media_type: row.MediaType,
        physical_medium_type: row.PhysicalMediumType,
        flags: row.InterfaceAndOperStatusFlags._bitfield,
    };

    Ok(Some(TransferPath {
        kind: classify_interface(facts),
        local_address: None,
        interface_index: Some(interface_index),
        interface_alias: decode_wide_string(&row.Alias),
        mtu: (row.Mtu != 0).then_some(row.Mtu),
        transmit_link_speed_bps: (row.TransmitLinkSpeed != 0).then_some(row.TransmitLinkSpeed),
        receive_link_speed_bps: (row.ReceiveLinkSpeed != 0).then_some(row.ReceiveLinkSpeed),
    }))
}

fn classify_interface(facts: InterfaceFacts) -> TransferPathKind {
    if facts.interface_type == IF_TYPE_SOFTWARE_LOOPBACK || facts.media_type == NDIS_MEDIUM_LOOPBACK
    {
        return TransferPathKind::Loopback;
    }

    if facts.interface_type == IF_TYPE_TUNNEL
        || facts.interface_type == IF_TYPE_PPP
        || facts.media_type == NDIS_MEDIUM_TUNNEL
    {
        return TransferPathKind::Tunnel;
    }

    let filter = facts.flags & IF_FLAG_FILTER_INTERFACE != 0;
    let endpoint = facts.flags & IF_FLAG_ENDPOINT_INTERFACE != 0;
    let hardware = facts.flags & IF_FLAG_HARDWARE_INTERFACE != 0;
    let connector = facts.flags & IF_FLAG_CONNECTOR_PRESENT != 0;

    if facts.interface_type == IF_TYPE_PROP_VIRTUAL || filter || endpoint || !hardware {
        return TransferPathKind::Virtual;
    }

    if !connector {
        return TransferPathKind::Unknown;
    }

    if facts.interface_type == IF_TYPE_IEEE80211
        || facts.media_type == NDIS_MEDIUM_NATIVE_802_11
        || matches!(
            facts.physical_medium_type,
            NDIS_PHYSICAL_MEDIUM_WIRELESS_LAN | NDIS_PHYSICAL_MEDIUM_NATIVE_802_11
        )
    {
        return TransferPathKind::Wifi;
    }

    if facts.interface_type == IF_TYPE_ETHERNET_CSMACD
        || facts.physical_medium_type == NDIS_PHYSICAL_MEDIUM_802_3
    {
        return TransferPathKind::PhysicalEthernet;
    }

    TransferPathKind::Unknown
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

fn ipv6_address(row: &MIB_UNICASTIPADDRESS_ROW) -> Option<Ipv6Addr> {
    let family = unsafe { row.Address.si_family };

    if family != AF_INET6 {
        return None;
    }

    let socket_address = unsafe { row.Address.Ipv6 };

    let octets = unsafe { socket_address.sin6_addr.u.Byte };

    Some(Ipv6Addr::from(octets))
}

fn decode_wide_string(value: &[u16]) -> Option<String> {
    let length = value
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(value.len());

    if length == 0 {
        return None;
    }

    Some(String::from_utf16_lossy(&value[..length]))
}

#[cfg(test)]
mod tests {
    use super::{
        IF_FLAG_CONNECTOR_PRESENT, IF_FLAG_ENDPOINT_INTERFACE, IF_FLAG_HARDWARE_INTERFACE,
        IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, IF_TYPE_PROP_VIRTUAL, IF_TYPE_TUNNEL,
        InterfaceFacts, NDIS_PHYSICAL_MEDIUM_802_3, NDIS_PHYSICAL_MEDIUM_WIRELESS_LAN,
        TransferPathKind, classify_interface, inspect_tcp_stream,
    };
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn physical(interface_type: u32, physical_medium_type: i32) -> InterfaceFacts {
        InterfaceFacts {
            interface_type,
            media_type: 0,
            physical_medium_type,
            flags: IF_FLAG_HARDWARE_INTERFACE | IF_FLAG_CONNECTOR_PRESENT,
        }
    }

    #[test]
    fn physical_ethernet_is_classified() {
        let facts = physical(IF_TYPE_ETHERNET_CSMACD, NDIS_PHYSICAL_MEDIUM_802_3);

        assert_eq!(
            classify_interface(facts),
            TransferPathKind::PhysicalEthernet,
        );
    }

    #[test]
    fn physical_wifi_is_classified() {
        let facts = physical(IF_TYPE_IEEE80211, NDIS_PHYSICAL_MEDIUM_WIRELESS_LAN);

        assert_eq!(classify_interface(facts), TransferPathKind::Wifi);
    }

    #[test]
    fn tunnel_is_classified_before_virtual_flags() {
        let facts = InterfaceFacts {
            interface_type: IF_TYPE_TUNNEL,
            media_type: 0,
            physical_medium_type: 0,
            flags: 0,
        };

        assert_eq!(classify_interface(facts), TransferPathKind::Tunnel);
    }

    #[test]
    fn software_ethernet_is_virtual() {
        let facts = InterfaceFacts {
            interface_type: IF_TYPE_ETHERNET_CSMACD,
            media_type: 0,
            physical_medium_type: NDIS_PHYSICAL_MEDIUM_802_3,
            flags: 0,
        };

        assert_eq!(classify_interface(facts), TransferPathKind::Virtual);
    }

    #[test]
    fn endpoint_interface_is_virtual() {
        let facts = InterfaceFacts {
            interface_type: IF_TYPE_PROP_VIRTUAL,
            media_type: 0,
            physical_medium_type: 0,
            flags: IF_FLAG_HARDWARE_INTERFACE
                | IF_FLAG_CONNECTOR_PRESENT
                | IF_FLAG_ENDPOINT_INTERFACE,
        };

        assert_eq!(classify_interface(facts), TransferPathKind::Virtual);
    }

    #[test]
    fn connectorless_hardware_stays_unknown() {
        let facts = InterfaceFacts {
            interface_type: IF_TYPE_ETHERNET_CSMACD,
            media_type: 0,
            physical_medium_type: NDIS_PHYSICAL_MEDIUM_802_3,
            flags: IF_FLAG_HARDWARE_INTERFACE,
        };

        assert_eq!(classify_interface(facts), TransferPathKind::Unknown);
    }

    #[test]
    fn real_loopback_tcp_stream_is_classified() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let receiver = thread::spawn(move || listener.accept().unwrap().0);

        let sender = TcpStream::connect(address).unwrap();
        let accepted = receiver.join().unwrap();

        let sender_path = inspect_tcp_stream(&sender).unwrap();
        let receiver_path = inspect_tcp_stream(&accepted).unwrap();

        assert_eq!(sender_path.kind, TransferPathKind::Loopback);
        assert_eq!(receiver_path.kind, TransferPathKind::Loopback);

        drop(sender);
        drop(accepted);
    }
}
