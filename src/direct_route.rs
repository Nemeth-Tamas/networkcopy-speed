use std::collections::HashSet;
use std::io;
use std::ptr;
use std::slice;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIpForwardTable2, MIB_IPFORWARD_TABLE2,
};
use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;

const NO_ERROR: u32 = 0;

struct RouteTable(*mut MIB_IPFORWARD_TABLE2);

impl Drop for RouteTable {
    fn drop(&mut self) {
        unsafe {
            FreeMibTable(self.0.cast());
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RouteCandidates {
    pub(crate) direct: Vec<u32>,
    pub(crate) routed: Vec<u32>,
}

pub(crate) fn classify_candidates(interface_indices: &[u32]) -> io::Result<RouteCandidates> {
    let default_routes = default_route_interfaces()?;

    Ok(partition_candidates(interface_indices, &default_routes))
}

fn default_route_interfaces() -> io::Result<HashSet<u32>> {
    let mut raw_table = ptr::null_mut();

    let status = unsafe { GetIpForwardTable2(AF_UNSPEC, &mut raw_table) };

    if status != NO_ERROR {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    if raw_table.is_null() {
        return Err(io::Error::other(
            "GetIpForwardTable2 returned a null route table",
        ));
    }

    let table = RouteTable(raw_table);

    let entry_count = unsafe { (*table.0).NumEntries as usize };

    let first_entry = unsafe { (*table.0).Table.as_ptr() };

    let rows = unsafe { slice::from_raw_parts(first_entry, entry_count) };

    Ok(rows
        .iter()
        .filter(|row| row.DestinationPrefix.PrefixLength == 0)
        .map(|row| row.InterfaceIndex)
        .collect())
}

fn partition_candidates(
    interface_indices: &[u32],
    default_routes: &HashSet<u32>,
) -> RouteCandidates {
    let (routed, direct): (Vec<_>, Vec<_>) = interface_indices
        .iter()
        .copied()
        .partition(|interface_index| default_routes.contains(interface_index));

    RouteCandidates { direct, routed }
}

#[cfg(test)]
mod tests {
    use super::{RouteCandidates, partition_candidates};
    use std::collections::HashSet;

    #[test]
    fn default_route_interfaces_are_rejected() {
        let default_routes = HashSet::from([3_u32]);

        let actual = partition_candidates(&[3, 10], &default_routes);

        assert_eq!(
            actual,
            RouteCandidates {
                direct: vec![10],
                routed: vec![3],
            },
        );
    }

    #[test]
    fn gateway_free_interfaces_are_preserved() {
        let actual = partition_candidates(&[10, 12], &HashSet::new());

        assert_eq!(
            actual,
            RouteCandidates {
                direct: vec![10, 12],
                routed: Vec::new(),
            },
        );
    }
}
