use crate::management_active_binding::ActiveQueueBinding;
use crate::management_route::ManagementRouteMode;
use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;

pub const MAX_QUEUE_ENTRIES: usize = 256;

const MAX_QUEUE_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct QueuedTransferId(u64);

impl QueuedTransferId {
    pub const fn from_raw(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for QueuedTransferId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueuedTransferKind {
    Fresh,

    Resume { data_stream_count: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueuedTransferState {
    Pending,

    Running,

    Blocked,

    Failed,

    Completed,

    Cancelled,
}

impl QueuedTransferState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::Completed | Self::Cancelled)
    }

    pub const fn may_retain_active_binding(self) -> bool {
        matches!(self, Self::Running | Self::Blocked)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedTransferRequest {
    pub sender_agent: SocketAddr,

    pub receiver_agent: SocketAddr,

    pub route_mode: ManagementRouteMode,

    pub source_root: String,

    pub destination_root: String,

    pub update_existing: bool,

    pub worker_count: usize,

    pub calibration_mib: u64,

    pub kind: QueuedTransferKind,
}

impl QueuedTransferRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.sender_agent == self.receiver_agent {
            return Err("queued sender and receiver agents must be different".to_string());
        }

        validate_required_text(&self.source_root, "queued source root")?;

        validate_required_text(&self.destination_root, "queued destination root")?;

        if self.worker_count == 0 {
            return Err("queued worker count must not be zero".to_string());
        }

        if self.calibration_mib == 0 {
            return Err("queued calibration size must not be zero".to_string());
        }

        if let QueuedTransferKind::Resume { data_stream_count } = self.kind
            && data_stream_count == 0
        {
            return Err("queued resume stream count must not be zero".to_string());
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedTransfer {
    pub id: QueuedTransferId,

    pub request: QueuedTransferRequest,

    pub preserve_desktop_layout: bool,

    pub state: QueuedTransferState,

    pub status_message: String,
}

impl QueuedTransfer {
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    fn validate(&self) -> Result<(), String> {
        self.request.validate()?;

        validate_optional_text(&self.status_message, "queue status message")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferQueue {
    next_id: u64,

    paused_after_current: bool,

    items: Vec<QueuedTransfer>,

    active_binding: Option<ActiveQueueBinding>,
}

impl Default for TransferQueue {
    fn default() -> Self {
        Self {
            next_id: 1,

            paused_after_current: false,

            items: Vec::new(),

            active_binding: None,
        }
    }
}

impl TransferQueue {
    pub fn from_parts(
        next_id: u64,
        paused_after_current: bool,
        items: Vec<QueuedTransfer>,
    ) -> Result<Self, String> {
        Self::from_parts_with_active_binding(next_id, paused_after_current, items, None)
    }

    pub fn from_parts_with_active_binding(
        next_id: u64,
        paused_after_current: bool,
        items: Vec<QueuedTransfer>,
        active_binding: Option<ActiveQueueBinding>,
    ) -> Result<Self, String> {
        let queue = Self {
            next_id,

            paused_after_current,

            items,

            active_binding,
        };

        queue.validate()?;

        Ok(queue)
    }

    pub const fn next_id(&self) -> u64 {
        self.next_id
    }

    pub const fn paused_after_current(&self) -> bool {
        self.paused_after_current
    }

    pub fn set_paused_after_current(&mut self, paused: bool) {
        self.paused_after_current = paused;
    }

    pub fn items(&self) -> &[QueuedTransfer] {
        &self.items
    }

    pub(crate) fn items_mut(&mut self) -> &mut [QueuedTransfer] {
        &mut self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub const fn active_binding(&self) -> Option<ActiveQueueBinding> {
        self.active_binding
    }

    pub fn set_active_binding(&mut self, binding: ActiveQueueBinding) -> Result<(), String> {
        if let Some(existing) = self.active_binding {
            if existing == binding {
                return Ok(());
            }

            return Err(format!(
                "transfer queue already retains an active binding for transfer #{}",
                existing.queue_id,
            ));
        }

        binding
            .validate_for_queue(self)
            .map_err(|error| error.to_string())?;

        self.active_binding = Some(binding);

        Ok(())
    }

    pub fn clear_active_binding(&mut self) -> Option<ActiveQueueBinding> {
        self.active_binding.take()
    }

    pub fn add(&mut self, request: QueuedTransferRequest) -> Result<QueuedTransferId, String> {
        self.add_with_desktop_layout(request, false)
    }

    pub fn add_with_desktop_layout(
        &mut self,
        request: QueuedTransferRequest,
        preserve_desktop_layout: bool,
    ) -> Result<QueuedTransferId, String> {
        request.validate()?;

        if self.items.len() >= MAX_QUEUE_ENTRIES {
            return Err(format!(
                "transfer queue already contains the maximum of {MAX_QUEUE_ENTRIES} items",
            ));
        }

        let id = QueuedTransferId::from_raw(self.next_id)
            .ok_or_else(|| "transfer queue next ID must not be zero".to_string())?;

        let next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "transfer queue ID space is exhausted".to_string())?;

        self.items.push(QueuedTransfer {
            id,

            request,

            preserve_desktop_layout,

            state: QueuedTransferState::Pending,

            status_message: String::new(),
        });

        self.next_id = next_id;

        Ok(id)
    }

    pub fn move_up(&mut self, id: QueuedTransferId) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == id) else {
            return false;
        };

        if index == 0
            || self.is_bound(id)
            || self.is_bound(self.items[index - 1].id)
            || self.items[index].state == QueuedTransferState::Running
            || self.items[index - 1].state == QueuedTransferState::Running
        {
            return false;
        }

        self.items.swap(index, index - 1);

        true
    }

    pub fn move_down(&mut self, id: QueuedTransferId) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == id) else {
            return false;
        };

        if index + 1 >= self.items.len()
            || self.is_bound(id)
            || self.is_bound(self.items[index + 1].id)
            || self.items[index].state == QueuedTransferState::Running
            || self.items[index + 1].state == QueuedTransferState::Running
        {
            return false;
        }

        self.items.swap(index, index + 1);

        true
    }

    pub fn remove(&mut self, id: QueuedTransferId) -> Option<QueuedTransfer> {
        let index = self.items.iter().position(|item| item.id == id)?;

        if self.is_bound(id) || self.items[index].state == QueuedTransferState::Running {
            return None;
        }

        Some(self.items.remove(index))
    }

    pub fn clear_completed(&mut self) -> usize {
        let previous_len = self.items.len();

        let bound_id = self.active_binding.map(|binding| binding.queue_id);

        self.items.retain(|item| {
            item.state != QueuedTransferState::Completed || bound_id == Some(item.id)
        });

        previous_len - self.items.len()
    }

    pub fn set_state(
        &mut self,
        id: QueuedTransferId,
        state: QueuedTransferState,
        status_message: impl Into<String>,
    ) -> Result<(), String> {
        let status_message = status_message.into();

        validate_optional_text(&status_message, "queue status message")?;

        let bound_item = self.is_bound(id);

        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| format!("queued transfer {id} does not exist",))?;

        item.state = state;

        item.status_message = status_message;

        if !state.may_retain_active_binding() && bound_item {
            self.active_binding = None;
        }

        Ok(())
    }

    pub fn reset_to_pending(&mut self, id: QueuedTransferId) -> bool {
        if self.is_bound(id) {
            return false;
        }

        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return false;
        };

        if matches!(
            item.state,
            QueuedTransferState::Pending | QueuedTransferState::Running
        ) {
            return false;
        }

        item.state = QueuedTransferState::Pending;

        item.status_message.clear();

        true
    }

