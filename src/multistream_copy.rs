use crate::adaptive_compression::{MAX_COMPRESSED_CHUNK_BYTES, PayloadDecoder, PayloadEncoder};
use crate::cdc_lane;
use crate::compression_probe;
use crate::console_progress::ProgressCounter;
use crate::content_hash;
use crate::control_plane::{self, ConnectionRole, Handshake, ManifestSummary};
use crate::copy_bench::{binary_mebibytes_per_second, decimal_megabytes_per_second, format_bytes};
use crate::destination_inventory;
use crate::file_metadata;
use crate::manifest_scan::{self, FileClass, ManifestEntry};
use crate::resume_state::{JOURNAL_FILE_NAME, ResumeJournal, ResumeStripe};
use crate::session_cdc_catalog::{
    self, CatalogCandidate, CatalogGeneration, CatalogLimits, CatalogPlan,
};
use crate::session_cdc_lane;
use crate::striped_file;
use crate::tcp_connect;
use crate::tiny_file_pool;
use crate::tiny_pack_codec::{self, TinyPackEncoding};
use crate::transfer_memory;
use crate::update_verification::{self, FILE_DIGEST_BYTES, FileDigest};
use crate::windows_file_replace;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

const MESSAGE_RECEIVER_READY: u8 = 0x30;
const MESSAGE_FILE: u8 = 0x31;
const MESSAGE_STREAM_END: u8 = 0x32;
const MESSAGE_TRANSFER_ACK: u8 = 0x33;
const MESSAGE_FILE_STRIPE: u8 = 0x34;
const MESSAGE_UPDATE_VERIFY_REQUEST: u8 = 0x35;
const MESSAGE_TINY_PACK_V2: u8 = 0x36;
const MESSAGE_UPDATE_VERIFY_RESPONSE: u8 = 0x37;
const MESSAGE_LARGE_CDC_BEGIN: u8 = 0x3C;
const MESSAGE_EXACT_REUSE_PLAN: u8 = 0x3D;
const MESSAGE_GENERATION_COMMIT: u8 = 0x3E;
const MESSAGE_GENERATION_END: u8 = 0x3F;

const NETWORK_BUFFER_BYTES: usize = 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 8 * 1024 * 1024;
const TINY_PACK_TARGET_BYTES: u64 = tiny_pack_codec::MAX_TINY_PACK_BYTES as u64;

const CODEC_BUFFER_BYTES: usize =
    MAX_COMPRESSED_CHUNK_BYTES + tiny_pack_codec::MAX_TINY_PACK_WIRE_BYTES;
const MAX_TINY_PACK_FILES: usize = 4096;
const CONTENT_DIGEST_BYTES: usize = 32;

const TINY_PACK_FIXED_WIRE_BYTES: u64 = 1 + 4 + 8 + 1 + 8 + CONTENT_DIGEST_BYTES as u64;

const TINY_PACK_FILE_METADATA_WIRE_BYTES: u64 = 8 + 8 + CONTENT_DIGEST_BYTES as u64;

const LARGE_CDC_BEGIN_FIXED_WIRE_BYTES: u64 = 1 + 4;

const LARGE_CDC_FILE_ID_WIRE_BYTES: u64 = 8;

const CDC_UNAVAILABLE_WIRE_BYTES: u64 = 1 + 8;

const CDC_INDEX_FIXED_WIRE_BYTES: u64 = 1 + 8 + 8;

const CDC_FALLBACK_WIRE_BYTES: u64 = 1 + 8;

const CDC_PLAN_FIXED_WIRE_BYTES: u64 = 1 + 8 + 8;

const EXACT_REUSE_PLAN_FIXED_WIRE_BYTES: u64 = 1 + 4;

const EXACT_REUSE_RECORD_WIRE_BYTES: u64 = 8 + 8 + CONTENT_DIGEST_BYTES as u64;

const GENERATION_COMMIT_FIXED_WIRE_BYTES: u64 = 1 + 8 + 4 + 4 + 4;
const GENERATION_COMMIT_FILE_ID_WIRE_BYTES: u64 = 8;

const MAX_RESUME_OFFER_STRIPES: u32 = 1_000_000;
const MAX_UNCHANGED_OFFER_FILES: u32 = 1_000_000;
const MAX_GENERATION_COMMIT_FILE_IDS: u32 = 1_000_000;
const CONNECT_RETRY_TIMEOUT: Duration = Duration::from_secs(10);

const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(100);

pub const DEFAULT_DATA_STREAMS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DestinationMode {
    Fresh,
    UpdateVerified,
}

#[derive(Debug)]
pub struct MultistreamCopyReport {
    pub worker_count: usize,
    pub data_stream_count: usize,
    pub tiny_materialization_workers: usize,
    pub files_copied: u64,
    pub bytes_copied: u64,
    pub data_wire_bytes: u64,
    pub compressed_records: u64,

    pub cdc_offered_files: u64,
    pub cdc_files: u64,
    pub cdc_fallback_files: u64,
    pub cdc_logical_bytes: u64,
    pub cdc_reused_bytes: u64,
    pub cdc_literal_bytes: u64,
    pub cdc_index_wire_bytes: u64,
    pub cdc_plan_wire_bytes: u64,
    pub exact_reused_files: u64,
    pub exact_reused_bytes: u64,
    pub exact_reuse_plan_wire_bytes: u64,
    pub resumed_stripes: u64,
    pub resumed_bytes: u64,
    pub skipped_files: u64,
    pub skipped_bytes: u64,
    pub buffer_bytes_per_lane: u64,
    pub buffer_bytes_per_peer: u64,
    pub process_buffer_bytes: u64,
    pub transfer_buffer_budget_bytes: u64,
    pub manifest_wire_bytes: u64,
    pub tiny_pack_count: u64,
    pub compressed_tiny_pack_count: u64,
    pub raw_tiny_pack_count: u64,
    pub tiny_files_packed: u64,
    pub tiny_bytes_packed: u64,
    pub tiny_pack_wire_bytes: u64,
    pub scan_elapsed: Duration,
    pub connection_elapsed: Duration,
    pub manifest_elapsed: Duration,
    pub data_elapsed: Duration,
    pub total_elapsed: Duration,
}

impl MultistreamCopyReport {
    pub fn print(&self) {
        println!("Multistream TCP folder copy complete");
        println!("  Scanner workers:      {}", self.worker_count);
        println!("  TCP data streams:     {}", self.data_stream_count);
        println!(
            "  Tiny write workers:   {}",
            self.tiny_materialization_workers,
        );
        println!(
            "  Files copied:         {}",
            format_bytes(self.files_copied)
        );
        println!(
            "  Data copied:          {} bytes",
            format_bytes(self.bytes_copied)
        );
        println!(
            "  Data wire size:       {} bytes",
            format_bytes(self.data_wire_bytes)
        );
        if self.exact_reused_files > 0 {
            println!(
                "  Exact reused files:   {}",
                format_bytes(self.exact_reused_files,),
            );

            println!(
                "  Exact reused data:    {} bytes",
                format_bytes(self.exact_reused_bytes,),
            );

            println!(
                "  Exact reuse wire:     {} bytes",
                format_bytes(self.exact_reuse_plan_wire_bytes,),
            );

            println!(
                "  Exact reuse savings:  {:.2}%",
                wire_savings_percent(self.exact_reused_bytes, self.exact_reuse_plan_wire_bytes,),
            );
        }

        println!(
            "  CDC offers:           {}",
            format_bytes(self.cdc_offered_files),
        );

        println!("  CDC updated files:    {}", format_bytes(self.cdc_files),);

        println!(
            "  CDC fallbacks:        {}",
            format_bytes(self.cdc_fallback_files),
        );

        println!(
            "  CDC logical data:     {} bytes",
            format_bytes(self.cdc_logical_bytes),
        );

        println!(
            "  CDC reused data:      {} bytes",
            format_bytes(self.cdc_reused_bytes),
        );

        println!(
            "  CDC literal data:     {} bytes",
            format_bytes(self.cdc_literal_bytes),
        );

        println!(
            "  CDC index wire:       {} bytes",
            format_bytes(self.cdc_index_wire_bytes),
        );

        println!(
            "  CDC plan wire:        {} bytes",
            format_bytes(self.cdc_plan_wire_bytes),
        );

        println!("  Compression strategy: adaptive per-record probing");
        println!(
            "  Compressed records:   {}",
            format_bytes(self.compressed_records)
        );
        println!(
            "  Resumed stripes:      {}",
            format_bytes(self.resumed_stripes)
        );

        println!(
            "  Resumed data:         {} bytes",
            format_bytes(self.resumed_bytes)
        );
        println!(
            "  Skipped unchanged:    {} files / {} bytes",
            format_bytes(self.skipped_files),
            format_bytes(self.skipped_bytes),
        );
        println!(
            "  Wire savings:         {:.2}%",
            wire_savings_percent(self.bytes_copied, self.data_wire_bytes,)
        );
        println!(
            "  Buffers per lane:     {} bytes",
            format_bytes(self.buffer_bytes_per_lane)
        );

        println!(
            "  Buffers per peer:     {} bytes",
            format_bytes(self.buffer_bytes_per_peer)
        );

        println!(
            "  Process buffers:      {} / {} bytes",
            format_bytes(self.process_buffer_bytes,),
            format_bytes(self.transfer_buffer_budget_bytes,)
        );
        println!(
            "  Manifest wire size:   {} bytes",
            format_bytes(self.manifest_wire_bytes)
        );
        println!(
            "  Tiny packs:           {} total, {} compressed, {} raw",
            format_bytes(self.tiny_pack_count),
            format_bytes(self.compressed_tiny_pack_count),
            format_bytes(self.raw_tiny_pack_count),
        );
        println!(
            "  Packed tiny files:    {} / {} logical bytes",
            format_bytes(self.tiny_files_packed),
            format_bytes(self.tiny_bytes_packed),
        );
        println!(
            "  Tiny-pack wire size:  {} bytes",
            format_bytes(self.tiny_pack_wire_bytes),
        );
        println!(
            "  Tiny-pack savings:    {:.2}%",
            wire_savings_percent(self.tiny_bytes_packed, self.tiny_pack_wire_bytes),
        );
        println!("  Integrity:            BLAKE3 verified");
        println!(
            "  Scan time:            {:.6} s",
            self.scan_elapsed.as_secs_f64()
        );
        println!(
            "  Connection time:      {:.6} s",
            self.connection_elapsed.as_secs_f64()
        );
        println!(
            "  Manifest time:        {:.6} s",
            self.manifest_elapsed.as_secs_f64()
        );
        println!(
            "  Data transfer time:   {:.6} s",
            self.data_elapsed.as_secs_f64()
        );
        println!(
            "  Total time:           {:.6} s",
            self.total_elapsed.as_secs_f64()
        );
        println!(
            "  Payload throughput:   {:.2} MB/s ({:.2} MiB/s)",
            decimal_megabytes_per_second(self.bytes_copied, self.data_elapsed,),
            binary_mebibytes_per_second(self.bytes_copied, self.data_elapsed,)
        );
    }
}

#[derive(Debug)]
pub struct ReceiveReport {
    pub session_id: u64,
    pub data_stream_count: usize,
    pub tiny_materialization_workers: usize,
    pub files_received: u64,
    pub bytes_received: u64,
    pub data_wire_bytes: u64,
    pub compressed_records: u64,

    pub cdc_offered_files: u64,
    pub cdc_files: u64,
    pub cdc_fallback_files: u64,
    pub cdc_logical_bytes: u64,
    pub cdc_reused_bytes: u64,
    pub cdc_literal_bytes: u64,
    pub cdc_index_wire_bytes: u64,
    pub cdc_plan_wire_bytes: u64,
    pub exact_reused_files: u64,
    pub exact_reused_bytes: u64,
    pub exact_reuse_plan_wire_bytes: u64,
    pub resumed_stripes: u64,
    pub resumed_bytes: u64,
    pub skipped_files: u64,
    pub skipped_bytes: u64,
    pub tiny_pack_count: u64,
    pub compressed_tiny_pack_count: u64,
    pub raw_tiny_pack_count: u64,
    pub tiny_files_packed: u64,
    pub tiny_bytes_packed: u64,
    pub tiny_pack_wire_bytes: u64,
    pub elapsed: Duration,
}

impl ReceiveReport {
    pub fn print(&self) {
        println!("NetworkCopy receive session complete");

        println!("  Session ID:           {:016X}", self.session_id);

        println!("  TCP data streams:     {}", self.data_stream_count);
        println!(
            "  Tiny write workers:     {}",
            self.tiny_materialization_workers,
        );

        println!(
            "  Files received:       {}",
            format_bytes(self.files_received,)
        );

        println!(
            "  Data received:        {} bytes",
            format_bytes(self.bytes_received,)
        );

        println!(
            "  Data wire size:       {} bytes",
            format_bytes(self.data_wire_bytes,)
        );

        if self.exact_reused_files > 0 {
            println!(
                "  Exact reused files:   {}",
                format_bytes(self.exact_reused_files,),
            );

            println!(
                "  Exact reused data:    {} bytes",
                format_bytes(self.exact_reused_bytes,),
            );

            println!(
                "  Exact reuse wire:     {} bytes",
                format_bytes(self.exact_reuse_plan_wire_bytes,),
            );

            println!(
                "  Exact reuse savings:  {:.2}%",
                wire_savings_percent(self.exact_reused_bytes, self.exact_reuse_plan_wire_bytes,),
            );
        }

        println!(
            "  CDC offers:           {}",
            format_bytes(self.cdc_offered_files),
        );

        println!("  CDC updated files:    {}", format_bytes(self.cdc_files),);

        println!(
            "  CDC fallbacks:        {}",
            format_bytes(self.cdc_fallback_files),
        );

        println!(
            "  CDC logical data:     {} bytes",
            format_bytes(self.cdc_logical_bytes),
        );

        println!(
            "  CDC reused data:      {} bytes",
            format_bytes(self.cdc_reused_bytes),
        );

        println!(
            "  CDC literal data:     {} bytes",
            format_bytes(self.cdc_literal_bytes),
        );

        println!(
            "  CDC index wire:       {} bytes",
            format_bytes(self.cdc_index_wire_bytes),
        );

        println!(
            "  CDC plan wire:        {} bytes",
            format_bytes(self.cdc_plan_wire_bytes),
        );

        println!("  Compression strategy: adaptive per-record probing");
        println!(
            "  Compressed records:   {}",
            format_bytes(self.compressed_records,)
        );

        println!(
            "  Tiny packs:           {} total, {} compressed, {} raw",
            format_bytes(self.tiny_pack_count),
            format_bytes(self.compressed_tiny_pack_count),
            format_bytes(self.raw_tiny_pack_count),
        );

        println!(
            "  Packed tiny files:    {} / {} logical bytes",
            format_bytes(self.tiny_files_packed),
            format_bytes(self.tiny_bytes_packed),
        );

        println!(
            "  Tiny-pack wire size:  {} bytes",
            format_bytes(self.tiny_pack_wire_bytes),
        );

        println!(
            "  Tiny-pack savings:    {:.2}%",
            wire_savings_percent(self.tiny_bytes_packed, self.tiny_pack_wire_bytes),
        );

        println!(
            "  Resumed stripes:      {}",
            format_bytes(self.resumed_stripes,)
        );

        println!(
            "  Resumed data:         {} bytes",
            format_bytes(self.resumed_bytes,)
        );
        println!(
            "  Skipped unchanged:    {} files / {} bytes",
            format_bytes(self.skipped_files),
            format_bytes(self.skipped_bytes),
        );

        println!(
            "  Wire savings:         {:.2}%",
            wire_savings_percent(self.bytes_received, self.data_wire_bytes,)
        );

        println!("  Integrity:            BLAKE3 verified");

        println!(
            "  Session time:         {:.6} s",
            self.elapsed.as_secs_f64()
        );

        println!(
            "  Payload throughput:   {:.2} MB/s ({:.2} MiB/s)",
            decimal_megabytes_per_second(self.bytes_received, self.elapsed,),
            binary_mebibytes_per_second(self.bytes_received, self.elapsed,)
        );
    }
}

fn wire_savings_percent(logical_bytes: u64, wire_bytes: u64) -> f64 {
    if logical_bytes == 0 {
        return 0.0;
    }

    100.0 - wire_bytes as f64 / logical_bytes as f64 * 100.0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TransferAck {
    files_copied: u64,
    bytes_copied: u64,
    data_wire_bytes: u64,
    compressed_records: u64,

    cdc: cdc_lane::CdcLaneStats,

    tiny_materialization_workers: u32,
    tiny_pack_count: u64,
    compressed_tiny_pack_count: u64,
    raw_tiny_pack_count: u64,
    tiny_files_packed: u64,
    tiny_bytes_packed: u64,
    tiny_pack_wire_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReceiverReady {
    summary: ManifestSummary,
    completed_stripes: BTreeSet<ResumeStripe>,
    unchanged_file_ids: BTreeSet<usize>,
}

#[derive(Debug)]
struct PreparedDestination {
    journal: ResumeJournal,
    unchanged_file_ids: BTreeSet<usize>,
}

#[derive(Clone, Copy, Debug, Default)]
struct LaneReport {
    files_copied: u64,
    bytes_copied: u64,
    data_wire_bytes: u64,
    compressed_records: u64,

    cdc: cdc_lane::CdcLaneStats,
    exact_reused_files: u64,
    exact_reused_bytes: u64,
    exact_reuse_plan_wire_bytes: u64,
    tiny_pack_count: u64,
    compressed_tiny_pack_count: u64,
    raw_tiny_pack_count: u64,
    tiny_files_packed: u64,
    tiny_bytes_packed: u64,
    tiny_pack_wire_bytes: u64,
}

#[derive(Clone, Debug, Default)]
struct LargeCdcPreflight {
    completed_file_ids: BTreeSet<usize>,

    report: LaneReport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExactFileReuse {
    basis_file_id: usize,
    target_file_id: usize,
    digest: [u8; CONTENT_DIGEST_BYTES],
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ExactReusePlan {
    entries: Vec<ExactFileReuse>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ResumeApplication {
    logical_report: LaneReport,
    stripe_count: u64,
    resumed_bytes: u64,
    unchanged_file_count: u64,
    unchanged_bytes: u64,
}

#[derive(Debug)]
struct TransferFault {
    catalog_limits: CatalogLimits,

    fail_after_checkpointed_stripes: Option<u64>,

    checkpointed_stripes: AtomicU64,

    fail_after_reconstructed_cdc_files: Option<u64>,

    reconstructed_cdc_files: AtomicU64,

    fail_after_persisted_generations: Option<u64>,

    persisted_generations: AtomicU64,
}

impl TransferFault {
    fn disabled() -> Self {
        Self {
            catalog_limits: CatalogLimits::default(),

            fail_after_checkpointed_stripes: None,

            checkpointed_stripes: AtomicU64::new(0),

            fail_after_reconstructed_cdc_files: None,

            reconstructed_cdc_files: AtomicU64::new(0),

            fail_after_persisted_generations: None,

            persisted_generations: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    fn with_catalog_limits(catalog_limits: CatalogLimits) -> io::Result<Self> {
        catalog_limits.validate()?;

        Ok(Self {
            catalog_limits,
            ..Self::disabled()
        })
    }

    #[cfg(test)]
    fn fail_after_checkpointed_stripes(stripe_count: u64) -> io::Result<Self> {
        if stripe_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fault injection stripe count must not be zero",
            ));
        }

        Ok(Self {
            catalog_limits: CatalogLimits::default(),

            fail_after_checkpointed_stripes: Some(stripe_count),

            checkpointed_stripes: AtomicU64::new(0),

            fail_after_reconstructed_cdc_files: None,

            reconstructed_cdc_files: AtomicU64::new(0),

            fail_after_persisted_generations: None,

            persisted_generations: AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    fn fail_after_reconstructed_cdc_files(file_count: u64) -> io::Result<Self> {
        if file_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fault injection CDC file count \
                 must not be zero",
            ));
        }

        Ok(Self {
            catalog_limits: CatalogLimits::default(),

            fail_after_checkpointed_stripes: None,

            checkpointed_stripes: AtomicU64::new(0),

            fail_after_reconstructed_cdc_files: Some(file_count),

            reconstructed_cdc_files: AtomicU64::new(0),

            fail_after_persisted_generations: None,

            persisted_generations: AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    fn fail_after_persisted_generations(generation_count: u64) -> io::Result<Self> {
        if generation_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fault injection generation count must not be zero",
            ));
        }

        Ok(Self {
            catalog_limits: CatalogLimits::default(),

            fail_after_checkpointed_stripes: None,

            checkpointed_stripes: AtomicU64::new(0),

            fail_after_reconstructed_cdc_files: None,

            reconstructed_cdc_files: AtomicU64::new(0),

            fail_after_persisted_generations: Some(generation_count),

            persisted_generations: AtomicU64::new(0),
        })
    }

    fn after_checkpointed_stripe(&self) -> io::Result<()> {
        let Some(failure_limit) = self.fail_after_checkpointed_stripes else {
            return Ok(());
        };

        let completed = self
            .checkpointed_stripes
            .fetch_add(1, Ordering::SeqCst)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("fault-injection stripe count overflowed"))?;

        if completed < failure_limit {
            return Ok(());
        }

        Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!(
                "fault injection stopped the receiver after {completed} checkpointed stripe(s)"
            ),
        ))
    }

    fn after_reconstructed_cdc_file(&self) -> io::Result<()> {
        let Some(failure_limit) = self.fail_after_reconstructed_cdc_files else {
            return Ok(());
        };

        let completed = self
            .reconstructed_cdc_files
            .fetch_add(1, Ordering::SeqCst)
            .checked_add(1)
            .ok_or_else(|| {
                io::Error::other(
                    "fault-injection CDC file \
                         count overflowed",
                )
            })?;

        if completed < failure_limit {
            return Ok(());
        }

        Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!(
                "fault injection stopped the \
                 receiver after {completed} \
                 reconstructed CDC file(s)",
            ),
        ))
    }

    fn after_persisted_generation(&self) -> io::Result<()> {
        let Some(failure_limit) = self.fail_after_persisted_generations else {
            return Ok(());
        };

        let completed = self
            .persisted_generations
            .fetch_add(1, Ordering::SeqCst)
            .checked_add(1)
            .ok_or_else(|| {
                io::Error::other("fault-injection persisted-generation count overflowed")
            })?;

        if completed < failure_limit {
            return Ok(());
        }

        Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!(
                "fault injection stopped the receiver after {completed} persisted generation(s)"
            ),
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TransferTask {
    WholeFile {
        file_id: usize,
    },
    TinyPack {
        file_ids: Vec<usize>,
        total_bytes: u64,
    },
    Stripe {
        file_id: usize,
        offset: u64,
        length: u64,
    },
}

#[derive(Debug)]
struct TransferPlan {
    lanes: Vec<Vec<TransferTask>>,
}

#[derive(Debug)]
struct FreshGenerationPlan {
    catalog: CatalogPlan,
    generation_lanes: Vec<Vec<Vec<TransferTask>>>,
    trailing_lanes: Vec<Vec<TransferTask>>,
    completed_generation_count: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct GenerationCommit {
    generation_index: usize,
    committed_file_ids: Vec<usize>,
    published_file_ids: Vec<usize>,
    evicted_file_ids: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaneEnd {
    Generation(usize),
    Stream,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TransferPlanStats {
    tiny_pack_count: u64,
    tiny_files_packed: u64,
    tiny_bytes_packed: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TinyPackSummary {
    files: u64,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TinyPackTransferSummary {
    summary: TinyPackSummary,

    compressed: bool,

    wire_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StripeDescriptor {
    file_id: usize,
    offset: u64,
    length: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AcceptedSession {
    session_id: u64,
    data_stream_count: usize,
}

struct SendInternalOptions {
    server: Option<thread::JoinHandle<io::Result<ReceiveReport>>>,
    progress: Option<ProgressCounter>,
    catalog_limits: CatalogLimits,
}

pub fn run(
    source_root: &Path,
    destination_root: &Path,
    worker_count: usize,
    data_stream_count: usize,
) -> io::Result<MultistreamCopyReport> {
    run_with_fault(
        source_root,
        destination_root,
        worker_count,
        data_stream_count,
        Arc::new(TransferFault::disabled()),
    )
}

pub fn run_update(
    source_root: &Path,
    destination_root: &Path,
    worker_count: usize,
    data_stream_count: usize,
) -> io::Result<MultistreamCopyReport> {
    run_update_with_fault(
        source_root,
        destination_root,
        worker_count,
        data_stream_count,
        Arc::new(TransferFault::disabled()),
    )
}

fn run_update_with_fault(
    source_root: &Path,
    destination_root: &Path,
    worker_count: usize,
    data_stream_count: usize,
    fault_injection: Arc<TransferFault>,
) -> io::Result<MultistreamCopyReport> {
    manifest_scan::validate_worker_count(worker_count)?;

    control_plane::validate_data_stream_count(data_stream_count)?;

    let memory_plan = transfer_memory::plan_loopback(
        data_stream_count,
        NETWORK_BUFFER_BYTES as u64,
        COPY_BUFFER_BYTES as u64,
        CODEC_BUFFER_BYTES as u64,
    )?;

    let listener = TcpListener::bind(("127.0.0.1", 0))?;

    let address = listener.local_addr()?;

    let server_destination = destination_root.to_path_buf();

    let catalog_limits = fault_injection.catalog_limits;

    let server = thread::Builder::new()
        .name("networkcopy-update-server".to_string())
        .spawn(move || {
            run_server_with_mode(
                &listener,
                server_destination,
                fault_injection,
                None,
                DestinationMode::UpdateVerified,
            )
        })?;

    send_internal(
        address,
        source_root,
        worker_count,
        data_stream_count,
        memory_plan,
        SendInternalOptions {
            server: Some(server),
            progress: None,
            catalog_limits,
        },
    )
}

fn run_with_fault(
    source_root: &Path,
    destination_root: &Path,
    worker_count: usize,
    data_stream_count: usize,
    fault_injection: Arc<TransferFault>,
) -> io::Result<MultistreamCopyReport> {
    manifest_scan::validate_worker_count(worker_count)?;

    control_plane::validate_data_stream_count(data_stream_count)?;

    let memory_plan = transfer_memory::plan_loopback(
        data_stream_count,
        NETWORK_BUFFER_BYTES as u64,
        COPY_BUFFER_BYTES as u64,
        CODEC_BUFFER_BYTES as u64,
    )?;

    let listener = TcpListener::bind(("127.0.0.1", 0))?;

    let address = listener.local_addr()?;

    let server_destination = destination_root.to_path_buf();

    let catalog_limits = fault_injection.catalog_limits;

    let server = thread::Builder::new()
        .name("networkcopy-transfer-server".to_string())
        .spawn(move || run_server(&listener, server_destination, fault_injection, None))?;

    send_internal(
        address,
        source_root,
        worker_count,
        data_stream_count,
        memory_plan,
        SendInternalOptions {
            server: Some(server),
            progress: None,
            catalog_limits,
        },
    )
}

pub fn send(
    receiver_address: SocketAddr,
    source_root: &Path,
    worker_count: usize,
    data_stream_count: usize,
) -> io::Result<MultistreamCopyReport> {
    send_configured(
        receiver_address,
        source_root,
        worker_count,
        data_stream_count,
        None,
    )
}

pub(crate) fn send_with_progress(
    receiver_address: SocketAddr,
    source_root: &Path,
    worker_count: usize,
    data_stream_count: usize,
    progress: ProgressCounter,
) -> io::Result<MultistreamCopyReport> {
    send_configured(
        receiver_address,
        source_root,
        worker_count,
        data_stream_count,
        Some(progress),
    )
}

fn send_configured(
    receiver_address: SocketAddr,
    source_root: &Path,
    worker_count: usize,
    data_stream_count: usize,
    progress: Option<ProgressCounter>,
) -> io::Result<MultistreamCopyReport> {
    manifest_scan::validate_worker_count(worker_count)?;

    control_plane::validate_data_stream_count(data_stream_count)?;

    let memory_plan = transfer_memory::plan_loopback(
        data_stream_count,
        NETWORK_BUFFER_BYTES as u64,
        COPY_BUFFER_BYTES as u64,
        CODEC_BUFFER_BYTES as u64,
    )?;

    send_internal(
        receiver_address,
        source_root,
        worker_count,
        data_stream_count,
        memory_plan,
        SendInternalOptions {
            server: None,
            progress,
            catalog_limits: CatalogLimits::default(),
        },
    )
}

pub(crate) fn connect_with_retry(receiver_address: SocketAddr) -> io::Result<TcpStream> {
    connect_with_retry_config(receiver_address, CONNECT_RETRY_TIMEOUT, CONNECT_RETRY_DELAY)
}

fn connect_with_retry_config(
    receiver_address: SocketAddr,
    timeout: Duration,
    retry_delay: Duration,
) -> io::Result<TcpStream> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "connection retry timeout must not be zero",
        ));
    }

    if retry_delay.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "connection retry delay must not be zero",
        ));
    }

    let started = Instant::now();

    loop {
        match tcp_connect::connect(receiver_address) {
            Ok(stream) => {
                return Ok(stream);
            }

            Err(_) if started.elapsed() < timeout => {
                thread::sleep(retry_delay);
            }

            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "failed to connect to {receiver_address} within {:.1} seconds: {error}",
                        timeout.as_secs_f64()
                    ),
                ));
            }
        }
    }
}

