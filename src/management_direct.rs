use crate::direct_address;
use crate::direct_discovery::{self, DiscoveredPath};
use crate::direct_link;
use crate::direct_route;
use crate::management_control::{self, ManagementHello};
use crate::management_discovery::DiscoveredAgent;
use crate::management_protocol::MANAGEMENT_CONTROL_PORT;
use std::io;
use std::net::SocketAddr;
use std::thread;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectDiscoveredAgent {
    pub interface_index: u32,

    pub local_endpoint: SocketAddr,

    pub agent: DiscoveredAgent,
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

pub fn spawn_responder() -> io::Result<usize> {
    let report = match discover_candidates() {
        Ok(report) => report,

        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(0);
        }

        Err(error) => {
            return Err(error);
        }
    };

    let interface_count = report.candidates.len();

    if interface_count == 0 {
        return Ok(0);
    }

    thread::Builder::new()
        .name("networkcopy-management-direct-discovery".to_string())
        .spawn(|| {
            if let Err(error) = direct_discovery::receive_all_on_port(MANAGEMENT_CONTROL_PORT) {
                eprintln!("Direct Link management discovery responder failed: {error}",);
            }
        })?;

    Ok(interface_count)
}

pub fn discover_agents() -> io::Result<Vec<DirectDiscoveredAgent>> {
    let paths = direct_discovery::discover_all()?;

    let mut agents = Vec::new();

    let mut failures = Vec::new();

    for path in paths {
        match management_control::hello(path.endpoint) {
            Ok(hello) => {
                agents.push(direct_agent_from_hello(path, hello));
            }

            Err(error) => {
                failures.push(format!(
                    "{} through interface {}: {error}",
                    path.endpoint, path.interface_index,
                ));
            }
        }
    }

    if agents.is_empty() {
        let details = if failures.is_empty() {
            "no Direct Link discovery response identified a management agent".to_string()
        } else {
            failures.join("; ")
        };

        return Err(io::Error::new(io::ErrorKind::NotFound, details));
    }

    agents.sort_by(|left, right| {
        left.agent
            .hostname
            .cmp(&right.agent.hostname)
            .then_with(|| left.interface_index.cmp(&right.interface_index))
            .then_with(|| {
                left.agent
                    .endpoint
                    .to_string()
                    .cmp(&right.agent.endpoint.to_string())
            })
    });

    agents.dedup_by(|left, right| {
        left.interface_index == right.interface_index && left.agent.endpoint == right.agent.endpoint
    });

    Ok(agents)
}

fn direct_agent_from_hello(path: DiscoveredPath, hello: ManagementHello) -> DirectDiscoveredAgent {
    DirectDiscoveredAgent {
        interface_index: path.interface_index,

        local_endpoint: path.local_endpoint,

        agent: DiscoveredAgent {
            hostname: hello.hostname,

            endpoint: path.endpoint,

            protocol_version: hello.protocol_version,

            state: hello.state,

            capabilities: hello.capabilities,
        },
    }
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
    use super::{
        DirectManagementCandidate, candidate_from_endpoints, direct_agent_from_hello,
        optional_endpoint,
    };
    use crate::direct_discovery::DiscoveredPath;
    use crate::management_control::ManagementHello;
    use crate::management_discovery::{AgentCapabilities, AgentState};
    use std::io;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

    #[test]
    fn discovered_management_agent_preserves_direct_path() {
        let local_endpoint = "[fe80::10%42]:7339".parse().unwrap();

        let peer_endpoint = "[fe80::20%42]:7339".parse().unwrap();

        let path = DiscoveredPath {
            interface_index: 42,

            local_endpoint,

            endpoint: peer_endpoint,
        };

        let hello = ManagementHello {
            hostname: "DIRECT-PC".to_string(),

            application_version: "2.3.0-dev".to_string(),

            protocol_version: 1,

            state: AgentState::Idle,

            capabilities: AgentCapabilities::SEND_RECEIVE,
        };

        let actual = direct_agent_from_hello(path, hello);

        assert_eq!(actual.interface_index, 42,);

        assert_eq!(actual.local_endpoint, local_endpoint,);

        assert_eq!(actual.agent.endpoint, peer_endpoint,);

        assert_eq!(actual.agent.hostname, "DIRECT-PC",);

        assert!(actual.agent.capabilities.can_send(),);

        assert!(actual.agent.capabilities.can_receive(),);
    }

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