    pub fn skip(&mut self, id: QueuedTransferId) -> bool {
        if self.is_bound(id) {
            return false;
        }

        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return false;
        };

        if !matches!(
            item.state,
            QueuedTransferState::Pending
                | QueuedTransferState::Blocked
                | QueuedTransferState::Failed
        ) {
            return false;
        }

        item.state = QueuedTransferState::Cancelled;

        item.status_message = "Skipped by user.".to_string();

        true
    }

    pub fn first_pending(&self) -> Option<&QueuedTransfer> {
        self.items
            .iter()
            .find(|item| item.state == QueuedTransferState::Pending)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.next_id == 0 {
            return Err("transfer queue next ID must not be zero".to_string());
        }

        if self.items.len() > MAX_QUEUE_ENTRIES {
            return Err(format!(
                "transfer queue contains {} items, exceeding the {MAX_QUEUE_ENTRIES} item limit",
                self.items.len(),
            ));
        }

        let mut ids = HashSet::with_capacity(self.items.len());

        for item in &self.items {
            if item.id.get() >= self.next_id {
                return Err(format!(
                    "queued transfer {} is not below next queue ID {}",
                    item.id, self.next_id,
                ));
            }

            if !ids.insert(item.id) {
                return Err(format!("transfer queue contains duplicate ID {}", item.id,));
            }

            item.validate()?;
        }

        if let Some(binding) = self.active_binding {
            binding
                .validate_for_queue(self)
                .map_err(|error| error.to_string())?;
        }

        Ok(())
    }

    fn is_bound(&self, id: QueuedTransferId) -> bool {
        self.active_binding
            .is_some_and(|binding| binding.queue_id == id)
    }
}