fn send_internal(
    receiver_address: SocketAddr,
    source_root: &Path,
    worker_count: usize,
    data_stream_count: usize,
    memory_plan: transfer_memory::TransferMemoryPlan,
    options: SendInternalOptions,
) -> io::Result<MultistreamCopyReport> {
    let SendInternalOptions {
        server,
        progress,
        catalog_limits,
    } = options;

    let total_started = Instant::now();

    let process_buffer_bytes = if server.is_some() {
        memory_plan.loopback_bytes
    } else {
        memory_plan.per_peer_bytes
    };

    let source_root = source_root.canonicalize()?;

    let scan_result = manifest_scan::run(&source_root, worker_count)?;

    let scan_elapsed = scan_result.report.elapsed;

    let manifest = Arc::new(scan_result.manifest);

    let summary = control_plane::summarize_manifest(&manifest)?;

    if let Some(progress) = &progress {
        progress.check_cancelled()?;

        progress.set_label("Transfer send");

        progress.set_completed(0);

        progress.set_total(summary.total_file_bytes);
    }

    let session_id = control_plane::create_session_id();

    let connection_started = Instant::now();

    let mut control_stream = connect_with_retry(receiver_address)?;
    control_plane::configure_stream(&control_stream)?;

    control_plane::write_handshake(
        &mut control_stream,
        Handshake {
            role: ConnectionRole::Control,
            session_id,
            stream_id: 0,
            stream_count: data_stream_count as u32,
        },
    )?;

    let mut data_streams = Vec::with_capacity(data_stream_count);

    for stream_id in 0..data_stream_count {
        let mut stream = connect_with_retry(receiver_address)?;
        control_plane::configure_stream(&stream)?;

        control_plane::write_handshake(
            &mut stream,
            Handshake {
                role: ConnectionRole::Data,
                session_id,
                stream_id: stream_id as u32,
                stream_count: data_stream_count as u32,
            },
        )?;

        data_streams.push(stream);
    }

    let connection_elapsed = connection_started.elapsed();
    let manifest_started = Instant::now();

    let manifest_wire_bytes = control_plane::send_manifest(&mut control_stream, &manifest)?;

    let manifest_elapsed = manifest_started.elapsed();

    let data_started = Instant::now();

    let mut update_session = false;

    let mut preparation_message = read_u8(&mut control_stream).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "receiver disconnected \
                     while preparing the \
                     transfer: {error}",
            ),
        )
    })?;

    if preparation_message == MESSAGE_UPDATE_VERIFY_REQUEST {
        update_session = true;
        let destination_digests = read_update_verification_request(&mut control_stream)?;

        let candidate_file_ids: BTreeSet<usize> = destination_digests.keys().copied().collect();

        validate_unchanged_offer(&manifest, &candidate_file_ids)?;

        if let Some(progress) = &progress {
            progress.check_cancelled()?;

            progress.set_label("Verifying source files");

            progress.set_completed(0);

            progress.set_total(0);
        }

        let source_digests =
            update_verification::hash_candidates(&source_root, &manifest, &candidate_file_ids)?;

        let matching_file_ids =
            update_verification::matching_candidates(&destination_digests, &source_digests)?;

        write_update_verification_response(&mut control_stream, &matching_file_ids)?;

        preparation_message = read_u8(&mut control_stream).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "receiver disconnected \
                         during large-file CDC \
                         preparation: {error}",
                ),
            )
        })?;
    }

    let large_cdc_preflight = if preparation_message == MESSAGE_LARGE_CDC_BEGIN {
        update_session = true;
        let mut control_reader = control_stream.try_clone()?;

        let preflight = negotiate_large_cdc_sender(
            &mut control_reader,
            &mut control_stream,
            &source_root,
            &manifest,
            progress.as_ref(),
        )?;

        preparation_message = read_u8(&mut control_stream).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "receiver disconnected \
                             after large-file CDC \
                             preparation: {error}",
                ),
            )
        })?;

        preflight
    } else {
        LargeCdcPreflight::default()
    };

    let receiver_ready =
        read_receiver_ready_after_message(&mut control_stream, preparation_message)?;

    if receiver_ready.summary != summary {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receiver acknowledged a different manifest",
        ));
    }

    let mut transfer_plan = build_transfer_plan(&manifest, data_stream_count)?;

    apply_large_cdc_preflight(&mut transfer_plan, &manifest, &large_cdc_preflight)?;

    let exact_reuse_plan = if update_session {
        ExactReusePlan::default()
    } else {
        build_fresh_exact_reuse_plan(&source_root, &manifest, &transfer_plan, progress.as_ref())?
    };

    write_exact_reuse_plan(&mut control_stream, &exact_reuse_plan)?;

    let exact_reuse_report = apply_exact_reuse_plan(
        &mut transfer_plan,
        &manifest,
        &exact_reuse_plan,
        !update_session,
    )?;

    let mut fresh_generation_plan = if update_session {
        None
    } else {
        Some(build_fresh_generation_plan_with_limits(
            &manifest,
            &transfer_plan,
            catalog_limits,
        )?)
    };

    if let Some(generation_plan) = fresh_generation_plan.as_mut() {
        apply_fresh_resume_prefix(generation_plan, &receiver_ready.unchanged_file_ids)?;
    }

    let resume_application = apply_resume_offer(
        &mut transfer_plan,
        &manifest,
        &receiver_ready.completed_stripes,
        &receiver_ready.unchanged_file_ids,
    )?;

    if let Some(generation_plan) = fresh_generation_plan.as_mut() {
        rebuild_fresh_generation_execution(generation_plan, &transfer_plan)?;
    }

    let effective_plan_stats = summarize_transfer_plan(&transfer_plan)?;

    let completed_before_transfer = large_cdc_preflight
        .report
        .bytes_copied
        .checked_add(resume_application.logical_report.bytes_copied)
        .and_then(|bytes| bytes.checked_add(exact_reuse_report.bytes_copied))
        .ok_or_else(|| io::Error::other("initial transfer progress overflowed"))?;

    if let Some(progress) = &progress {
        progress.check_cancelled()?;

        progress.set_label("Transfer send");

        progress.set_total(summary.total_file_bytes);

        progress.set_completed(completed_before_transfer);
    }

    let source_root = Arc::new(source_root);

    let transferred_sender_report = if let Some(generation_plan) = &fresh_generation_plan {
        send_fresh_generation_plan(
            &mut control_stream,
            &data_streams,
            source_root.as_path(),
            manifest.as_slice(),
            generation_plan,
            progress.clone(),
        )?
    } else {
        send_lane_group(
            &data_streams,
            &transfer_plan.lanes,
            source_root.as_path(),
            manifest.as_slice(),
            progress.clone(),
            None,
            LaneEnd::Stream,
        )?
    };

    let sender_report = merge_lane_reports(vec![
        large_cdc_preflight.report,
        resume_application.logical_report,
        exact_reuse_report,
        transferred_sender_report,
    ])?;

    validate_tiny_pack_plan(&sender_report, effective_plan_stats)?;

    if let Some(progress) = &progress {
        progress.check_cancelled()?;

        progress.set_label("Waiting for receiver finalization");

        progress.set_completed(0);

        progress.set_total(0);
    }

    let transfer_ack = read_transfer_ack(&mut control_stream)?;

    let tiny_materialization_workers = usize::try_from(transfer_ack.tiny_materialization_workers)
        .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "receiver tiny-write worker count cannot be represented",
        )
    })?;

    tiny_file_pool::validate_worker_count(tiny_materialization_workers).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "receiver reported an invalid tiny-write worker count: \
                 {error}",
            ),
        )
    })?;

    let data_elapsed = data_started.elapsed();

    drop(control_stream);

    if let Some(server) = server {
        let receiver_report = server
            .join()
            .map_err(|_| io::Error::other("multistream receiver thread panicked"))??;

        if transfer_ack.files_copied != receiver_report.files_received
            || transfer_ack.bytes_copied != receiver_report.bytes_received
            || transfer_ack.data_wire_bytes != receiver_report.data_wire_bytes
            || transfer_ack.compressed_records != receiver_report.compressed_records
            || transfer_ack.cdc.offered_files != receiver_report.cdc_offered_files
            || transfer_ack.cdc.completed_files != receiver_report.cdc_files
            || transfer_ack.cdc.fallback_files != receiver_report.cdc_fallback_files
            || transfer_ack.cdc.logical_bytes != receiver_report.cdc_logical_bytes
            || transfer_ack.cdc.reused_bytes != receiver_report.cdc_reused_bytes
            || transfer_ack.cdc.literal_bytes != receiver_report.cdc_literal_bytes
            || transfer_ack.cdc.index_wire_bytes != receiver_report.cdc_index_wire_bytes
            || transfer_ack.cdc.plan_wire_bytes != receiver_report.cdc_plan_wire_bytes
            || sender_report.exact_reused_files != receiver_report.exact_reused_files
            || sender_report.exact_reused_bytes != receiver_report.exact_reused_bytes
            || sender_report.exact_reuse_plan_wire_bytes
                != receiver_report.exact_reuse_plan_wire_bytes
            || tiny_materialization_workers != receiver_report.tiny_materialization_workers
            || transfer_ack.tiny_pack_count != receiver_report.tiny_pack_count
            || transfer_ack.compressed_tiny_pack_count != receiver_report.compressed_tiny_pack_count
            || transfer_ack.raw_tiny_pack_count != receiver_report.raw_tiny_pack_count
            || transfer_ack.tiny_files_packed != receiver_report.tiny_files_packed
            || transfer_ack.tiny_bytes_packed != receiver_report.tiny_bytes_packed
            || transfer_ack.tiny_pack_wire_bytes != receiver_report.tiny_pack_wire_bytes
        {
            return Err(io::Error::other(
                "client and server transfer reports differ",
            ));
        }
    }

    if transfer_ack.files_copied != summary.entries
        || transfer_ack.bytes_copied != summary.total_file_bytes
    {
        return Err(io::Error::other(
            "receiver did not copy the complete manifest",
        ));
    }

    if sender_report.files_copied != transfer_ack.files_copied
        || sender_report.bytes_copied != transfer_ack.bytes_copied
        || sender_report.data_wire_bytes != transfer_ack.data_wire_bytes
        || sender_report.compressed_records != transfer_ack.compressed_records
        || sender_report.cdc != transfer_ack.cdc
        || sender_report.tiny_pack_count != transfer_ack.tiny_pack_count
        || sender_report.compressed_tiny_pack_count != transfer_ack.compressed_tiny_pack_count
        || sender_report.raw_tiny_pack_count != transfer_ack.raw_tiny_pack_count
        || sender_report.tiny_files_packed != transfer_ack.tiny_files_packed
        || sender_report.tiny_bytes_packed != transfer_ack.tiny_bytes_packed
        || sender_report.tiny_pack_wire_bytes != transfer_ack.tiny_pack_wire_bytes
    {
        return Err(io::Error::other(
            "sender and receiver transfer reports differ",
        ));
    }

    Ok(MultistreamCopyReport {
        worker_count,
        data_stream_count,
        tiny_materialization_workers,
        files_copied: transfer_ack.files_copied,
        bytes_copied: transfer_ack.bytes_copied,
        data_wire_bytes: transfer_ack.data_wire_bytes,
        compressed_records: transfer_ack.compressed_records,

        cdc_offered_files: transfer_ack.cdc.offered_files,

        cdc_files: transfer_ack.cdc.completed_files,

        cdc_fallback_files: transfer_ack.cdc.fallback_files,

        cdc_logical_bytes: transfer_ack.cdc.logical_bytes,

        cdc_reused_bytes: transfer_ack.cdc.reused_bytes,

        cdc_literal_bytes: transfer_ack.cdc.literal_bytes,

        cdc_index_wire_bytes: transfer_ack.cdc.index_wire_bytes,

        cdc_plan_wire_bytes: transfer_ack.cdc.plan_wire_bytes,

        exact_reused_files: sender_report.exact_reused_files,

        exact_reused_bytes: sender_report.exact_reused_bytes,

        exact_reuse_plan_wire_bytes: sender_report.exact_reuse_plan_wire_bytes,

        resumed_stripes: resume_application.stripe_count,

        resumed_bytes: resume_application.resumed_bytes,

        skipped_files: resume_application.unchanged_file_count,

        skipped_bytes: resume_application.unchanged_bytes,

        buffer_bytes_per_lane: memory_plan.per_lane_per_peer_bytes,

        buffer_bytes_per_peer: memory_plan.per_peer_bytes,

        process_buffer_bytes,

        transfer_buffer_budget_bytes: memory_plan.budget_bytes,

        manifest_wire_bytes,
        tiny_pack_count: transfer_ack.tiny_pack_count,
        compressed_tiny_pack_count: transfer_ack.compressed_tiny_pack_count,
        raw_tiny_pack_count: transfer_ack.raw_tiny_pack_count,
        tiny_files_packed: transfer_ack.tiny_files_packed,
        tiny_bytes_packed: transfer_ack.tiny_bytes_packed,
        tiny_pack_wire_bytes: transfer_ack.tiny_pack_wire_bytes,
        scan_elapsed,
        connection_elapsed,
        manifest_elapsed,
        data_elapsed,
        total_elapsed: total_started.elapsed(),
    })
}

pub fn receive_once(listener: TcpListener, destination_root: &Path) -> io::Result<ReceiveReport> {
    receive_on_listener(&listener, destination_root)
}

pub(crate) fn receive_on_listener(
    listener: &TcpListener,
    destination_root: &Path,
) -> io::Result<ReceiveReport> {
    receive_on_listener_internal(listener, destination_root, None)
}

pub(crate) fn receive_on_listener_with_progress(
    listener: &TcpListener,
    destination_root: &Path,
    progress: ProgressCounter,
) -> io::Result<ReceiveReport> {
    receive_on_listener_internal(listener, destination_root, Some(progress))
}

pub(crate) fn receive_on_listener_with_progress_and_mode(
    listener: &TcpListener,
    destination_root: &Path,
    progress: ProgressCounter,
    destination_mode: DestinationMode,
) -> io::Result<ReceiveReport> {
    run_server_with_mode(
        listener,
        destination_root.to_path_buf(),
        Arc::new(TransferFault::disabled()),
        Some(progress),
        destination_mode,
    )
}

fn receive_on_listener_internal(
    listener: &TcpListener,
    destination_root: &Path,
    progress: Option<ProgressCounter>,
) -> io::Result<ReceiveReport> {
    run_server(
        listener,
        destination_root.to_path_buf(),
        Arc::new(TransferFault::disabled()),
        progress,
    )
}

fn run_server(
    listener: &TcpListener,
    destination_root: PathBuf,
    fault_injection: Arc<TransferFault>,
    progress: Option<ProgressCounter>,
) -> io::Result<ReceiveReport> {
    run_server_with_mode(
        listener,
        destination_root,
        fault_injection,
        progress,
        DestinationMode::Fresh,
    )
}

fn run_server_with_mode(
    listener: &TcpListener,
    destination_root: PathBuf,
    fault_injection: Arc<TransferFault>,
    progress: Option<ProgressCounter>,
    destination_mode: DestinationMode,
) -> io::Result<ReceiveReport> {
    let (mut control_stream, data_streams, accepted_session) =
        accept_session(listener, progress.as_ref())?;

    let session_started = Instant::now();

    let data_stream_count = accepted_session.data_stream_count;

    let (manifest, summary, _) = control_plane::receive_manifest_entries(&mut control_stream)?;

    if let Some(progress) = &progress {
        progress.set_label("Transfer receive");

        progress.set_completed(0);

        progress.set_total(summary.total_file_bytes);
    }

    let mut transfer_plan = build_transfer_plan(&manifest, data_stream_count)?;

    let verified_unchanged_file_ids = if destination_mode == DestinationMode::UpdateVerified {
        Some(negotiate_verified_unchanged_files(
            &mut control_stream,
            &destination_root,
            &manifest,
            progress.as_ref(),
        )?)
    } else {
        None
    };

    let large_cdc_preflight = if destination_mode == DestinationMode::UpdateVerified {
        let mut control_reader = control_stream.try_clone()?;

        negotiate_large_cdc_receiver(
            &mut control_reader,
            &mut control_stream,
            &destination_root,
            &manifest,
            verified_unchanged_file_ids.as_ref(),
            progress.as_ref(),
            fault_injection.as_ref(),
        )?
    } else {
        LargeCdcPreflight::default()
    };

    let mut preserved_file_ids = verified_unchanged_file_ids.clone().unwrap_or_default();

    preserved_file_ids.extend(large_cdc_preflight.completed_file_ids.iter().copied());

    let preparation_verified_file_ids = if destination_mode == DestinationMode::UpdateVerified {
        Some(&preserved_file_ids)
    } else {
        None
    };

    let PreparedDestination {
        journal: resume_journal,
        unchanged_file_ids: preserved_file_ids,
    } = prepare_destination(
        &destination_root,
        &manifest,
        summary,
        data_stream_count,
        &transfer_plan,
        destination_mode,
        preparation_verified_file_ids,
    )?;

    let completed_stripes = resume_journal
        .completed_stripes()
        .filter(|stripe| {
            usize::try_from(stripe.file_id)
                .map_or(true, |file_id| !preserved_file_ids.contains(&file_id))
        })
        .collect();

    let mut unchanged_file_ids = preserved_file_ids;

    for &file_id in &large_cdc_preflight.completed_file_ids {
        if !unchanged_file_ids.remove(&file_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "completed large CDC file \
             was not preserved during \
             destination preparation",
            ));
        }
    }

    let receiver_ready = ReceiverReady {
        summary,
        completed_stripes,
        unchanged_file_ids,
    };

    apply_large_cdc_preflight(&mut transfer_plan, &manifest, &large_cdc_preflight)?;

    write_receiver_ready(&mut control_stream, &receiver_ready)?;

    let exact_reuse_plan = read_exact_reuse_plan(&mut control_stream)?;

    let exact_reuse_report = apply_exact_reuse_plan(
        &mut transfer_plan,
        &manifest,
        &exact_reuse_plan,
        destination_mode == DestinationMode::Fresh,
    )?;

    let mut fresh_generation_plan = if destination_mode == DestinationMode::Fresh {
        Some(build_fresh_generation_plan_with_limits(
            &manifest,
            &transfer_plan,
            fault_injection.catalog_limits,
        )?)
    } else {
        None
    };

    if let Some(generation_plan) = fresh_generation_plan.as_mut() {
        apply_fresh_resume_prefix(generation_plan, &receiver_ready.unchanged_file_ids)?;
    }

    let resume_application = apply_resume_offer(
        &mut transfer_plan,
        &manifest,
        &receiver_ready.completed_stripes,
        &receiver_ready.unchanged_file_ids,
    )?;

    if let Some(generation_plan) = fresh_generation_plan.as_mut() {
        rebuild_fresh_generation_execution(generation_plan, &transfer_plan)?;
    }

    let completed_before_transfer = large_cdc_preflight
        .report
        .bytes_copied
        .checked_add(resume_application.logical_report.bytes_copied)
        .ok_or_else(|| io::Error::other("initial receive progress overflowed"))?;

    if let Some(progress) = &progress {
        progress.set_completed(completed_before_transfer);
    }

    let resume_journal = Arc::new(Mutex::new(resume_journal));

    let manifest = Arc::new(manifest);

    let destination_root = Arc::new(destination_root);

    let tiny_materializer = tiny_file_pool::TinyFileMaterializer::start(
        Arc::clone(&destination_root),
        progress.clone(),
    )?;

    let tiny_materialization_workers = tiny_materializer.worker_count();

    let tiny_materializer_handle = tiny_materializer.handle();

    let lane_result = if let Some(generation_plan) = &fresh_generation_plan {
        receive_fresh_generation_plan(
            &mut control_stream,
            &data_streams,
            destination_root.as_path(),
            manifest.as_slice(),
            resume_journal.as_ref(),
            fault_injection.as_ref(),
            &tiny_materializer_handle,
            progress.clone(),
            generation_plan,
        )
    } else {
        receive_lane_group(
            &data_streams,
            &transfer_plan.lanes,
            destination_root.as_path(),
            manifest.as_slice(),
            resume_journal.as_ref(),
            fault_injection.as_ref(),
            &tiny_materializer_handle,
            progress.clone(),
            true,
            None,
            LaneEnd::Stream,
        )
    };

    let materializer_result = tiny_materializer.finish();

    let transferred_report = match (lane_result, materializer_result) {
        (Ok(report), Ok(())) => report,

        (Err(error), _) => return Err(error),

        (Ok(_), Err(error)) => return Err(error),
    };

    let report = merge_lane_reports(vec![
        large_cdc_preflight.report,
        resume_application.logical_report,
        exact_reuse_report,
        transferred_report,
    ])?;

    if let Some(progress) = &progress {
        progress.check_cancelled()?;

        progress.set_label("Finalizing destination");

        progress.set_completed(0);

        progress.set_total(0);
    }

    finalize_large_files(destination_root.as_path(), &manifest, destination_mode)?;

    materialize_exact_reuse_files(
        destination_root.as_path(),
        &manifest,
        &exact_reuse_plan,
        progress.as_ref(),
    )?;

    file_metadata::restore_manifest_files(destination_root.as_path(), &manifest)?;

    if let Some(progress) = &progress {
        progress.check_cancelled()?;
    }

    let ack = TransferAck {
        files_copied: report.files_copied,
        bytes_copied: report.bytes_copied,
        data_wire_bytes: report.data_wire_bytes,
        compressed_records: report.compressed_records,
        cdc: report.cdc,
        tiny_materialization_workers: u32::try_from(tiny_materialization_workers).map_err(
            |_| io::Error::other("tiny-file materialization worker count cannot be represented"),
        )?,
        tiny_pack_count: report.tiny_pack_count,
        compressed_tiny_pack_count: report.compressed_tiny_pack_count,
        raw_tiny_pack_count: report.raw_tiny_pack_count,
        tiny_files_packed: report.tiny_files_packed,
        tiny_bytes_packed: report.tiny_bytes_packed,
        tiny_pack_wire_bytes: report.tiny_pack_wire_bytes,
    };

    if ack.files_copied != summary.entries || ack.bytes_copied != summary.total_file_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "received payload does not match the manifest summary",
        ));
    }

    write_transfer_ack(&mut control_stream, ack)?;

    drop(resume_journal);

    ResumeJournal::remove(destination_root.as_path())?;

    Ok(ReceiveReport {
        session_id: accepted_session.session_id,

        data_stream_count,

        tiny_materialization_workers,

        files_received: ack.files_copied,

        bytes_received: ack.bytes_copied,

        data_wire_bytes: ack.data_wire_bytes,

        compressed_records: ack.compressed_records,

        cdc_offered_files: ack.cdc.offered_files,

        cdc_files: ack.cdc.completed_files,

        cdc_fallback_files: ack.cdc.fallback_files,

        cdc_logical_bytes: ack.cdc.logical_bytes,

        cdc_reused_bytes: ack.cdc.reused_bytes,

        cdc_literal_bytes: ack.cdc.literal_bytes,

        cdc_index_wire_bytes: ack.cdc.index_wire_bytes,

        cdc_plan_wire_bytes: ack.cdc.plan_wire_bytes,

        exact_reused_files: report.exact_reused_files,

        exact_reused_bytes: report.exact_reused_bytes,

        exact_reuse_plan_wire_bytes: report.exact_reuse_plan_wire_bytes,

        resumed_stripes: resume_application.stripe_count,

        resumed_bytes: resume_application.resumed_bytes,

        skipped_files: resume_application.unchanged_file_count,

        skipped_bytes: resume_application.unchanged_bytes,

        tiny_pack_count: ack.tiny_pack_count,

        compressed_tiny_pack_count: ack.compressed_tiny_pack_count,

        raw_tiny_pack_count: ack.raw_tiny_pack_count,

        tiny_files_packed: ack.tiny_files_packed,

        tiny_bytes_packed: ack.tiny_bytes_packed,

        tiny_pack_wire_bytes: ack.tiny_pack_wire_bytes,

        elapsed: session_started.elapsed(),
    })
}

fn accept_with_progress(
    listener: &TcpListener,
    progress: Option<&ProgressCounter>,
) -> io::Result<(TcpStream, SocketAddr)> {
    let Some(progress) = progress else {
        return listener.accept();
    };

    listener.set_nonblocking(true)?;

    let result = loop {
        if let Err(error) = progress.check_cancelled() {
            break Err(error);
        }

        match listener.accept() {
            Ok(accepted) => {
                break Ok(accepted);
            }

            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }

            Err(error) => {
                break Err(error);
            }
        }
    };

    let restore = listener.set_nonblocking(false);

    match (result, restore) {
        (Ok((stream, address)), Ok(())) => {
            stream.set_nonblocking(false)?;

            Ok((stream, address))
        }

        (Err(error), _) => Err(error),

        (Ok(_), Err(error)) => Err(error),
    }
}

fn accept_session(
    listener: &TcpListener,
    progress: Option<&ProgressCounter>,
) -> io::Result<(TcpStream, Vec<TcpStream>, AcceptedSession)> {
    let (mut control_stream, _control_peer) = accept_with_progress(listener, progress)?;

    control_plane::configure_stream(&control_stream)?;

    let control_handshake = control_plane::read_handshake(&mut control_stream)?;

    if control_handshake.role != ConnectionRole::Control {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "first session connection must be the control connection",
        ));
    }

    if control_handshake.stream_id != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control stream used a nonzero ID",
        ));
    }

    let data_stream_count = usize::try_from(control_handshake.stream_count).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "control stream count cannot be represented",
        )
    })?;

    control_plane::validate_data_stream_count(data_stream_count)?;

    let accepted_session = AcceptedSession {
        session_id: control_handshake.session_id,

        data_stream_count,
    };

    let mut data_streams: Vec<Option<TcpStream>> = std::iter::repeat_with(|| None)
        .take(data_stream_count)
        .collect();

    for _ in 0..data_stream_count {
        let (mut stream, _data_peer) = accept_with_progress(listener, progress)?;

        control_plane::configure_stream(&stream)?;

        let handshake = control_plane::read_handshake(&mut stream)?;

        if handshake.role != ConnectionRole::Data {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "additional session connection must be a data connection",
            ));
        }

        if handshake.session_id != accepted_session.session_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "connection used an incorrect session ID",
            ));
        }

        if handshake.stream_count != control_handshake.stream_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "connection used an incorrect stream count",
            ));
        }

        let stream_id = usize::try_from(handshake.stream_id).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "data stream ID cannot be represented",
            )
        })?;

        let slot = data_streams.get_mut(stream_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "data stream ID is outside the negotiated range",
            )
        })?;

        if slot.replace(stream).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "duplicate data stream ID",
            ));
        }
    }

    let mut ordered_streams = Vec::with_capacity(data_stream_count);

    for (stream_id, stream) in data_streams.into_iter().enumerate() {
        ordered_streams.push(stream.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("data stream {stream_id} was not established"),
            )
        })?);
    }

    Ok((control_stream, ordered_streams, accepted_session))
}

fn negotiate_verified_unchanged_files(
    control_stream: &mut TcpStream,
    destination_root: &Path,
    manifest: &[ManifestEntry],
    progress: Option<&ProgressCounter>,
) -> io::Result<BTreeSet<usize>> {
    if !destination_root.try_exists()? {
        return Ok(BTreeSet::new());
    }

    let inventory = destination_inventory::compare_fast(destination_root, manifest)?;

    validate_update_inventory(destination_root, &inventory)?;

    let candidate_file_ids = inventory.unchanged_file_ids;

    if candidate_file_ids.is_empty() {
        return Ok(candidate_file_ids);
    }

    if let Some(progress) = progress {
        progress.check_cancelled()?;
        progress.set_label("Verifying destination files");
        progress.set_completed(0);
        progress.set_total(0);
    }

    let destination_digests =
        update_verification::hash_candidates(destination_root, manifest, &candidate_file_ids)?;

    write_update_verification_request(control_stream, &destination_digests)?;

    let matching_file_ids = read_update_verification_response(control_stream)?;

    if !matching_file_ids.is_subset(&candidate_file_ids) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sender verified a file that was not offered as a candidate",
        ));
    }

    if let Some(progress) = progress {
        progress.check_cancelled()?;
        progress.set_label("Transfer receive");
        progress.set_completed(0);
    }

    Ok(matching_file_ids)
}

