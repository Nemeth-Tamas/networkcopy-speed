use crate::management_orchestration::ManagedTransferRecord;
use crate::management_snapshot::{
    ManagementActiveJobDetails, ManagementAgentSnapshot, ManagementJobRole,
};
use std::io;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};

pub fn reconstruct_active_transfer(
    sender_agent: SocketAddr,
    receiver_agent: SocketAddr,
    sender_snapshot: &ManagementAgentSnapshot,
    receiver_snapshot: &ManagementAgentSnapshot,
) -> io::Result<ManagedTransferRecord> {
    if sender_agent == receiver_agent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "active transfer reconnection requires two different agents",
        ));
    }

    let sender = sender_snapshot.active.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "selected sender agent has no active job",
        )
    })?;

    if sender.role != ManagementJobRole::Sender {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selected sender agent is not running a sender job",
        ));
    }

    let (receiver_address, source_root, worker_count, calibration_mib) = match &sender.details {
        ManagementActiveJobDetails::Sender {
            receiver_address,
            source_root,
            worker_count,
            calibration_mib,
        } => (
            *receiver_address,
            source_root.clone(),
            *worker_count,
            *calibration_mib,
        ),

        ManagementActiveJobDetails::Receiver { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sender snapshot contained receiver details",
            ));
        }
    };

    let receiver = receiver_snapshot.active.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "selected receiver agent has no active job",
        )
    })?;

    if receiver.role != ManagementJobRole::Receiver {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selected receiver agent is not running a receiver job",
        ));
    }

    let (transfer_port, destination_root, update_existing) = match &receiver.details {
        ManagementActiveJobDetails::Receiver {
            transfer_port,
            destination_root,
            update_existing,
        } => (*transfer_port, destination_root.clone(), *update_existing),

        ManagementActiveJobDetails::Sender { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "receiver snapshot contained sender details",
            ));
        }
    };

    let expected_receiver_address = payload_endpoint(receiver_agent, transfer_port);

    if receiver_address != expected_receiver_address {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "sender targets {receiver_address}, but selected receiver advertises {expected_receiver_address}"
            ),
        ));
    }

    Ok(ManagedTransferRecord {
        sender_agent,

        sender_job_id: sender.job_id,

        receiver_agent,

        receiver_job_id: receiver.job_id,

        receiver_payload: receiver_address,

        source_root,

        destination_root,

        update_existing,

        worker_count,

        calibration_mib,
    })
}

fn payload_endpoint(control_endpoint: SocketAddr, transfer_port: u16) -> SocketAddr {
    match control_endpoint {
        SocketAddr::V4(address) => SocketAddr::V4(SocketAddrV4::new(*address.ip(), transfer_port)),

        SocketAddr::V6(address) => SocketAddr::V6(SocketAddrV6::new(
            *address.ip(),
            transfer_port,
            address.flowinfo(),
            address.scope_id(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::reconstruct_active_transfer;
    use crate::management_instance::AgentInstanceId;
    use crate::management_snapshot::{
        ManagementActiveJobDetails, ManagementActiveJobSnapshot, ManagementAgentSnapshot,
        ManagementJobRole,
    };

    fn active_snapshot(
        role: ManagementJobRole,
        job_id: u64,
        details: ManagementActiveJobDetails,
    ) -> ManagementAgentSnapshot {
        ManagementAgentSnapshot {
            agent_instance_id: AgentInstanceId::from_raw(u128::from(job_id)).unwrap(),

            active: Some(ManagementActiveJobSnapshot {
                role,

                job_id,

                phase: "Transfer".to_string(),

                completed: 10,

                total: 100,

                cancel_requested: false,

                details,
            }),

            latest_result: None,
        }
    }

    #[test]
    fn reconstructs_matching_active_jobs() {
        let sender_agent = "127.0.0.1:7339".parse().unwrap();

        let receiver_agent = "127.0.0.2:7339".parse().unwrap();

        let sender_snapshot = active_snapshot(
            ManagementJobRole::Sender,
            11,
            ManagementActiveJobDetails::Sender {
                receiver_address: "127.0.0.2:7337".parse().unwrap(),

                source_root: r"C:\Source".to_string(),

                worker_count: 4,

                calibration_mib: 8,
            },
        );

        let receiver_snapshot = active_snapshot(
            ManagementJobRole::Receiver,
            17,
            ManagementActiveJobDetails::Receiver {
                transfer_port: 7337,

                destination_root: r"D:\Destination".to_string(),

                update_existing: true,
            },
        );

        let transfer = reconstruct_active_transfer(
            sender_agent,
            receiver_agent,
            &sender_snapshot,
            &receiver_snapshot,
        )
        .unwrap();

        assert_eq!(transfer.sender_job_id, 11,);

        assert_eq!(transfer.receiver_job_id, 17,);

        assert_eq!(transfer.source_root, r"C:\Source",);

        assert_eq!(transfer.destination_root, r"D:\Destination",);

        assert!(transfer.update_existing);
    }

    #[test]
    fn rejects_mismatched_receiver() {
        let sender_snapshot = active_snapshot(
            ManagementJobRole::Sender,
            11,
            ManagementActiveJobDetails::Sender {
                receiver_address: "127.0.0.9:7337".parse().unwrap(),

                source_root: r"C:\Source".to_string(),

                worker_count: 4,

                calibration_mib: 8,
            },
        );

        let receiver_snapshot = active_snapshot(
            ManagementJobRole::Receiver,
            17,
            ManagementActiveJobDetails::Receiver {
                transfer_port: 7337,

                destination_root: r"D:\Destination".to_string(),

                update_existing: false,
            },
        );

        let error = reconstruct_active_transfer(
            "127.0.0.1:7339".parse().unwrap(),
            "127.0.0.2:7339".parse().unwrap(),
            &sender_snapshot,
            &receiver_snapshot,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData,);
    }
}
