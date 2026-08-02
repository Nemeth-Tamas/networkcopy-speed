use crate::direct_address;
use crate::direct_link;
use crate::direct_route;
use crate::management_protocol::MANAGEMENT_CONTROL_PORT;
use std::io;
use std::net::SocketAddr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectManagementCandidate {
    pub interface_index: u32,

    pub ipv6_endpoint: Option<SocketAddr>,

    pub ipv4_endpoint: Option<SocketAddr>,
}

impl DirectManagementCandidate {
    pub const fn preferred_endpoint(&self) -> Option<SocketAddr> {
        match self.ipv6_endpoint {
            Some(endpoint) => Some(endpoint),

            None => self.ipv4_endpoint,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DirectManagementCandidateReport {
    pub candidates: Vec<DirectManagementCandidate>,

    pub routed_interface_indices: Vec<u32>,

    pub addressless_interface_indices: Vec<u32>,
}

pub fn discover_candidates() -> io::Result<DirectManagementCandidateReport> {
    let strict_indices = direct_link::strict_candidate_indices()?;

    let routes = direct_route::classify_candidates(&strict_indices)?;

    let mut candidates = Vec::new();

    let mut addressless_interface_indices = Vec::new();

    for interface_index in routes.direct {
        let ipv6_endpoint = optional_endpoint(
            direct_address::link_local_endpoint(interface_index, MANAGEMENT_CONTROL_PORT)
                .map(SocketAddr::V6),
        )?;

        let ipv4_endpoint = optional_endpoint(
            direct_address::apipa_endpoint(interface_index, MANAGEMENT_CONTROL_PORT)
                .map(SocketAddr::V4),
        )?;

        let Some(candidate) =
            candidate_from_endpoints(interface_index, ipv6_endpoint, ipv4_endpoint)
        else {
            addressless_interface_indices.push(interface_index);

            continue;
        };

        candidates.push(candidate);
    }

    candidates.sort_by_key(|candidate| candidate.interface_index);

    addressless_interface_indices.sort_unstable();

    let mut routed_interface_indices = routes.routed;

    routed_interface_indices.sort_unstable();

    Ok(DirectManagementCandidateReport {
        candidates,

        routed_interface_indices,

        addressless_interface_indices,
    })
}

fn candidate_from_endpoints(
    interface_index: u32,
    ipv6_endpoint: Option<SocketAddr>,
    ipv4_endpoint: Option<SocketAddr>,
) -> Option<DirectManagementCandidate> {
    if ipv6_endpoint.is_none() && ipv4_endpoint.is_none() {
        return None;
    }

    Some(DirectManagementCandidate {
        interface_index,

        ipv6_endpoint,

        ipv4_endpoint,
    })
}

fn optional_endpoint(result: io::Result<SocketAddr>) -> io::Result<Option<SocketAddr>> {
    match result {
        Ok(endpoint) => Ok(Some(endpoint)),

        Err(error) if error.kind() == io::ErrorKind::AddrNotAvailable => Ok(None),

        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{DirectManagementCandidate, candidate_from_endpoints, optional_endpoint};
    use std::io;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

    #[test]
    fn scoped_ipv6_is_preferred_over_apipa() {
        let ipv6 = SocketAddr::V6(SocketAddrV6::new(
            "fe80::1234".parse::<Ipv6Addr>().unwrap(),
            7339,
            0,
            42,
        ));

        let ipv4 = SocketAddr::V4(SocketAddrV4::new(
            "169.254.10.20".parse::<Ipv4Addr>().unwrap(),
            7339,
        ));

        let candidate = DirectManagementCandidate {
            interface_index: 42,

            ipv6_endpoint: Some(ipv6),

            ipv4_endpoint: Some(ipv4),
        };

        assert_eq!(candidate.preferred_endpoint(), Some(ipv6),);
    }

    #[test]
    fn apipa_is_used_when_ipv6_is_missing() {
        let ipv4 = SocketAddr::V4(SocketAddrV4::new(
            "169.254.10.20".parse::<Ipv4Addr>().unwrap(),
            7339,
        ));

        let candidate = DirectManagementCandidate {
            interface_index: 17,

            ipv6_endpoint: None,

            ipv4_endpoint: Some(ipv4),
        };

        assert_eq!(candidate.preferred_endpoint(), Some(ipv4),);
    }

    #[test]
    fn interface_without_addresses_is_omitted() {
        assert_eq!(candidate_from_endpoints(10, None, None,), None,);
    }

    #[test]
    fn candidate_preserves_ipv6_scope_id() {
        let ipv6 = SocketAddr::V6(SocketAddrV6::new(
            "fe80::beef".parse::<Ipv6Addr>().unwrap(),
            7339,
            0,
            91,
        ));

        let candidate = candidate_from_endpoints(91, Some(ipv6), None).unwrap();

        let SocketAddr::V6(endpoint) = candidate.preferred_endpoint().unwrap() else {
            panic!("IPv6 candidate became IPv4",);
        };

        assert_eq!(endpoint.scope_id(), 91);
    }

    #[test]
    fn unavailable_address_becomes_none() {
        let actual = optional_endpoint(Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "missing address",
        )))
        .unwrap();

        assert_eq!(actual, None);
    }

    #[test]
    fn unexpected_address_error_is_preserved() {
        let error = optional_endpoint(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "denied",
        )))
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied,);
    }
}