fn prepare_destination(
    destination_root: &Path,
    manifest: &[ManifestEntry],
    summary: ManifestSummary,
    data_stream_count: usize,
    transfer_plan: &TransferPlan,
    destination_mode: DestinationMode,
    verified_unchanged_file_ids: Option<&BTreeSet<usize>>,
) -> io::Result<PreparedDestination> {
    if destination_root.try_exists()? {
        let root_metadata = fs::metadata(destination_root)?;

        if !root_metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "destination is not a directory: {}",
                    destination_root.display()
                ),
            ));
        }

        let journal_path = destination_root.join(JOURNAL_FILE_NAME);

        if journal_path.try_exists()? {
            let (unchanged_file_ids, reset_file_ids) = if destination_mode == DestinationMode::Fresh
            {
                (BTreeSet::new(), BTreeSet::new())
            } else {
                let inventory = destination_inventory::compare_fast(destination_root, manifest)?;

                validate_update_inventory(destination_root, &inventory)?;

                select_update_file_sets(destination_mode, &inventory, verified_unchanged_file_ids)?
            };

            let (journal, unchanged_file_ids) =
                prepare_resume_destination(ResumeDestinationPreparation {
                    destination_root,
                    manifest,
                    summary,
                    data_stream_count,
                    transfer_plan,
                    unchanged_file_ids: &unchanged_file_ids,
                    reset_file_ids: &reset_file_ids,
                    preserve_existing_final: destination_mode != DestinationMode::Fresh,
                })?;

            return Ok(PreparedDestination {
                journal,
                unchanged_file_ids,
            });
        }

        let mut entries = fs::read_dir(destination_root)?;

        if entries.next().transpose()?.is_some() {
            let inventory = destination_inventory::compare_fast(destination_root, manifest)?;

            if destination_mode == DestinationMode::Fresh {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "destination directory is not empty and update mode is not enabled: {}. Fast comparison found {} unchanged file(s) / {} byte(s), {} changed file(s), {} missing file(s), and {} conflicting entry or entries",
                        destination_root.display(),
                        inventory.unchanged_files(),
                        inventory.unchanged_bytes,
                        inventory.changed_files,
                        inventory.missing_files,
                        inventory.conflicting_entries,
                    ),
                ));
            }

            validate_update_inventory(destination_root, &inventory)?;

            let (unchanged_file_ids, _reset_file_ids) =
                select_update_file_sets(destination_mode, &inventory, verified_unchanged_file_ids)?;

            let journal = prepare_update_destination(
                destination_root,
                manifest,
                summary,
                data_stream_count,
                &unchanged_file_ids,
            )?;

            return Ok(PreparedDestination {
                journal,
                unchanged_file_ids,
            });
        }
    } else {
        fs::create_dir_all(destination_root)?;
    }

    let journal =
        prepare_fresh_destination(destination_root, manifest, summary, data_stream_count)?;

    Ok(PreparedDestination {
        journal,

        unchanged_file_ids: BTreeSet::new(),
    })
}

fn select_update_file_sets(
    destination_mode: DestinationMode,
    inventory: &destination_inventory::DestinationInventory,
    verified_unchanged_file_ids: Option<&BTreeSet<usize>>,
) -> io::Result<(BTreeSet<usize>, BTreeSet<usize>)> {
    match destination_mode {
        DestinationMode::Fresh => Ok((BTreeSet::new(), BTreeSet::new())),

        DestinationMode::UpdateVerified => {
            let verified = verified_unchanged_file_ids.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "verified update mode did not negotiate unchanged files",
                )
            })?;

            if !verified.is_subset(&inventory.unchanged_file_ids) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "verified unchanged set is not a subset of the current destination inventory",
                ));
            }

            let reset_file_ids = inventory
                .unchanged_file_ids
                .difference(verified)
                .copied()
                .collect();

            Ok((verified.clone(), reset_file_ids))
        }
    }
}

fn validate_update_inventory(
    destination_root: &Path,
    inventory: &destination_inventory::DestinationInventory,
) -> io::Result<()> {
    if inventory.conflicting_entries == 0 {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "destination contains {} conflicting entry or entries at source file paths and cannot be updated safely: {}",
            inventory.conflicting_entries,
            destination_root.display(),
        ),
    ))
}

fn prepare_fresh_destination(
    destination_root: &Path,
    manifest: &[ManifestEntry],
    summary: ManifestSummary,
    data_stream_count: usize,
) -> io::Result<ResumeJournal> {
    for (file_id, entry) in manifest.iter().enumerate() {
        create_destination_parent(destination_root, entry)?;

        if entry.class != FileClass::Large {
            continue;
        }

        let final_path = destination_root.join(&entry.relative_path);

        let temporary_path = temporary_path(&final_path, file_id);

        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(temporary_path)?;

        file.set_len(entry.file_size)?;
    }

    let journal = ResumeJournal::new(summary.fingerprint, data_stream_count)?;

    journal.save_atomic(destination_root)?;

    ResumeJournal::load_existing(destination_root, summary.fingerprint, data_stream_count)
}

fn prepare_update_destination(
    destination_root: &Path,
    manifest: &[ManifestEntry],
    summary: ManifestSummary,
    data_stream_count: usize,
    unchanged_file_ids: &BTreeSet<usize>,
) -> io::Result<ResumeJournal> {
    for (file_id, entry) in manifest.iter().enumerate() {
        create_destination_parent(destination_root, entry)?;

        let final_path = destination_root.join(&entry.relative_path);

        let temporary_path = temporary_path(&final_path, file_id);

        remove_file_if_present(&temporary_path)?;

        if unchanged_file_ids.contains(&file_id) {
            continue;
        }

        if entry.class != FileClass::Large {
            continue;
        }

        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)?;

        file.set_len(entry.file_size)?;
    }

    let journal = ResumeJournal::new(summary.fingerprint, data_stream_count)?;

    journal.save_atomic(destination_root)?;

    ResumeJournal::load_existing(destination_root, summary.fingerprint, data_stream_count)
}

struct ResumeDestinationPreparation<'a> {
    destination_root: &'a Path,

    manifest: &'a [ManifestEntry],

    summary: ManifestSummary,

    data_stream_count: usize,

    transfer_plan: &'a TransferPlan,

    unchanged_file_ids: &'a BTreeSet<usize>,

    reset_file_ids: &'a BTreeSet<usize>,

    preserve_existing_final: bool,
}

fn prepare_resume_destination(
    preparation: ResumeDestinationPreparation<'_>,
) -> io::Result<(ResumeJournal, BTreeSet<usize>)> {
    let ResumeDestinationPreparation {
        destination_root,
        manifest,
        summary,
        data_stream_count,
        transfer_plan,
        unchanged_file_ids,
        reset_file_ids,
        preserve_existing_final,
    } = preparation;

    let root_metadata = fs::metadata(destination_root)?;

    if !root_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "resume destination is not a directory: {}",
                destination_root.display()
            ),
        ));
    }

    let mut journal =
        ResumeJournal::load_existing(destination_root, summary.fingerprint, data_stream_count)?;

    if journal.remove_completed_files(reset_file_ids) {
        journal.save_atomic(destination_root)?;
    }

    let mut effective_unchanged_file_ids = unchanged_file_ids.clone();

    if !preserve_existing_final {
        let completed_file_ids: BTreeSet<usize> = journal.completed_file_ids().collect();

        validate_fresh_resume_committed_files(destination_root, manifest, &completed_file_ids)?;

        effective_unchanged_file_ids.extend(completed_file_ids);
    }

    let completed_stripes: BTreeSet<ResumeStripe> = journal.completed_stripes().collect();

    validate_resume_offer(transfer_plan, &completed_stripes)?;

    for (file_id, entry) in manifest.iter().enumerate() {
        create_destination_parent(destination_root, entry)?;

        let final_path = destination_root.join(&entry.relative_path);

        let temporary_path = temporary_path(&final_path, file_id);

        if effective_unchanged_file_ids.contains(&file_id) {
            remove_file_if_present(&temporary_path)?;
            continue;
        }

        match entry.class {
            FileClass::Large => {
                prepare_resume_large_file(ResumeLargeFilePreparation {
                    file_id,
                    entry,
                    final_path: &final_path,
                    temporary_path: &temporary_path,
                    transfer_plan,
                    completed_stripes: &completed_stripes,
                    reset_completed_stripes: reset_file_ids.contains(&file_id),
                    preserve_existing_final,
                })?;
            }

            FileClass::Medium | FileClass::Tiny => {
                remove_file_if_present(&temporary_path)?;

                if !preserve_existing_final {
                    remove_file_if_present(&final_path)?;
                }
            }
        }
    }

    Ok((journal, effective_unchanged_file_ids))
}

fn validate_fresh_resume_committed_files(
    destination_root: &Path,
    manifest: &[ManifestEntry],
    completed_file_ids: &BTreeSet<usize>,
) -> io::Result<()> {
    for &file_id in completed_file_ids {
        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("resume journal references unknown completed file ID {file_id}"),
            )
        })?;

        if entry.class == FileClass::Tiny {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("resume journal marks tiny file {file_id} as a committed catalog file"),
            ));
        }

        let final_path = destination_root.join(&entry.relative_path);

        let metadata = fs::symlink_metadata(&final_path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "journaled committed file {file_id} is unavailable: {}: {error}",
                    final_path.display(),
                ),
            )
        })?;

        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "journaled committed file {file_id} is not a regular file: {}",
                    final_path.display(),
                ),
            ));
        }

        if metadata.len() != entry.file_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "journaled committed file {file_id} contains {} bytes, expected {}: {}",
                    metadata.len(),
                    entry.file_size,
                    final_path.display(),
                ),
            ));
        }

        if metadata.last_write_time() != entry.last_write_time {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "journaled committed file {file_id} has a different last-write time: {}",
                    final_path.display(),
                ),
            ));
        }

        if metadata.file_attributes() != entry.file_attributes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "journaled committed file {file_id} has different Windows attributes: {}",
                    final_path.display(),
                ),
            ));
        }
    }

    Ok(())
}

fn create_destination_parent(destination_root: &Path, entry: &ManifestEntry) -> io::Result<()> {
    let Some(parent) = entry.relative_path.parent() else {
        return Ok(());
    };

    if parent.as_os_str().is_empty() {
        return Ok(());
    }

    fs::create_dir_all(destination_root.join(parent))
}

struct ResumeLargeFilePreparation<'a> {
    file_id: usize,

    entry: &'a ManifestEntry,

    final_path: &'a Path,

    temporary_path: &'a Path,

    transfer_plan: &'a TransferPlan,

    completed_stripes: &'a BTreeSet<ResumeStripe>,

    reset_completed_stripes: bool,

    preserve_existing_final: bool,
}

fn prepare_resume_large_file(preparation: ResumeLargeFilePreparation<'_>) -> io::Result<()> {
    let ResumeLargeFilePreparation {
        file_id,
        entry,
        final_path,
        temporary_path,
        transfer_plan,
        completed_stripes,
        reset_completed_stripes,
        preserve_existing_final,
    } = preparation;
    let final_exists = final_path.try_exists()?;

    let temporary_exists = temporary_path.try_exists()?;

    match (final_exists, temporary_exists) {
        (false, true) => {
            validate_resume_file(temporary_path, entry.file_size, "partial large file")
        }

        (true, false) if preserve_existing_final && reset_completed_stripes => {
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(temporary_path)?;

            file.set_len(entry.file_size)
        }

        (true, false) => {
            if !all_file_stripes_completed(file_id, transfer_plan, completed_stripes)? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "final large file exists but its resume journal is incomplete: {}",
                        final_path.display()
                    ),
                ));
            }

            validate_resume_file(final_path, entry.file_size, "finalized large file")
        }

        (true, true) if preserve_existing_final => {
            validate_resume_file(temporary_path, entry.file_size, "partial update file")
        }

        (true, true) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "both final and temporary large files exist during resume: {}",
                final_path.display()
            ),
        )),

        (false, false) => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "resume journal exists but the large partial file is missing: {}",
                temporary_path.display()
            ),
        )),
    }
}

fn all_file_stripes_completed(
    file_id: usize,
    transfer_plan: &TransferPlan,
    completed_stripes: &BTreeSet<ResumeStripe>,
) -> io::Result<bool> {
    let mut found_stripe = false;

    for task in transfer_plan.lanes.iter().flatten() {
        let TransferTask::Stripe {
            file_id: task_file_id,
            offset,
            length,
        } = task
        else {
            continue;
        };

        if *task_file_id != file_id {
            continue;
        }

        found_stripe = true;

        let stripe = ResumeStripe::new(*task_file_id, *offset, *length)?;

        if !completed_stripes.contains(&stripe) {
            return Ok(false);
        }
    }

    Ok(found_stripe)
}

fn validate_resume_file(path: &Path, expected_size: u64, description: &str) -> io::Result<()> {
    let metadata = fs::metadata(path)?;

    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} is not a regular file: {}", path.display()),
        ));
    }

    let actual_size = metadata.len();

    if actual_size != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{description} has {actual_size} bytes, expected {expected_size}: {}",
                path.display()
            ),
        ));
    }

    Ok(())
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),

        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),

        Err(error) => Err(error),
    }
}

fn build_transfer_plan(
    manifest: &[ManifestEntry],
    data_stream_count: usize,
) -> io::Result<TransferPlan> {
    let mut lanes = vec![Vec::new(); data_stream_count];

    let mut assigned_bytes = vec![0_u64; data_stream_count];

    let mut medium_file_ids = Vec::new();
    let mut tiny_file_ids = Vec::new();

    for (file_id, entry) in manifest.iter().enumerate() {
        match entry.class {
            FileClass::Large => {
                for lane_id in 0..data_stream_count {
                    let (offset, length) =
                        striped_file::stripe_range(entry.file_size, lane_id, data_stream_count)?;

                    if length == 0 {
                        continue;
                    }

                    lanes[lane_id].push(TransferTask::Stripe {
                        file_id,
                        offset,
                        length,
                    });

                    assigned_bytes[lane_id] = assigned_bytes[lane_id]
                        .checked_add(length)
                        .ok_or_else(|| io::Error::other("striped lane size overflowed"))?;
                }
            }

            FileClass::Medium => {
                medium_file_ids.push(file_id);
            }

            FileClass::Tiny => {
                tiny_file_ids.push(file_id);
            }
        }
    }

    medium_file_ids
        .sort_unstable_by(|left, right| manifest[*right].file_size.cmp(&manifest[*left].file_size));

    for file_id in medium_file_ids {
        let lane = least_loaded_lane(&lanes, &assigned_bytes)?;

        lanes[lane].push(TransferTask::WholeFile { file_id });

        assigned_bytes[lane] = assigned_bytes[lane]
            .checked_add(manifest[file_id].file_size)
            .ok_or_else(|| io::Error::other("data-lane assignment size overflowed"))?;
    }

    for (file_ids, total_bytes) in build_tiny_packs(manifest, tiny_file_ids)? {
        let lane = least_loaded_lane(&lanes, &assigned_bytes)?;

        lanes[lane].push(TransferTask::TinyPack {
            file_ids,
            total_bytes,
        });

        assigned_bytes[lane] = assigned_bytes[lane]
            .checked_add(total_bytes)
            .ok_or_else(|| io::Error::other("tiny-pack lane size overflowed"))?;
    }

    Ok(TransferPlan { lanes })
}

fn build_fresh_generation_plan_with_limits(
    manifest: &[ManifestEntry],
    transfer_plan: &TransferPlan,
    limits: CatalogLimits,
) -> io::Result<FreshGenerationPlan> {
    let candidates = collect_fresh_catalog_candidates(manifest, transfer_plan)?;

    let catalog = session_cdc_catalog::plan(&candidates, limits)?;

    let lane_count = transfer_plan.lanes.len();

    let mut generation_by_file_id = BTreeMap::new();

    for (generation_index, generation) in catalog.generations.iter().enumerate() {
        if generation.index != generation_index {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fresh-transfer catalog generation index is not contiguous",
            ));
        }

        for candidate in &generation.transfer_files {
            if generation_by_file_id
                .insert(candidate.file_id, generation_index)
                .is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer catalog schedules the same file in multiple generations",
                ));
            }
        }
    }

    let mut generation_lanes: Vec<Vec<Vec<TransferTask>>> = (0..catalog.generations.len())
        .map(|_| vec![Vec::new(); lane_count])
        .collect();

    let mut trailing_lanes = vec![Vec::new(); lane_count];

    for (lane_id, lane) in transfer_plan.lanes.iter().enumerate() {
        for task in lane {
            let Some(file_id) = catalog_task_file_id(task) else {
                trailing_lanes[lane_id].push(task.clone());

                continue;
            };

            let generation_index = generation_by_file_id.get(&file_id).copied().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "fresh-transfer task for file {file_id} was not assigned to a catalog generation"
                    ),
                )
            })?;

            let generation = generation_lanes.get_mut(generation_index).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer generation index is outside the execution plan",
                )
            })?;

            let lane = generation.get_mut(lane_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer lane index is outside the execution plan",
                )
            })?;

            lane.push(task.clone());
        }
    }

    let plan = FreshGenerationPlan {
        catalog,
        generation_lanes,
        trailing_lanes,
        completed_generation_count: 0,
    };

    validate_fresh_generation_plan(manifest, transfer_plan, &plan)?;

    Ok(plan)
}

fn apply_fresh_resume_prefix(
    plan: &mut FreshGenerationPlan,
    completed_file_ids: &BTreeSet<usize>,
) -> io::Result<()> {
    if plan.completed_generation_count != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fresh resume prefix was already applied",
        ));
    }

    let mut expected_prefix_file_ids = BTreeSet::new();
    let mut completed_generation_count = 0_usize;

    for generation in &plan.catalog.generations {
        let generation_file_ids: BTreeSet<usize> = generation
            .transfer_files
            .iter()
            .map(|candidate| candidate.file_id)
            .collect();

        if generation_file_ids.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fresh-transfer catalog contains an empty generation",
            ));
        }

        let completed_in_generation = generation_file_ids.intersection(completed_file_ids).count();

        if completed_in_generation == generation_file_ids.len() {
            expected_prefix_file_ids.extend(generation_file_ids);
            completed_generation_count = completed_generation_count
                .checked_add(1)
                .ok_or_else(|| io::Error::other("completed fresh-generation count overflowed"))?;

            continue;
        }

        if completed_in_generation != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "resume journal contains only part of fresh generation {}",
                    generation.index,
                ),
            ));
        }

        break;
    }

    if let Some(file_id) = completed_file_ids
        .difference(&expected_prefix_file_ids)
        .next()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "resume journal completed file {file_id} does not belong to a contiguous generation prefix"
            ),
        ));
    }

    plan.completed_generation_count = completed_generation_count;

    Ok(())
}

fn rebuild_fresh_generation_execution(
    plan: &mut FreshGenerationPlan,
    transfer_plan: &TransferPlan,
) -> io::Result<()> {
    let lane_count = transfer_plan.lanes.len();

    let mut generation_by_file_id = BTreeMap::new();

    for (generation_index, generation) in plan.catalog.generations.iter().enumerate() {
        for candidate in &generation.transfer_files {
            if generation_by_file_id
                .insert(candidate.file_id, generation_index)
                .is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer catalog schedules the same file in multiple generations",
                ));
            }
        }
    }

    let mut generation_lanes: Vec<Vec<Vec<TransferTask>>> = (0..plan.catalog.generations.len())
        .map(|_| vec![Vec::new(); lane_count])
        .collect();

    let mut trailing_lanes = vec![Vec::new(); lane_count];

    for (lane_id, lane) in transfer_plan.lanes.iter().enumerate() {
        for task in lane {
            let Some(file_id) = catalog_task_file_id(task) else {
                trailing_lanes[lane_id].push(task.clone());

                continue;
            };

            let generation_index =
                generation_by_file_id
                    .get(&file_id)
                    .copied()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "remaining fresh-transfer task for file {file_id} is absent from the original catalog"
                            ),
                        )
                    })?;

            let generation = generation_lanes.get_mut(generation_index).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer generation index is outside the rebuilt execution plan",
                )
            })?;

            let rebuilt_lane = generation.get_mut(lane_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer lane index is outside the rebuilt execution plan",
                )
            })?;

            rebuilt_lane.push(task.clone());
        }
    }

    plan.generation_lanes = generation_lanes;
    plan.trailing_lanes = trailing_lanes;

    validate_rebuilt_fresh_generation_execution(plan, transfer_plan)
}

fn validate_rebuilt_fresh_generation_execution(
    plan: &FreshGenerationPlan,
    transfer_plan: &TransferPlan,
) -> io::Result<()> {
    let lane_count = transfer_plan.lanes.len();

    if plan.completed_generation_count > plan.catalog.generations.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "completed fresh-generation count exceeds the catalog",
        ));
    }

    if plan.generation_lanes.len() != plan.catalog.generations.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rebuilt fresh-generation count differs from the catalog",
        ));
    }

    if plan.trailing_lanes.len() != lane_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rebuilt trailing lane count differs from the transfer plan",
        ));
    }

    for (generation_index, (generation, lanes)) in plan
        .catalog
        .generations
        .iter()
        .zip(&plan.generation_lanes)
        .enumerate()
    {
        if lanes.len() != lane_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rebuilt fresh generation has an incorrect lane count",
            ));
        }

        if generation_index < plan.completed_generation_count
            && lanes.iter().any(|lane| !lane.is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "completed fresh generation {generation_index} still contains transfer tasks"
                ),
            ));
        }

        let expected_file_ids: BTreeSet<usize> = generation
            .transfer_files
            .iter()
            .map(|candidate| candidate.file_id)
            .collect();

        for task in lanes.iter().flatten() {
            let file_id = catalog_task_file_id(task).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "rebuilt catalog generation contains a trailing task",
                )
            })?;

            if !expected_file_ids.contains(&file_id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("rebuilt fresh generation contains unexpected file ID {file_id}"),
                ));
            }
        }
    }

    for task in plan.trailing_lanes.iter().flatten() {
        if catalog_task_file_id(task).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "rebuilt trailing lanes contain a catalog-eligible task",
            ));
        }
    }

    let expected_task_count = transfer_plan
        .lanes
        .iter()
        .try_fold(0_usize, |count, lane| {
            count
                .checked_add(lane.len())
                .ok_or_else(|| io::Error::other("remaining transfer task count overflowed"))
        })?;

    let generation_task_count = plan
        .generation_lanes
        .iter()
        .flat_map(|generation| generation.iter())
        .try_fold(0_usize, |count, lane| {
            count
                .checked_add(lane.len())
                .ok_or_else(|| io::Error::other("rebuilt generation task count overflowed"))
        })?;

    let trailing_task_count = plan
        .trailing_lanes
        .iter()
        .try_fold(0_usize, |count, lane| {
            count
                .checked_add(lane.len())
                .ok_or_else(|| io::Error::other("rebuilt trailing task count overflowed"))
        })?;

    let actual_task_count = generation_task_count
        .checked_add(trailing_task_count)
        .ok_or_else(|| io::Error::other("rebuilt execution task count overflowed"))?;

    if actual_task_count != expected_task_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "rebuilt fresh execution contains {actual_task_count} tasks, expected {expected_task_count}"
            ),
        ));
    }

    Ok(())
}

fn collect_fresh_catalog_candidates(
    manifest: &[ManifestEntry],
    transfer_plan: &TransferPlan,
) -> io::Result<Vec<CatalogCandidate>> {
    let mut file_ids = BTreeSet::new();

    for task in transfer_plan.lanes.iter().flatten() {
        let Some(file_id) = catalog_task_file_id(task) else {
            continue;
        };

        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("fresh-transfer catalog task references unknown file ID {file_id}"),
            )
        })?;

        match task {
            TransferTask::WholeFile { .. } if entry.class != FileClass::Medium => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer catalog found a whole-file task that is not medium",
                ));
            }

            TransferTask::Stripe { .. } if entry.class != FileClass::Large => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer catalog found a stripe task that is not large",
                ));
            }

            TransferTask::WholeFile { .. } | TransferTask::Stripe { .. } => {}

            TransferTask::TinyPack { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tiny-pack task unexpectedly entered fresh-transfer catalog planning",
                ));
            }
        }

        file_ids.insert(file_id);
    }

    file_ids
        .into_iter()
        .map(|file_id| {
            let entry = manifest.get(file_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer catalog candidate disappeared from the manifest",
                )
            })?;

            CatalogCandidate::new(file_id, entry.file_size)
        })
        .collect()
}

fn catalog_task_file_id(task: &TransferTask) -> Option<usize> {
    match task {
        TransferTask::WholeFile { file_id } | TransferTask::Stripe { file_id, .. } => {
            Some(*file_id)
        }

        TransferTask::TinyPack { .. } => None,
    }
}

fn validate_fresh_generation_plan(
    manifest: &[ManifestEntry],
    transfer_plan: &TransferPlan,
    plan: &FreshGenerationPlan,
) -> io::Result<()> {
    let lane_count = transfer_plan.lanes.len();

    if plan.generation_lanes.len() != plan.catalog.generations.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fresh-transfer execution generation count differs from its catalog",
        ));
    }

    if plan.trailing_lanes.len() != lane_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fresh-transfer trailing lane count differs from the transfer plan",
        ));
    }

    let mut scheduled_file_ids = BTreeSet::new();

    for (generation, lanes) in plan.catalog.generations.iter().zip(&plan.generation_lanes) {
        if lanes.len() != lane_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fresh-transfer generation has an incorrect lane count",
            ));
        }

        let expected_file_ids: BTreeSet<usize> = generation
            .transfer_files
            .iter()
            .map(|candidate| candidate.file_id)
            .collect();

        let mut actual_file_ids = BTreeSet::new();

        for task in lanes.iter().flatten() {
            let file_id = catalog_task_file_id(task).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer catalog generation contains a trailing task",
                )
            })?;

            if !expected_file_ids.contains(&file_id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("fresh-transfer generation contains unexpected file ID {file_id}"),
                ));
            }

            actual_file_ids.insert(file_id);
        }

        if actual_file_ids != expected_file_ids {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fresh-transfer generation tasks do not match its catalog files",
            ));
        }

        for file_id in actual_file_ids {
            if !scheduled_file_ids.insert(file_id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh-transfer execution schedules the same file in multiple generations",
                ));
            }
        }
    }

    for task in plan.trailing_lanes.iter().flatten() {
        if catalog_task_file_id(task).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fresh-transfer trailing lanes contain a catalog-eligible task",
            ));
        }
    }

    let expected_file_ids: BTreeSet<usize> =
        collect_fresh_catalog_candidates(manifest, transfer_plan)?
            .into_iter()
            .map(|candidate| candidate.file_id)
            .collect();

    if scheduled_file_ids != expected_file_ids {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fresh-transfer execution file set differs from its catalog candidates",
        ));
    }

    let expected_task_count = transfer_plan
        .lanes
        .iter()
        .try_fold(0_usize, |count, lane| {
            count
                .checked_add(lane.len())
                .ok_or_else(|| io::Error::other("fresh-transfer task count overflowed"))
        })?;

    let generation_task_count = plan
        .generation_lanes
        .iter()
        .flat_map(|generation| generation.iter())
        .try_fold(0_usize, |count, lane| {
            count
                .checked_add(lane.len())
                .ok_or_else(|| io::Error::other("generation task count overflowed"))
        })?;

    let trailing_task_count = plan
        .trailing_lanes
        .iter()
        .try_fold(0_usize, |count, lane| {
            count
                .checked_add(lane.len())
                .ok_or_else(|| io::Error::other("trailing task count overflowed"))
        })?;

    let actual_task_count = generation_task_count
        .checked_add(trailing_task_count)
        .ok_or_else(|| io::Error::other("fresh-transfer execution task count overflowed"))?;

    if actual_task_count != expected_task_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fresh-transfer execution did not preserve every scheduled task",
        ));
    }

    Ok(())
}

fn least_loaded_lane(lanes: &[Vec<TransferTask>], assigned_bytes: &[u64]) -> io::Result<usize> {
    assigned_bytes
        .iter()
        .enumerate()
        .min_by_key(|(lane, bytes)| (**bytes, lanes[*lane].len()))
        .map(|(lane, _)| lane)
        .ok_or_else(|| io::Error::other("no TCP data lanes are available"))
}

