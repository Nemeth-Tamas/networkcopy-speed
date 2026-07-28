mod adaptive_compression;
mod compression_probe;
mod content_hash;
mod control_plane;
mod copy_bench;
mod file_metadata;
mod iocp_copy;
mod iocp_file_probe;
mod iocp_probe;
mod manifest_scan;
mod multistream_copy;
mod network_calibration;
mod pipeline_bench;
mod resume_state;
mod striped_file;
mod transfer_memory;

use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os();
    let program = arguments
        .next()
        .unwrap_or_else(|| OsString::from("networkcopy-speed"));

    let Some(command) = arguments.next() else {
        print_usage(&program);
        return Ok(());
    };

    match command.to_string_lossy().as_ref() {
        "bench-network-matrix-send" => run_network_matrix_send(&mut arguments),
        "bench-network-matrix-receive" => run_network_matrix_receive(&mut arguments),
        "bench-network-send" => run_network_bench_send(&mut arguments),
        "bench-network-receive" => run_network_bench_receive(&mut arguments),
        "send" => run_send(&mut arguments),
        "receive" => run_receive(&mut arguments),
        "bench-copy" => run_bench_copy(&mut arguments),
        "bench-hash" => run_hash_bench(&mut arguments),
        "probe-compression" => run_compression_probe(&mut arguments),
        "bench-pipeline" => run_bench_pipeline(&mut arguments),
        "probe-iocp" => run_iocp_probe(&mut arguments),
        "probe-overlapped-read" => run_overlapped_read_probe(&mut arguments),
        "bench-iocp-copy" => run_iocp_copy_bench(&mut arguments),
        "bench-scan" => run_manifest_scan_bench(&mut arguments),
        "probe-control" => run_control_plane_probe(&mut arguments),
        "bench-multistream-copy" => run_multistream_copy_bench(&mut arguments),
        "bench-striped-file" => run_striped_file_bench(&mut arguments),
        "help" | "--help" | "-h" => {
            print_usage(&program);
            Ok(())
        }
        unknown => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown command: {unknown}"),
        )
        .into()),
    }
}

