use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const BAR_WIDTH: usize = 20;

const REFRESH_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub struct ProgressCounter {
    inner: Arc<ProgressInner>,
}

#[derive(Debug)]
struct ProgressInner {
    label: Mutex<String>,
    completed: AtomicU64,
    total: AtomicU64,
    finished: AtomicBool,
}

pub struct ConsoleProgress {
    counter: ProgressCounter,

    display: Option<JoinHandle<io::Result<()>>>,
}

impl ProgressCounter {
    fn new(label: &str, total: u64) -> Self {
        Self {
            inner: Arc::new(ProgressInner {
                label: Mutex::new(label.to_string()),

                completed: AtomicU64::new(0),

                total: AtomicU64::new(total),

                finished: AtomicBool::new(false),
            }),
        }
    }

    pub fn add(&self, bytes: u64) {
        let _ =
            self.inner
                .completed
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_add(bytes))
                });
    }

    pub fn set_completed(&self, bytes: u64) {
        self.inner.completed.store(bytes, Ordering::Relaxed);
    }

    pub fn set_total(&self, bytes: u64) {
        self.inner.total.store(bytes, Ordering::Relaxed);
    }

    pub fn total(&self) -> u64 {
        self.inner.total.load(Ordering::Relaxed)
    }

    pub fn set_label(&self, label: impl Into<String>) {
        let mut current = self
            .inner
            .label
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        *current = label.into();
    }

    fn complete(&self) {
        let total = self.total();

        self.set_completed(total);

        self.stop();
    }

    fn stop(&self) {
        self.inner.finished.store(true, Ordering::Release);
    }

    fn is_finished(&self) -> bool {
        self.inner.finished.load(Ordering::Acquire)
    }

    fn snapshot(&self) -> ProgressSnapshot {
        let label = self
            .inner
            .label
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        ProgressSnapshot {
            label,

            completed: self.inner.completed.load(Ordering::Relaxed),

            total: self.inner.total.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
struct ProgressSnapshot {
    label: String,
    completed: u64,
    total: u64,
}

impl ConsoleProgress {
    pub fn start(label: &str, total: u64) -> io::Result<Self> {
        let counter = ProgressCounter::new(label, total);

        let display = if io::stdout().is_terminal() {
            let display_counter = counter.clone();

            Some(
                thread::Builder::new()
                    .name("networkcopy-progress".to_string())
                    .spawn(move || display_loop(display_counter))?,
            )
        } else {
            None
        };

        Ok(Self { counter, display })
    }

    pub fn counter(&self) -> ProgressCounter {
        self.counter.clone()
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.counter.complete();

        self.join_display()
    }

    fn join_display(&mut self) -> io::Result<()> {
        let Some(display) = self.display.take() else {
            return Ok(());
        };

        display
            .join()
            .map_err(|_| io::Error::other("progress display thread panicked"))?
    }
}

impl Drop for ConsoleProgress {
    fn drop(&mut self) {
        self.counter.stop();

        if let Some(display) = self.display.take() {
            let _ = display.join();
        }
    }
}

fn display_loop(counter: ProgressCounter) -> io::Result<()> {
    let started = Instant::now();

    let mut previous_width = 0_usize;

    loop {
        render_progress(&counter, started.elapsed(), &mut previous_width)?;

        if counter.is_finished() {
            break;
        }

        thread::sleep(REFRESH_INTERVAL);
    }

    let mut stdout = io::stdout().lock();

    writeln!(stdout)?;

    stdout.flush()
}

fn render_progress(
    counter: &ProgressCounter,
    elapsed: Duration,
    previous_width: &mut usize,
) -> io::Result<()> {
    let snapshot = counter.snapshot();

    let line = format_progress_line(&snapshot, elapsed);

    let current_width = line.chars().count();

    let clear_width = previous_width.saturating_sub(current_width);

    let mut stdout = io::stdout().lock();

    write!(stdout, "\r{line}{}", " ".repeat(clear_width,),)?;

    stdout.flush()?;

    *previous_width = current_width;

    Ok(())
}

fn format_progress_line(snapshot: &ProgressSnapshot, elapsed: Duration) -> String {
    let megabytes_per_second = if elapsed.is_zero() {
        0.0
    } else {
        snapshot.completed as f64 / elapsed.as_secs_f64() / 1_000_000.0
    };

    if snapshot.total == 0 {
        return format!(
            "{:<24} [waiting]  {}  {:>8.2} MB/s",
            snapshot.label,
            format_amount(snapshot.completed,),
            megabytes_per_second,
        );
    }

    let completed = snapshot.completed.min(snapshot.total);

    let fraction = completed as f64 / snapshot.total as f64;

    let filled = (fraction * BAR_WIDTH as f64).round() as usize;

    let bar = format!(
        "{}{}",
        "#".repeat(filled.min(BAR_WIDTH,),),
        "-".repeat(BAR_WIDTH.saturating_sub(filled,),),
    );

    format!(
        "{:<24} [{}] {:>6.2}%  {} / {}  {:>8.2} MB/s",
        snapshot.label,
        bar,
        fraction * 100.0,
        format_amount(completed),
        format_amount(snapshot.total,),
        megabytes_per_second,
    )
}

fn format_amount(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;
    const TIB: f64 = 1024.0 * GIB;

    let bytes = bytes as f64;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes / TIB,)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB,)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes / MIB,)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes / KIB,)
    } else {
        format!("{bytes:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::{ProgressSnapshot, format_progress_line};
    use std::time::Duration;

    #[test]
    fn progress_line_formats_half_complete_transfer() {
        let line = format_progress_line(
            &ProgressSnapshot {
                label: "Transfer send".to_string(),

                completed: 1024 * 1024,

                total: 2 * 1024 * 1024,
            },
            Duration::from_secs(1),
        );

        assert!(line.contains("50.00%",));

        assert!(line.contains("1.00 MiB / 2.00 MiB",));

        assert!(line.contains("Transfer send",));

        assert!(line.chars().count() < 100);
    }
}