fn build_tiny_packs(
    manifest: &[ManifestEntry],
    tiny_file_ids: Vec<usize>,
) -> io::Result<Vec<(Vec<usize>, u64)>> {
    let mut packs = Vec::new();
    let mut current_file_ids = Vec::new();
    let mut current_bytes = 0_u64;

    for file_id in tiny_file_ids {
        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tiny-pack planner received an invalid file ID",
            )
        })?;

        if entry.class != FileClass::Tiny {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tiny-pack planner received a non-tiny file",
            ));
        }

        let candidate_bytes = current_bytes
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("tiny-pack byte count overflowed"))?;

        let exceeds_target =
            !current_file_ids.is_empty() && candidate_bytes > TINY_PACK_TARGET_BYTES;

        let exceeds_file_limit = current_file_ids.len() >= MAX_TINY_PACK_FILES;

        if exceeds_target || exceeds_file_limit {
            packs.push((std::mem::take(&mut current_file_ids), current_bytes));

            current_bytes = 0;
        }

        current_bytes = current_bytes
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("tiny-pack byte count overflowed"))?;

        current_file_ids.push(file_id);
    }

    if !current_file_ids.is_empty() {
        packs.push((current_file_ids, current_bytes));
    }

    Ok(packs)
}

fn summarize_transfer_plan(transfer_plan: &TransferPlan) -> io::Result<TransferPlanStats> {
    let mut stats = TransferPlanStats::default();

    for task in transfer_plan.lanes.iter().flatten() {
        let TransferTask::TinyPack {
            file_ids,
            total_bytes,
        } = task
        else {
            continue;
        };

        stats.tiny_pack_count = stats
            .tiny_pack_count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("tiny-pack count overflowed"))?;

        stats.tiny_files_packed = stats
            .tiny_files_packed
            .checked_add(
                u64::try_from(file_ids.len())
                    .map_err(|_| io::Error::other("tiny-pack file count cannot be represented"))?,
            )
            .ok_or_else(|| io::Error::other("packed tiny-file count overflowed"))?;

        stats.tiny_bytes_packed = stats
            .tiny_bytes_packed
            .checked_add(*total_bytes)
            .ok_or_else(|| io::Error::other("packed tiny-file byte count overflowed"))?;
    }

    Ok(stats)
}

fn planned_resume_stripes(transfer_plan: &TransferPlan) -> io::Result<BTreeSet<ResumeStripe>> {
    let mut planned = BTreeSet::new();

    for task in transfer_plan.lanes.iter().flatten() {
        let TransferTask::Stripe {
            file_id,
            offset,
            length,
        } = task
        else {
            continue;
        };

        let stripe = ResumeStripe::new(*file_id, *offset, *length)?;

        if !planned.insert(stripe) {
            return Err(io::Error::other(
                "transfer plan contains a duplicate resume stripe",
            ));
        }
    }

    Ok(planned)
}

fn validate_resume_offer(
    transfer_plan: &TransferPlan,
    offered: &BTreeSet<ResumeStripe>,
) -> io::Result<()> {
    let planned = planned_resume_stripes(transfer_plan)?;

    if let Some(unplanned) = offered.difference(&planned).next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("receiver offered an unplanned completed stripe: {unplanned:?}"),
        ));
    }

    Ok(())
}

fn validate_unchanged_offer(
    manifest: &[ManifestEntry],
    unchanged_file_ids: &BTreeSet<usize>,
) -> io::Result<()> {
    if unchanged_file_ids.len() > MAX_UNCHANGED_OFFER_FILES as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unchanged-file offer exceeds the supported limit",
        ));
    }

    for &file_id in unchanged_file_ids {
        if file_id >= manifest.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("receiver offered unknown unchanged file ID {file_id}"),
            ));
        }
    }

    Ok(())
}

fn large_cdc_candidate_file_ids(
    manifest: &[ManifestEntry],
    excluded_file_ids: Option<&BTreeSet<usize>>,
) -> Vec<usize> {
    manifest
        .iter()
        .enumerate()
        .filter_map(|(file_id, entry)| {
            let excluded = excluded_file_ids.is_some_and(|file_ids| file_ids.contains(&file_id));

            if entry.class == FileClass::Large && !excluded {
                Some(file_id)
            } else {
                None
            }
        })
        .collect()
}

fn write_large_cdc_begin(writer: &mut impl Write, file_ids: &[usize]) -> io::Result<()> {
    let count = u32::try_from(file_ids.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "large CDC candidate \
                     count cannot be \
                     represented",
        )
    })?;

    if count > MAX_UNCHANGED_OFFER_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "large CDC candidate count \
             exceeds the supported limit",
        ));
    }

    write_u8(writer, MESSAGE_LARGE_CDC_BEGIN)?;

    write_u32(writer, count)?;

    for &file_id in file_ids {
        write_u64(
            writer,
            u64::try_from(file_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "large CDC file ID \
                         cannot be \
                         represented",
                )
            })?,
        )?;
    }

    writer.flush()
}

fn read_large_cdc_candidate_file_ids(
    reader: &mut impl Read,
    manifest: &[ManifestEntry],
) -> io::Result<Vec<usize>> {
    let count = read_u32(reader)?;

    if count > MAX_UNCHANGED_OFFER_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "large CDC candidate count \
             exceeds the supported limit",
        ));
    }

    let mut file_ids = Vec::with_capacity(count as usize);

    let mut seen = BTreeSet::new();

    for _ in 0..count {
        let file_id = read_file_id(reader)?;

        if !seen.insert(file_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "large CDC candidate list \
                 contains a duplicate \
                 file ID",
            ));
        }

        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "large CDC candidate \
                         references an \
                         unknown file",
            )
        })?;

        if entry.class != FileClass::Large {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "large CDC candidate is \
                 not a large file",
            ));
        }

        file_ids.push(file_id);
    }

    Ok(file_ids)
}

fn large_cdc_begin_wire_bytes(file_count: usize) -> io::Result<u64> {
    let file_count = u64::try_from(file_count).map_err(|_| {
        io::Error::other(
            "large CDC candidate \
                     count cannot be \
                     represented",
        )
    })?;

    LARGE_CDC_BEGIN_FIXED_WIRE_BYTES
        .checked_add(
            file_count
                .checked_mul(LARGE_CDC_FILE_ID_WIRE_BYTES)
                .ok_or_else(|| {
                    io::Error::other(
                        "large CDC candidate \
                         wire size overflowed",
                    )
                })?,
        )
        .ok_or_else(|| {
            io::Error::other(
                "large CDC begin wire size \
                 overflowed",
            )
        })
}

fn large_cdc_decision_wire_bytes(decision: cdc_lane::CdcLaneDecision) -> io::Result<u64> {
    let stats = decision.stats;

    match (
        stats.offered_files,
        stats.completed_files,
        stats.fallback_files,
        decision.completed,
    ) {
        (0, 0, 0, false) => Ok(CDC_UNAVAILABLE_WIRE_BYTES),

        (1, 1, 0, true) => CDC_INDEX_FIXED_WIRE_BYTES
            .checked_add(stats.index_wire_bytes)
            .and_then(|bytes| bytes.checked_add(CDC_PLAN_FIXED_WIRE_BYTES))
            .and_then(|bytes| bytes.checked_add(stats.plan_wire_bytes))
            .ok_or_else(|| {
                io::Error::other(
                    "completed large CDC \
                         wire size overflowed",
                )
            }),

        (1, 0, 1, false) => CDC_INDEX_FIXED_WIRE_BYTES
            .checked_add(stats.index_wire_bytes)
            .and_then(|bytes| bytes.checked_add(CDC_FALLBACK_WIRE_BYTES))
            .ok_or_else(|| {
                io::Error::other(
                    "large CDC fallback \
                         wire size overflowed",
                )
            }),

        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "large CDC negotiation returned \
             inconsistent statistics",
        )),
    }
}

fn add_large_cdc_decision(
    preflight: &mut LargeCdcPreflight,
    file_id: usize,
    entry: &ManifestEntry,
    decision: cdc_lane::CdcLaneDecision,
    side: &str,
) -> io::Result<()> {
    if entry.class != FileClass::Large {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "large CDC decision referenced \
             a non-large file",
        ));
    }

    let wire_bytes = large_cdc_decision_wire_bytes(decision)?;

    preflight.report.data_wire_bytes = preflight
        .report
        .data_wire_bytes
        .checked_add(wire_bytes)
        .ok_or_else(|| {
            io::Error::other(
                "large CDC wire-byte \
                     count overflowed",
            )
        })?;

    preflight.report.cdc.merge(decision.stats)?;

    if !decision.completed {
        return Ok(());
    }

    if !preflight.completed_file_ids.insert(file_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "large CDC completed the same \
             file more than once",
        ));
    }

    add_lane_counts(&mut preflight.report, 1, entry.file_size, side)
}

fn negotiate_large_cdc_sender(
    reader: &mut impl Read,
    writer: &mut impl Write,
    source_root: &Path,
    manifest: &[ManifestEntry],
    progress: Option<&ProgressCounter>,
) -> io::Result<LargeCdcPreflight> {
    let file_ids = read_large_cdc_candidate_file_ids(reader, manifest)?;

    let mut preflight = LargeCdcPreflight::default();

    preflight.report.data_wire_bytes = large_cdc_begin_wire_bytes(file_ids.len())?;

    if let Some(progress) = progress {
        progress.check_cancelled()?;

        progress.set_label("Planning large-file CDC");

        progress.set_completed(0);
        progress.set_total(0);
    }

    for file_id in file_ids {
        if let Some(progress) = progress {
            progress.check_cancelled()?;
        }

        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "large CDC sender \
                         referenced an \
                         unknown file",
            )
        })?;

        let decision = cdc_lane::sender_negotiate(reader, writer, source_root, file_id, entry)?;

        add_large_cdc_decision(&mut preflight, file_id, entry, decision, "sender large CDC")?;
    }

    Ok(preflight)
}

fn negotiate_large_cdc_receiver(
    reader: &mut impl Read,
    writer: &mut impl Write,
    destination_root: &Path,
    manifest: &[ManifestEntry],
    excluded_file_ids: Option<&BTreeSet<usize>>,
    progress: Option<&ProgressCounter>,
    fault_injection: &TransferFault,
) -> io::Result<LargeCdcPreflight> {
    let file_ids = large_cdc_candidate_file_ids(manifest, excluded_file_ids);

    write_large_cdc_begin(writer, &file_ids)?;

    let mut preflight = LargeCdcPreflight::default();

    preflight.report.data_wire_bytes = large_cdc_begin_wire_bytes(file_ids.len())?;

    if let Some(progress) = progress {
        progress.check_cancelled()?;

        progress.set_label("Planning large-file CDC");

        progress.set_completed(0);
        progress.set_total(0);
    }

    for file_id in file_ids {
        if let Some(progress) = progress {
            progress.check_cancelled()?;
        }

        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "large CDC receiver \
                         referenced an \
                         unknown file",
            )
        })?;

        let decision =
            cdc_lane::receiver_negotiate(reader, writer, destination_root, file_id, entry, true)?;

        if decision.completed {
            let completed_path = destination_root.join(&entry.relative_path);

            file_metadata::restore_file(
                &completed_path,
                entry.last_write_time,
                entry.file_attributes,
            )?;
        }

        add_large_cdc_decision(
            &mut preflight,
            file_id,
            entry,
            decision,
            "receiver large CDC",
        )?;

        if decision.completed {
            fault_injection.after_reconstructed_cdc_file()?;
        }
    }

    Ok(preflight)
}

fn apply_large_cdc_preflight(
    transfer_plan: &mut TransferPlan,
    manifest: &[ManifestEntry],
    preflight: &LargeCdcPreflight,
) -> io::Result<()> {
    let expected_files = u64::try_from(preflight.completed_file_ids.len()).map_err(|_| {
        io::Error::other(
            "large CDC completed-file \
                 count cannot be represented",
        )
    })?;

    let mut expected_bytes = 0_u64;

    for &file_id in &preflight.completed_file_ids {
        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "large CDC completed \
                         an unknown file",
            )
        })?;

        if entry.class != FileClass::Large {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "large CDC completed a \
                 non-large file",
            ));
        }

        expected_bytes = expected_bytes.checked_add(entry.file_size).ok_or_else(|| {
            io::Error::other(
                "large CDC logical \
                         byte count overflowed",
            )
        })?;
    }

    if preflight.report.files_copied != expected_files
        || preflight.report.bytes_copied != expected_bytes
        || preflight.report.cdc.completed_files != expected_files
        || preflight.report.cdc.logical_bytes != expected_bytes
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "large CDC report does not \
             match its completed file set",
        ));
    }

    let mut removed_file_ids = BTreeSet::new();

    for lane in &mut transfer_plan.lanes {
        let mut remaining_tasks = Vec::with_capacity(lane.len());

        for task in lane.drain(..) {
            let task_file_id = match &task {
                TransferTask::Stripe { file_id, .. } => Some(*file_id),

                TransferTask::WholeFile { file_id } => Some(*file_id),

                TransferTask::TinyPack { .. } => None,
            };

            if let Some(file_id) = task_file_id
                && preflight.completed_file_ids.contains(&file_id)
            {
                if !matches!(&task, TransferTask::Stripe { .. },) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "large CDC file \
                             was not \
                             scheduled as \
                             stripes",
                    ));
                }

                removed_file_ids.insert(file_id);

                continue;
            }

            remaining_tasks.push(task);
        }

        *lane = remaining_tasks;
    }

    if removed_file_ids != preflight.completed_file_ids {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "large CDC completed-file set \
             does not match the striped \
             transfer plan",
        ));
    }

    Ok(())
}

fn exact_reuse_plan_wire_bytes(entry_count: usize) -> io::Result<u64> {
    let entry_count = u64::try_from(entry_count)
        .map_err(|_| io::Error::other("exact-reuse entry count cannot be represented"))?;

    EXACT_REUSE_PLAN_FIXED_WIRE_BYTES
        .checked_add(
            entry_count
                .checked_mul(EXACT_REUSE_RECORD_WIRE_BYTES)
                .ok_or_else(|| io::Error::other("exact-reuse record size overflowed"))?,
        )
        .ok_or_else(|| io::Error::other("exact-reuse plan size overflowed"))
}

fn planned_whole_file_ids(transfer_plan: &TransferPlan) -> BTreeSet<usize> {
    transfer_plan
        .lanes
        .iter()
        .flatten()
        .filter_map(|task| {
            let TransferTask::WholeFile { file_id } = task else {
                return None;
            };

            Some(*file_id)
        })
        .collect()
}

fn hash_exact_reuse_candidate(
    source_root: &Path,
    file_id: usize,
    entry: &ManifestEntry,
) -> io::Result<[u8; CONTENT_DIGEST_BYTES]> {
    let path = source_root.join(&entry.relative_path);

    let report = content_hash::run(&path, content_hash::DEFAULT_BUFFER_MIB)?;

    let metadata = fs::metadata(&path)?;

    if metadata.len() != entry.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "source size changed while planning exact reuse: {}",
                path.display(),
            ),
        ));
    }

    if metadata.last_write_time() != entry.last_write_time {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "source timestamp changed while planning exact reuse: {}",
                path.display(),
            ),
        ));
    }

    if report.bytes_hashed != entry.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("exact-reuse hash length differs from manifest for file {file_id}",),
        ));
    }

    Ok(report.digest)
}

fn build_fresh_exact_reuse_plan(
    source_root: &Path,
    manifest: &[ManifestEntry],
    transfer_plan: &TransferPlan,
    progress: Option<&ProgressCounter>,
) -> io::Result<ExactReusePlan> {
    let whole_file_ids = planned_whole_file_ids(transfer_plan);

    let mut by_size = BTreeMap::<u64, Vec<usize>>::new();

    for file_id in whole_file_ids {
        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "exact-reuse planner found an unknown file",
            )
        })?;

        if entry.class != FileClass::Medium {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "whole-file task was not a medium file",
            ));
        }

        if entry.file_size == 0 {
            continue;
        }

        by_size.entry(entry.file_size).or_default().push(file_id);
    }

    if let Some(progress) = progress {
        progress.check_cancelled()?;
        progress.set_label("Finding exact duplicates");
        progress.set_completed(0);
        progress.set_total(0);
    }

    let mut entries = Vec::new();

    for mut same_size_ids in by_size.into_values() {
        if same_size_ids.len() < 2 {
            continue;
        }

        same_size_ids.sort_unstable();

        let mut by_digest = BTreeMap::<[u8; CONTENT_DIGEST_BYTES], Vec<usize>>::new();

        for file_id in same_size_ids {
            if let Some(progress) = progress {
                progress.check_cancelled()?;
            }

            let entry = manifest.get(file_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "exact-reuse planner found an unknown candidate",
                )
            })?;

            let digest = hash_exact_reuse_candidate(source_root, file_id, entry)?;

            by_digest.entry(digest).or_default().push(file_id);
        }

        for (digest, mut identical_ids) in by_digest {
            if identical_ids.len() < 2 {
                continue;
            }

            identical_ids.sort_unstable();

            let basis_file_id = identical_ids[0];

            for &target_file_id in &identical_ids[1..] {
                entries.push(ExactFileReuse {
                    basis_file_id,
                    target_file_id,
                    digest,
                });
            }
        }
    }

    entries.sort_unstable_by_key(|entry| (entry.basis_file_id, entry.target_file_id));

    Ok(ExactReusePlan { entries })
}

fn write_exact_reuse_plan(writer: &mut impl Write, plan: &ExactReusePlan) -> io::Result<()> {
    let count = u32::try_from(plan.entries.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "exact-reuse plan contains too many entries",
        )
    })?;

    if count > MAX_UNCHANGED_OFFER_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "exact-reuse plan exceeds the supported limit",
        ));
    }

    write_u8(writer, MESSAGE_EXACT_REUSE_PLAN)?;
    write_u32(writer, count)?;

    for entry in &plan.entries {
        write_u64(
            writer,
            u64::try_from(entry.basis_file_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "exact-reuse basis ID cannot be represented",
                )
            })?,
        )?;

        write_u64(
            writer,
            u64::try_from(entry.target_file_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "exact-reuse target ID cannot be represented",
                )
            })?,
        )?;

        write_digest(writer, &entry.digest)?;
    }

    writer.flush()
}

fn read_exact_reuse_plan(reader: &mut impl Read) -> io::Result<ExactReusePlan> {
    let message = read_u8(reader)?;

    if message != MESSAGE_EXACT_REUSE_PLAN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected exact-reuse plan, received 0x{message:02X}",),
        ));
    }

    let count = read_u32(reader)?;

    if count > MAX_UNCHANGED_OFFER_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exact-reuse plan exceeds the supported limit",
        ));
    }

    let mut entries = Vec::with_capacity(count as usize);

    for _ in 0..count {
        entries.push(ExactFileReuse {
            basis_file_id: read_file_id(reader)?,
            target_file_id: read_file_id(reader)?,
            digest: read_digest(reader)?,
        });
    }

    Ok(ExactReusePlan { entries })
}

fn apply_exact_reuse_plan(
    transfer_plan: &mut TransferPlan,
    manifest: &[ManifestEntry],
    plan: &ExactReusePlan,
    allowed: bool,
) -> io::Result<LaneReport> {
    if !allowed && !plan.entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exact reuse is currently allowed only for fresh transfers",
        ));
    }

    let mut target_file_ids = BTreeSet::new();

    let mut basis_file_ids = BTreeSet::new();

    let mut logical_bytes = 0_u64;

    for reuse in &plan.entries {
        if reuse.basis_file_id == reuse.target_file_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exact-reuse basis and target are the same file",
            ));
        }

        let basis = manifest.get(reuse.basis_file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "exact-reuse basis references an unknown file",
            )
        })?;

        let target = manifest.get(reuse.target_file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "exact-reuse target references an unknown file",
            )
        })?;

        if basis.class != FileClass::Medium || target.class != FileClass::Medium {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exact reuse currently requires medium files",
            ));
        }

        if basis.file_size == 0 || basis.file_size != target.file_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exact-reuse files have incompatible sizes",
            ));
        }

        if !target_file_ids.insert(reuse.target_file_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exact-reuse plan contains a duplicate target",
            ));
        }

        basis_file_ids.insert(reuse.basis_file_id);

        logical_bytes = logical_bytes
            .checked_add(target.file_size)
            .ok_or_else(|| io::Error::other("exact-reuse logical byte count overflowed"))?;
    }

    if basis_file_ids
        .iter()
        .any(|file_id| target_file_ids.contains(file_id))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exact-reuse plan contains a dependency chain",
        ));
    }

    let planned_whole_files = planned_whole_file_ids(transfer_plan);

    for file_id in basis_file_ids.iter().chain(target_file_ids.iter()) {
        if !planned_whole_files.contains(file_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("exact-reuse file {file_id} is not an active whole-file task",),
            ));
        }
    }

    for lane in &mut transfer_plan.lanes {
        lane.retain(|task| {
            let TransferTask::WholeFile { file_id } = task else {
                return true;
            };

            !target_file_ids.contains(file_id)
        });
    }

    let exact_reused_files = u64::try_from(plan.entries.len())
        .map_err(|_| io::Error::other("exact-reuse file count cannot be represented"))?;

    let plan_wire_bytes = exact_reuse_plan_wire_bytes(plan.entries.len())?;

    let telemetry_wire_bytes = if plan.entries.is_empty() {
        0
    } else {
        plan_wire_bytes
    };

    Ok(LaneReport {
        files_copied: exact_reused_files,

        bytes_copied: logical_bytes,

        data_wire_bytes: plan_wire_bytes,

        exact_reused_files,
        exact_reused_bytes: logical_bytes,
        exact_reuse_plan_wire_bytes: telemetry_wire_bytes,

        ..LaneReport::default()
    })
}

fn materialize_exact_reuse_files(
    destination_root: &Path,
    manifest: &[ManifestEntry],
    plan: &ExactReusePlan,
    progress: Option<&ProgressCounter>,
) -> io::Result<()> {
    for reuse in &plan.entries {
        if let Some(progress) = progress {
            progress.check_cancelled()?;
        }

        let basis = manifest.get(reuse.basis_file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "exact-reuse basis references an unknown file",
            )
        })?;

        let target = manifest.get(reuse.target_file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "exact-reuse target references an unknown file",
            )
        })?;

        let basis_path = destination_root.join(&basis.relative_path);

        let target_path = destination_root.join(&target.relative_path);

        let temporary_path = temporary_path(&target_path, reuse.target_file_id);

        validate_resume_file(&basis_path, basis.file_size, "exact-reuse basis file")?;

        if target_path.try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "exact-reuse target already exists: {}",
                    target_path.display(),
                ),
            ));
        }

        remove_file_if_present(&temporary_path)?;

        let materialize_result = (|| -> io::Result<()> {
            let copied = fs::copy(&basis_path, &temporary_path)?;

            if copied != target.file_size {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "exact-reuse copy produced {copied} bytes, expected {}",
                        target.file_size,
                    ),
                ));
            }

            let hash = content_hash::run(&temporary_path, content_hash::DEFAULT_BUFFER_MIB)?;

            let context = format!(
                "exact-reuse target {} ({})",
                reuse.target_file_id,
                target.relative_path.display(),
            );

            verify_content_digest(&context, &hash.digest, &reuse.digest)?;

            windows_file_replace::replace(&temporary_path, &target_path)?;

            file_metadata::restore_file(
                &target_path,
                target.last_write_time,
                target.file_attributes,
            )?;

            Ok(())
        })();

        if let Err(error) = materialize_result {
            let _ = fs::remove_file(&temporary_path);

            return Err(error);
        }

        if let Some(progress) = progress {
            progress.add(target.file_size);
        }
    }

    Ok(())
}

fn apply_resume_offer(
    transfer_plan: &mut TransferPlan,
    manifest: &[ManifestEntry],
    offered: &BTreeSet<ResumeStripe>,
    unchanged_file_ids: &BTreeSet<usize>,
) -> io::Result<ResumeApplication> {
    validate_resume_offer(transfer_plan, offered)?;

    validate_unchanged_offer(manifest, unchanged_file_ids)?;

    if let Some(overlap) = offered.iter().find(|stripe| {
        usize::try_from(stripe.file_id)
            .ok()
            .is_some_and(|file_id| unchanged_file_ids.contains(&file_id))
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "receiver offered file {} as both unchanged and partially resumed",
                overlap.file_id,
            ),
        ));
    }

    let mut application = ResumeApplication::default();

    for &file_id in unchanged_file_ids {
        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("receiver offered unknown unchanged file ID {file_id}"),
            )
        })?;

        add_lane_counts(
            &mut application.logical_report,
            1,
            entry.file_size,
            "unchanged",
        )?;

        application.unchanged_file_count = application
            .unchanged_file_count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("unchanged file count overflowed"))?;

        application.unchanged_bytes = application
            .unchanged_bytes
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("unchanged byte count overflowed"))?;
    }

    for lane in &mut transfer_plan.lanes {
        let mut remaining_tasks = Vec::with_capacity(lane.len());

        for task in lane.drain(..) {
            match task {
                TransferTask::WholeFile { file_id } if unchanged_file_ids.contains(&file_id) => {}

                TransferTask::Stripe { file_id, .. } if unchanged_file_ids.contains(&file_id) => {}

                TransferTask::TinyPack { mut file_ids, .. } => {
                    file_ids.retain(|file_id| !unchanged_file_ids.contains(file_id));

                    if file_ids.is_empty() {
                        continue;
                    }

                    let summary = summarize_tiny_pack(manifest, &file_ids)?;

                    remaining_tasks.push(TransferTask::TinyPack {
                        file_ids,
                        total_bytes: summary.bytes,
                    });
                }

                TransferTask::Stripe {
                    file_id,
                    offset,
                    length,
                } => {
                    let stripe = ResumeStripe::new(file_id, offset, length)?;

                    if offered.contains(&stripe) {
                        add_lane_counts(
                            &mut application.logical_report,
                            u64::from(offset == 0),
                            length,
                            "resumed",
                        )?;

                        application.resumed_bytes =
                            application
                                .resumed_bytes
                                .checked_add(length)
                                .ok_or_else(|| io::Error::other("resumed byte count overflowed"))?;

                        application.stripe_count = application
                            .stripe_count
                            .checked_add(1)
                            .ok_or_else(|| io::Error::other("resumed stripe count overflowed"))?;
                    } else {
                        remaining_tasks.push(TransferTask::Stripe {
                            file_id,
                            offset,
                            length,
                        });
                    }
                }

                task => {
                    remaining_tasks.push(task);
                }
            }
        }

        *lane = remaining_tasks;
    }

    let expected_stripe_count = u64::try_from(offered.len())
        .map_err(|_| io::Error::other("resume offer stripe count cannot be represented"))?;

    if application.stripe_count != expected_stripe_count {
        return Err(io::Error::other(format!(
            "applied {} resumed stripes, expected {expected_stripe_count}",
            application.stripe_count,
        )));
    }

    let expected_unchanged_count = u64::try_from(unchanged_file_ids.len())
        .map_err(|_| io::Error::other("unchanged-file offer count cannot be represented"))?;

    if application.unchanged_file_count != expected_unchanged_count {
        return Err(io::Error::other(format!(
            "applied {} unchanged files, expected {expected_unchanged_count}",
            application.unchanged_file_count,
        )));
    }

    Ok(application)
}

fn write_lane_end(writer: &mut impl Write, lane_end: LaneEnd) -> io::Result<()> {
    match lane_end {
        LaneEnd::Generation(generation_index) => {
            write_u8(writer, MESSAGE_GENERATION_END)?;

            write_u64(
                writer,
                u64::try_from(generation_index).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "generation index cannot be represented",
                    )
                })?,
            )
        }

        LaneEnd::Stream => write_u8(writer, MESSAGE_STREAM_END),
    }
}

fn read_lane_end(reader: &mut impl Read, expected: LaneEnd) -> io::Result<()> {
    let message = read_u8(reader)?;

    match expected {
        LaneEnd::Generation(expected_index) => {
            if message != MESSAGE_GENERATION_END {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected generation-end message, received 0x{message:02X}",),
                ));
            }

            let actual_index = usize::try_from(read_u64(reader)?).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "received generation index cannot be represented",
                )
            })?;

            if actual_index != expected_index {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "received generation-end index {actual_index}, expected {expected_index}",
                    ),
                ));
            }

            Ok(())
        }

        LaneEnd::Stream => {
            if message != MESSAGE_STREAM_END {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected stream-end message, received 0x{message:02X}",),
                ));
            }

            Ok(())
        }
    }
}

fn send_lane_group(
    data_streams: &[TcpStream],
    lanes: &[Vec<TransferTask>],
    source_root: &Path,
    manifest: &[ManifestEntry],
    progress: Option<ProgressCounter>,
    session_basis_file_ids: Option<&[usize]>,
    lane_end: LaneEnd,
) -> io::Result<LaneReport> {
    if data_streams.len() != lanes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sender lane count differs from the connected data-stream count",
        ));
    }

    let reports = thread::scope(|scope| -> io::Result<Vec<LaneReport>> {
        let mut handles = Vec::with_capacity(data_streams.len());

        for (stream, tasks) in data_streams.iter().zip(lanes) {
            let lane_progress = progress.clone();

            handles.push(
                thread::Builder::new()
                    .name("networkcopy-data-sender".to_string())
                    .spawn_scoped(scope, move || {
                        send_lane(
                            stream,
                            source_root,
                            manifest,
                            tasks,
                            lane_progress,
                            session_basis_file_ids,
                            lane_end,
                        )
                    })?,
            );
        }

        join_lane_threads(handles)
    })?;

    merge_lane_reports(reports)
}

