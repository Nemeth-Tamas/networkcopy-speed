use crate::management_instance::AgentInstanceId;
use crate::management_queue::{QueuedTransferId, QueuedTransferState, TransferQueue};
use crate::management_snapshot::{ManagementAgentSnapshot, ManagementJobRole};
use std::io;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveQueueBinding {
    pub queue_id: QueuedTransferId,

    pub sender_instance_id: AgentInstanceId,

    pub sender_job_id: u64,

    pub receiver_instance_id: AgentInstanceId,

    pub receiver_job_id: u64,
}

impl ActiveQueueBinding {
    pub fn new(
        queue_id: QueuedTransferId,
        sender_instance_id: AgentInstanceId,
        sender_job_id: u64,
        receiver_instance_id: AgentInstanceId,
        receiver_job_id: u64,
    ) -> io::Result<Self> {
        validate_job_id(sender_job_id, "sender")?;

        validate_job_id(receiver_job_id, "receiver")?;

        Ok(Self {
            queue_id,

            sender_instance_id,

            sender_job_id,

            receiver_instance_id,

            receiver_job_id,
        })
    }

    pub fn validate_for_queue(self, queue: &TransferQueue) -> io::Result<()> {
        let item = queue
            .items()
            .iter()
            .find(|item| item.id == self.queue_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "active queue binding references missing transfer #{}",
                        self.queue_id,
                    ),
                )
            })?;

        if !matches!(
            item.state,
            QueuedTransferState::Running | QueuedTransferState::Blocked
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "active queue binding references transfer #{} in {:?} state; only Running or Blocked items may retain endpoint jobs",
                    self.queue_id, item.state,
                ),
            ));
        }

        Ok(())
    }

    pub fn validate_active_snapshots(
        self,
        sender_snapshot: &ManagementAgentSnapshot,
        receiver_snapshot: &ManagementAgentSnapshot,
    ) -> io::Result<()> {
        validate_snapshot(
            "sender",
            ManagementJobRole::Sender,
            self.sender_instance_id,
            self.sender_job_id,
            sender_snapshot,
        )?;

        validate_snapshot(
            "receiver",
            ManagementJobRole::Receiver,
            self.receiver_instance_id,
            self.receiver_job_id,
            receiver_snapshot,
        )
    }
}

fn validate_job_id(job_id: u64, role: &str) -> io::Result<()> {
    if job_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("active queue binding {role} job ID must not be zero",),
        ));
    }

    Ok(())
}