fn run_compression_probe(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source = PathBuf::from(required_argument(arguments, "source path")?);

    let level = match arguments.next() {
        Some(value) => parse_compression_level(&value)?,

        None => compression_probe::DEFAULT_LEVEL,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    println!("NetworkCopy Speed Edition adaptive compression probe");
    println!("  Source: {}", source.display());
    println!("  Zstandard level: {level}");
    println!();

    let report = compression_probe::run(&source, level)?;

    report.print();
    Ok(())
}

fn run_hash_bench(arguments: &mut impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let source = PathBuf::from(required_argument(arguments, "source path")?);

    let buffer_mib = match arguments.next() {
        Some(value) => parse_buffer_mib(&value)?,
        None => content_hash::DEFAULT_BUFFER_MIB,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    println!("NetworkCopy Speed Edition BLAKE3 benchmark");
    println!("  Source: {}", source.display());
    println!("  Buffer: {buffer_mib} MiB");
    println!();

    let report = content_hash::run(&source, buffer_mib)?;

    report.print();
    Ok(())
}

fn run_bench_copy(arguments: &mut impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let source = PathBuf::from(required_argument(arguments, "source path")?);
    let destination = PathBuf::from(required_argument(arguments, "destination path")?);

    let buffer_mib = match arguments.next() {
        Some(value) => parse_buffer_mib(&value)?,
        None => copy_bench::DEFAULT_BUFFER_MIB,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    println!("NetworkCopy Speed Edition local-copy baseline");
    println!("  Source:      {}", source.display());
    println!("  Destination: {}", destination.display());
    println!("  Buffer:      {buffer_mib} MiB");
    println!();

    let report = copy_bench::run(&source, &destination, buffer_mib)?;
    report.print();

    Ok(())
}

fn run_bench_pipeline(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source = PathBuf::from(required_argument(arguments, "source path")?);
    let destination = PathBuf::from(required_argument(arguments, "destination path")?);

    let chunk_mib = match arguments.next() {
        Some(value) => parse_buffer_mib(&value)?,
        None => pipeline_bench::DEFAULT_CHUNK_MIB,
    };

    let buffer_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,
        None => pipeline_bench::DEFAULT_BUFFER_COUNT,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    pipeline_bench::validate_config(chunk_mib, buffer_count)?;

    println!("NetworkCopy Speed Edition buffered pipeline");
    println!("  Source:      {}", source.display());
    println!("  Destination: {}", destination.display());
    println!("  Chunk:       {chunk_mib} MiB");
    println!("  Buffers:     {buffer_count}");
    println!();

    let report = pipeline_bench::run(&source, &destination, chunk_mib, buffer_count)?;

    report.print();

    Ok(())
}

fn run_iocp_probe(arguments: &mut impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    println!("NetworkCopy Speed Edition native Windows IOCP probe");
    println!();

    let report = iocp_probe::run()?;
    report.print();

    Ok(())
}

fn run_overlapped_read_probe(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source = PathBuf::from(required_argument(arguments, "source path")?);

    let read_mib = match arguments.next() {
        Some(value) => parse_buffer_mib(&value)?,
        None => iocp_file_probe::DEFAULT_READ_MIB,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    println!("NetworkCopy Speed Edition native overlapped read probe");
    println!("  Source: {}", source.display());
    println!("  Buffer: {read_mib} MiB");
    println!();

    let report = iocp_file_probe::run(&source, read_mib)?;
    report.print();

    Ok(())
}

fn run_iocp_copy_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source = PathBuf::from(required_argument(arguments, "source path")?);

    let destination = PathBuf::from(required_argument(arguments, "destination path")?);

    let chunk_mib = match arguments.next() {
        Some(value) => parse_buffer_mib(&value)?,
        None => iocp_copy::DEFAULT_CHUNK_MIB,
    };

    let operation_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,
        None => iocp_copy::DEFAULT_OPERATION_COUNT,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    pipeline_bench::validate_config(chunk_mib, operation_count)?;

    println!("NetworkCopy Speed Edition native IOCP copy benchmark");
    println!("  Source:      {}", source.display());
    println!("  Destination: {}", destination.display());
    println!("  Chunk:       {chunk_mib} MiB");
    println!("  Operations:  {operation_count}");
    println!();

    let report = iocp_copy::run(&source, &destination, chunk_mib, operation_count)?;

    report.print();

    Ok(())
}

fn run_manifest_scan_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(required_argument(arguments, "root directory")?);

    let worker_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,
        None => manifest_scan::default_worker_count(),
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    manifest_scan::validate_worker_count(worker_count)?;

    println!("NetworkCopy Speed Edition parallel manifest benchmark");
    println!("  Root:    {}", root.display());
    println!("  Workers: {worker_count}");
    println!();

    let result = manifest_scan::run(&root, worker_count)?;
    result.report.print();

    Ok(())
}

fn run_control_plane_probe(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(required_argument(arguments, "root directory")?);

    let worker_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,
        None => manifest_scan::default_worker_count(),
    };

    let data_stream_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,
        None => control_plane::DEFAULT_DATA_STREAMS,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    manifest_scan::validate_worker_count(worker_count)?;
    control_plane::validate_data_stream_count(data_stream_count)?;

    println!("NetworkCopy Speed Edition TCP control-plane probe");
    println!("  Root:         {}", root.display());
    println!("  Scan workers: {worker_count}");
    println!("  Data streams: {data_stream_count}");
    println!();

    let report = control_plane::run(&root, worker_count, data_stream_count)?;

    report.print();

    Ok(())
}

fn run_network_matrix_send(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let receiver_address = parse_socket_address(
        &required_argument(arguments, "receiver address")?,
        "receiver address",
    )?;

    let total_mib = match arguments.next() {
        Some(value) => parse_u64_count(&value, "total MiB")?,

        None => network_calibration::DEFAULT_TOTAL_MIB,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    let total_bytes = network_calibration::bytes_from_mib(total_mib)?;

    println!("NetworkCopy Speed Edition raw TCP path matrix sender");

    println!("  Receiver:     {receiver_address}");

    println!("  Payload/run:  {total_mib} MiB");

    println!("  Stream tests: 1, 2, 4, 8");

    println!("  Source:       generated memory buffers");

    println!();

    let report = network_calibration::send_matrix(receiver_address, total_bytes)?;

    report.print("send");

    Ok(())
}

fn run_network_matrix_receive(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let bind_address = parse_socket_address(
        &required_argument(arguments, "bind address")?,
        "bind address",
    )?;

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    let listener = TcpListener::bind(bind_address)?;

    println!("NetworkCopy Speed Edition raw TCP path matrix receiver");

    println!("  Listening:    {}", listener.local_addr()?);

    println!("  Stream tests: 1, 2, 4, 8");

    println!("  Destination:  discarded memory buffers");

    println!("  Mode:         four calibrations, then exit");

    println!();

    let report = network_calibration::receive_matrix(listener)?;

    report.print("receive");

    Ok(())
}

fn run_network_bench_send(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let receiver_address = parse_socket_address(
        &required_argument(arguments, "receiver address")?,
        "receiver address",
    )?;

    let total_mib = match arguments.next() {
        Some(value) => parse_u64_count(&value, "total MiB")?,

        None => network_calibration::DEFAULT_TOTAL_MIB,
    };

    let data_stream_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,

        None => network_calibration::DEFAULT_DATA_STREAMS,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    let total_bytes = network_calibration::bytes_from_mib(total_mib)?;

    control_plane::validate_data_stream_count(data_stream_count)?;

    println!("NetworkCopy Speed Edition raw TCP sender");

    println!("  Receiver:     {receiver_address}");

    println!("  Payload:      {total_mib} MiB");

    println!("  Data streams: {data_stream_count}");

    println!("  Source:       generated memory buffers");

    println!();

    let report = network_calibration::send(receiver_address, total_bytes, data_stream_count)?;

    report.print("send");

    Ok(())
}

fn run_network_bench_receive(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let bind_address = parse_socket_address(
        &required_argument(arguments, "bind address")?,
        "bind address",
    )?;

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    let listener = TcpListener::bind(bind_address)?;

    println!("NetworkCopy Speed Edition raw TCP receiver");

    println!("  Listening:    {}", listener.local_addr()?);

    println!("  Destination:  discarded memory buffers");

    println!("  Mode:         one calibration, then exit");

    println!();

    let report = network_calibration::receive_once(listener)?;

    report.print("receive");

    Ok(())
}

fn run_send(arguments: &mut impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let receiver_address = parse_socket_address(
        &required_argument(arguments, "receiver address")?,
        "receiver address",
    )?;

    let source_root = PathBuf::from(required_argument(arguments, "source root directory")?);

    let worker_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,

        None => manifest_scan::default_worker_count(),
    };

    let data_stream_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,

        None => multistream_copy::DEFAULT_DATA_STREAMS,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    manifest_scan::validate_worker_count(worker_count)?;

    control_plane::validate_data_stream_count(data_stream_count)?;

    println!("NetworkCopy Speed Edition sender");

    println!("  Receiver:     {receiver_address}");

    println!("  Source:       {}", source_root.display());

    println!("  Scan workers: {worker_count}");

    println!("  Data streams: {data_stream_count}");

    println!();

    let report = multistream_copy::send(
        receiver_address,
        &source_root,
        worker_count,
        data_stream_count,
    )?;

    report.print();

    Ok(())
}

fn run_receive(arguments: &mut impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let bind_address = parse_socket_address(
        &required_argument(arguments, "bind address")?,
        "bind address",
    )?;

    let destination_root =
        PathBuf::from(required_argument(arguments, "destination root directory")?);

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    let listener = TcpListener::bind(bind_address)?;

    let local_address = listener.local_addr()?;

    println!("NetworkCopy Speed Edition receiver");

    println!("  Listening:    {local_address}");

    println!("  Destination:  {}", destination_root.display());

    println!("  Mode:         one transfer, then exit");

    println!();

    let report = multistream_copy::receive_once(listener, &destination_root)?;

    report.print();

    Ok(())
}

fn run_multistream_copy_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source_root = PathBuf::from(required_argument(arguments, "source root directory")?);

    let destination_root =
        PathBuf::from(required_argument(arguments, "destination root directory")?);

    let worker_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,
        None => manifest_scan::default_worker_count(),
    };

    let data_stream_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,
        None => multistream_copy::DEFAULT_DATA_STREAMS,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    manifest_scan::validate_worker_count(worker_count)?;

    control_plane::validate_data_stream_count(data_stream_count)?;

    println!("NetworkCopy Speed Edition multistream TCP copy");
    println!("  Source:       {}", source_root.display());
    println!("  Destination:  {}", destination_root.display());
    println!("  Scan workers: {worker_count}");
    println!("  Data streams: {data_stream_count}");
    println!();

    let report = multistream_copy::run(
        &source_root,
        &destination_root,
        worker_count,
        data_stream_count,
    )?;

    report.print();

    Ok(())
}