fn send_fresh_generation_plan(
    control_stream: &mut TcpStream,
    data_streams: &[TcpStream],
    source_root: &Path,
    manifest: &[ManifestEntry],
    plan: &FreshGenerationPlan,
    progress: Option<ProgressCounter>,
) -> io::Result<LaneReport> {
    let remaining_generation_count = plan
        .catalog
        .generations
        .len()
        .checked_sub(plan.completed_generation_count)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "completed fresh-generation count exceeds the catalog",
            )
        })?;

    let report_capacity = remaining_generation_count
        .checked_add(1)
        .ok_or_else(|| io::Error::other("fresh-generation report capacity overflowed"))?;

    let mut reports = Vec::with_capacity(report_capacity);

    for (generation, lanes) in plan
        .catalog
        .generations
        .iter()
        .zip(&plan.generation_lanes)
        .skip(plan.completed_generation_count)
    {
        if let Some(progress) = &progress {
            progress.check_cancelled()?;
        }

        let report = send_lane_group(
            data_streams,
            lanes,
            source_root,
            manifest,
            progress.clone(),
            Some(&generation.basis_file_ids),
            LaneEnd::Generation(generation.index),
        )?;

        if let Some(progress) = &progress {
            progress.check_cancelled()?;
        }

        let commit = read_generation_commit(control_stream)?;

        validate_generation_commit(&commit, generation, manifest.len())?;

        reports.push(report);
    }

    if let Some(progress) = &progress {
        progress.check_cancelled()?;
    }

    reports.push(send_lane_group(
        data_streams,
        &plan.trailing_lanes,
        source_root,
        manifest,
        progress,
        None,
        LaneEnd::Stream,
    )?);

    merge_lane_reports(reports)
}

#[allow(clippy::too_many_arguments)]
fn receive_lane_group(
    data_streams: &[TcpStream],
    lanes: &[Vec<TransferTask>],
    destination_root: &Path,
    manifest: &[ManifestEntry],
    resume_journal: &Mutex<ResumeJournal>,
    fault_injection: &TransferFault,
    tiny_materializer: &tiny_file_pool::TinyFileMaterializerHandle,
    progress: Option<ProgressCounter>,
    cdc_enabled: bool,
    session_basis_file_ids: Option<&[usize]>,
    lane_end: LaneEnd,
) -> io::Result<LaneReport> {
    if data_streams.len() != lanes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "receiver lane count differs from the connected data-stream count",
        ));
    }

    let reports = thread::scope(|scope| -> io::Result<Vec<LaneReport>> {
        let mut handles = Vec::with_capacity(data_streams.len());

        for (stream, tasks) in data_streams.iter().zip(lanes) {
            let lane_progress = progress.clone();

            let lane_tiny_materializer = tiny_materializer.clone();

            handles.push(
                thread::Builder::new()
                    .name("networkcopy-data-receiver".to_string())
                    .spawn_scoped(scope, move || {
                        receive_lane(
                            stream,
                            tasks,
                            ReceiveLaneContext {
                                destination_root,
                                manifest,
                                resume_journal,
                                fault_injection,
                                tiny_materializer: &lane_tiny_materializer,
                                progress: lane_progress,
                                cdc_enabled,
                                session_basis_file_ids,
                            },
                            lane_end,
                        )
                    })?,
            );
        }

        join_lane_threads(handles)
    })?;

    merge_lane_reports(reports)
}

#[allow(clippy::too_many_arguments)]
fn receive_fresh_generation_plan(
    control_stream: &mut TcpStream,
    data_streams: &[TcpStream],
    destination_root: &Path,
    manifest: &[ManifestEntry],
    resume_journal: &Mutex<ResumeJournal>,
    fault_injection: &TransferFault,
    tiny_materializer: &tiny_file_pool::TinyFileMaterializerHandle,
    progress: Option<ProgressCounter>,
    plan: &FreshGenerationPlan,
) -> io::Result<LaneReport> {
    let remaining_generation_count = plan
        .catalog
        .generations
        .len()
        .checked_sub(plan.completed_generation_count)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "completed fresh-generation count exceeds the catalog",
            )
        })?;

    let report_capacity = remaining_generation_count
        .checked_add(1)
        .ok_or_else(|| io::Error::other("fresh-generation report capacity overflowed"))?;

    let mut reports = Vec::with_capacity(report_capacity);

    for (generation, lanes) in plan
        .catalog
        .generations
        .iter()
        .zip(&plan.generation_lanes)
        .skip(plan.completed_generation_count)
    {
        if let Some(progress) = &progress {
            progress.check_cancelled()?;
        }

        let report = receive_lane_group(
            data_streams,
            lanes,
            destination_root,
            manifest,
            resume_journal,
            fault_injection,
            tiny_materializer,
            progress.clone(),
            false,
            Some(&generation.basis_file_ids),
            LaneEnd::Generation(generation.index),
        )?;

        if let Some(progress) = &progress {
            progress.check_cancelled()?;
        }

        let commit =
            commit_fresh_generation(destination_root, manifest, generation, progress.as_ref())?;

        persist_generation_commit(destination_root, resume_journal, &commit, fault_injection)?;

        write_generation_commit(control_stream, &commit)?;

        reports.push(report);
    }

    if let Some(progress) = &progress {
        progress.check_cancelled()?;
    }

    reports.push(receive_lane_group(
        data_streams,
        &plan.trailing_lanes,
        destination_root,
        manifest,
        resume_journal,
        fault_injection,
        tiny_materializer,
        progress,
        false,
        None,
        LaneEnd::Stream,
    )?);

    merge_lane_reports(reports)
}

fn persist_generation_commit(
    destination_root: &Path,
    resume_journal: &Mutex<ResumeJournal>,
    commit: &GenerationCommit,
    fault_injection: &TransferFault,
) -> io::Result<()> {
    let mut journal = resume_journal
        .lock()
        .map_err(|_| io::Error::other("resume journal mutex was poisoned"))?;

    for &file_id in &commit.committed_file_ids {
        journal.mark_file_completed(file_id);
    }

    let persisted_file_ids: BTreeSet<usize> = journal.completed_file_ids().collect();

    if !commit
        .committed_file_ids
        .iter()
        .all(|file_id| persisted_file_ids.contains(file_id))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resume journal did not retain every committed generation file",
        ));
    }

    journal.save_atomic(destination_root)?;

    drop(journal);

    fault_injection.after_persisted_generation()
}

fn commit_fresh_generation(
    destination_root: &Path,
    manifest: &[ManifestEntry],
    generation: &CatalogGeneration,
    progress: Option<&ProgressCounter>,
) -> io::Result<GenerationCommit> {
    for candidate in &generation.transfer_files {
        if let Some(progress) = progress {
            progress.check_cancelled()?;
        }

        let file_id = candidate.file_id;

        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("fresh generation committed unknown file ID {file_id}",),
            )
        })?;

        let final_path = destination_root.join(&entry.relative_path);

        match entry.class {
            FileClass::Large => {
                finalize_large_file(destination_root, file_id, entry, DestinationMode::Fresh)?;
            }

            FileClass::Medium => {
                validate_resume_file(&final_path, entry.file_size, "committed generation file")?;
            }

            FileClass::Tiny => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fresh catalog generation unexpectedly committed a tiny file",
                ));
            }
        }

        file_metadata::restore_file(&final_path, entry.last_write_time, entry.file_attributes)?;
    }

    if let Some(progress) = progress {
        progress.check_cancelled()?;
    }

    let commit = expected_generation_commit(generation);

    validate_generation_commit(&commit, generation, manifest.len())?;

    Ok(commit)
}

fn send_lane(
    stream: &TcpStream,
    source_root: &Path,
    manifest: &[ManifestEntry],
    tasks: &[TransferTask],
    progress: Option<ProgressCounter>,
    session_basis_file_ids: Option<&[usize]>,
    lane_end: LaneEnd,
) -> io::Result<LaneReport> {
    let reader_stream = stream.try_clone()?;

    let writer_stream = stream.try_clone()?;

    let buffered_reader = BufReader::with_capacity(NETWORK_BUFFER_BYTES, reader_stream);

    let mut reader = CountingReader::new(buffered_reader);

    let buffered_writer = BufWriter::with_capacity(NETWORK_BUFFER_BYTES, writer_stream);

    let mut writer = CountingWriter::new(buffered_writer);

    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];

    let mut encoder = PayloadEncoder::new(compression_probe::DEFAULT_LEVEL)?;

    let mut report = LaneReport::default();

    for task in tasks {
        if let Some(progress) = &progress {
            progress.check_cancelled()?;
        }

        match task {
            TransferTask::WholeFile { file_id } => {
                let file_id = *file_id;

                let entry = manifest.get(file_id).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "scheduler returned an invalid file ID",
                    )
                })?;

                let cdc_decision = if let Some(basis_file_ids) = session_basis_file_ids {
                    session_cdc_lane::sender_try_plan(
                        &mut writer,
                        source_root,
                        manifest,
                        file_id,
                        basis_file_ids,
                    )?
                } else {
                    writer.flush()?;

                    cdc_lane::sender_negotiate(
                        &mut reader,
                        &mut writer,
                        source_root,
                        file_id,
                        entry,
                    )?
                };

                report.cdc.merge(cdc_decision.stats)?;

                if cdc_decision.completed {
                    add_lane_counts(&mut report, 1, entry.file_size, "sender CDC")?;

                    add_progress(&progress, entry.file_size);
                } else {
                    let compressed = send_whole_file(
                        &mut writer,
                        source_root,
                        file_id,
                        entry,
                        &mut buffer,
                        &mut encoder,
                        progress.as_ref(),
                    )?;

                    add_lane_counts(&mut report, 1, entry.file_size, "sender")?;

                    add_compressed_record(&mut report, compressed)?;
                }
            }

            TransferTask::TinyPack {
                file_ids,
                total_bytes,
            } => {
                let transfer = send_tiny_pack(
                    &mut writer,
                    source_root,
                    manifest,
                    file_ids,
                    *total_bytes,
                    &mut buffer,
                    progress.as_ref(),
                )?;

                add_lane_counts(
                    &mut report,
                    transfer.summary.files,
                    transfer.summary.bytes,
                    "sender",
                )?;

                add_compressed_record(&mut report, transfer.compressed)?;

                add_tiny_pack_stats(&mut report, transfer, "sender")?;
            }

            TransferTask::Stripe {
                file_id,
                offset,
                length,
            } => {
                let file_id = *file_id;
                let offset = *offset;
                let length = *length;

                let entry = manifest.get(file_id).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "scheduler returned an invalid striped file ID",
                    )
                })?;

                let compressed = send_file_stripe(
                    &mut writer,
                    source_root,
                    entry,
                    StripeDescriptor {
                        file_id,
                        offset,
                        length,
                    },
                    &mut buffer,
                    &mut encoder,
                    progress.as_ref(),
                )?;

                let completed_files = u64::from(offset == 0);

                add_lane_counts(&mut report, completed_files, length, "sender")?;

                add_compressed_record(&mut report, compressed)?;
            }
        }

        if let TransferTask::TinyPack { total_bytes, .. } = task {
            add_progress(&progress, *total_bytes);
        }
    }

    write_lane_end(&mut writer, lane_end)?;

    writer.flush()?;

    report.data_wire_bytes = writer
        .bytes_written()
        .checked_add(reader.bytes_read())
        .ok_or_else(|| io::Error::other("sender bidirectional wire count overflowed"))?;

    Ok(report)
}

fn add_progress(progress: &Option<ProgressCounter>, bytes: u64) {
    if let Some(progress) = progress {
        progress.add(bytes);
    }
}

fn complete_received_cdc_file(
    report: &mut LaneReport,
    destination_root: &Path,
    entry: &ManifestEntry,
    progress: &Option<ProgressCounter>,
    fault_injection: &TransferFault,
) -> io::Result<()> {
    let completed_path = destination_root.join(&entry.relative_path);

    file_metadata::restore_file(
        &completed_path,
        entry.last_write_time,
        entry.file_attributes,
    )?;

    add_lane_counts(report, 1, entry.file_size, "receiver CDC")?;

    add_progress(progress, entry.file_size);

    fault_injection.after_reconstructed_cdc_file()
}

fn add_lane_counts(report: &mut LaneReport, files: u64, bytes: u64, side: &str) -> io::Result<()> {
    report.files_copied = report
        .files_copied
        .checked_add(files)
        .ok_or_else(|| io::Error::other(format!("{side} file count overflowed")))?;

    report.bytes_copied = report
        .bytes_copied
        .checked_add(bytes)
        .ok_or_else(|| io::Error::other(format!("{side} byte count overflowed")))?;

    Ok(())
}

fn add_compressed_record(report: &mut LaneReport, compressed: bool) -> io::Result<()> {
    if !compressed {
        return Ok(());
    }

    report.compressed_records = report
        .compressed_records
        .checked_add(1)
        .ok_or_else(|| io::Error::other("compressed record count overflowed"))?;

    Ok(())
}

fn add_tiny_pack_stats(
    report: &mut LaneReport,
    transfer: TinyPackTransferSummary,
    side: &str,
) -> io::Result<()> {
    report.tiny_pack_count = report
        .tiny_pack_count
        .checked_add(1)
        .ok_or_else(|| io::Error::other(format!("{side} tiny-pack count overflowed")))?;

    if transfer.compressed {
        report.compressed_tiny_pack_count = report
            .compressed_tiny_pack_count
            .checked_add(1)
            .ok_or_else(|| {
                io::Error::other(format!("{side} compressed tiny-pack count overflowed"))
            })?;
    } else {
        report.raw_tiny_pack_count = report
            .raw_tiny_pack_count
            .checked_add(1)
            .ok_or_else(|| io::Error::other(format!("{side} raw tiny-pack count overflowed")))?;
    }

    report.tiny_files_packed = report
        .tiny_files_packed
        .checked_add(transfer.summary.files)
        .ok_or_else(|| io::Error::other(format!("{side} packed tiny-file count overflowed")))?;

    report.tiny_bytes_packed = report
        .tiny_bytes_packed
        .checked_add(transfer.summary.bytes)
        .ok_or_else(|| io::Error::other(format!("{side} packed tiny-byte count overflowed")))?;

    report.tiny_pack_wire_bytes = report
        .tiny_pack_wire_bytes
        .checked_add(transfer.wire_bytes)
        .ok_or_else(|| io::Error::other(format!("{side} tiny-pack wire-byte count overflowed")))?;

    Ok(())
}

fn tiny_pack_record_wire_bytes(file_count: usize, payload_bytes: usize) -> io::Result<u64> {
    let file_count = u64::try_from(file_count)
        .map_err(|_| io::Error::other("tiny-pack file count cannot be represented"))?;

    let payload_bytes = u64::try_from(payload_bytes)
        .map_err(|_| io::Error::other("tiny-pack payload length cannot be represented"))?;

    let metadata_bytes = file_count
        .checked_mul(TINY_PACK_FILE_METADATA_WIRE_BYTES)
        .ok_or_else(|| io::Error::other("tiny-pack metadata size overflowed"))?;

    TINY_PACK_FIXED_WIRE_BYTES
        .checked_add(metadata_bytes)
        .and_then(|bytes| bytes.checked_add(payload_bytes))
        .ok_or_else(|| io::Error::other("tiny-pack record size overflowed"))
}

fn send_tiny_pack(
    writer: &mut impl Write,
    source_root: &Path,
    manifest: &[ManifestEntry],
    file_ids: &[usize],
    expected_total_bytes: u64,
    buffer: &mut [u8],
    progress: Option<&ProgressCounter>,
) -> io::Result<TinyPackTransferSummary> {
    let summary = summarize_tiny_pack(manifest, file_ids)?;

    if summary.bytes != expected_total_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tiny-pack plan byte count changed",
        ));
    }

    let total_bytes = usize::try_from(summary.bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "tiny-pack byte count cannot be represented",
        )
    })?;

    if total_bytes > buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "tiny pack requires {total_bytes} bytes but its buffer contains only {} bytes",
                buffer.len(),
            ),
        ));
    }

    let mut digests = Vec::with_capacity(file_ids.len());

    let mut offset = 0_usize;

    for &file_id in file_ids {
        if let Some(progress) = progress {
            progress.check_cancelled()?;
        }

        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tiny pack contains an invalid file ID",
            )
        })?;

        let file_bytes = usize::try_from(entry.file_size).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tiny-file size cannot be represented",
            )
        })?;

        let end = offset
            .checked_add(file_bytes)
            .ok_or_else(|| io::Error::other("tiny-pack buffer offset overflowed"))?;

        let destination = buffer.get_mut(offset..end).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tiny-pack contents exceed their transfer buffer",
            )
        })?;

        let path = source_root.join(&entry.relative_path);

        let mut file = File::open(&path)?;

        validate_source_metadata(&file, &path, entry)?;

        file.read_exact(destination)?;

        validate_source_metadata(&file, &path, entry)?;

        digests.push(*blake3::hash(destination).as_bytes());

        offset = end;
    }

    if offset != total_bytes {
        return Err(io::Error::other(format!(
            "tiny-pack assembly produced {offset} bytes, expected {total_bytes}",
        )));
    }

    if let Some(progress) = progress {
        progress.check_cancelled()?;
    }

    let raw_payload = &buffer[..total_bytes];

    let encoded = tiny_pack_codec::encode(raw_payload, compression_probe::DEFAULT_LEVEL)?;

    let wire_payload = encoded.wire_payload(raw_payload);

    let record_wire_bytes = tiny_pack_record_wire_bytes(file_ids.len(), wire_payload.len())?;

    let pack_digest = *blake3::hash(raw_payload).as_bytes();

    write_u8(writer, MESSAGE_TINY_PACK_V2)?;

    write_u32(
        writer,
        u32::try_from(file_ids.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tiny pack contains too many files",
            )
        })?,
    )?;

    write_u64(writer, summary.bytes)?;

    for (&file_id, digest) in file_ids.iter().zip(&digests) {
        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tiny pack contains an invalid file ID",
            )
        })?;

        write_u64(
            writer,
            u64::try_from(file_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "tiny-pack file ID cannot be represented",
                )
            })?,
        )?;

        write_u64(writer, entry.file_size)?;

        write_digest(writer, digest)?;
    }

    write_u8(writer, encoded.encoding().wire_value())?;

    write_u64(
        writer,
        u64::try_from(wire_payload.len())
            .map_err(|_| io::Error::other("tiny-pack wire length cannot be represented"))?,
    )?;

    writer.write_all(wire_payload)?;

    write_digest(writer, &pack_digest)?;

    Ok(TinyPackTransferSummary {
        summary,

        compressed: encoded.is_compressed(),

        wire_bytes: record_wire_bytes,
    })
}

fn summarize_tiny_pack(
    manifest: &[ManifestEntry],
    file_ids: &[usize],
) -> io::Result<TinyPackSummary> {
    if file_ids.is_empty() || file_ids.len() > MAX_TINY_PACK_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "tiny pack must contain between 1 and {} files",
                MAX_TINY_PACK_FILES
            ),
        ));
    }

    let mut bytes = 0_u64;

    for &file_id in file_ids {
        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tiny pack contains an invalid file ID",
            )
        })?;

        if entry.class != FileClass::Tiny {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tiny pack contains a non-tiny file",
            ));
        }

        bytes = bytes
            .checked_add(entry.file_size)
            .ok_or_else(|| io::Error::other("tiny-pack byte count overflowed"))?;
    }

    Ok(TinyPackSummary {
        files: u64::try_from(file_ids.len())
            .map_err(|_| io::Error::other("tiny-pack file count cannot be represented"))?,
        bytes,
    })
}

fn send_whole_file(
    writer: &mut impl Write,
    source_root: &Path,
    file_id: usize,
    entry: &ManifestEntry,
    buffer: &mut [u8],
    encoder: &mut PayloadEncoder,
    progress: Option<&ProgressCounter>,
) -> io::Result<bool> {
    write_u8(writer, MESSAGE_FILE)?;

    write_u64(
        writer,
        u64::try_from(file_id).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "file ID cannot be represented")
        })?,
    )?;

    write_u64(writer, entry.file_size)?;

    let path = source_root.join(&entry.relative_path);

    let mut file = File::open(&path)?;

    validate_source_metadata(&file, &path, entry)?;

    let decision = compression_probe::decide_file_range(
        &file,
        0,
        entry.file_size,
        compression_probe::DEFAULT_LEVEL,
    )?;

    let compressed = encoder.send_sequential_with_progress(
        writer,
        &mut file,
        entry.file_size,
        buffer,
        decision,
        progress,
    )?;

    validate_source_metadata(&file, &path, entry)?;

    Ok(compressed)
}

fn send_file_stripe(
    writer: &mut impl Write,
    source_root: &Path,
    entry: &ManifestEntry,
    stripe: StripeDescriptor,
    buffer: &mut [u8],
    encoder: &mut PayloadEncoder,
    progress: Option<&ProgressCounter>,
) -> io::Result<bool> {
    let StripeDescriptor {
        file_id,
        offset,
        length,
    } = stripe;

    validate_stripe(entry, offset, length)?;

    write_u8(writer, MESSAGE_FILE_STRIPE)?;

    write_u64(
        writer,
        u64::try_from(file_id).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "striped file ID cannot be represented",
            )
        })?,
    )?;

    write_u64(writer, offset)?;
    write_u64(writer, length)?;

    let path = source_root.join(&entry.relative_path);

    let file = File::open(&path)?;

    validate_source_metadata(&file, &path, entry)?;

    let decision = compression_probe::decide_file_range(
        &file,
        offset,
        length,
        compression_probe::DEFAULT_LEVEL,
    )?;

    let stripe_end = offset
        .checked_add(length)
        .ok_or_else(|| io::Error::other("stripe description overflowed"))?;

    let compressed = encoder.send_positional_with_progress(
        writer,
        &file,
        offset..stripe_end,
        buffer,
        decision,
        progress,
    )?;

    validate_source_metadata(&file, &path, entry)?;

    Ok(compressed)
}

fn validate_source_metadata(file: &File, path: &Path, entry: &ManifestEntry) -> io::Result<()> {
    let metadata = file.metadata()?;

    let current_size = metadata.len();

    if current_size != entry.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "source size changed after scanning: expected {} bytes, found {current_size}: {}",
                entry.file_size,
                path.display()
            ),
        ));
    }

    let current_last_write_time = metadata.last_write_time();

    if current_last_write_time != entry.last_write_time {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "source last-write time changed after scanning: expected {}, found {current_last_write_time}: {}",
                entry.last_write_time,
                path.display()
            ),
        ));
    }

    Ok(())
}

struct ReceiveLaneContext<'a> {
    destination_root: &'a Path,
    manifest: &'a [ManifestEntry],
    resume_journal: &'a Mutex<ResumeJournal>,
    fault_injection: &'a TransferFault,
    tiny_materializer: &'a tiny_file_pool::TinyFileMaterializerHandle,
    progress: Option<ProgressCounter>,
    cdc_enabled: bool,
    session_basis_file_ids: Option<&'a [usize]>,
}

fn receive_lane(
    stream: &TcpStream,
    tasks: &[TransferTask],
    context: ReceiveLaneContext<'_>,
    lane_end: LaneEnd,
) -> io::Result<LaneReport> {
    let ReceiveLaneContext {
        destination_root,
        manifest,
        resume_journal,
        fault_injection,
        tiny_materializer,
        progress,
        cdc_enabled,
        session_basis_file_ids,
    } = context;
    let reader_stream = stream.try_clone()?;

    let writer_stream = stream.try_clone()?;

    let buffered_reader = BufReader::with_capacity(NETWORK_BUFFER_BYTES, reader_stream);

    let mut reader = CountingReader::new(buffered_reader);

    let buffered_writer = BufWriter::with_capacity(NETWORK_BUFFER_BYTES, writer_stream);

    let mut writer = CountingWriter::new(buffered_writer);

    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];

    let mut decoder = PayloadDecoder::new()?;

    let mut report = LaneReport::default();

    for task in tasks {
        if let Some(progress) = &progress {
            progress.check_cancelled()?;
        }

        match task {
            TransferTask::WholeFile { file_id } => {
                let file_id = *file_id;

                let entry = manifest.get(file_id).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("received unknown file ID {file_id}",),
                    )
                })?;

                let message = if let Some(basis_file_ids) = session_basis_file_ids {
                    let message = read_u8(&mut reader)?;

                    if message == session_cdc_lane::MESSAGE_SESSION_CDC_PLAN {
                        let cdc_decision = session_cdc_lane::receiver_apply_plan(
                            &mut reader,
                            destination_root,
                            manifest,
                            file_id,
                            basis_file_ids,
                        )?;

                        report.cdc.merge(cdc_decision.stats)?;

                        if !cdc_decision.completed {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "session CDC plan did not complete its target",
                            ));
                        }

                        complete_received_cdc_file(
                            &mut report,
                            destination_root,
                            entry,
                            &progress,
                            fault_injection,
                        )?;

                        continue;
                    }

                    message
                } else {
                    let cdc_decision = cdc_lane::receiver_negotiate(
                        &mut reader,
                        &mut writer,
                        destination_root,
                        file_id,
                        entry,
                        cdc_enabled,
                    )?;

                    report.cdc.merge(cdc_decision.stats)?;

                    if cdc_decision.completed {
                        complete_received_cdc_file(
                            &mut report,
                            destination_root,
                            entry,
                            &progress,
                            fault_injection,
                        )?;

                        continue;
                    }

                    read_u8(&mut reader)?
                };

                if message != MESSAGE_FILE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("expected whole-file message, received 0x{message:02X}"),
                    ));
                }

                let announced_file_id = read_file_id(&mut reader)?;

                if announced_file_id != file_id {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("received file ID {announced_file_id}, expected {file_id}"),
                    ));
                }

                let announced_size = read_u64(&mut reader)?;

                if announced_size != entry.file_size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "file {file_id} announced {announced_size} bytes but manifest expects {}",
                            entry.file_size
                        ),
                    ));
                }

                let compressed = receive_file(
                    &mut reader,
                    destination_root,
                    file_id,
                    entry,
                    &mut buffer,
                    &mut decoder,
                    progress.as_ref(),
                )?;

                add_lane_counts(&mut report, 1, entry.file_size, "receiver")?;

                add_compressed_record(&mut report, compressed)?;
            }

            TransferTask::TinyPack {
                file_ids,
                total_bytes,
            } => {
                let transfer = receive_tiny_pack(
                    &mut reader,
                    manifest,
                    file_ids,
                    *total_bytes,
                    &mut buffer,
                    tiny_materializer,
                    progress.as_ref(),
                )?;

                add_lane_counts(
                    &mut report,
                    transfer.summary.files,
                    transfer.summary.bytes,
                    "receiver",
                )?;

                add_compressed_record(&mut report, transfer.compressed)?;

                add_tiny_pack_stats(&mut report, transfer, "receiver")?;
            }

            TransferTask::Stripe {
                file_id,
                offset,
                length,
            } => {
                let file_id = *file_id;
                let offset = *offset;
                let length = *length;

                let message = read_u8(&mut reader)?;

                if message != MESSAGE_FILE_STRIPE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("expected stripe message, received 0x{message:02X}"),
                    ));
                }

                let announced_file_id = read_file_id(&mut reader)?;

                let announced_offset = read_u64(&mut reader)?;

                let announced_length = read_u64(&mut reader)?;

                if announced_file_id != file_id
                    || announced_offset != offset
                    || announced_length != length
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "received stripe ({announced_file_id}, {announced_offset}, {announced_length}), expected ({file_id}, {offset}, {length})"
                        ),
                    ));
                }

                let entry = manifest.get(file_id).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("received unknown striped file ID {file_id}"),
                    )
                })?;

                let compressed = receive_file_stripe(
                    &mut reader,
                    destination_root,
                    entry,
                    StripeDescriptor {
                        file_id,
                        offset,
                        length,
                    },
                    &mut buffer,
                    &mut decoder,
                    progress.as_ref(),
                )?;

                let completed_files = u64::from(offset == 0);

                add_lane_counts(&mut report, completed_files, length, "receiver")?;

                add_compressed_record(&mut report, compressed)?;
            }
        }

        if checkpoint_completed_stripe(task, destination_root, resume_journal)? {
            fault_injection.after_checkpointed_stripe()?;
        }

        if let TransferTask::TinyPack { total_bytes, .. } = task {
            add_progress(&progress, *total_bytes);
        }
    }

    read_lane_end(&mut reader, lane_end)?;

    report.data_wire_bytes = reader
        .bytes_read()
        .checked_add(writer.bytes_written())
        .ok_or_else(|| io::Error::other("receiver bidirectional wire count overflowed"))?;

    Ok(report)
}

