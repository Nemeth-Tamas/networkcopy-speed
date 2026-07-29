use crate::console_progress::ProgressCounter;
use crate::tiny_file_materialize;
use std::collections::VecDeque;
use std::io;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_MATERIALIZATION_WORKERS: usize = 2;
const QUEUED_JOBS_PER_WORKER: usize = 64;
const WAIT_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn validate_worker_count(worker_count: usize) -> io::Result<()> {
    if !(1..=MAX_MATERIALIZATION_WORKERS).contains(&worker_count) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "tiny-file materialization workers must be between 1 and \
                 {MAX_MATERIALIZATION_WORKERS}",
            ),
        ));
    }

    Ok(())
}

pub(crate) struct TinyFileMaterializer {
    handle: TinyFileMaterializerHandle,
    workers: Vec<JoinHandle<io::Result<()>>>,
}

#[derive(Clone)]
pub(crate) struct TinyFileMaterializerHandle {
    inner: Arc<MaterializerInner>,
}

pub(crate) struct TinyFileMaterializeRequest {
    file_id: usize,
    relative_path: PathBuf,
    range: Range<usize>,
}

struct MaterializerInner {
    destination_root: Arc<PathBuf>,
    progress: Option<ProgressCounter>,
    queue_capacity: usize,
    state: Mutex<QueueState>,
    changed: Condvar,
}

struct QueueState {
    jobs: VecDeque<MaterializeJob>,
    shutdown: bool,
    failure: Option<SharedError>,
}

struct MaterializeJob {
    file_id: usize,
    relative_path: PathBuf,
    payload: Arc<[u8]>,
    range: Range<usize>,
    batch: Arc<BatchCompletion>,
}

struct BatchCompletion {
    remaining: AtomicUsize,
}

#[derive(Clone)]
struct SharedError {
    kind: io::ErrorKind,
    message: String,
}

impl TinyFileMaterializeRequest {
    pub(crate) fn new(file_id: usize, relative_path: PathBuf, range: Range<usize>) -> Self {
        Self {
            file_id,
            relative_path,
            range,
        }
    }

    fn validate(&self, payload_bytes: usize) -> io::Result<()> {
        if self.range.start > self.range.end || self.range.end > payload_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "tiny-file materialization range {}..{} exceeds \
                     the {payload_bytes}-byte decoded payload",
                    self.range.start, self.range.end,
                ),
            ));
        }

        Ok(())
    }
}