fn run_striped_file_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source = PathBuf::from(required_argument(arguments, "source file")?);

    let destination = PathBuf::from(required_argument(arguments, "destination file")?);

    let data_stream_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,
        None => striped_file::DEFAULT_DATA_STREAMS,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    control_plane::validate_data_stream_count(data_stream_count)?;

    println!("NetworkCopy Speed Edition striped TCP file copy");
    println!("  Source:       {}", source.display());
    println!("  Destination:  {}", destination.display());
    println!("  Data streams: {data_stream_count}");
    println!();

    let report = striped_file::run(&source, &destination, data_stream_count)?;

    report.print();
    Ok(())
}

fn parse_socket_address(value: &OsStr, description: &str) -> io::Result<SocketAddr> {
    let value = value.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} must contain valid Unicode"),
        )
    })?;

    value.parse::<SocketAddr>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {description} {value:?}: {error}"),
        )
    })
}

fn required_argument(
    arguments: &mut impl Iterator<Item = OsString>,
    description: &str,
) -> io::Result<OsString> {
    arguments.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("missing required {description}"),
        )
    })
}

fn parse_compression_level(value: &OsStr) -> io::Result<i32> {
    let value = value.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "compression level must contain valid Unicode digits",
        )
    })?;

    let level = value.parse::<i32>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid compression level {value:?}: {error}"),
        )
    })?;

    compression_probe::validate_level(level)?;
    Ok(level)
}