fn checkpoint_completed_stripe(
    task: &TransferTask,
    destination_root: &Path,
    resume_journal: &Mutex<ResumeJournal>,
) -> io::Result<bool> {
    let TransferTask::Stripe {
        file_id,
        offset,
        length,
    } = task
    else {
        return Ok(false);
    };

    let stripe = ResumeStripe::new(*file_id, *offset, *length)?;

    let mut journal = resume_journal
        .lock()
        .map_err(|_| io::Error::other("resume journal lock poisoned"))?;

    let newly_completed = journal.mark_completed(stripe);

    if newly_completed {
        journal.save_atomic(destination_root)?;
    }

    Ok(newly_completed)
}

fn receive_tiny_pack(
    reader: &mut impl Read,
    manifest: &[ManifestEntry],
    file_ids: &[usize],
    expected_total_bytes: u64,
    buffer: &mut [u8],
    tiny_materializer: &tiny_file_pool::TinyFileMaterializerHandle,
    progress: Option<&ProgressCounter>,
) -> io::Result<TinyPackTransferSummary> {
    let message = read_u8(reader)?;

    if message != MESSAGE_TINY_PACK_V2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected v1.3 tiny-pack message, received 0x{message:02X}",),
        ));
    }

    let announced_file_count = usize::try_from(read_u32(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "tiny-pack file count cannot be represented",
        )
    })?;

    let announced_total_bytes = read_u64(reader)?;

    let summary = summarize_tiny_pack(manifest, file_ids)?;

    if summary.bytes != expected_total_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receiver tiny-pack plan byte count changed",
        ));
    }

    if announced_file_count != file_ids.len() || announced_total_bytes != summary.bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "tiny pack announced {announced_file_count} files and {announced_total_bytes} bytes, expected {} files and {} bytes",
                file_ids.len(),
                summary.bytes,
            ),
        ));
    }

    let mut expected_digests = Vec::with_capacity(file_ids.len());

    for &expected_file_id in file_ids {
        let announced_file_id = read_file_id(reader)?;

        let announced_size = read_u64(reader)?;

        if announced_file_id != expected_file_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "tiny pack announced file ID {announced_file_id}, expected {expected_file_id}",
                ),
            ));
        }

        let entry = manifest.get(expected_file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "tiny pack referenced an unknown file",
            )
        })?;

        if announced_size != entry.file_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "tiny file {expected_file_id} announced {announced_size} bytes, expected {}",
                    entry.file_size,
                ),
            ));
        }

        expected_digests.push(read_digest(reader)?);
    }

    let encoding = TinyPackEncoding::from_wire(read_u8(reader)?)?;

    let wire_bytes = usize::try_from(read_u64(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "tiny-pack wire length cannot be represented",
        )
    })?;

    if wire_bytes > tiny_pack_codec::MAX_TINY_PACK_WIRE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "tiny-pack wire payload contains {wire_bytes} bytes, exceeding the supported limit",
            ),
        ));
    }

    let record_wire_bytes = tiny_pack_record_wire_bytes(file_ids.len(), wire_bytes)?;

    if let Some(progress) = progress {
        progress.check_cancelled()?;
    }

    let mut wire_payload = vec![0_u8; wire_bytes];

    reader.read_exact(&mut wire_payload)?;

    let total_bytes = usize::try_from(summary.bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "tiny-pack logical length cannot be represented",
        )
    })?;

    tiny_pack_codec::decode(encoding, &wire_payload, total_bytes, buffer)?;

    let expected_pack_digest = read_digest(reader)?;

    let actual_pack_digest = *blake3::hash(&buffer[..total_bytes]).as_bytes();

    verify_content_digest("tiny pack", &actual_pack_digest, &expected_pack_digest)?;

    let payload: Arc<[u8]> = Arc::from(&buffer[..total_bytes]);

    let mut materialization_requests = Vec::with_capacity(file_ids.len());

    let mut offset = 0_usize;

    for (&file_id, expected_digest) in file_ids.iter().zip(&expected_digests) {
        if let Some(progress) = progress {
            progress.check_cancelled()?;
        }

        let entry = manifest.get(file_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "tiny pack referenced an unknown file",
            )
        })?;

        let file_bytes = usize::try_from(entry.file_size).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "tiny-file size cannot be represented",
            )
        })?;

        let end = offset
            .checked_add(file_bytes)
            .ok_or_else(|| io::Error::other("tiny-pack receive offset overflowed"))?;

        let contents = payload.get(offset..end).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "tiny-pack file contents exceed the decoded payload",
            )
        })?;

        materialization_requests.push(prepare_buffered_tiny_file(
            file_id,
            entry,
            contents,
            expected_digest,
            offset..end,
        )?);

        offset = end;
    }

    if offset != total_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("decoded tiny files consumed {offset} bytes, expected {total_bytes}",),
        ));
    }

    tiny_materializer.materialize_batch(payload, materialization_requests)?;

    Ok(TinyPackTransferSummary {
        summary,

        compressed: matches!(encoding, TinyPackEncoding::Zstandard,),

        wire_bytes: record_wire_bytes,
    })
}

fn prepare_buffered_tiny_file(
    file_id: usize,
    entry: &ManifestEntry,
    contents: &[u8],
    expected_digest: &[u8; CONTENT_DIGEST_BYTES],
    range: std::ops::Range<usize>,
) -> io::Result<tiny_file_pool::TinyFileMaterializeRequest> {
    let actual_digest = *blake3::hash(contents).as_bytes();

    let context = format!("tiny file {file_id} ({})", entry.relative_path.display(),);

    verify_content_digest(&context, &actual_digest, expected_digest)?;

    Ok(tiny_file_pool::TinyFileMaterializeRequest::new(
        file_id,
        entry.relative_path.clone(),
        range,
    ))
}

fn receive_file_stripe(
    reader: &mut impl Read,
    destination_root: &Path,
    entry: &ManifestEntry,
    stripe: StripeDescriptor,
    buffer: &mut [u8],
    decoder: &mut PayloadDecoder,
    progress: Option<&ProgressCounter>,
) -> io::Result<bool> {
    let StripeDescriptor {
        file_id,
        offset,
        length,
    } = stripe;

    validate_stripe(entry, offset, length)?;

    let final_path = destination_root.join(&entry.relative_path);

    let temporary_path = temporary_path(&final_path, file_id);

    let file = OpenOptions::new().write(true).open(temporary_path)?;

    let stripe_end = offset
        .checked_add(length)
        .ok_or_else(|| io::Error::other("stripe description overflowed"))?;

    let context = format!(
        "file {file_id} stripe {offset}..{stripe_end} ({})",
        entry.relative_path.display()
    );

    let compressed = decoder.receive_positional_with_progress(
        reader,
        &file,
        offset..stripe_end,
        buffer,
        &context,
        progress,
    )?;

    file.sync_all()?;

    Ok(compressed)
}

fn validate_stripe(entry: &ManifestEntry, offset: u64, length: u64) -> io::Result<()> {
    if entry.class != FileClass::Large {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stripe was assigned to a non-large file",
        ));
    }

    let end = offset
        .checked_add(length)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "stripe range overflowed"))?;

    if length == 0 || end > entry.file_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "stripe range {offset}..{end} exceeds file length {}",
                entry.file_size
            ),
        ));
    }

    Ok(())
}

fn finalize_large_files(
    destination_root: &Path,
    manifest: &[ManifestEntry],
    destination_mode: DestinationMode,
) -> io::Result<()> {
    for (file_id, entry) in manifest.iter().enumerate() {
        if entry.class != FileClass::Large {
            continue;
        }

        finalize_large_file(destination_root, file_id, entry, destination_mode)?;
    }

    Ok(())
}

fn finalize_large_file(
    destination_root: &Path,
    file_id: usize,
    entry: &ManifestEntry,
    destination_mode: DestinationMode,
) -> io::Result<()> {
    if entry.class != FileClass::Large {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "large-file finalization received a non-large file",
        ));
    }

    let final_path = destination_root.join(&entry.relative_path);

    let temporary_path = temporary_path(&final_path, file_id);

    let final_exists = final_path.try_exists()?;

    let temporary_exists = temporary_path.try_exists()?;

    match (final_exists, temporary_exists) {
        (false, true) => {
            validate_resume_file(&temporary_path, entry.file_size, "striped temporary file")?;

            windows_file_replace::replace(&temporary_path, &final_path)?;
        }

        (true, false) => {
            validate_resume_file(&final_path, entry.file_size, "finalized striped file")?;
        }

        (true, true) if destination_mode == DestinationMode::UpdateVerified => {
            validate_resume_file(
                &temporary_path,
                entry.file_size,
                "striped update temporary file",
            )?;

            windows_file_replace::replace(&temporary_path, &final_path)?;
        }

        (true, true) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "both final and temporary striped files exist: {}",
                    final_path.display(),
                ),
            ));
        }

        (false, false) => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "striped temporary file is missing: {}",
                    temporary_path.display(),
                ),
            ));
        }
    }

    Ok(())
}

fn receive_file(
    reader: &mut impl Read,
    destination_root: &Path,
    file_id: usize,
    entry: &ManifestEntry,
    buffer: &mut [u8],
    decoder: &mut PayloadDecoder,
    progress: Option<&ProgressCounter>,
) -> io::Result<bool> {
    let final_path = destination_root.join(&entry.relative_path);

    let temporary_path = temporary_path(&final_path, file_id);

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)?;

    file.set_len(entry.file_size)?;

    let context = format!("file {file_id} ({})", entry.relative_path.display());

    let transfer_result = (|| -> io::Result<bool> {
        let compressed = decoder.receive_sequential_with_progress(
            reader,
            &mut file,
            entry.file_size,
            buffer,
            &context,
            progress,
        )?;

        file.flush()?;
        Ok(compressed)
    })();

    let compressed = match transfer_result {
        Ok(compressed) => compressed,

        Err(error) => {
            drop(file);

            let _ = fs::remove_file(&temporary_path);

            return Err(error);
        }
    };

    drop(file);

    windows_file_replace::replace(&temporary_path, &final_path)?;

    Ok(compressed)
}

fn temporary_path(final_path: &Path, file_id: usize) -> PathBuf {
    let mut temporary = OsString::from(final_path.as_os_str());

    temporary.push(format!(".ncs-part-{file_id}"));
    PathBuf::from(temporary)
}

fn join_lane_threads<T>(
    handles: Vec<thread::ScopedJoinHandle<'_, io::Result<T>>>,
) -> io::Result<Vec<T>> {
    let mut results = Vec::with_capacity(handles.len());
    let mut first_error = None;

    for handle in handles {
        match handle.join() {
            Ok(Ok(result)) => results.push(result),

            Ok(Err(error)) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }

            Err(_) => {
                if first_error.is_none() {
                    first_error = Some(io::Error::other("TCP data-lane thread panicked"));
                }
            }
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    Ok(results)
}

fn merge_lane_reports(reports: Vec<LaneReport>) -> io::Result<LaneReport> {
    let mut merged = LaneReport::default();

    for report in reports {
        merged.files_copied = merged
            .files_copied
            .checked_add(report.files_copied)
            .ok_or_else(|| io::Error::other("merged file count overflowed"))?;

        merged.bytes_copied = merged
            .bytes_copied
            .checked_add(report.bytes_copied)
            .ok_or_else(|| io::Error::other("merged byte count overflowed"))?;

        merged.data_wire_bytes = merged
            .data_wire_bytes
            .checked_add(report.data_wire_bytes)
            .ok_or_else(|| io::Error::other("merged wire-byte count overflowed"))?;

        merged.compressed_records = merged
            .compressed_records
            .checked_add(report.compressed_records)
            .ok_or_else(|| io::Error::other("merged compressed-record count overflowed"))?;

        merged.cdc.merge(report.cdc)?;

        merged.exact_reused_files = merged
            .exact_reused_files
            .checked_add(report.exact_reused_files)
            .ok_or_else(|| io::Error::other("merged exact-reuse file count overflowed"))?;

        merged.exact_reused_bytes = merged
            .exact_reused_bytes
            .checked_add(report.exact_reused_bytes)
            .ok_or_else(|| io::Error::other("merged exact-reuse byte count overflowed"))?;

        merged.exact_reuse_plan_wire_bytes = merged
            .exact_reuse_plan_wire_bytes
            .checked_add(report.exact_reuse_plan_wire_bytes)
            .ok_or_else(|| io::Error::other("merged exact-reuse wire count overflowed"))?;

        merged.tiny_pack_count = merged
            .tiny_pack_count
            .checked_add(report.tiny_pack_count)
            .ok_or_else(|| io::Error::other("merged tiny-pack count overflowed"))?;

        merged.compressed_tiny_pack_count = merged
            .compressed_tiny_pack_count
            .checked_add(report.compressed_tiny_pack_count)
            .ok_or_else(|| io::Error::other("merged compressed tiny-pack count overflowed"))?;

        merged.raw_tiny_pack_count = merged
            .raw_tiny_pack_count
            .checked_add(report.raw_tiny_pack_count)
            .ok_or_else(|| io::Error::other("merged raw tiny-pack count overflowed"))?;

        merged.tiny_files_packed = merged
            .tiny_files_packed
            .checked_add(report.tiny_files_packed)
            .ok_or_else(|| io::Error::other("merged packed tiny-file count overflowed"))?;

        merged.tiny_bytes_packed = merged
            .tiny_bytes_packed
            .checked_add(report.tiny_bytes_packed)
            .ok_or_else(|| io::Error::other("merged packed tiny-byte count overflowed"))?;

        merged.tiny_pack_wire_bytes = merged
            .tiny_pack_wire_bytes
            .checked_add(report.tiny_pack_wire_bytes)
            .ok_or_else(|| io::Error::other("merged tiny-pack wire-byte count overflowed"))?;
    }

    Ok(merged)
}

fn validate_tiny_pack_plan(report: &LaneReport, expected: TransferPlanStats) -> io::Result<()> {
    let encoded_pack_count = report
        .compressed_tiny_pack_count
        .checked_add(report.raw_tiny_pack_count)
        .ok_or_else(|| io::Error::other("tiny-pack encoding count overflowed"))?;

    if report.tiny_pack_count != expected.tiny_pack_count
        || report.tiny_files_packed != expected.tiny_files_packed
        || report.tiny_bytes_packed != expected.tiny_bytes_packed
        || encoded_pack_count != report.tiny_pack_count
    {
        return Err(io::Error::other(
            "actual tiny-pack statistics do not match the transfer plan",
        ));
    }

    Ok(())
}

fn write_update_verification_request(
    writer: &mut impl Write,
    digests: &BTreeMap<usize, FileDigest>,
) -> io::Result<()> {
    let count = u32::try_from(digests.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "verification request contains too many files",
        )
    })?;

    if count > MAX_UNCHANGED_OFFER_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "verification request exceeds the supported file limit",
        ));
    }

    write_u8(writer, MESSAGE_UPDATE_VERIFY_REQUEST)?;

    write_u32(writer, count)?;

    for (&file_id, digest) in digests {
        write_u64(
            writer,
            u64::try_from(file_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "verification file ID cannot be represented",
                )
            })?,
        )?;

        writer.write_all(digest)?;
    }

    writer.flush()
}

fn read_update_verification_request(
    reader: &mut impl Read,
) -> io::Result<BTreeMap<usize, FileDigest>> {
    let count = read_u32(reader)?;

    if count > MAX_UNCHANGED_OFFER_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "verification request exceeds the supported file limit",
        ));
    }

    let mut digests = BTreeMap::new();

    for _ in 0..count {
        let file_id = read_file_id(reader)?;

        let mut digest = [0_u8; FILE_DIGEST_BYTES];

        reader.read_exact(&mut digest)?;

        if digests.insert(file_id, digest).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "verification request contains a duplicate file ID",
            ));
        }
    }

    Ok(digests)
}

fn write_update_verification_response(
    writer: &mut impl Write,
    matching_file_ids: &BTreeSet<usize>,
) -> io::Result<()> {
    let count = u32::try_from(matching_file_ids.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "verification response contains too many files",
        )
    })?;

    write_u8(writer, MESSAGE_UPDATE_VERIFY_RESPONSE)?;

    write_u32(writer, count)?;

    for &file_id in matching_file_ids {
        write_u64(
            writer,
            u64::try_from(file_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "verified file ID cannot be represented",
                )
            })?,
        )?;
    }

    writer.flush()
}

fn read_update_verification_response(reader: &mut impl Read) -> io::Result<BTreeSet<usize>> {
    let message = read_u8(reader)?;

    if message != MESSAGE_UPDATE_VERIFY_RESPONSE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected update-verification response, received 0x{message:02X}"),
        ));
    }

    let count = read_u32(reader)?;

    if count > MAX_UNCHANGED_OFFER_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "verification response exceeds the supported file limit",
        ));
    }

    let mut matching_file_ids = BTreeSet::new();

    for _ in 0..count {
        let file_id = read_file_id(reader)?;

        if !matching_file_ids.insert(file_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "verification response contains a duplicate file ID",
            ));
        }
    }

    Ok(matching_file_ids)
}

fn write_receiver_ready(writer: &mut impl Write, ready: &ReceiverReady) -> io::Result<()> {
    let stripe_count = u32::try_from(ready.completed_stripes.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "resume offer contains too many completed stripes",
        )
    })?;

    if stripe_count > MAX_RESUME_OFFER_STRIPES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("resume offer contains {stripe_count} stripes, exceeding the supported limit"),
        ));
    }

    write_u8(writer, MESSAGE_RECEIVER_READY)?;

    write_u64(writer, ready.summary.entries)?;

    write_u64(writer, ready.summary.total_file_bytes)?;

    write_u64(writer, ready.summary.fingerprint)?;

    write_u32(writer, stripe_count)?;

    for stripe in &ready.completed_stripes {
        write_u64(writer, stripe.file_id)?;

        write_u64(writer, stripe.offset)?;

        write_u64(writer, stripe.length)?;
    }

    let unchanged_count = u32::try_from(ready.unchanged_file_ids.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "unchanged-file offer contains too many file IDs",
        )
    })?;

    if unchanged_count > MAX_UNCHANGED_OFFER_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unchanged-file offer contains {unchanged_count} files, exceeding the supported limit",
            ),
        ));
    }

    write_u32(writer, unchanged_count)?;

    for &file_id in &ready.unchanged_file_ids {
        write_u64(
            writer,
            u64::try_from(file_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unchanged file ID cannot be represented",
                )
            })?,
        )?;
    }

    writer.flush()
}

#[cfg(test)]
fn read_receiver_ready(reader: &mut impl Read) -> io::Result<ReceiverReady> {
    let message = read_u8(reader).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("receiver disconnected before the file transfer became ready: {error}"),
        )
    })?;

    read_receiver_ready_after_message(reader, message)
}

fn read_receiver_ready_after_message(
    reader: &mut impl Read,
    message: u8,
) -> io::Result<ReceiverReady> {
    if message != MESSAGE_RECEIVER_READY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected receiver-ready message, received 0x{message:02X}"),
        ));
    }

    let summary = ManifestSummary {
        entries: read_u64(reader)?,
        total_file_bytes: read_u64(reader)?,
        fingerprint: read_u64(reader)?,
    };

    let stripe_count = read_u32(reader)?;

    if stripe_count > MAX_RESUME_OFFER_STRIPES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "receiver offered {stripe_count} completed stripes, exceeding the supported limit",
            ),
        ));
    }

    let mut completed_stripes = BTreeSet::new();

    for _ in 0..stripe_count {
        let file_id = usize::try_from(read_u64(reader)?).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "resume stripe file ID cannot be represented",
            )
        })?;

        let offset = read_u64(reader)?;

        let length = read_u64(reader)?;

        let stripe = ResumeStripe::new(file_id, offset, length).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("received invalid resume stripe: {error}"),
            )
        })?;

        if !completed_stripes.insert(stripe) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "receiver offered a duplicate completed stripe",
            ));
        }
    }

    let unchanged_count = read_u32(reader)?;

    if unchanged_count > MAX_UNCHANGED_OFFER_FILES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "receiver offered {unchanged_count} unchanged files, exceeding the supported limit",
            ),
        ));
    }

    let mut unchanged_file_ids = BTreeSet::new();

    for _ in 0..unchanged_count {
        let file_id = read_file_id(reader)?;

        if !unchanged_file_ids.insert(file_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "receiver offered a duplicate unchanged file ID",
            ));
        }
    }

    Ok(ReceiverReady {
        summary,
        completed_stripes,
        unchanged_file_ids,
    })
}

fn generation_commit_list_count(file_ids: &[usize], description: &str) -> io::Result<u32> {
    let count = u32::try_from(file_ids.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} count cannot be represented"),
        )
    })?;

    if count > MAX_GENERATION_COMMIT_FILE_IDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} contains {count} file IDs, exceeding the supported limit",),
        ));
    }

    Ok(count)
}

fn generation_commit_wire_bytes(commit: &GenerationCommit) -> io::Result<u64> {
    let committed = u64::from(generation_commit_list_count(
        &commit.committed_file_ids,
        "generation committed-file list",
    )?);

    let published = u64::from(generation_commit_list_count(
        &commit.published_file_ids,
        "generation published-file list",
    )?);

    let evicted = u64::from(generation_commit_list_count(
        &commit.evicted_file_ids,
        "generation evicted-file list",
    )?);

    let total_file_ids = committed
        .checked_add(published)
        .and_then(|count| count.checked_add(evicted))
        .ok_or_else(|| io::Error::other("generation-commit file-ID count overflowed"))?;

    GENERATION_COMMIT_FIXED_WIRE_BYTES
        .checked_add(
            total_file_ids
                .checked_mul(GENERATION_COMMIT_FILE_ID_WIRE_BYTES)
                .ok_or_else(|| {
                    io::Error::other("generation-commit file-ID wire size overflowed")
                })?,
        )
        .ok_or_else(|| io::Error::other("generation-commit wire size overflowed"))
}

fn validate_generation_file_ids_for_write(file_ids: &[usize], description: &str) -> io::Result<()> {
    generation_commit_list_count(file_ids, description)?;

    let mut seen = BTreeSet::new();

    for &file_id in file_ids {
        if !seen.insert(file_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{description} contains duplicate file ID {file_id}"),
            ));
        }
    }

    Ok(())
}

fn write_generation_file_ids(
    writer: &mut impl Write,
    file_ids: &[usize],
    description: &str,
) -> io::Result<()> {
    let count = generation_commit_list_count(file_ids, description)?;

    write_u32(writer, count)?;

    for &file_id in file_ids {
        write_u64(
            writer,
            u64::try_from(file_id).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{description} file ID cannot be represented",),
                )
            })?,
        )?;
    }

    Ok(())
}

fn read_generation_file_ids(reader: &mut impl Read, description: &str) -> io::Result<Vec<usize>> {
    let count = read_u32(reader)?;

    if count > MAX_GENERATION_COMMIT_FILE_IDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} contains {count} file IDs, exceeding the supported limit",),
        ));
    }

    let capacity = usize::try_from(count).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} count cannot be represented"),
        )
    })?;

    let mut file_ids = Vec::with_capacity(capacity);

    let mut seen = BTreeSet::new();

    for _ in 0..count {
        let file_id = read_file_id(reader)?;

        if !seen.insert(file_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{description} contains duplicate file ID {file_id}",),
            ));
        }

        file_ids.push(file_id);
    }

    Ok(file_ids)
}

fn write_generation_commit(writer: &mut impl Write, commit: &GenerationCommit) -> io::Result<()> {
    validate_generation_file_ids_for_write(
        &commit.committed_file_ids,
        "generation committed-file list",
    )?;

    validate_generation_file_ids_for_write(
        &commit.published_file_ids,
        "generation published-file list",
    )?;

    validate_generation_file_ids_for_write(
        &commit.evicted_file_ids,
        "generation evicted-file list",
    )?;

    generation_commit_wire_bytes(commit)?;

    write_u8(writer, MESSAGE_GENERATION_COMMIT)?;

    write_u64(
        writer,
        u64::try_from(commit.generation_index).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "generation-commit index cannot be represented",
            )
        })?,
    )?;

    write_generation_file_ids(
        writer,
        &commit.committed_file_ids,
        "generation committed-file list",
    )?;

    write_generation_file_ids(
        writer,
        &commit.published_file_ids,
        "generation published-file list",
    )?;

    write_generation_file_ids(
        writer,
        &commit.evicted_file_ids,
        "generation evicted-file list",
    )?;

    writer.flush()
}

fn read_generation_commit(reader: &mut impl Read) -> io::Result<GenerationCommit> {
    let message = read_u8(reader)?;

    if message != MESSAGE_GENERATION_COMMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected generation-commit message, received 0x{message:02X}",),
        ));
    }

    let generation_index = usize::try_from(read_u64(reader)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "received generation-commit index cannot be represented",
        )
    })?;

    let commit = GenerationCommit {
        generation_index,

        committed_file_ids: read_generation_file_ids(reader, "generation committed-file list")?,

        published_file_ids: read_generation_file_ids(reader, "generation published-file list")?,

        evicted_file_ids: read_generation_file_ids(reader, "generation evicted-file list")?,
    };

    generation_commit_wire_bytes(&commit).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("received generation commit has an invalid wire size: {error}",),
        )
    })?;

    Ok(commit)
}

fn expected_generation_commit(generation: &CatalogGeneration) -> GenerationCommit {
    GenerationCommit {
        generation_index: generation.index,

        committed_file_ids: generation
            .transfer_files
            .iter()
            .map(|candidate| candidate.file_id)
            .collect(),

        published_file_ids: generation.published_file_ids.clone(),

        evicted_file_ids: generation.evicted_file_ids.clone(),
    }
}

fn validate_generation_commit_ids(
    file_ids: &[usize],
    manifest_entries: usize,
    description: &str,
) -> io::Result<()> {
    let mut seen = BTreeSet::new();

    for &file_id in file_ids {
        if file_id >= manifest_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{description} references unknown file ID {file_id}",),
            ));
        }

        if !seen.insert(file_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{description} contains duplicate file ID {file_id}",),
            ));
        }
    }

    Ok(())
}

fn validate_generation_commit(
    commit: &GenerationCommit,
    generation: &CatalogGeneration,
    manifest_entries: usize,
) -> io::Result<()> {
    if commit.generation_index != generation.index {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "receiver committed generation {}, expected {}",
                commit.generation_index, generation.index,
            ),
        ));
    }

    validate_generation_commit_ids(
        &commit.committed_file_ids,
        manifest_entries,
        "generation committed-file list",
    )?;

    validate_generation_commit_ids(
        &commit.published_file_ids,
        manifest_entries,
        "generation published-file list",
    )?;

    validate_generation_commit_ids(
        &commit.evicted_file_ids,
        manifest_entries,
        "generation evicted-file list",
    )?;

    let expected = expected_generation_commit(generation);

    if commit.committed_file_ids != expected.committed_file_ids {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receiver generation committed-file list differs from the deterministic plan",
        ));
    }

    if commit.published_file_ids != expected.published_file_ids {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receiver generation published-file list differs from the deterministic plan",
        ));
    }

    if commit.evicted_file_ids != expected.evicted_file_ids {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "receiver generation eviction list differs from the deterministic plan",
        ));
    }

    Ok(())
}

fn write_transfer_ack(writer: &mut impl Write, ack: TransferAck) -> io::Result<()> {
    write_u8(writer, MESSAGE_TRANSFER_ACK)?;

    write_u64(writer, ack.files_copied)?;

    write_u64(writer, ack.bytes_copied)?;

    write_u64(writer, ack.data_wire_bytes)?;

    write_u64(writer, ack.compressed_records)?;

    write_u64(writer, ack.cdc.offered_files)?;

    write_u64(writer, ack.cdc.completed_files)?;

    write_u64(writer, ack.cdc.fallback_files)?;

    write_u64(writer, ack.cdc.logical_bytes)?;

    write_u64(writer, ack.cdc.reused_bytes)?;

    write_u64(writer, ack.cdc.literal_bytes)?;

    write_u64(writer, ack.cdc.index_wire_bytes)?;

    write_u64(writer, ack.cdc.plan_wire_bytes)?;

    write_u32(writer, ack.tiny_materialization_workers)?;

    write_u64(writer, ack.tiny_pack_count)?;

    write_u64(writer, ack.compressed_tiny_pack_count)?;

    write_u64(writer, ack.raw_tiny_pack_count)?;

    write_u64(writer, ack.tiny_files_packed)?;

    write_u64(writer, ack.tiny_bytes_packed)?;

    write_u64(writer, ack.tiny_pack_wire_bytes)?;

    writer.flush()
}

