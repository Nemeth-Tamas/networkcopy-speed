use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StageSample {
    pub elapsed: Duration,

    pub bytes: u64,

    pub operations: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransferStageProfile {
    pub sender_source_read: StageSample,

    pub sender_compression: StageSample,

    pub sender_socket_write: StageSample,

    pub receiver_socket_read: StageSample,

    pub receiver_decompression: StageSample,

    pub receiver_destination_write: StageSample,
}

#[derive(Debug, Default)]
pub(crate) struct TransferProfiler {
    sender_source_read: StageAccumulator,

    sender_compression: StageAccumulator,

    sender_socket_write: StageAccumulator,

    receiver_socket_read: StageAccumulator,

    receiver_decompression: StageAccumulator,

    receiver_destination_write: StageAccumulator,
}

#[derive(Debug, Default)]
struct StageAccumulator {
    elapsed_nanos: AtomicU64,

    bytes: AtomicU64,

    operations: AtomicU64,
}

impl StageAccumulator {
    fn record(&self, elapsed: Duration, bytes: u64) {
        let elapsed_nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);

        atomic_saturating_add(&self.elapsed_nanos, elapsed_nanos);

        atomic_saturating_add(&self.bytes, bytes);

        atomic_saturating_add(&self.operations, 1);
    }

    fn snapshot(&self) -> StageSample {
        StageSample {
            elapsed: Duration::from_nanos(self.elapsed_nanos.load(Ordering::Relaxed)),

            bytes: self.bytes.load(Ordering::Relaxed),

            operations: self.operations.load(Ordering::Relaxed),
        }
    }
}

fn atomic_saturating_add(target: &AtomicU64, value: u64) {
    let _ = target.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(value))
    });
}

impl TransferProfiler {
    pub(crate) fn record_sender_source_read(&self, elapsed: Duration, bytes: u64) {
        self.sender_source_read.record(elapsed, bytes);
    }

    pub(crate) fn record_sender_compression(&self, elapsed: Duration, bytes: u64) {
        self.sender_compression.record(elapsed, bytes);
    }

    pub(crate) fn record_receiver_decompression(&self, elapsed: Duration, bytes: u64) {
        self.receiver_decompression.record(elapsed, bytes);
    }

    pub(crate) fn record_receiver_destination_write(&self, elapsed: Duration, bytes: u64) {
        self.receiver_destination_write.record(elapsed, bytes);
    }

    pub(crate) fn sender_socket_writer<W>(&self, inner: W) -> ProfiledWriter<'_, W>
    where
        W: Write,
    {
        ProfiledWriter {
            inner,

            stage: &self.sender_socket_write,
        }
    }

    pub(crate) fn receiver_socket_reader<R>(&self, inner: R) -> ProfiledReader<'_, R>
    where
        R: Read,
    {
        ProfiledReader {
            inner,

            stage: &self.receiver_socket_read,
        }
    }

    pub(crate) fn snapshot(&self) -> TransferStageProfile {
        TransferStageProfile {
            sender_source_read: self.sender_source_read.snapshot(),

            sender_compression: self.sender_compression.snapshot(),

            sender_socket_write: self.sender_socket_write.snapshot(),

            receiver_socket_read: self.receiver_socket_read.snapshot(),

            receiver_decompression: self.receiver_decompression.snapshot(),

            receiver_destination_write: self.receiver_destination_write.snapshot(),
        }
    }
}

pub(crate) struct ProfiledReader<'a, R> {
    inner: R,

    stage: &'a StageAccumulator,
}

impl<R> Read for ProfiledReader<'_, R>
where
    R: Read,
{
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let started = std::time::Instant::now();

        let result = self.inner.read(buffer);

        if let Ok(bytes) = result {
            self.stage
                .record(started.elapsed(), u64::try_from(bytes).unwrap_or(u64::MAX));
        }

        result
    }
}

pub(crate) struct ProfiledWriter<'a, W> {
    inner: W,

    stage: &'a StageAccumulator,
}

impl<W> Write for ProfiledWriter<'_, W>
where
    W: Write,
{
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let started = std::time::Instant::now();

        let result = self.inner.write(buffer);

        if let Ok(bytes) = result {
            self.stage
                .record(started.elapsed(), u64::try_from(bytes).unwrap_or(u64::MAX));
        }

        result
    }

    fn flush(&mut self) -> io::Result<()> {
        let started = std::time::Instant::now();

        let result = self.inner.flush();

        if result.is_ok() {
            self.stage.record(started.elapsed(), 0);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::TransferProfiler;
    use std::io::{Cursor, Read, Write};
    use std::time::Duration;

    #[test]
    fn stage_samples_accumulate_work() {
        let profiler = TransferProfiler::default();

        profiler.record_sender_source_read(Duration::from_nanos(10), 100);

        profiler.record_sender_source_read(Duration::from_nanos(20), 200);

        let profile = profiler.snapshot();

        assert_eq!(profile.sender_source_read.elapsed, Duration::from_nanos(30),);

        assert_eq!(profile.sender_source_read.bytes, 300,);

        assert_eq!(profile.sender_source_read.operations, 2,);
    }

    #[test]
    fn socket_wrappers_count_actual_io() {
        let profiler = TransferProfiler::default();

        {
            let mut writer = profiler.sender_socket_writer(Vec::<u8>::new());

            writer.write_all(b"hello").unwrap();

            writer.flush().unwrap();
        }

        {
            let source = Cursor::new(b"world".to_vec());

            let mut reader = profiler.receiver_socket_reader(source);

            let mut contents = [0_u8; 5];

            reader.read_exact(&mut contents).unwrap();

            assert_eq!(&contents, b"world");
        }

        let profile = profiler.snapshot();

        assert_eq!(profile.sender_socket_write.bytes, 5,);

        assert!(profile.sender_socket_write.operations >= 1,);

        assert_eq!(profile.receiver_socket_read.bytes, 5,);

        assert!(profile.receiver_socket_read.operations >= 1,);
    }
}