impl SharedError {
    fn from_error(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn to_io_error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

impl TinyFileMaterializer {
    pub(crate) fn start(
        destination_root: Arc<PathBuf>,
        progress: Option<ProgressCounter>,
    ) -> io::Result<Self> {
        let worker_count = recommended_worker_count();

        Self::start_with_worker_count(destination_root, progress, worker_count)
    }

    fn start_with_worker_count(
        destination_root: Arc<PathBuf>,
        progress: Option<ProgressCounter>,
        worker_count: usize,
    ) -> io::Result<Self> {
        validate_worker_count(worker_count)?;

        let queue_capacity = worker_count
            .checked_mul(QUEUED_JOBS_PER_WORKER)
            .ok_or_else(|| io::Error::other("tiny-file materialization queue size overflowed"))?;

        let inner = Arc::new(MaterializerInner {
            destination_root,
            progress,
            queue_capacity,
            state: Mutex::new(QueueState {
                jobs: VecDeque::with_capacity(queue_capacity),
                shutdown: false,
                failure: None,
            }),
            changed: Condvar::new(),
        });

        let mut workers = Vec::with_capacity(worker_count);

        for worker_index in 0..worker_count {
            let worker_inner = Arc::clone(&inner);

            match thread::Builder::new()
                .name(format!("networkcopy-tiny-materializer-{worker_index}"))
                .spawn(move || worker_loop(worker_inner))
            {
                Ok(worker) => workers.push(worker),

                Err(error) => {
                    request_shutdown(&inner);

                    for worker in workers {
                        let _ = worker.join();
                    }

                    return Err(error);
                }
            }
        }

        Ok(Self {
            handle: TinyFileMaterializerHandle { inner },
            workers,
        })
    }

    pub(crate) fn handle(&self) -> TinyFileMaterializerHandle {
        self.handle.clone()
    }

    pub(crate) fn worker_count(&self) -> usize {
        self.workers.len()
    }

    pub(crate) fn finish(self) -> io::Result<()> {
        let Self { handle, workers } = self;

        request_shutdown(&handle.inner);

        let mut first_worker_error = None;
        let mut worker_panicked = false;

        for worker in workers {
            match worker.join() {
                Ok(Ok(())) => {}

                Ok(Err(error)) => {
                    if first_worker_error.is_none() {
                        first_worker_error = Some(error);
                    }
                }

                Err(_) => {
                    worker_panicked = true;
                }
            }
        }

        let shared_failure = {
            let state = handle
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            state.failure.clone()
        };

        if let Some(failure) = shared_failure {
            return Err(failure.to_io_error());
        }

        if let Some(error) = first_worker_error {
            return Err(error);
        }

        if worker_panicked {
            return Err(io::Error::other(
                "tiny-file materialization worker panicked",
            ));
        }

        Ok(())
    }
}

impl TinyFileMaterializerHandle {
    pub(crate) fn materialize_batch(
        &self,
        payload: Arc<[u8]>,
        requests: Vec<TinyFileMaterializeRequest>,
    ) -> io::Result<()> {
        if requests.is_empty() {
            return Ok(());
        }

        for request in &requests {
            request.validate(payload.len())?;
        }

        let batch = Arc::new(BatchCompletion {
            remaining: AtomicUsize::new(requests.len()),
        });

        for request in requests {
            self.enqueue(MaterializeJob {
                file_id: request.file_id,
                relative_path: request.relative_path,
                payload: Arc::clone(&payload),
                range: request.range,
                batch: Arc::clone(&batch),
            })?;
        }

        self.wait_for_batch(&batch)
    }

    fn enqueue(&self, job: MaterializeJob) -> io::Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| io::Error::other("tiny-file materialization queue lock poisoned"))?;

        loop {
            if let Some(failure) = &state.failure {
                return Err(failure.to_io_error());
            }

            if state.shutdown {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "tiny-file materialization pool has shut down",
                ));
            }

            if let Some(progress) = &self.inner.progress
                && let Err(error) = progress.check_cancelled()
            {
                record_failure_locked(&mut state, &error);
                self.inner.changed.notify_all();

                return Err(error);
            }

            if state.jobs.len() < self.inner.queue_capacity {
                state.jobs.push_back(job);
                self.inner.changed.notify_all();

                return Ok(());
            }

            let waited = self
                .inner
                .changed
                .wait_timeout(state, WAIT_INTERVAL)
                .map_err(|_| {
                    io::Error::other("tiny-file materialization queue lock poisoned while waiting")
                })?;

            state = waited.0;
        }
    }

    fn wait_for_batch(&self, batch: &BatchCompletion) -> io::Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| io::Error::other("tiny-file materialization queue lock poisoned"))?;

        loop {
            if let Some(failure) = &state.failure {
                return Err(failure.to_io_error());
            }

            if batch.remaining.load(Ordering::Acquire) == 0 {
                return Ok(());
            }

            if let Some(progress) = &self.inner.progress
                && let Err(error) = progress.check_cancelled()
            {
                record_failure_locked(&mut state, &error);
                self.inner.changed.notify_all();

                return Err(error);
            }

            let waited = self
                .inner
                .changed
                .wait_timeout(state, WAIT_INTERVAL)
                .map_err(|_| {
                    io::Error::other("tiny-file materialization queue lock poisoned while waiting")
                })?;

            state = waited.0;
        }
    }
}

fn worker_loop(inner: Arc<MaterializerInner>) -> io::Result<()> {
    loop {
        let Some(job) = take_job(&inner)? else {
            return Ok(());
        };

        let result = (|| -> io::Result<()> {
            if let Some(progress) = &inner.progress {
                progress.check_cancelled()?;
            }

            let contents = job.payload.get(job.range.clone()).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "queued tiny-file range exceeds its decoded payload",
                )
            })?;

            tiny_file_materialize::write_verified(
                inner.destination_root.as_path(),
                job.file_id,
                &job.relative_path,
                contents,
            )
        })();

        if let Err(error) = result {
            record_failure(&inner, &error);

            return Err(error);
        }

        let previous = job.batch.remaining.fetch_sub(1, Ordering::AcqRel);

        if previous == 0 {
            let error = io::Error::other("tiny-file materialization batch counter underflowed");

            record_failure(&inner, &error);

            return Err(error);
        }

        inner.changed.notify_all();
    }
}