fn read_transfer_ack(reader: &mut impl Read) -> io::Result<TransferAck> {
    let message = read_u8(reader)?;

    if message != MESSAGE_TRANSFER_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected transfer acknowledgement, received 0x{message:02X}"),
        ));
    }

    Ok(TransferAck {
        files_copied: read_u64(reader)?,

        bytes_copied: read_u64(reader)?,

        data_wire_bytes: read_u64(reader)?,

        compressed_records: read_u64(reader)?,

        cdc: cdc_lane::CdcLaneStats {
            offered_files: read_u64(reader)?,

            completed_files: read_u64(reader)?,

            fallback_files: read_u64(reader)?,

            logical_bytes: read_u64(reader)?,

            reused_bytes: read_u64(reader)?,

            literal_bytes: read_u64(reader)?,

            index_wire_bytes: read_u64(reader)?,

            plan_wire_bytes: read_u64(reader)?,
        },

        tiny_materialization_workers: read_u32(reader)?,

        tiny_pack_count: read_u64(reader)?,

        compressed_tiny_pack_count: read_u64(reader)?,

        raw_tiny_pack_count: read_u64(reader)?,

        tiny_files_packed: read_u64(reader)?,

        tiny_bytes_packed: read_u64(reader)?,

        tiny_pack_wire_bytes: read_u64(reader)?,
    })
}

#[derive(Debug)]
struct CountingWriter<W> {
    inner: W,
    bytes_written: u64,
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            bytes_written: 0,
        }
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;

        self.bytes_written = self
            .bytes_written
            .checked_add(
                u64::try_from(written)
                    .map_err(|_| io::Error::other("wire write length cannot be represented"))?,
            )
            .ok_or_else(|| io::Error::other("data wire-byte count overflowed"))?;

        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug)]
struct CountingReader<R> {
    inner: R,
    bytes_read: u64,
}

impl<R> CountingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            bytes_read: 0,
        }
    }

    fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;

        self.bytes_read = self
            .bytes_read
            .checked_add(
                u64::try_from(read)
                    .map_err(|_| io::Error::other("wire read length cannot be represented"))?,
            )
            .ok_or_else(|| io::Error::other("data wire read count overflowed"))?;

        Ok(read)
    }
}

fn write_digest(writer: &mut impl Write, digest: &[u8; CONTENT_DIGEST_BYTES]) -> io::Result<()> {
    writer.write_all(digest)
}

fn read_digest(reader: &mut impl Read) -> io::Result<[u8; CONTENT_DIGEST_BYTES]> {
    let mut digest = [0_u8; CONTENT_DIGEST_BYTES];
    reader.read_exact(&mut digest)?;
    Ok(digest)
}

fn verify_content_digest(
    context: &str,
    actual: &[u8; CONTENT_DIGEST_BYTES],
    expected: &[u8; CONTENT_DIGEST_BYTES],
) -> io::Result<()> {
    if actual == expected {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "BLAKE3 verification failed for {context}: expected {}, calculated {}",
            content_hash::format_digest(expected),
            content_hash::format_digest(actual)
        ),
    ))
}

fn write_u8(writer: &mut impl Write, value: u8) -> io::Result<()> {
    writer.write_all(&[value])
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_be_bytes())
}

fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut value = [0_u8; 1];
    reader.read_exact(&mut value)?;
    Ok(value[0])
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut value = [0_u8; 4];
    reader.read_exact(&mut value)?;
    Ok(u32::from_be_bytes(value))
}

fn read_file_id(reader: &mut impl Read) -> io::Result<usize> {
    let file_id = read_u64(reader)?;

    usize::try_from(file_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "received file ID is too large"))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = [0_u8; 8];
    reader.read_exact(&mut value)?;
    Ok(u64::from_be_bytes(value))
}

#[cfg(test)]
mod tests {
    use super::{
        DestinationMode, GenerationCommit, LaneEnd, MESSAGE_GENERATION_COMMIT, ReceiverReady,
        TINY_PACK_TARGET_BYTES, TransferAck, TransferFault, TransferTask, accept_session,
        apply_fresh_resume_prefix, apply_resume_offer, build_fresh_generation_plan_with_limits,
        build_transfer_plan, catalog_task_file_id, connect_with_retry_config,
        expected_generation_commit, finalize_large_files, generation_commit_wire_bytes,
        persist_generation_commit, prepare_destination, read_generation_commit, read_lane_end,
        read_receiver_ready, read_transfer_ack, rebuild_fresh_generation_execution, receive_once,
        run, run_update, run_update_with_fault, run_with_fault, send, temporary_path,
        tiny_pack_record_wire_bytes, validate_generation_commit, validate_resume_offer,
        validate_source_metadata, verify_content_digest, write_generation_commit, write_lane_end,
        write_receiver_ready, write_transfer_ack,
    };
    use crate::control_plane::{self, ManifestSummary};
    use crate::file_metadata;
    use crate::manifest_scan::{self, FileClass, ManifestEntry};
    use crate::resume_state::{JOURNAL_FILE_NAME, ResumeJournal, ResumeStripe};
    use crate::session_cdc_catalog::CatalogLimits;
    use std::collections::BTreeSet;
    use std::env;
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Cursor, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::windows::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;

    #[test]
    fn tiny_pack_wire_size_includes_protocol_metadata() {
        assert_eq!(tiny_pack_record_wire_bytes(2, 100,).unwrap(), 250,);

        assert!(tiny_pack_record_wire_bytes(usize::MAX, usize::MAX,).is_err(),);
    }

    #[test]
    fn scheduler_stripes_large_files_and_packs_tiny_files() {
        let manifest = vec![
            entry("large.bin", 1_000, FileClass::Large),
            entry("medium.bin", 500_000, FileClass::Medium),
            entry("tiny-a.bin", 10, FileClass::Tiny),
            entry("tiny-b.bin", 20, FileClass::Tiny),
        ];

        let plan = build_transfer_plan(&manifest, 2).unwrap();

        assert_eq!(plan.lanes.len(), 2);

        let mut stripe_count = 0_usize;
        let mut stripe_bytes = 0_u64;
        let mut whole_file_count = 0_usize;
        let mut tiny_pack_count = 0_usize;
        let mut tiny_file_count = 0_usize;

        for lane in &plan.lanes {
            for task in lane {
                match task {
                    TransferTask::Stripe {
                        file_id, length, ..
                    } => {
                        assert_eq!(*file_id, 0);
                        stripe_count += 1;
                        stripe_bytes += *length;
                    }

                    TransferTask::WholeFile { file_id } => {
                        assert_eq!(*file_id, 1);
                        whole_file_count += 1;
                    }

                    TransferTask::TinyPack {
                        file_ids,
                        total_bytes,
                    } => {
                        assert_eq!(file_ids.as_slice(), &[2, 3]);

                        assert_eq!(*total_bytes, 30);

                        tiny_pack_count += 1;
                        tiny_file_count += file_ids.len();
                    }
                }
            }
        }

        assert_eq!(stripe_count, 2);
        assert_eq!(stripe_bytes, 1_000);
        assert_eq!(whole_file_count, 1);
        assert_eq!(tiny_pack_count, 1);
        assert_eq!(tiny_file_count, 2);
    }

    #[test]
    fn fresh_generation_plan_partitions_catalog_files_and_keeps_tiny_packs_trailing() {
        let manifest = vec![
            entry("large.bin", 300, FileClass::Large),
            entry("medium.bin", 200, FileClass::Medium),
            entry("tiny-a.bin", 10, FileClass::Tiny),
            entry("tiny-b.bin", 20, FileClass::Tiny),
        ];

        let transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let generation_plan = build_fresh_generation_plan_with_limits(
            &manifest,
            &transfer_plan,
            CatalogLimits {
                generation_target_bytes: 250,
                max_catalog_entries: 100,
            },
        )
        .unwrap();

        assert_eq!(generation_plan.catalog.generations.len(), 2);
        assert_eq!(generation_plan.generation_lanes.len(), 2);

        assert!(
            generation_plan.catalog.generations[0]
                .basis_file_ids
                .is_empty()
        );

        assert_eq!(
            generation_plan.catalog.generations[0].published_file_ids,
            vec![0],
        );

        assert_eq!(
            generation_plan.catalog.generations[1].basis_file_ids,
            vec![0],
        );

        assert_eq!(
            generation_plan.catalog.generations[1].published_file_ids,
            vec![1],
        );

        let first_generation_file_ids: BTreeSet<usize> = generation_plan.generation_lanes[0]
            .iter()
            .flatten()
            .filter_map(catalog_task_file_id)
            .collect();

        let second_generation_file_ids: BTreeSet<usize> = generation_plan.generation_lanes[1]
            .iter()
            .flatten()
            .filter_map(catalog_task_file_id)
            .collect();

        assert_eq!(first_generation_file_ids, BTreeSet::from([0]));
        assert_eq!(second_generation_file_ids, BTreeSet::from([1]));

        let trailing_tasks = generation_plan
            .trailing_lanes
            .iter()
            .flatten()
            .collect::<Vec<_>>();

        assert_eq!(trailing_tasks.len(), 1);

        match trailing_tasks[0] {
            TransferTask::TinyPack {
                file_ids,
                total_bytes,
            } => {
                assert_eq!(file_ids.as_slice(), &[2, 3]);
                assert_eq!(*total_bytes, 30);
            }

            task => {
                panic!("unexpected trailing task: {task:?}");
            }
        }
    }

    #[test]
    fn fresh_generation_plan_keeps_tiny_only_transfer_on_one_shot_lanes() {
        let manifest = vec![
            entry("tiny-a.bin", 10, FileClass::Tiny),
            entry("tiny-b.bin", 20, FileClass::Tiny),
        ];

        let transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let expected_task_count = transfer_plan.lanes.iter().map(Vec::len).sum::<usize>();

        let generation_plan = build_fresh_generation_plan_with_limits(
            &manifest,
            &transfer_plan,
            CatalogLimits {
                generation_target_bytes: 1,
                max_catalog_entries: 100,
            },
        )
        .unwrap();

        assert!(generation_plan.catalog.generations.is_empty());
        assert!(generation_plan.generation_lanes.is_empty());

        let trailing_task_count = generation_plan
            .trailing_lanes
            .iter()
            .map(Vec::len)
            .sum::<usize>();

        assert_eq!(trailing_task_count, expected_task_count);

        assert!(
            generation_plan
                .trailing_lanes
                .iter()
                .flatten()
                .all(|task| matches!(task, TransferTask::TinyPack { .. }))
        );
    }

    #[test]
    fn fresh_resume_accepts_complete_generation_prefix() {
        let manifest = vec![
            entry("medium-a.bin", 100, FileClass::Medium),
            entry("medium-b.bin", 100, FileClass::Medium),
            entry("medium-c.bin", 100, FileClass::Medium),
        ];

        let transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let mut generation_plan = build_fresh_generation_plan_with_limits(
            &manifest,
            &transfer_plan,
            CatalogLimits {
                generation_target_bytes: 100,
                max_catalog_entries: 100,
            },
        )
        .unwrap();

        assert_eq!(generation_plan.catalog.generations.len(), 3,);

        let completed_file_ids: BTreeSet<usize> = generation_plan.catalog.generations[0]
            .transfer_files
            .iter()
            .map(|candidate| candidate.file_id)
            .collect();

        let expected_next_basis = generation_plan.catalog.generations[1]
            .basis_file_ids
            .clone();

        apply_fresh_resume_prefix(&mut generation_plan, &completed_file_ids).unwrap();

        assert_eq!(generation_plan.completed_generation_count, 1,);

        assert_eq!(
            generation_plan.catalog.generations[1].basis_file_ids,
            expected_next_basis,
        );

        assert!(
            expected_next_basis
                .iter()
                .all(|file_id| { completed_file_ids.contains(file_id) }),
        );
    }

    #[test]
    fn fresh_resume_rejects_partial_generation() {
        let manifest = vec![
            entry("medium-a.bin", 100, FileClass::Medium),
            entry("medium-b.bin", 100, FileClass::Medium),
        ];

        let transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let mut generation_plan = build_fresh_generation_plan_with_limits(
            &manifest,
            &transfer_plan,
            CatalogLimits {
                generation_target_bytes: 1_000,
                max_catalog_entries: 100,
            },
        )
        .unwrap();

        assert_eq!(generation_plan.catalog.generations.len(), 1,);

        assert_eq!(
            generation_plan.catalog.generations[0].transfer_files.len(),
            2,
        );

        let completed_file_ids =
            BTreeSet::from([generation_plan.catalog.generations[0].transfer_files[0].file_id]);

        let error =
            apply_fresh_resume_prefix(&mut generation_plan, &completed_file_ids).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);

