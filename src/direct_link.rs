use std::io;
use std::ptr;
use std::slice;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
};

const NO_ERROR: u32 = 0;

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_IEEE80211: u32 = 71;
const IF_TYPE_TUNNEL: u32 = 131;

const IF_OPER_STATUS_UP: i32 = 1;
const MEDIA_CONNECT_STATE_CONNECTED: i32 = 1;
const NET_IF_CONNECTION_DEDICATED: i32 = 1;

const NDIS_PHYSICAL_MEDIUM_UNSPECIFIED: i32 = 0;
const NDIS_PHYSICAL_MEDIUM_WIRELESS_LAN: i32 = 1;
const NDIS_PHYSICAL_MEDIUM_802_3: i32 = 14;

const IF_FLAG_HARDWARE_INTERFACE: u8 = 1 << 0;
const IF_FLAG_FILTER_INTERFACE: u8 = 1 << 1;
const IF_FLAG_CONNECTOR_PRESENT: u8 = 1 << 2;
const IF_FLAG_ENDPOINT_INTERFACE: u8 = 1 << 7;

struct InterfaceTable(*mut MIB_IF_TABLE2);

impl Drop for InterfaceTable {
    fn drop(&mut self) {
        unsafe {
            FreeMibTable(self.0.cast());
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectLinkEligibility {
    Eligible,
    NeedsReview,
    Rejected,
}

impl DirectLinkEligibility {
    fn label(self) -> &'static str {
        match self {
            Self::Eligible => "ELIGIBLE",

            Self::NeedsReview => "REVIEW",

            Self::Rejected => "REJECTED",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Eligible => 2,
            Self::NeedsReview => 1,
            Self::Rejected => 0,
        }
    }
}

#[derive(Clone, Debug)]
struct DirectLinkInterface {
    interface_index: u32,

    alias: String,

    description: String,

    physical_address: String,

    mtu: u32,

    interface_type: u32,

    physical_medium_type: i32,

    hardware_interface: bool,

    connector_present: bool,

    transmit_link_speed: u64,

    receive_link_speed: u64,

    usable_link_speed: u64,

    eligibility: DirectLinkEligibility,

    reason: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct InterfaceFacts {
    interface_type: u32,

    physical_medium_type: i32,

    hardware_interface: bool,

    filter_interface: bool,

    connector_present: bool,

    endpoint_interface: bool,

    operational_status: i32,

    media_connect_state: i32,

    connection_type: i32,

    physical_address_length: u32,

    transmit_link_speed: u64,

    receive_link_speed: u64,
}

pub(crate) fn print_inventory() -> io::Result<()> {
    let interfaces = enumerate_interfaces()?;

    let eligible_count = interfaces
        .iter()
        .filter(|interface| interface.eligibility == DirectLinkEligibility::Eligible)
        .count();

    println!("NetworkCopy Speed Edition direct-link interface inventory");

    println!("  Interfaces found:  {}", interfaces.len(),);

    println!("  Strict candidates: {}", eligible_count,);

    for interface in interfaces {
        println!();

        println!("[{}] {}", interface.eligibility.label(), interface.alias,);

        println!("  Description:       {}", interface.description,);

        println!("  Interface index:   {}", interface.interface_index,);

        println!("  Physical address:  {}", interface.physical_address,);

        println!("  MTU:               {}", interface.mtu,);

        println!("  Interface type:    {}", interface.interface_type,);

        println!("  Physical medium:   {}", interface.physical_medium_type,);

        println!(
            "  Hardware interface: {}",
            yes_no(interface.hardware_interface),
        );

        println!(
            "  Connector present:  {}",
            yes_no(interface.connector_present),
        );

        println!(
            "  Transmit speed:    {}",
            format_link_speed(interface.transmit_link_speed,),
        );

        println!(
            "  Receive speed:     {}",
            format_link_speed(interface.receive_link_speed,),
        );

        println!(
            "  Usable speed:      {}",
            format_link_speed(interface.usable_link_speed,),
        );

        println!("  Classification:    {}", interface.reason,);
    }

    if eligible_count == 0 {
        println!();

        println!("No strict connected physical Ethernet interface was found.");
    }

    Ok(())
}

fn enumerate_interfaces() -> io::Result<Vec<DirectLinkInterface>> {
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

    let mut interfaces = rows
        .iter()
        .filter(|row| !interface_flag(row, IF_FLAG_FILTER_INTERFACE))
        .map(interface_from_row)
        .collect::<Vec<_>>();

    interfaces.sort_by(|left, right| {
        right
            .eligibility
            .rank()
            .cmp(&left.eligibility.rank())
            .then_with(|| right.usable_link_speed.cmp(&left.usable_link_speed))
            .then_with(|| left.interface_index.cmp(&right.interface_index))
    });

    Ok(interfaces)
}

fn interface_from_row(row: &MIB_IF_ROW2) -> DirectLinkInterface {
    let status_flags = row.InterfaceAndOperStatusFlags._bitfield;

    let facts = InterfaceFacts {
        interface_type: row.Type,

        physical_medium_type: row.PhysicalMediumType,

        hardware_interface: status_flags & IF_FLAG_HARDWARE_INTERFACE != 0,

        filter_interface: status_flags & IF_FLAG_FILTER_INTERFACE != 0,

        connector_present: status_flags & IF_FLAG_CONNECTOR_PRESENT != 0,

        endpoint_interface: status_flags & IF_FLAG_ENDPOINT_INTERFACE != 0,

        operational_status: row.OperStatus,

        media_connect_state: row.MediaConnectState,

        connection_type: row.ConnectionType,

        physical_address_length: row.PhysicalAddressLength,

        transmit_link_speed: row.TransmitLinkSpeed,

        receive_link_speed: row.ReceiveLinkSpeed,
    };

    let (eligibility, reason) = classify(facts);

    DirectLinkInterface {
        interface_index: row.InterfaceIndex,

        alias: wide_string(&row.Alias),

        description: wide_string(&row.Description),

        physical_address: format_physical_address(row),

        mtu: row.Mtu,

        interface_type: row.Type,

        physical_medium_type: row.PhysicalMediumType,

        hardware_interface: status_flags & IF_FLAG_HARDWARE_INTERFACE != 0,

        connector_present: status_flags & IF_FLAG_CONNECTOR_PRESENT != 0,

        transmit_link_speed: row.TransmitLinkSpeed,

        receive_link_speed: row.ReceiveLinkSpeed,

        usable_link_speed: row.TransmitLinkSpeed.min(row.ReceiveLinkSpeed),

        eligibility,

        reason,
    }
}

fn classify(facts: InterfaceFacts) -> (DirectLinkEligibility, &'static str) {
    if facts.filter_interface {
        return (DirectLinkEligibility::Rejected, "NDIS filter interface");
    }

    if facts.endpoint_interface {
        return (
            DirectLinkEligibility::Rejected,
            "endpoint device rather than a network path",
        );
    }

    if facts.interface_type != IF_TYPE_ETHERNET_CSMACD {
        let reason = match facts.interface_type {
            IF_TYPE_IEEE80211 => "Wi-Fi interface",

            IF_TYPE_TUNNEL => "tunnel or VPN interface",

            IF_TYPE_SOFTWARE_LOOPBACK => "software loopback interface",

            _ => "not an Ethernet CSMACD interface",
        };

        return (DirectLinkEligibility::Rejected, reason);
    }

    if facts.operational_status != IF_OPER_STATUS_UP {
        return (
            DirectLinkEligibility::Rejected,
            "Ethernet interface is not operational",
        );
    }

    if facts.media_connect_state != MEDIA_CONNECT_STATE_CONNECTED {
        return (
            DirectLinkEligibility::Rejected,
            "Ethernet media is disconnected",
        );
    }

    if facts.connection_type != NET_IF_CONNECTION_DEDICATED {
        return (
            DirectLinkEligibility::Rejected,
            "interface is not a dedicated connection",
        );
    }

    if facts.physical_address_length == 0 {
        return (
            DirectLinkEligibility::Rejected,
            "interface has no physical address",
        );
    }

    if facts.transmit_link_speed == 0 || facts.receive_link_speed == 0 {
        return (
            DirectLinkEligibility::NeedsReview,
            "Ethernet interface reports no negotiated link speed",
        );
    }

    match facts.physical_medium_type {
        NDIS_PHYSICAL_MEDIUM_802_3 if facts.hardware_interface && facts.connector_present => (
            DirectLinkEligibility::Eligible,
            "connected hardware 802.3 Ethernet interface",
        ),

        NDIS_PHYSICAL_MEDIUM_802_3 => (
            DirectLinkEligibility::NeedsReview,
            "802.3 interface is not marked as hardware with a connector",
        ),

        NDIS_PHYSICAL_MEDIUM_UNSPECIFIED => (
            DirectLinkEligibility::NeedsReview,
            "Ethernet interface has an unspecified physical medium",
        ),

        NDIS_PHYSICAL_MEDIUM_WIRELESS_LAN => (
            DirectLinkEligibility::Rejected,
            "driver reports a wireless physical medium",
        ),

        _ => (
            DirectLinkEligibility::Rejected,
            "not an 802.3 Ethernet medium",
        ),
    }
}

fn interface_flag(row: &MIB_IF_ROW2, mask: u8) -> bool {
    row.InterfaceAndOperStatusFlags._bitfield & mask != 0
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn wide_string(value: &[u16]) -> String {
    let length = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());

    String::from_utf16_lossy(&value[..length])
}

fn format_physical_address(row: &MIB_IF_ROW2) -> String {
    let length = (row.PhysicalAddressLength as usize).min(row.PhysicalAddress.len());

    if length == 0 {
        return "none".to_string();
    }

    row.PhysicalAddress[..length]
        .iter()
        .map(|byte| format!("{byte:02X}",))
        .collect::<Vec<_>>()
        .join("-")
}

fn format_link_speed(bits_per_second: u64) -> String {
    if bits_per_second >= 1_000_000_000 {
        return format!("{:.2} Gbit/s", bits_per_second as f64 / 1_000_000_000.0,);
    }

    if bits_per_second >= 1_000_000 {
        return format!("{:.2} Mbit/s", bits_per_second as f64 / 1_000_000.0,);
    }

    if bits_per_second >= 1_000 {
        return format!("{:.2} Kbit/s", bits_per_second as f64 / 1_000.0,);
    }

    format!("{bits_per_second} bit/s",)
}

#[cfg(test)]
mod tests {
    use super::{
        DirectLinkEligibility, IF_OPER_STATUS_UP, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211,
        InterfaceFacts, MEDIA_CONNECT_STATE_CONNECTED, NDIS_PHYSICAL_MEDIUM_802_3,
        NDIS_PHYSICAL_MEDIUM_UNSPECIFIED, NDIS_PHYSICAL_MEDIUM_WIRELESS_LAN,
        NET_IF_CONNECTION_DEDICATED, classify,
    };

    fn physical_ethernet_facts() -> InterfaceFacts {
        InterfaceFacts {
            interface_type: IF_TYPE_ETHERNET_CSMACD,

            physical_medium_type: NDIS_PHYSICAL_MEDIUM_802_3,

            hardware_interface: true,

            filter_interface: false,

            connector_present: true,

            endpoint_interface: false,

            operational_status: IF_OPER_STATUS_UP,

            media_connect_state: MEDIA_CONNECT_STATE_CONNECTED,

            connection_type: NET_IF_CONNECTION_DEDICATED,

            physical_address_length: 6,

            transmit_link_speed: 2_500_000_000,

            receive_link_speed: 2_500_000_000,
        }
    }

    #[test]
    fn physical_ethernet_is_eligible() {
        let (eligibility, _) = classify(physical_ethernet_facts());

        assert_eq!(eligibility, DirectLinkEligibility::Eligible,);
    }

    #[test]
    fn unspecified_ethernet_requires_review() {
        let mut facts = physical_ethernet_facts();

        facts.physical_medium_type = NDIS_PHYSICAL_MEDIUM_UNSPECIFIED;

        let (eligibility, _) = classify(facts);

        assert_eq!(eligibility, DirectLinkEligibility::NeedsReview,);
    }

    #[test]
    fn wifi_is_rejected() {
        let facts = InterfaceFacts {
            interface_type: IF_TYPE_IEEE80211,

            physical_medium_type: NDIS_PHYSICAL_MEDIUM_WIRELESS_LAN,

            hardware_interface: true,

            filter_interface: false,

            connector_present: true,

            endpoint_interface: false,

            operational_status: IF_OPER_STATUS_UP,

            media_connect_state: MEDIA_CONNECT_STATE_CONNECTED,

            connection_type: NET_IF_CONNECTION_DEDICATED,

            physical_address_length: 6,

            transmit_link_speed: 1_000_000_000,

            receive_link_speed: 1_000_000_000,
        };

        let (eligibility, _) = classify(facts);

        assert_eq!(eligibility, DirectLinkEligibility::Rejected,);
    }

    #[test]
    fn filter_interface_is_rejected() {
        let mut facts = physical_ethernet_facts();

        facts.filter_interface = true;

        let (eligibility, _) = classify(facts);

        assert_eq!(eligibility, DirectLinkEligibility::Rejected,);
    }
}
