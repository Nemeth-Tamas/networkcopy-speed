use crate::management_control;
use crate::management_jobs::{PreparedReceiveJob, StartedSendJob};
use crate::network_calibration;
use std::io;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedTransferRequest {
    pub sender_agent: SocketAddr,

    pub receiver_agent: SocketAddr,

    pub source_root: String,

    pub destination_root: String,

    pub update_existing: bool,

    pub worker_count: usize,

    pub calibration_mib: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedTransferRecord {
    pub sender_agent: SocketAddr,

    pub sender_job_id: u64,

    pub receiver_agent: SocketAddr,

    pub receiver_job_id: u64,

    pub receiver_payload: SocketAddr,

    pub source_root: String,

    pub destination_root: String,

    pub update_existing: bool,

    pub worker_count: usize,

    pub calibration_mib: u64,
}

pub fn start_transfer(request: ManagedTransferRequest) -> io::Result<ManagedTransferRecord> {
    start_transfer_with_desktop_layout(request, false)
}

pub fn start_transfer_with_desktop_layout(
    request: ManagedTransferRequest,
    preserve_desktop_layout: bool,
) -> io::Result<ManagedTransferRecord> {
    start_transfer_with(
        request,
        preserve_desktop_layout,
        management_control::prepare_receive,
        |endpoint,
         receiver_address,
         source_root,
         worker_count,
         calibration_mib,
         preserve_desktop_layout| {
            management_control::start_send_with_stream_count_and_desktop_layout(
                endpoint,
                receiver_address,
                source_root,
                worker_count,
                calibration_mib,
                None,
                preserve_desktop_layout,
            )
        },
        management_control::cancel_job,
    )
}

pub fn resume_transfer(
    request: ManagedTransferRequest,
    data_stream_count: usize,
) -> io::Result<ManagedTransferRecord> {
    resume_transfer_with_desktop_layout(request, data_stream_count, false)
}

pub fn resume_transfer_with_desktop_layout(
    request: ManagedTransferRequest,
    data_stream_count: usize,
    preserve_desktop_layout: bool,
) -> io::Result<ManagedTransferRecord> {
    network_calibration::validate_matrix_stream_count(data_stream_count)?;

    start_transfer_with(
        request,
        preserve_desktop_layout,
        management_control::prepare_receive,
        move |endpoint,
              receiver_address,
              source_root,
              worker_count,
              calibration_mib,
              preserve_desktop_layout| {
            management_control::start_send_with_stream_count_and_desktop_layout(
                endpoint,
                receiver_address,
                source_root,
                worker_count,
                calibration_mib,
                Some(data_stream_count),
                preserve_desktop_layout,
            )
        },
        management_control::cancel_job,
    )
}

fn start_transfer_with<PrepareReceiver, StartSender, CancelJob>(
    request: ManagedTransferRequest,
    preserve_desktop_layout: bool,
    prepare_receiver: PrepareReceiver,
    start_sender: StartSender,
    cancel_job: CancelJob,
) -> io::Result<ManagedTransferRecord>
where
    PrepareReceiver: FnOnce(SocketAddr, &str, bool) -> io::Result<PreparedReceiveJob>,
    StartSender:
        FnOnce(SocketAddr, SocketAddr, &str, usize, u64, bool) -> io::Result<StartedSendJob>,
    CancelJob: FnOnce(SocketAddr, u64) -> io::Result<u64>,
{
    if request.sender_agent == request.receiver_agent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "paired management transfer requires two different agents",
        ));
    }

    let receiver_job = prepare_receiver(
        request.receiver_agent,
        &request.destination_root,
        request.update_existing,
    )?;

    let receiver_payload = payload_endpoint(request.receiver_agent, receiver_job.transfer_port);

    let sender_job = match start_sender(
        request.sender_agent,
        receiver_payload,
        &request.source_root,
        request.worker_count,
        request.calibration_mib,
        preserve_desktop_layout,
    ) {
        Ok(job) => job,

        Err(sender_error) => {
            return Err(rollback_receiver(
                request.receiver_agent,
                receiver_job.job_id,
                sender_error,
                cancel_job,
            ));
        }
    };

    Ok(ManagedTransferRecord {
        sender_agent: request.sender_agent,

        sender_job_id: sender_job.job_id,

        receiver_agent: request.receiver_agent,

        receiver_job_id: receiver_job.job_id,

        receiver_payload,

        source_root: sender_job.source_root,

        destination_root: receiver_job.destination_root,

        update_existing: receiver_job.update_existing,

        worker_count: sender_job.worker_count,

        calibration_mib: sender_job.calibration_mib,
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

fn rollback_receiver<CancelJob>(
    receiver_agent: SocketAddr,
    receiver_job_id: u64,
    sender_error: io::Error,
    cancel_job: CancelJob,
) -> io::Error
where
    CancelJob: FnOnce(SocketAddr, u64) -> io::Result<u64>,
{
    match cancel_job(receiver_agent, receiver_job_id) {
        Ok(cancelled_job_id) if cancelled_job_id == receiver_job_id => io::Error::new(
            sender_error.kind(),
            format!(
                "failed to start sender after receiver job {receiver_job_id} was prepared; receiver job was rolled back: {sender_error}"
            ),
        ),

        Ok(cancelled_job_id) => io::Error::other(format!(
            "failed to start sender after receiver job {receiver_job_id} was prepared; rollback returned unexpected job ID {cancelled_job_id}: {sender_error}"
        )),

        Err(rollback_error) => io::Error::other(format!(
            "failed to start sender after receiver job {receiver_job_id} was prepared: {sender_error}; receiver rollback also failed: {rollback_error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{ManagedTransferRequest, payload_endpoint, resume_transfer, start_transfer_with};
    use crate::management_jobs::{PreparedReceiveJob, StartedSendJob};
    use std::cell::Cell;
    use std::io;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

    fn example_request() -> ManagedTransferRequest {
        ManagedTransferRequest {
            sender_agent: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7341)),

            receiver_agent: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7342)),

            source_root: r"C:\Source".to_string(),

            destination_root: r"D:\Destination".to_string(),

            update_existing: true,

            worker_count: 4,

            calibration_mib: 8,
        }
    }

    #[test]
    fn orchestration_returns_paired_record() {
        let request = example_request();

        let expected_sender = request.sender_agent;

        let expected_receiver = request.receiver_agent;

        let record = start_transfer_with(
            request,
            true,
            move |receiver_agent, destination, update_existing| {
                assert_eq!(receiver_agent, expected_receiver,);

                assert_eq!(destination, r"D:\Destination",);

                assert!(update_existing,);

                Ok(PreparedReceiveJob {
                    job_id: 17,

                    transfer_port: 7337,

                    destination_root: destination.to_string(),

                    update_existing,
                })
            },
            move |sender_agent,
                  receiver_payload,
                  source,
                  worker_count,
                  calibration_mib,
                  preserve_desktop_layout| {
                assert_eq!(sender_agent, expected_sender,);

                assert_eq!(
                    receiver_payload,
                    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7337,)),
                );

                assert_eq!(source, r"C:\Source",);

                assert_eq!(worker_count, 4,);

                assert_eq!(calibration_mib, 8,);

                assert!(preserve_desktop_layout);

                Ok(StartedSendJob {
                    job_id: 29,

                    receiver_address: receiver_payload,

                    source_root: source.to_string(),

                    worker_count,

                    calibration_mib,
                })
            },
            |_, _| -> io::Result<u64> {
                panic!("successful orchestration must not roll back");
            },
        )
        .unwrap();

        assert_eq!(record.sender_job_id, 29,);

        assert_eq!(record.receiver_job_id, 17,);

        assert_eq!(
            record.receiver_payload,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7337,)),
        );

        assert_eq!(record.source_root, r"C:\Source",);

        assert_eq!(record.destination_root, r"D:\Destination",);

        assert!(record.update_existing);
    }

    #[test]
    fn sender_failure_rolls_back_receiver() {
        let rolled_back = Cell::new(false);

        let error = start_transfer_with(
            example_request(),
            false,
            |_, destination, update_existing| {
                Ok(PreparedReceiveJob {
                    job_id: 71,

                    transfer_port: 7337,

                    destination_root: destination.to_string(),

                    update_existing,
                })
            },
            |_, _, _, _, _, _| {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "sender agent unavailable",
                ))
            },
            |receiver, job_id| {
                assert_eq!(
                    receiver,
                    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7342,)),
                );

                assert_eq!(job_id, 71,);

                rolled_back.set(true);

                Ok(job_id)
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused,);

        assert!(rolled_back.get());

        assert!(error.to_string().contains("rolled back"),);
    }

    #[test]
    fn same_agent_is_rejected() {
        let mut request = example_request();

        request.receiver_agent = request.sender_agent;

        let error = start_transfer_with(
            request,
            false,
            |_, _, _| -> io::Result<PreparedReceiveJob> {
                panic!("receiver must not be prepared");
            },
            |_, _, _, _, _, _| -> io::Result<StartedSendJob> {
                panic!("sender must not start");
            },
            |_, _| -> io::Result<u64> {
                panic!("rollback must not run");
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput,);
    }

    #[test]
    fn ipv6_payload_preserves_scope() {
        let address = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1);

        let control = SocketAddr::V6(SocketAddrV6::new(address, 7339, 17, 42));

        let payload = payload_endpoint(control, 7337);

        let SocketAddr::V6(payload) = payload else {
            panic!("IPv6 control endpoint produced IPv4 payload endpoint");
        };

        assert_eq!(*payload.ip(), address,);

        assert_eq!(payload.port(), 7337,);

        assert_eq!(payload.flowinfo(), 17,);

        assert_eq!(payload.scope_id(), 42,);
    }

    #[test]
    fn resume_rejects_stream_count_outside_matrix_before_networking() {
        let error = resume_transfer(example_request(), 3).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput,);
    }
}