        assert!(error.to_string().contains("only part of fresh generation"),);
    }

    #[test]
    fn fresh_resume_rejects_nonprefix_generation() {
        let manifest = vec![
            entry("medium-a.bin", 100, FileClass::Medium),
            entry("medium-b.bin", 100, FileClass::Medium),
        ];

        let transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let mut generation_plan = build_fresh_generation_plan_with_limits(
            &manifest,
            &transfer_plan,
            CatalogLimits {
                generation_target_bytes: 100,
                max_catalog_entries: 100,
            },
        )
        .unwrap();

        assert_eq!(generation_plan.catalog.generations.len(), 2,);

        let completed_file_ids: BTreeSet<usize> = generation_plan.catalog.generations[1]
            .transfer_files
            .iter()
            .map(|candidate| candidate.file_id)
            .collect();

        let error =
            apply_fresh_resume_prefix(&mut generation_plan, &completed_file_ids).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);

        assert!(error.to_string().contains("contiguous generation prefix"),);
    }

    #[test]
    fn fresh_resume_rebuild_removes_completed_stripe_from_execution() {
        let manifest = vec![
            entry("large.bin", 1_000, FileClass::Large),
            entry("medium.bin", 200, FileClass::Medium),
        ];

        let mut transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let completed_stripe = transfer_plan
            .lanes
            .iter()
            .flatten()
            .find_map(|task| {
                let TransferTask::Stripe {
                    file_id,
                    offset,
                    length,
                } = task
                else {
                    return None;
                };

                Some(ResumeStripe::new(*file_id, *offset, *length).unwrap())
            })
            .unwrap();

        let mut generation_plan = build_fresh_generation_plan_with_limits(
            &manifest,
            &transfer_plan,
            CatalogLimits {
                generation_target_bytes: 2_000,
                max_catalog_entries: 100,
            },
        )
        .unwrap();

        let offered = BTreeSet::from([completed_stripe]);

        let resume_application =
            apply_resume_offer(&mut transfer_plan, &manifest, &offered, &BTreeSet::new()).unwrap();

        assert_eq!(resume_application.stripe_count, 1,);

        rebuild_fresh_generation_execution(&mut generation_plan, &transfer_plan).unwrap();

        assert!(
            generation_plan
                .generation_lanes
                .iter()
                .flatten()
                .flatten()
                .all(|task| {
                    !matches!(
                        task,
                        TransferTask::Stripe {
                            file_id,
                            offset,
                            length,
                        } if *file_id
                            == usize::try_from(
                                completed_stripe.file_id,
                            )
                            .unwrap()
                            && *offset
                                == completed_stripe.offset
                            && *length
                                == completed_stripe.length
                    )
                }),
        );

        assert_eq!(generation_plan.completed_generation_count, 0,);

        assert!(
            generation_plan
                .catalog
                .generations
                .iter()
                .flat_map(|generation| { generation.transfer_files.iter() })
                .any(|candidate| {
                    candidate.file_id == usize::try_from(completed_stripe.file_id).unwrap()
                }),
        );
    }

    #[test]
    fn generation_commit_round_trips() {
        let expected = GenerationCommit {
            generation_index: 7,
            committed_file_ids: vec![2, 4, 6],
            published_file_ids: vec![2, 6],
            evicted_file_ids: vec![0, 1],
        };

        let expected_wire_bytes = generation_commit_wire_bytes(&expected).unwrap();

        let mut bytes = Vec::new();

        write_generation_commit(&mut bytes, &expected).unwrap();

        assert_eq!(u64::try_from(bytes.len()).unwrap(), expected_wire_bytes,);

        let actual = read_generation_commit(&mut Cursor::new(bytes)).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn generation_commit_rejects_duplicate_file_ids() {
        let mut bytes = Vec::new();

        bytes.push(MESSAGE_GENERATION_COMMIT);

        bytes.extend_from_slice(&0_u64.to_be_bytes());

        bytes.extend_from_slice(&2_u32.to_be_bytes());

        bytes.extend_from_slice(&1_u64.to_be_bytes());

        bytes.extend_from_slice(&1_u64.to_be_bytes());

        bytes.extend_from_slice(&0_u32.to_be_bytes());

        bytes.extend_from_slice(&0_u32.to_be_bytes());

        let error = read_generation_commit(&mut Cursor::new(bytes)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        assert!(error.to_string().contains("duplicate file ID 1"),);
    }

    #[test]
    fn generation_commit_must_match_the_deterministic_plan() {
        let manifest = vec![
            entry("medium-a.bin", 100, FileClass::Medium),
            entry("medium-b.bin", 200, FileClass::Medium),
        ];

        let transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let generation_plan = build_fresh_generation_plan_with_limits(
            &manifest,
            &transfer_plan,
            CatalogLimits {
                generation_target_bytes: 1_000,
                max_catalog_entries: 100,
            },
        )
        .unwrap();

        let generation = &generation_plan.catalog.generations[0];

        let expected = expected_generation_commit(generation);

        validate_generation_commit(&expected, generation, manifest.len()).unwrap();

        let mut wrong_index = expected.clone();

        wrong_index.generation_index = wrong_index.generation_index.checked_add(1).unwrap();

        let error =
            validate_generation_commit(&wrong_index, generation, manifest.len()).unwrap_err();

        assert!(error.to_string().contains("expected"),);

        let mut unknown_file = expected.clone();

        unknown_file.committed_file_ids[0] = manifest.len();

        let error =
            validate_generation_commit(&unknown_file, generation, manifest.len()).unwrap_err();

        assert!(error.to_string().contains("unknown file ID"),);

        let mut wrong_publication = expected.clone();

        wrong_publication.published_file_ids.clear();

        let error =
            validate_generation_commit(&wrong_publication, generation, manifest.len()).unwrap_err();

        assert!(error.to_string().contains("published-file list differs"),);
    }

    #[test]
    fn lane_generation_end_round_trips_and_validates_index() {
        let mut generation_bytes = Vec::new();

        write_lane_end(&mut generation_bytes, LaneEnd::Generation(4)).unwrap();

        read_lane_end(
            &mut Cursor::new(generation_bytes.clone()),
            LaneEnd::Generation(4),
        )
        .unwrap();

        let error =
            read_lane_end(&mut Cursor::new(generation_bytes), LaneEnd::Generation(5)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        assert!(error.to_string().contains("expected 5"),);

        let mut stream_bytes = Vec::new();

        write_lane_end(&mut stream_bytes, LaneEnd::Stream).unwrap();

        read_lane_end(&mut Cursor::new(stream_bytes), LaneEnd::Stream).unwrap();
    }

    #[test]
    fn loopback_tiny_only_transfer_uses_one_shot_lane_end() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent = env::temp_dir().join(format!(
            "networkcopy-v10-tiny-only-{}-{unique}",
            process::id(),
        ));

        let source = parent.join("source");

        let destination = parent.join("destination");

        fs::create_dir_all(&source).unwrap();

        fs::write(source.join("tiny-a.txt"), b"protocol v10 tiny file A").unwrap();

        fs::write(source.join("tiny-b.txt"), b"protocol v10 tiny file B").unwrap();

        let transfer_result = run(&source, &destination, 2, 2);

        let cleanup_result = fs::remove_dir_all(&parent);

        let report = transfer_result.unwrap();

        cleanup_result.unwrap();

        assert_eq!(report.files_copied, 2);

        assert_eq!(
            report.bytes_copied,
            u64::try_from(b"protocol v10 tiny file A".len() + b"protocol v10 tiny file B".len(),)
                .unwrap(),
        );

        assert_eq!(report.tiny_files_packed, 2);
    }

    #[test]
    fn tiny_packs_respect_the_payload_target() {
        let tiny_file_size = TINY_PACK_TARGET_BYTES / 32;

        let manifest: Vec<ManifestEntry> = (0..33)
            .map(|index| {
                entry(
                    &format!("tiny-{index}.bin"),
                    tiny_file_size,
                    FileClass::Tiny,
                )
            })
            .collect();

        let plan = build_transfer_plan(&manifest, 2).unwrap();

        let mut pack_payloads = Vec::new();
        let mut packed_files = 0_usize;

        for lane in &plan.lanes {
            for task in lane {
                if let TransferTask::TinyPack {
                    file_ids,
                    total_bytes,
                } = task
                {
                    pack_payloads.push(*total_bytes);
                    packed_files += file_ids.len();
                }
            }
        }

        pack_payloads.sort_unstable();

        assert_eq!(packed_files, 33);
        assert_eq!(pack_payloads, vec![tiny_file_size, TINY_PACK_TARGET_BYTES,]);
    }

    #[test]
    fn digest_mismatch_is_rejected() {
        let actual = [0xA5_u8; 32];
        let expected = [0x5A_u8; 32];

        let error = verify_content_digest("test file", &actual, &expected).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        assert!(error.to_string().contains("BLAKE3 verification failed"));
    }

    #[test]
    fn receiver_derives_session_from_control_handshake() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let address = listener.local_addr().unwrap();

        let session_id = 0x1234_5678_9ABC_DEF0;

        let receiver = thread::spawn(move || accept_session(&listener, None));

        let mut control_stream = TcpStream::connect(address).unwrap();

        control_plane::configure_stream(&control_stream).unwrap();

        control_plane::write_handshake(
            &mut control_stream,
            control_plane::Handshake {
                role: control_plane::ConnectionRole::Control,

                session_id,

                stream_id: 0,

                stream_count: 2,
            },
        )
        .unwrap();

        let mut data_stream_one = TcpStream::connect(address).unwrap();

        control_plane::configure_stream(&data_stream_one).unwrap();

        control_plane::write_handshake(
            &mut data_stream_one,
            control_plane::Handshake {
                role: control_plane::ConnectionRole::Data,

                session_id,

                stream_id: 1,

                stream_count: 2,
            },
        )
        .unwrap();

        let mut data_stream_zero = TcpStream::connect(address).unwrap();

        control_plane::configure_stream(&data_stream_zero).unwrap();

        control_plane::write_handshake(
            &mut data_stream_zero,
            control_plane::Handshake {
                role: control_plane::ConnectionRole::Data,

                session_id,

                stream_id: 0,

                stream_count: 2,
            },
        )
        .unwrap();

        let (accepted_control, accepted_data, accepted_session) = receiver.join().unwrap().unwrap();

        assert_eq!(accepted_session.session_id, session_id);

        assert_eq!(accepted_session.data_stream_count, 2);

        assert_eq!(accepted_data.len(), 2);

        drop(accepted_control);
        drop(accepted_data);
        drop(control_stream);
        drop(data_stream_zero);
        drop(data_stream_one);
    }

    #[test]
    fn sender_retries_until_receiver_starts() {
        let reserved_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let address = reserved_listener.local_addr().unwrap();

        drop(reserved_listener);

        let receiver = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));

            let listener = TcpListener::bind(address).unwrap();

            listener.accept().unwrap().0
        });

        let started = Instant::now();

        let sender =
            connect_with_retry_config(address, Duration::from_secs(2), Duration::from_millis(20))
                .unwrap();

        assert!(started.elapsed() >= Duration::from_millis(100));

        drop(sender);

        let accepted = receiver.join().unwrap();

        drop(accepted);
    }

    #[test]
    fn source_metadata_rejects_size_change() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let path = env::temp_dir().join(format!(
            "networkcopy-source-size-{}-{unique}.bin",
            process::id()
        ));

        fs::write(&path, b"original").unwrap();

        let metadata = fs::metadata(&path).unwrap();

        let entry = ManifestEntry {
            relative_path: PathBuf::from("source.bin"),

            file_size: metadata.len(),

            last_write_time: metadata.last_write_time(),

            file_attributes: metadata.file_attributes(),

            class: FileClass::Tiny,
        };

        fs::write(&path, b"changed-length").unwrap();

        let file = File::open(&path).unwrap();

        let error = validate_source_metadata(&file, &path, &entry).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        assert!(
            error
                .to_string()
                .contains("source size changed after scanning",)
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn source_metadata_rejects_timestamp_change() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let path = env::temp_dir().join(format!(
            "networkcopy-source-time-{}-{unique}.bin",
            process::id()
        ));

        fs::write(&path, b"unchanged-size").unwrap();

        let metadata = fs::metadata(&path).unwrap();

        let entry = ManifestEntry {
            relative_path: PathBuf::from("source.bin"),

            file_size: metadata.len(),

            last_write_time: metadata.last_write_time(),

            file_attributes: metadata.file_attributes(),

            class: FileClass::Tiny,
        };

        file_metadata::restore_file(
            &path,
            entry.last_write_time.checked_add(10_000_000).unwrap(),
            entry.file_attributes,
        )
        .unwrap();

        let file = File::open(&path).unwrap();

        let error = validate_source_metadata(&file, &path, &entry).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        assert!(
            error
                .to_string()
                .contains("source last-write time changed after scanning",)
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn resume_offer_removes_completed_stripe() {
        let manifest = vec![
            entry("large.bin", 1_000, FileClass::Large),
            entry("medium.bin", 300, FileClass::Medium),
        ];

        let mut transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let completed_stripe = ResumeStripe::new(0, 0, 500).unwrap();

        let offered = BTreeSet::from([completed_stripe]);

        let application =
            apply_resume_offer(&mut transfer_plan, &manifest, &offered, &BTreeSet::new()).unwrap();

        assert_eq!(application.stripe_count, 1);

        assert_eq!(application.logical_report.files_copied, 1);

        assert_eq!(application.logical_report.bytes_copied, 500);

        assert_eq!(application.logical_report.data_wire_bytes, 0);

        assert!(!transfer_plan.lanes.iter().flatten().any(|task| {
            matches!(
                task,
                TransferTask::Stripe {
                    file_id: 0,
                    offset: 0,
                    length: 500,
                }
            )
        }));

        assert!(transfer_plan.lanes.iter().flatten().any(|task| {
            matches!(
                task,
                TransferTask::Stripe {
                    file_id: 0,
                    offset: 500,
                    length: 500,
                }
            )
        }));

        assert!(
            transfer_plan
                .lanes
                .iter()
                .flatten()
                .any(|task| { matches!(task, TransferTask::WholeFile { file_id: 1 }) })
        );
    }

    #[test]
    fn unchanged_offer_removes_complete_files_and_repacks_tiny_files() {
        let manifest = vec![
            entry("large.bin", 1_000, FileClass::Large),
            entry("medium.bin", 500, FileClass::Medium),
            entry("tiny-a.bin", 10, FileClass::Tiny),
            entry("tiny-b.bin", 20, FileClass::Tiny),
            entry("tiny-c.bin", 30, FileClass::Tiny),
        ];

        let mut plan = build_transfer_plan(&manifest, 2).unwrap();

        let unchanged = BTreeSet::from([0_usize, 1_usize, 3_usize]);

        let application =
            apply_resume_offer(&mut plan, &manifest, &BTreeSet::new(), &unchanged).unwrap();

        assert_eq!(application.unchanged_file_count, 3,);

        assert_eq!(application.unchanged_bytes, 1_520,);

        assert_eq!(application.logical_report.files_copied, 3,);

        assert_eq!(application.logical_report.bytes_copied, 1_520,);

        let remaining = plan.lanes.iter().flatten().collect::<Vec<_>>();

        assert_eq!(remaining.len(), 1,);

        match remaining[0] {
            TransferTask::TinyPack {
                file_ids,
                total_bytes,
            } => {
                assert_eq!(file_ids, &vec![2, 4],);

                assert_eq!(*total_bytes, 40,);
            }

            task => {
                panic!("unexpected remaining task: {task:?}",);
            }
        }
    }

    #[test]
    fn empty_resume_offer_preserves_tasks() {
        let manifest = vec![
            entry("large.bin", 1_000, FileClass::Large),
            entry("medium.bin", 300, FileClass::Medium),
        ];

        let mut transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let task_count_before = transfer_plan.lanes.iter().map(Vec::len).sum::<usize>();

        let application = apply_resume_offer(
            &mut transfer_plan,
            &manifest,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();

        let task_count_after = transfer_plan.lanes.iter().map(Vec::len).sum::<usize>();

        assert_eq!(task_count_after, task_count_before);

        assert_eq!(application.stripe_count, 0);

        assert_eq!(application.logical_report.files_copied, 0);

        assert_eq!(application.logical_report.bytes_copied, 0);
    }

    #[test]
    fn receiver_ready_round_trips_resume_offer() {
        let expected = ReceiverReady {
            summary: ManifestSummary {
                entries: 7,
                total_file_bytes: 12_345,
                fingerprint: 0x1234_5678_9ABC_DEF0,
            },

            completed_stripes: BTreeSet::from([
                ResumeStripe::new(2, 0, 4096).unwrap(),
                ResumeStripe::new(2, 4096, 2048).unwrap(),
            ]),

            unchanged_file_ids: BTreeSet::from([1_usize, 3_usize, 8_usize]),
        };

        let mut bytes = Vec::new();

        write_receiver_ready(&mut bytes, &expected).unwrap();

        let mut reader = Cursor::new(bytes);

        let actual = read_receiver_ready(&mut reader).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn resume_offer_rejects_unplanned_stripe() {
        let manifest = vec![entry("large.bin", 1_000, FileClass::Large)];

        let transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let valid = BTreeSet::from([ResumeStripe::new(0, 0, 500).unwrap()]);

        validate_resume_offer(&transfer_plan, &valid).unwrap();

        let invalid = BTreeSet::from([ResumeStripe::new(0, 1, 500).unwrap()]);

        let error = validate_resume_offer(&transfer_plan, &invalid).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        assert!(error.to_string().contains("unplanned completed stripe",));
    }

    #[test]
    fn separate_sender_and_receiver_copy_directory() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent =
            env::temp_dir().join(format!("networkcopy-separated-{}-{unique}", process::id()));

        let source = parent.join("source");

        let destination = parent.join("destination");

        fs::create_dir_all(&source).unwrap();

        fs::create_dir_all(&destination).unwrap();

        fs::write(source.join("small.txt"), b"separate sender and receiver").unwrap();

        let medium_contents = vec![0xA5_u8; 300 * 1024];

        fs::write(source.join("medium.bin"), &medium_contents).unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();

        let receiver_address = listener.local_addr().unwrap();

        let receiver_destination = destination.clone();

        let receiver = thread::spawn(move || receive_once(listener, &receiver_destination));

        let sender_report = send(receiver_address, &source, 2, 2).unwrap();

        let receiver_report = receiver.join().unwrap().unwrap();

        assert_eq!(sender_report.files_copied, 2);

        assert_eq!(receiver_report.files_received, 2);

        assert_eq!(sender_report.bytes_copied, receiver_report.bytes_received);

        assert_eq!(
            sender_report.data_wire_bytes,
            receiver_report.data_wire_bytes
        );

        assert_eq!(
            sender_report.process_buffer_bytes,
            sender_report.buffer_bytes_per_peer
        );

        assert_eq!(
            fs::read(destination.join("small.txt",),).unwrap(),
            b"separate sender and receiver"
        );

        assert_eq!(
            fs::read(destination.join("medium.bin",),).unwrap(),
            medium_contents
        );

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn loopback_session_resumes_after_injected_failure() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent = env::temp_dir().join(format!(
            "networkcopy-interruption-{}-{unique}",
            process::id()
        ));

        let source = parent.join("source");

        let destination = parent.join("destination");

        fs::create_dir_all(&source).unwrap();

        let large_contents = vec![0x5A_u8; 64 * 1024 * 1024 + 137];

        fs::write(source.join("large.bin"), &large_contents).unwrap();

        let fault_injection = Arc::new(TransferFault::fail_after_checkpointed_stripes(1).unwrap());

        let interrupted = run_with_fault(&source, &destination, 2, 1, fault_injection);

        assert!(interrupted.is_err());

        assert!(destination.join(JOURNAL_FILE_NAME).exists());

        assert!(!destination.join("large.bin").exists());

        let report = run(&source, &destination, 2, 1).unwrap();

        assert_eq!(report.files_copied, 1);

        assert_eq!(report.resumed_stripes, 1);

        assert_eq!(report.resumed_bytes, large_contents.len() as u64);

        assert!(report.data_wire_bytes < report.bytes_copied);

        assert_eq!(
            fs::read(destination.join("large.bin",),).unwrap(),
            large_contents
        );

        assert!(!destination.join(JOURNAL_FILE_NAME).exists());

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn loopback_update_skips_cdc_file_completed_before_ack() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent = env::temp_dir().join(format!(
            "networkcopy-cdc-interruption-{}-{unique}",
            process::id(),
        ));

        let source = parent.join("source");

        let destination = parent.join("destination");

        fs::create_dir_all(&source).unwrap();

        fs::create_dir_all(&destination).unwrap();

        let basis = vec![0x5A_u8; 8 * 1024 * 1024];

        let insertion = vec![0xC3_u8; 4097];

        let insertion_offset = 4 * 1024 * 1024 + 123;

        let mut candidate = Vec::with_capacity(basis.len() + insertion.len());

        candidate.extend_from_slice(&basis[..insertion_offset]);

        candidate.extend_from_slice(&insertion);

        candidate.extend_from_slice(&basis[insertion_offset..]);

        let source_path = source.join("medium.bin");

        let destination_path = destination.join("medium.bin");

        fs::write(&source_path, &candidate).unwrap();

        fs::write(&destination_path, &basis).unwrap();

        let fault_injection =
            Arc::new(TransferFault::fail_after_reconstructed_cdc_files(1).unwrap());

        let interrupted = run_update_with_fault(&source, &destination, 2, 1, fault_injection);

        assert!(interrupted.is_err(),);

        assert!(destination.join(JOURNAL_FILE_NAME,).exists(),);

        assert_eq!(fs::read(&destination_path,).unwrap(), candidate,);

        let report = run_update(&source, &destination, 2, 1).unwrap();

        assert_eq!(report.files_copied, 1,);

        assert_eq!(report.skipped_files, 1,);

        assert_eq!(report.skipped_bytes, candidate.len() as u64,);

        assert_eq!(report.cdc_offered_files, 0,);

        assert_eq!(report.cdc_files, 0,);

        assert_eq!(fs::read(&destination_path,).unwrap(), candidate,);

        assert!(!destination.join(JOURNAL_FILE_NAME,).exists(),);

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn loopback_update_uses_cdc_for_changed_large_file() {
        let root = temporary_directory("large-cdc-update");

        let source = root.join("source");

        let destination = root.join("destination");

        fs::create_dir_all(&source).unwrap();

        fs::create_dir_all(&destination).unwrap();

        let file_bytes = 72 * 1024 * 1024 + 137;

        let basis = vec![0x5A_u8; file_bytes];

        let mut candidate = basis.clone();

        let changed_offset = 24 * 1024 * 1024;

        candidate[changed_offset..changed_offset + 1024 * 1024].fill(0xC3);

        fs::write(source.join("large.bin"), &candidate).unwrap();

        fs::write(destination.join("large.bin"), &basis).unwrap();

        let report = run_update(&source, &destination, 2, 2).unwrap();

        assert_eq!(report.files_copied, 1,);

        assert_eq!(report.cdc_offered_files, 1,);

        assert_eq!(report.cdc_files, 1,);

        assert_eq!(report.cdc_fallback_files, 0,);

        assert_eq!(report.cdc_logical_bytes, candidate.len() as u64,);

        assert!(report.cdc_reused_bytes > candidate.len() as u64 * 90 / 100,);

        assert!(report.data_wire_bytes < 4 * 1024 * 1024,);

        assert_eq!(report.resumed_stripes, 0,);

        assert_eq!(
            fs::read(destination.join("large.bin",),).unwrap(),
            candidate,
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unprofitable_large_cdc_keeps_striped_fallback() {
        let root = temporary_directory("large-cdc-fallback");

        let source = root.join("source");

        let destination = root.join("destination");

        fs::create_dir_all(&source).unwrap();

        fs::create_dir_all(&destination).unwrap();

        let file_bytes = 64 * 1024 * 1024 + 137;

        let source_bytes = vec![0x11_u8; file_bytes];

        let destination_bytes = vec![0x22_u8; file_bytes];

        fs::write(source.join("large.bin"), &source_bytes).unwrap();

        fs::write(destination.join("large.bin"), &destination_bytes).unwrap();

        let report = run_update(&source, &destination, 2, 2).unwrap();

        assert_eq!(report.files_copied, 1,);

        assert_eq!(report.cdc_offered_files, 1,);

        assert_eq!(report.cdc_files, 0,);

        assert_eq!(report.cdc_fallback_files, 1,);

        assert_eq!(report.cdc_plan_wire_bytes, 0,);

        assert!(report.cdc_index_wire_bytes > 0,);

        assert_eq!(
            fs::read(destination.join("large.bin",),).unwrap(),
            source_bytes,
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loopback_session_resumes_verified_stripe() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent = env::temp_dir().join(format!(
            "networkcopy-resume-loopback-{}-{unique}",
            process::id()
        ));

        let source = parent.join("source");

        let destination = parent.join("destination");

        fs::create_dir_all(&source).unwrap();

        let mut large_contents = vec![0_u8; 64 * 1024 * 1024 + 137];

        let mut state = 0x1234_5678_u32;

        for byte in &mut large_contents {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;

            *byte = state as u8;
        }

        fs::write(source.join("large.bin"), &large_contents).unwrap();

        let medium_contents = vec![0xA5_u8; 300 * 1024];

        fs::write(source.join("medium.bin"), &medium_contents).unwrap();

        let scan = manifest_scan::run(&source, 2).unwrap();

        let summary = control_plane::summarize_manifest(&scan.manifest).unwrap();

        let large_file_id = scan
            .manifest
            .iter()
            .position(|entry| entry.relative_path == Path::new("large.bin"))
            .unwrap();

        let transfer_plan = build_transfer_plan(&scan.manifest, 2).unwrap();

        let (stripe_offset, stripe_length) = transfer_plan
            .lanes
            .iter()
            .flatten()
            .find_map(|task| {
                let TransferTask::Stripe {
                    file_id,
                    offset,
                    length,
                } = task
                else {
                    return None;
                };

                if *file_id == large_file_id && *offset == 0 {
                    Some((*offset, *length))
                } else {
                    None
                }
            })
            .unwrap();

        assert_eq!(stripe_offset, 0);

        fs::create_dir_all(&destination).unwrap();

        fs::write(destination.join("medium.bin"), b"stale previous copy").unwrap();

        let large_final = destination.join("large.bin");

        let large_temporary = temporary_path(&large_final, large_file_id);

        let mut partial = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&large_temporary)
            .unwrap();

        partial.set_len(large_contents.len() as u64).unwrap();

        partial
            .write_all(&large_contents[..stripe_length as usize])
            .unwrap();

        partial.sync_all().unwrap();
        drop(partial);

        let mut journal = ResumeJournal::new(summary.fingerprint, 2).unwrap();

        journal.mark_completed(
            ResumeStripe::new(large_file_id, stripe_offset, stripe_length).unwrap(),
        );

        journal.save_atomic(&destination).unwrap();

        let report = run(&source, &destination, 2, 2).unwrap();

        assert_eq!(report.files_copied, 2);

        assert_eq!(report.resumed_stripes, 1);

        assert_eq!(report.resumed_bytes, stripe_length);

        assert!(report.data_wire_bytes < report.bytes_copied);

        assert_eq!(
            fs::read(destination.join("large.bin",),).unwrap(),
            large_contents
        );

        assert_eq!(
            fs::read(destination.join("medium.bin",),).unwrap(),
            medium_contents
        );

        assert!(!destination.join(JOURNAL_FILE_NAME).exists());

        assert!(!large_temporary.exists());

        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn fresh_loopback_reuses_exact_medium_duplicates() {
        let root = temporary_directory("fresh-exact-reuse");

        let source = root.join("source");

        let destination = root.join("destination");

        fs::create_dir_all(&source).unwrap();

        let mut shared = vec![0_u8; 2 * 1024 * 1024 + 137];

        let mut state = 0x1234_5678_u32;

        for byte in &mut shared {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;

            *byte = state as u8;
        }

        let mut unique = vec![0_u8; shared.len()];

        let mut state = 0xA5A5_5A5A_u32;

        for byte in &mut unique {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;

            *byte = state as u8;
        }

        fs::write(source.join("duplicate-a.bin"), &shared).unwrap();

        fs::write(source.join("duplicate-b.bin"), &shared).unwrap();

        fs::write(source.join("duplicate-c.bin"), &shared).unwrap();

        fs::write(source.join("unique.bin"), &unique).unwrap();

        let report = run(&source, &destination, 2, 2).unwrap();

        assert_eq!(report.files_copied, 4,);

        assert_eq!(report.bytes_copied, (shared.len() * 4) as u64,);

        assert_eq!(report.exact_reused_files, 2,);

        assert_eq!(report.exact_reused_bytes, (shared.len() * 2) as u64,);

        assert!(report.exact_reuse_plan_wire_bytes > 0,);

        assert!(report.exact_reuse_plan_wire_bytes < report.exact_reused_bytes,);

        assert_eq!(report.cdc_offered_files, 0,);

        assert!(report.data_wire_bytes < report.bytes_copied * 3 / 4,);

        assert_eq!(
            fs::read(destination.join("duplicate-a.bin",),).unwrap(),
            shared,
        );

        assert_eq!(
            fs::read(destination.join("duplicate-b.bin",),).unwrap(),
            shared,
        );

        assert_eq!(
            fs::read(destination.join("duplicate-c.bin",),).unwrap(),
            shared,
        );

        assert_eq!(fs::read(destination.join("unique.bin",),).unwrap(), unique,);

        assert!(!destination.join(JOURNAL_FILE_NAME).exists(),);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_loopback_uses_committed_medium_file_as_session_cdc_basis() {
        let root = temporary_directory("fresh-session-cdc");

        let source = root.join("source");

        let destination = root.join("destination");

        fs::create_dir_all(&source).unwrap();

        let basis = deterministic_test_bytes(8 * 1024 * 1024, 0x1234_5678_90AB_CDEF);

        let insertion = deterministic_test_bytes(4097, 0xCAFE_BABE_DEAD_BEEF);

        let insertion_offset = 4 * 1024 * 1024 + 123;

        let mut target = Vec::with_capacity(basis.len() + insertion.len());

        target.extend_from_slice(&basis[..insertion_offset]);

        target.extend_from_slice(&insertion);

        target.extend_from_slice(&basis[insertion_offset..]);

        fs::write(source.join("00-basis.bin"), &basis).unwrap();

        fs::write(source.join("01-target.bin"), &target).unwrap();

        let catalog_limits = CatalogLimits {
            generation_target_bytes: basis.len() as u64,
            ..CatalogLimits::default()
        };

        let fault = TransferFault::with_catalog_limits(catalog_limits).unwrap();

        let report = run_with_fault(&source, &destination, 2, 2, Arc::new(fault)).unwrap();

        assert_eq!(report.files_copied, 2);

        assert_eq!(report.bytes_copied, (basis.len() + target.len()) as u64,);

        assert_eq!(report.exact_reused_files, 0);

        assert_eq!(report.cdc_offered_files, 1);

        assert_eq!(report.cdc_files, 1);

        assert_eq!(report.cdc_fallback_files, 0);

        assert_eq!(report.cdc_logical_bytes, target.len() as u64);

        assert!(report.cdc_reused_bytes > target.len() as u64 * 90 / 100,);

        assert!(report.cdc_literal_bytes < 1024 * 1024);

        assert_eq!(report.cdc_index_wire_bytes, 0);

        assert!(report.cdc_plan_wire_bytes > 0);

        assert!(report.data_wire_bytes < report.bytes_copied * 3 / 4,);

        assert_eq!(fs::read(destination.join("00-basis.bin")).unwrap(), basis,);

        assert_eq!(fs::read(destination.join("01-target.bin")).unwrap(), target,);

        assert!(!destination.join(JOURNAL_FILE_NAME).exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loopback_session_copies_complete_directory() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        let parent = env::temp_dir().join(format!(
            "networkcopy-multistream-{}-{unique}",
            process::id()
        ));

        let source = parent.join("source");
        let destination = parent.join("destination");
        let nested = source.join("árvíztűrő");

        fs::create_dir_all(&nested).unwrap();
        fs::write(source.join("empty.bin"), []).unwrap();
        fs::write(source.join("small.txt"), b"NetworkCopy Speed Edition").unwrap();
        let small_last_write_time = 132_537_600_123_456_789;

        file_metadata::restore_file(
            &source.join("small.txt"),
            small_last_write_time,
            FILE_ATTRIBUTE_HIDDEN,
        )
        .unwrap();

        fs::write(nested.join("medium.bin"), vec![0xA5_u8; 300 * 1024]).unwrap();

        fs::write(
            nested.join("large.bin"),
            vec![0x5A_u8; 64 * 1024 * 1024 + 137],
        )
        .unwrap();

        let transfer_result = run(&source, &destination, 4, 3);

        let report = transfer_result.unwrap();

        assert_eq!(report.files_copied, 4);

        assert!(!destination.join(JOURNAL_FILE_NAME).exists());

        assert!(report.process_buffer_bytes <= report.transfer_buffer_budget_bytes);

        assert_eq!(
            report.buffer_bytes_per_peer * 2,
            report.process_buffer_bytes
        );

        assert!(report.compressed_records > 0);

        assert!(report.data_wire_bytes < report.bytes_copied);

        assert_eq!(
            fs::read(source.join("empty.bin")).unwrap(),
            fs::read(destination.join("empty.bin")).unwrap()
        );

        assert_eq!(
            fs::read(source.join("small.txt")).unwrap(),
            fs::read(destination.join("small.txt")).unwrap()
        );

        let copied_small_metadata = fs::metadata(destination.join("small.txt")).unwrap();

        assert_eq!(
            copied_small_metadata.last_write_time(),
            small_last_write_time
        );

        assert_ne!(
            copied_small_metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN,
            0
        );

        assert_eq!(
            fs::read(nested.join("medium.bin")).unwrap(),
            fs::read(destination.join("árvíztűrő").join("medium.bin")).unwrap()
        );

        assert_eq!(
            fs::read(nested.join("large.bin")).unwrap(),
            fs::read(destination.join("árvíztűrő").join("large.bin")).unwrap()
        );

        fs::remove_dir_all(parent).unwrap();
    }

    fn deterministic_test_bytes(length: usize, mut state: u64) -> Vec<u8> {
        let mut bytes = vec![0_u8; length];

        for byte in &mut bytes {
            state ^= state << 13;

            state ^= state >> 7;

            state ^= state << 17;

            *byte = (state >> 24) as u8;
        }

        bytes
    }

    fn entry(path: &str, file_size: u64, class: FileClass) -> ManifestEntry {
        ManifestEntry {
            relative_path: PathBuf::from(path),
            file_size,
            last_write_time: 0,
            file_attributes: 0,
            class,
        }
    }

    fn temporary_directory(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        env::temp_dir().join(format!("networkcopy-{name}-{}-{unique}", process::id()))
    }

    #[test]
    fn update_preparation_preserves_old_files_and_offers_exact_matches() {
        let root = temporary_directory("update-preparation");

        fs::create_dir_all(&root).unwrap();

        let unchanged_path = root.join("unchanged.bin");

        let changed_path = root.join("changed.bin");

        let large_path = root.join("large.bin");

        let unrelated_path = root.join("unrelated.bin");

        fs::write(&unchanged_path, b"unchanged").unwrap();

        fs::write(&changed_path, b"old changed contents").unwrap();

        fs::write(&large_path, vec![0x11_u8; 1024]).unwrap();

        fs::write(&unrelated_path, b"leave me alone").unwrap();

        let unchanged_metadata = fs::metadata(&unchanged_path).unwrap();

        let changed_metadata = fs::metadata(&changed_path).unwrap();

        let large_metadata = fs::metadata(&large_path).unwrap();

        let manifest = vec![
            ManifestEntry {
                relative_path: PathBuf::from("unchanged.bin"),

                file_size: unchanged_metadata.len(),

                last_write_time: unchanged_metadata.last_write_time(),

                file_attributes: unchanged_metadata.file_attributes(),

                class: FileClass::Tiny,
            },
            ManifestEntry {
                relative_path: PathBuf::from("changed.bin"),

                file_size: changed_metadata.len(),

                last_write_time: changed_metadata.last_write_time().checked_add(1).unwrap(),

                file_attributes: changed_metadata.file_attributes(),

                class: FileClass::Medium,
            },
            ManifestEntry {
                relative_path: PathBuf::from("missing.bin"),

                file_size: 123,

                last_write_time: 456,

                file_attributes: 0,

                class: FileClass::Tiny,
            },
            ManifestEntry {
                relative_path: PathBuf::from("large.bin"),

                file_size: large_metadata.len(),

                last_write_time: large_metadata.last_write_time().checked_add(1).unwrap(),

                file_attributes: large_metadata.file_attributes(),

                class: FileClass::Large,
            },
        ];

        let summary = ManifestSummary {
            entries: manifest.len() as u64,

            total_file_bytes: manifest.iter().map(|entry| entry.file_size).sum(),

            fingerprint: 0x1234_5678_9ABC_DEF0,
        };

        let transfer_plan = build_transfer_plan(&manifest, 2).unwrap();

        let verified_unchanged_file_ids = BTreeSet::from([0_usize]);

        let prepared = prepare_destination(
            &root,
            &manifest,
            summary,
            2,
            &transfer_plan,
            DestinationMode::UpdateVerified,
            Some(&verified_unchanged_file_ids),
        )
        .unwrap();

        assert_eq!(prepared.unchanged_file_ids, BTreeSet::from([0_usize]),);

        assert_eq!(fs::read(&changed_path).unwrap(), b"old changed contents",);

        assert_eq!(fs::read(&unrelated_path).unwrap(), b"leave me alone",);

        let large_temporary = temporary_path(&large_path, 3);

        assert_eq!(fs::metadata(large_temporary).unwrap().len(), 1024,);

        assert!(root.join(JOURNAL_FILE_NAME).exists());

        drop(prepared);

        fs::remove_dir_all(root).unwrap();
    }

    fn temporary_root(name: &str) -> PathBuf {
        temporary_directory(name)
    }

    #[test]
    fn update_finalization_replaces_existing_large_file() {
        let root = temporary_root("update-large-finalization");

        fs::create_dir_all(&root).unwrap();

        let manifest = vec![entry("large.bin", 8, FileClass::Large)];

        let final_path = root.join("large.bin");

        let temporary_path = temporary_path(&final_path, 0);

        fs::write(&final_path, b"old-data").unwrap();

        fs::write(&temporary_path, b"new-data").unwrap();

        finalize_large_files(&root, &manifest, DestinationMode::UpdateVerified).unwrap();

        assert_eq!(fs::read(&final_path).unwrap(), b"new-data",);

        assert!(!temporary_path.try_exists().unwrap(),);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_finalization_rejects_existing_final_and_temporary() {
        let root = temporary_root("fresh-large-finalization");

        fs::create_dir_all(&root).unwrap();

        let manifest = vec![entry("large.bin", 8, FileClass::Large)];

        let final_path = root.join("large.bin");

        let temporary_path = temporary_path(&final_path, 0);

        fs::write(&final_path, b"old-data").unwrap();

        fs::write(&temporary_path, b"new-data").unwrap();

        let error = finalize_large_files(&root, &manifest, DestinationMode::Fresh).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);

        assert!(error.to_string().contains("both final and temporary",),);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generation_commit_is_persisted_before_acknowledgement() {
        let root = temporary_root("persist-generation-commit");

        fs::create_dir_all(&root).unwrap();

        let journal = ResumeJournal::new(0x1234_5678_9ABC_DEF0, 2).unwrap();
        journal.save_atomic(&root).unwrap();

        let journal = Mutex::new(journal);

        let commit = GenerationCommit {
            generation_index: 0,
            committed_file_ids: vec![2, 4, 7],
            published_file_ids: vec![2, 4],
            evicted_file_ids: Vec::new(),
        };

        persist_generation_commit(&root, &journal, &commit, &TransferFault::disabled()).unwrap();

        let loaded = ResumeJournal::load_existing(&root, 0x1234_5678_9ABC_DEF0, 2).unwrap();

        assert_eq!(
            loaded.completed_file_ids().collect::<Vec<_>>(),
            vec![2, 4, 7],
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn crash_after_persisted_generation_keeps_durable_file_ids() {
        let root = temporary_root("crash-after-persisted-generation");

        fs::create_dir_all(&root).unwrap();

        let journal = ResumeJournal::new(0x1234_5678_9ABC_DEF0, 2).unwrap();
        journal.save_atomic(&root).unwrap();

        let journal = Mutex::new(journal);

        let commit = GenerationCommit {
            generation_index: 0,
            committed_file_ids: vec![1, 3, 5],
            published_file_ids: vec![1, 3, 5],
            evicted_file_ids: Vec::new(),
        };

        let fault = TransferFault::fail_after_persisted_generations(1).unwrap();

        let error = persist_generation_commit(&root, &journal, &commit, &fault).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);

        assert!(error.to_string().contains("persisted generation"),);

        let loaded = ResumeJournal::load_existing(&root, 0x1234_5678_9ABC_DEF0, 2).unwrap();

        assert_eq!(
            loaded.completed_file_ids().collect::<Vec<_>>(),
            vec![1, 3, 5],
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_resume_preserves_fast_validated_committed_file() {
        let root = temporary_root("fresh-resume-preserves-committed");

        let source_root = root.join("source");
        let destination_root = root.join("destination");

        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();

        let source_path = source_root.join("basis.bin");
        let destination_path = destination_root.join("basis.bin");

        let contents = vec![0xA5_u8; 2 * 1024 * 1024];

        fs::write(&source_path, &contents).unwrap();

        let scan = crate::manifest_scan::run(&source_root, 1).unwrap();

        assert_eq!(scan.manifest.len(), 1);

        let summary = control_plane::summarize_manifest(&scan.manifest).unwrap();

        let entry = &scan.manifest[0];

        assert_eq!(entry.class, FileClass::Medium);

        fs::copy(&source_path, &destination_path).unwrap();

        file_metadata::restore_file(
            &destination_path,
            entry.last_write_time,
            entry.file_attributes,
        )
        .unwrap();

        let transfer_plan = build_transfer_plan(&scan.manifest, 1).unwrap();

        let mut journal = ResumeJournal::new(summary.fingerprint, 1).unwrap();

        journal.mark_file_completed(0);
        journal.save_atomic(&destination_root).unwrap();

        let stale_temporary_path = temporary_path(&destination_path, 0);

        fs::write(&stale_temporary_path, b"stale temporary data").unwrap();

        let prepared = prepare_destination(
            &destination_root,
            &scan.manifest,
            summary,
            1,
            &transfer_plan,
            super::DestinationMode::Fresh,
            None,
        )
        .unwrap();

        assert_eq!(
            prepared.unchanged_file_ids,
            std::collections::BTreeSet::from([0]),
        );

        assert_eq!(
            prepared.journal.completed_file_ids().collect::<Vec<_>>(),
            vec![0],
        );

        assert!(destination_path.exists());
        assert!(!stale_temporary_path.exists());

        assert_eq!(fs::read(&destination_path).unwrap(), contents,);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_resume_rejects_modified_committed_file() {
        let root = temporary_root("fresh-resume-rejects-modified");

        let source_root = root.join("source");
        let destination_root = root.join("destination");

        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();

        let source_path = source_root.join("basis.bin");
        let destination_path = destination_root.join("basis.bin");

        fs::write(&source_path, vec![0x5A_u8; 2 * 1024 * 1024]).unwrap();

        let scan = crate::manifest_scan::run(&source_root, 1).unwrap();

        let summary = control_plane::summarize_manifest(&scan.manifest).unwrap();

        let transfer_plan = build_transfer_plan(&scan.manifest, 1).unwrap();

        fs::copy(&source_path, &destination_path).unwrap();

        let mut journal = ResumeJournal::new(summary.fingerprint, 1).unwrap();

        journal.mark_file_completed(0);
        journal.save_atomic(&destination_root).unwrap();

        let file = OpenOptions::new()
            .write(true)
            .open(&destination_path)
            .unwrap();

        file.set_len(1024).unwrap();
        drop(file);

        let error = prepare_destination(
            &destination_root,
            &scan.manifest,
            summary,
            1,
            &transfer_plan,
            super::DestinationMode::Fresh,
            None,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        assert!(error.to_string().contains("journaled committed file"),);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transfer_ack_round_trips_tiny_write_workers() {
        let expected = TransferAck {
            files_copied: 10_000,
            bytes_copied: 1_920_000,
            data_wire_bytes: 1_430_000,
            compressed_records: 2,
            cdc: Default::default(),
            tiny_materialization_workers: 2,
            tiny_pack_count: 3,
            compressed_tiny_pack_count: 2,
            raw_tiny_pack_count: 1,
            tiny_files_packed: 10_000,
            tiny_bytes_packed: 1_920_000,
            tiny_pack_wire_bytes: 1_430_000,
        };

        let mut bytes = Vec::new();

        write_transfer_ack(&mut bytes, expected).unwrap();

        let actual = read_transfer_ack(&mut Cursor::new(bytes)).unwrap();

        assert_eq!(actual, expected);
    }
}