fn take_job(inner: &MaterializerInner) -> io::Result<Option<MaterializeJob>> {
    let mut state = inner
        .state
        .lock()
        .map_err(|_| io::Error::other("tiny-file materialization queue lock poisoned"))?;

    loop {
        if state.failure.is_some() || state.shutdown {
            return Ok(None);
        }

        if let Some(job) = state.jobs.pop_front() {
            inner.changed.notify_all();

            return Ok(Some(job));
        }

        state = inner.changed.wait(state).map_err(|_| {
            io::Error::other("tiny-file materialization queue lock poisoned while waiting")
        })?;
    }
}

fn record_failure(inner: &MaterializerInner, error: &io::Error) {
    let mut state = inner
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    record_failure_locked(&mut state, error);

    drop(state);

    inner.changed.notify_all();
}

fn record_failure_locked(state: &mut QueueState, error: &io::Error) {
    if state.failure.is_none() {
        state.failure = Some(SharedError::from_error(error));
    }

    state.shutdown = true;
}

fn request_shutdown(inner: &MaterializerInner) {
    let mut state = inner
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    state.shutdown = true;

    drop(state);

    inner.changed.notify_all();
}

fn recommended_worker_count() -> usize {
    thread::available_parallelism()
        .map(|parallelism| worker_count_for_parallelism(parallelism.get()))
        .unwrap_or(MAX_MATERIALIZATION_WORKERS)
}

fn worker_count_for_parallelism(parallelism: usize) -> usize {
    parallelism.clamp(1, MAX_MATERIALIZATION_WORKERS)
}

#[cfg(test)]
mod tests {
    use super::{TinyFileMaterializeRequest, TinyFileMaterializer, worker_count_for_parallelism};
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn worker_policy_caps_materialization_at_two() {
        assert_eq!(worker_count_for_parallelism(1), 1);
        assert_eq!(worker_count_for_parallelism(2), 2);
        assert_eq!(worker_count_for_parallelism(8), 2);
        assert_eq!(worker_count_for_parallelism(64), 2);
    }

    #[test]
    fn shared_pool_materializes_a_complete_batch() {
        let root = temporary_root();

        fs::create_dir_all(root.join("nested")).unwrap();

        let root = Arc::new(root);

        let materializer =
            TinyFileMaterializer::start_with_worker_count(Arc::clone(&root), None, 2).unwrap();

        let handle = materializer.handle();

        let payload: Arc<[u8]> = Arc::from(&b"hello world"[..]);

        handle
            .materialize_batch(
                payload,
                vec![
                    TinyFileMaterializeRequest::new(1, PathBuf::from("nested/hello.txt"), 0..5),
                    TinyFileMaterializeRequest::new(2, PathBuf::from("nested/world.txt"), 6..11),
                ],
            )
            .unwrap();

        materializer.finish().unwrap();

        assert_eq!(fs::read(root.join("nested/hello.txt")).unwrap(), b"hello",);

        assert_eq!(fs::read(root.join("nested/world.txt")).unwrap(), b"world",);

        fs::remove_dir_all(root.as_path()).unwrap();
    }

    #[test]
    fn invalid_payload_range_is_rejected_before_writing() {
        let root = temporary_root();

        fs::create_dir_all(&root).unwrap();

        let root = Arc::new(root);

        let materializer =
            TinyFileMaterializer::start_with_worker_count(Arc::clone(&root), None, 2).unwrap();

        let error = materializer
            .handle()
            .materialize_batch(
                Arc::from(&b"abc"[..]),
                vec![TinyFileMaterializeRequest::new(
                    1,
                    PathBuf::from("invalid.bin"),
                    0..4,
                )],
            )
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

        materializer.finish().unwrap();

        assert!(!root.join("invalid.bin").exists());

        fs::remove_dir_all(root.as_path()).unwrap();
    }

    fn temporary_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        env::temp_dir().join(format!("networkcopy-tiny-pool-{}-{unique}", process::id(),))
    }
}