fn parse_buffer_mib(value: &OsStr) -> io::Result<usize> {
    let value = value.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "buffer size must contain valid Unicode digits",
        )
    })?;

    let buffer_mib = value.parse::<usize>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid buffer size {value:?}: {error}"),
        )
    })?;

    copy_bench::buffer_bytes_from_mib(buffer_mib)?;
    Ok(buffer_mib)
}

fn parse_u64_count(value: &OsStr, description: &str) -> io::Result<u64> {
    let value = value.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} must contain valid Unicode digits"),
        )
    })?;

    value.parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {description} {value:?}: {error}"),
        )
    })
}

fn parse_buffer_count(value: &OsStr) -> io::Result<usize> {
    let value = value.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "buffer count must contain valid Unicode digits",
        )
    })?;

    value.parse::<usize>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid buffer count {value:?}: {error}"),
        )
    })
}

fn print_usage(program: &OsStr) {
    let program = program.to_string_lossy();

    println!("NetworkCopy Speed Edition");
    println!();
    println!("Usage:");
    println!("  {program} bench-network-matrix-receive <bind-address>");
    println!("  {program} bench-network-matrix-send <receiver-address> [total-mib]");
    println!("  {program} bench-network-receive <bind-address>");
    println!("  {program} bench-network-send <receiver-address> [total-mib] [data-streams]");
    println!("  {program} receive <bind-address> <destination-root>");
    println!("  {program} send <receiver-address> <source-root> [workers] [data-streams]");
    println!("  {program} bench-copy <source> <destination> [buffer-mib]");
    println!("  {program} bench-hash <source> [buffer-mib]");
    println!("  {program} probe-compression <source> [zstd-level]");
    println!("  {program} bench-pipeline <source> <destination> [chunk-mib] [buffers]");
    println!("  {program} probe-iocp");
    println!("  {program} probe-overlapped-read <source> [read-mib]");
    println!("  {program} bench-iocp-copy <source> <destination> [chunk-mib] [operations]");
    println!("  {program} bench-scan <root-directory> [workers]");
    println!("  {program} probe-control <root-directory> [workers] [data-streams]");
    println!(
        "  {program} bench-multistream-copy <source-root> <destination-root> \
         [workers] [data-streams]"
    );
    println!("  {program} bench-striped-file <source> <destination> [data-streams]");
    println!();
    println!(
        "The pipeline defaults to {} MiB chunks and {} reusable buffers.",
        pipeline_bench::DEFAULT_CHUNK_MIB,
        pipeline_bench::DEFAULT_BUFFER_COUNT
    );
}