fn validate_snapshot(
    endpoint_role: &str,
    expected_job_role: ManagementJobRole,
    expected_instance_id: AgentInstanceId,
    expected_job_id: u64,
    snapshot: &ManagementAgentSnapshot,
) -> io::Result<()> {
    if snapshot.agent_instance_id != expected_instance_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{endpoint_role} agent process identity changed; expected {}, found {}",
                expected_instance_id, snapshot.agent_instance_id,
            ),
        ));
    }

    let active = snapshot.active.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{endpoint_role} agent has no active job",),
        )
    })?;

    if active.role != expected_job_role {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{endpoint_role} agent active job has role {}, expected {}",
                active.role.label(),
                expected_job_role.label(),
            ),
        ));
    }

    if active.job_id != expected_job_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{endpoint_role} agent active job ID changed; expected {expected_job_id}, found {}",
                active.job_id,
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ActiveQueueBinding;
    use crate::management_instance::AgentInstanceId;
    use crate::management_queue::{
        QueuedTransferKind, QueuedTransferRequest, QueuedTransferState, TransferQueue,
    };
    use crate::management_route::ManagementRouteMode;
    use crate::management_snapshot::{
        ManagementActiveJobDetails, ManagementActiveJobSnapshot, ManagementAgentSnapshot,
        ManagementJobRole,
    };

    fn instance(value: u128) -> AgentInstanceId {
        AgentInstanceId::from_raw(value).unwrap()
    }

    fn queue_with_state(
        state: QueuedTransferState,
    ) -> (TransferQueue, crate::management_queue::QueuedTransferId) {
        let mut queue = TransferQueue::default();

        let id = queue
            .add(QueuedTransferRequest {
                sender_agent: "127.0.0.1:7339".parse().unwrap(),

                receiver_agent: "127.0.0.2:7339".parse().unwrap(),

                route_mode: ManagementRouteMode::AutomaticLan,

                source_root: r"C:\Source".to_string(),

                destination_root: r"D:\Destination".to_string(),

                update_existing: true,

                worker_count: 4,

                calibration_mib: 8,

                kind: QueuedTransferKind::Fresh,
            })
            .unwrap();

        queue.set_state(id, state, "test state").unwrap();

        (queue, id)
    }

    fn snapshot(
        instance_id: AgentInstanceId,
        role: ManagementJobRole,
        job_id: u64,
    ) -> ManagementAgentSnapshot {
        let details = match role {
            ManagementJobRole::Sender => ManagementActiveJobDetails::Sender {
                receiver_address: "127.0.0.2:7337".parse().unwrap(),

                source_root: r"C:\Source".to_string(),

                worker_count: 4,

                calibration_mib: 8,
            },

            ManagementJobRole::Receiver => ManagementActiveJobDetails::Receiver {
                transfer_port: 7337,

                destination_root: r"D:\Destination".to_string(),

                update_existing: true,
            },
        };

        ManagementAgentSnapshot {
            agent_instance_id: instance_id,

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
    fn binding_rejects_zero_job_ids() {
        let (_, queue_id) = queue_with_state(QueuedTransferState::Running);

        assert!(ActiveQueueBinding::new(queue_id, instance(1), 0, instance(2), 17,).is_err(),);

        assert!(ActiveQueueBinding::new(queue_id, instance(1), 11, instance(2), 0,).is_err(),);
    }

    #[test]
    fn running_and_blocked_items_may_retain_binding() {
        for state in [QueuedTransferState::Running, QueuedTransferState::Blocked] {
            let (queue, queue_id) = queue_with_state(state);

            let binding =
                ActiveQueueBinding::new(queue_id, instance(1), 11, instance(2), 17).unwrap();

            binding.validate_for_queue(&queue).unwrap();
        }
    }

    #[test]
    fn pending_and_terminal_items_reject_binding() {
        for state in [
            QueuedTransferState::Pending,
            QueuedTransferState::Failed,
            QueuedTransferState::Completed,
            QueuedTransferState::Cancelled,
        ] {
            let (queue, queue_id) = queue_with_state(state);

            let binding =
                ActiveQueueBinding::new(queue_id, instance(1), 11, instance(2), 17).unwrap();

            assert!(binding.validate_for_queue(&queue,).is_err(),);
        }
    }

    #[test]
    fn missing_queue_item_rejects_binding() {
        let (queue, _) = queue_with_state(QueuedTransferState::Running);

        let missing_id = crate::management_queue::QueuedTransferId::from_raw(999).unwrap();

        let binding =
            ActiveQueueBinding::new(missing_id, instance(1), 11, instance(2), 17).unwrap();

        assert!(binding.validate_for_queue(&queue).is_err(),);
    }

    #[test]
    fn matching_active_snapshots_validate() {
        let (_, queue_id) = queue_with_state(QueuedTransferState::Running);

        let binding =
            ActiveQueueBinding::new(queue_id, instance(101), 11, instance(202), 17).unwrap();

        let sender = snapshot(instance(101), ManagementJobRole::Sender, 11);

        let receiver = snapshot(instance(202), ManagementJobRole::Receiver, 17);

        binding
            .validate_active_snapshots(&sender, &receiver)
            .unwrap();
    }

    #[test]
    fn changed_agent_instance_is_rejected() {
        let (_, queue_id) = queue_with_state(QueuedTransferState::Running);

        let binding =
            ActiveQueueBinding::new(queue_id, instance(101), 11, instance(202), 17).unwrap();

        let sender = snapshot(instance(999), ManagementJobRole::Sender, 11);

        let receiver = snapshot(instance(202), ManagementJobRole::Receiver, 17);

        assert!(
            binding
                .validate_active_snapshots(&sender, &receiver,)
                .is_err(),
        );
    }

    #[test]
    fn changed_job_id_is_rejected() {
        let (_, queue_id) = queue_with_state(QueuedTransferState::Running);

        let binding =
            ActiveQueueBinding::new(queue_id, instance(101), 11, instance(202), 17).unwrap();

        let sender = snapshot(instance(101), ManagementJobRole::Sender, 12);

        let receiver = snapshot(instance(202), ManagementJobRole::Receiver, 17);

        assert!(
            binding
                .validate_active_snapshots(&sender, &receiver,)
                .is_err(),
        );
    }

    #[test]
    fn swapped_endpoint_roles_are_rejected() {
        let (_, queue_id) = queue_with_state(QueuedTransferState::Running);

        let binding =
            ActiveQueueBinding::new(queue_id, instance(101), 11, instance(202), 17).unwrap();

        let sender = snapshot(instance(101), ManagementJobRole::Receiver, 11);

        let receiver = snapshot(instance(202), ManagementJobRole::Sender, 17);

        assert!(
            binding
                .validate_active_snapshots(&sender, &receiver,)
                .is_err(),
        );
    }
}