fn validate_required_text(value: &str, description: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{description} must not be empty",));
    }

    validate_optional_text(value, description)
}

fn validate_optional_text(value: &str, description: &str) -> Result<(), String> {
    if value.len() > MAX_QUEUE_TEXT_BYTES {
        return Err(format!(
            "{description} contains {} bytes, exceeding the {MAX_QUEUE_TEXT_BYTES} byte limit",
            value.len(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{QueuedTransferKind, QueuedTransferRequest, QueuedTransferState, TransferQueue};
    use crate::management_active_binding::ActiveQueueBinding;
    use crate::management_instance::AgentInstanceId;
    use crate::management_route::ManagementRouteMode;

    fn request(name: &str) -> QueuedTransferRequest {
        QueuedTransferRequest {
            sender_agent: "192.0.2.10:7339".parse().unwrap(),

            receiver_agent: "192.0.2.11:7339".parse().unwrap(),

            route_mode: ManagementRouteMode::AutomaticLan,

            source_root: format!(r"C:\{name}"),

            destination_root: format!(r"D:\Backup\{name}",),

            update_existing: true,

            worker_count: 4,

            calibration_mib: 8,

            kind: QueuedTransferKind::Fresh,
        }
    }

    fn instance(value: u128) -> AgentInstanceId {
        AgentInstanceId::from_raw(value).unwrap()
    }

    fn binding(queue_id: super::QueuedTransferId) -> ActiveQueueBinding {
        ActiveQueueBinding::new(queue_id, instance(101), 11, instance(202), 17).unwrap()
    }

    #[test]
    fn queue_ids_survive_reorder_and_removal() {
        let mut queue = TransferQueue::default();

        let desktop = queue.add(request("Desktop")).unwrap();

        let documents = queue.add(request("Documents")).unwrap();

        let downloads = queue.add(request("Downloads")).unwrap();

        assert!(queue.move_up(downloads));

        assert_eq!(
            queue.items().iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![desktop, downloads, documents,],
        );

        assert_eq!(queue.remove(documents).unwrap().id, documents,);

        let pictures = queue.add(request("Pictures")).unwrap();

        assert_eq!(pictures.get(), 4);
    }
    #[test]
    fn desktop_layout_choice_is_retained_by_queue() {
        let mut queue = TransferQueue::default();

        let id = queue
            .add_with_desktop_layout(request("Desktop"), true)
            .unwrap();

        let item = queue.items().iter().find(|item| item.id == id).unwrap();

        assert!(item.preserve_desktop_layout);
    }
    #[test]
    fn running_item_cannot_be_removed_or_reordered() {
        let mut queue = TransferQueue::default();

        let desktop = queue.add(request("Desktop")).unwrap();

        let documents = queue.add(request("Documents")).unwrap();

        queue
            .set_state(desktop, QueuedTransferState::Running, "Transferring")
            .unwrap();

        assert!(!queue.move_down(desktop));

        assert!(!queue.move_up(documents));

        assert!(queue.remove(desktop).is_none());
    }

    #[test]
    fn terminal_and_blocked_items_can_be_reset_without_binding() {
        let mut queue = TransferQueue::default();

        for state in [
            QueuedTransferState::Blocked,
            QueuedTransferState::Failed,
            QueuedTransferState::Completed,
            QueuedTransferState::Cancelled,
        ] {
            let id = queue.add(request("Folder")).unwrap();

            queue.set_state(id, state, "Interrupted").unwrap();

            assert!(queue.reset_to_pending(id),);
        }
    }

    #[test]
    fn binding_requires_running_or_blocked_item() {
        let mut queue = TransferQueue::default();

        let id = queue.add(request("Desktop")).unwrap();

        assert!(queue.set_active_binding(binding(id),).is_err(),);

        queue
            .set_state(id, QueuedTransferState::Running, "Starting")
            .unwrap();

        queue.set_active_binding(binding(id)).unwrap();

        assert_eq!(queue.active_binding(), Some(binding(id)),);
    }

    #[test]
    fn unresolved_binding_blocks_destructive_queue_actions() {
        let mut queue = TransferQueue::default();

        let bound = queue.add(request("Desktop")).unwrap();

        let other = queue.add(request("Documents")).unwrap();

        queue
            .set_state(bound, QueuedTransferState::Blocked, "Endpoint unavailable")
            .unwrap();

        queue.set_active_binding(binding(bound)).unwrap();

        assert!(!queue.reset_to_pending(bound),);

        assert!(!queue.skip(bound));

        assert!(queue.remove(bound).is_none());

        assert!(!queue.move_down(bound));

        assert!(!queue.move_up(other));

        assert_eq!(queue.active_binding(), Some(binding(bound)),);
    }

    #[test]
    fn terminal_state_clears_binding() {
        let mut queue = TransferQueue::default();

        let id = queue.add(request("Desktop")).unwrap();

        queue
            .set_state(id, QueuedTransferState::Running, "Transferring")
            .unwrap();

        queue.set_active_binding(binding(id)).unwrap();

        queue
            .set_state(id, QueuedTransferState::Completed, "Complete")
            .unwrap();

        assert_eq!(queue.active_binding(), None,);
    }

    #[test]
    fn blocked_state_retains_binding() {
        let mut queue = TransferQueue::default();

        let id = queue.add(request("Desktop")).unwrap();

        queue
            .set_state(id, QueuedTransferState::Running, "Transferring")
            .unwrap();

        queue.set_active_binding(binding(id)).unwrap();

        queue
            .set_state(id, QueuedTransferState::Blocked, "Agent unreachable")
            .unwrap();

        assert_eq!(queue.active_binding(), Some(binding(id)),);
    }

    #[test]
    fn persisted_parts_validate_binding() {
        let mut queue = TransferQueue::default();

        let id = queue.add(request("Desktop")).unwrap();

        queue
            .set_state(id, QueuedTransferState::Running, "Transferring")
            .unwrap();

        let restored = TransferQueue::from_parts_with_active_binding(
            queue.next_id(),
            queue.paused_after_current(),
            queue.items().to_vec(),
            Some(binding(id)),
        )
        .unwrap();

        assert_eq!(restored.active_binding(), Some(binding(id)),);
    }

    #[test]
    fn mismatched_persisted_binding_is_rejected() {
        let mut queue = TransferQueue::default();

        let id = queue.add(request("Desktop")).unwrap();

        let result = TransferQueue::from_parts_with_active_binding(
            queue.next_id(),
            false,
            queue.items().to_vec(),
            Some(binding(id)),
        );

        assert!(result.is_err());
    }

    #[test]
    fn binding_replacement_requires_clear() {
        let mut queue = TransferQueue::default();

        let first = queue.add(request("Desktop")).unwrap();

        let second = queue.add(request("Documents")).unwrap();

        queue
            .set_state(first, QueuedTransferState::Running, "")
            .unwrap();

        queue.set_active_binding(binding(first)).unwrap();

        queue
            .set_state(second, QueuedTransferState::Blocked, "")
            .unwrap();

        assert!(queue.set_active_binding(binding(second),).is_err(),);

        assert_eq!(queue.clear_active_binding(), Some(binding(first)),);

        queue.set_active_binding(binding(second)).unwrap();
    }

    #[test]
    fn pending_and_interrupted_items_can_be_skipped_without_binding() {
        let mut queue = TransferQueue::default();

        let pending = queue.add(request("Desktop")).unwrap();

        let failed = queue.add(request("Documents")).unwrap();

        queue
            .set_state(failed, QueuedTransferState::Failed, "Receiver unavailable")
            .unwrap();

        assert!(queue.skip(pending));

        assert!(queue.skip(failed));

        for id in [pending, failed] {
            let item = queue.items().iter().find(|item| item.id == id).unwrap();

            assert_eq!(item.state, QueuedTransferState::Cancelled,);
        }
    }

    #[test]
    fn clear_completed_retains_other_terminal_items() {
        let mut queue = TransferQueue::default();

        let completed = queue.add(request("Desktop")).unwrap();

        let failed = queue.add(request("Documents")).unwrap();

        queue
            .set_state(completed, QueuedTransferState::Completed, "")
            .unwrap();

        queue
            .set_state(failed, QueuedTransferState::Failed, "")
            .unwrap();

        assert_eq!(queue.clear_completed(), 1,);

        assert_eq!(queue.len(), 1);

        assert_eq!(queue.items()[0].id, failed,);
    }
}
