use std::io;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

pub(crate) const MAX_TRANSFER_BUFFER_BYTES: u64 = 4 * GIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransferMemoryPlan {
    pub(crate) per_lane_per_peer_bytes: u64,
    pub(crate) per_peer_bytes: u64,
    pub(crate) loopback_bytes: u64,
    pub(crate) budget_bytes: u64,
}

pub(crate) fn plan_loopback(
    data_stream_count: usize,
    network_buffer_bytes: u64,
    copy_buffer_bytes: u64,
    codec_buffer_bytes: u64,
) -> io::Result<TransferMemoryPlan> {
    if data_stream_count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "transfer memory plan requires at least one data stream",
        ));
    }

    let stream_count = u64::try_from(data_stream_count).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "data stream count cannot be represented",
        )
    })?;

    let per_lane_per_peer_bytes = network_buffer_bytes
        .checked_add(copy_buffer_bytes)
        .and_then(|bytes| bytes.checked_add(codec_buffer_bytes))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "per-lane transfer memory overflowed",
            )
        })?;

    let per_peer_bytes = per_lane_per_peer_bytes
        .checked_mul(stream_count)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "per-peer transfer memory overflowed",
            )
        })?;

    let loopback_bytes = per_peer_bytes.checked_mul(2).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "loopback transfer memory overflowed",
        )
    })?;

    if loopback_bytes > MAX_TRANSFER_BUFFER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "planned loopback transfer buffers require \
                 {loopback_bytes} bytes, exceeding the \
                 {MAX_TRANSFER_BUFFER_BYTES}-byte budget"
            ),
        ));
    }

    Ok(TransferMemoryPlan {
        per_lane_per_peer_bytes,
        per_peer_bytes,
        loopback_bytes,
        budget_bytes: MAX_TRANSFER_BUFFER_BYTES,
    })
}

#[cfg(test)]
mod tests {
    use super::{GIB, MAX_TRANSFER_BUFFER_BYTES, MIB, plan_loopback};

    #[test]
    fn accounts_for_sender_and_receiver_lanes() {
        let plan = plan_loopback(4, MIB, 8 * MIB, 2 * MIB).unwrap();

        assert_eq!(plan.per_lane_per_peer_bytes, 11 * MIB);

        assert_eq!(plan.per_peer_bytes, 44 * MIB);

        assert_eq!(plan.loopback_bytes, 88 * MIB);

        assert_eq!(plan.budget_bytes, 4 * GIB);
    }

    #[test]
    fn maximum_stream_count_stays_bounded() {
        let plan = plan_loopback(32, MIB, 8 * MIB, 2 * MIB).unwrap();

        assert_eq!(plan.loopback_bytes, 704 * MIB);

        assert!(plan.loopback_bytes < MAX_TRANSFER_BUFFER_BYTES);
    }

    #[test]
    fn rejects_invalid_or_excessive_plans() {
        assert!(plan_loopback(0, MIB, 8 * MIB, 2 * MIB,).is_err());

        assert!(plan_loopback(1, 2 * GIB, 2 * GIB, MIB,).is_err());
    }
}
