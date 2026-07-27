mod copy_bench;
mod iocp_probe;
mod pipeline_bench;

use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::io;
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
        "bench-copy" => run_bench_copy(&mut arguments),
        "bench-pipeline" => run_bench_pipeline(&mut arguments),
        "probe-iocp" => run_iocp_probe(&mut arguments),
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
    println!("  {program} bench-copy <source> <destination> [buffer-mib]");
    println!("  {program} bench-pipeline <source> <destination> [chunk-mib] [buffers]");
    println!("  {program} bench-pipeline <source> <destination> [chunk-mib] [buffers]");
    println!();
    println!(
        "The pipeline defaults to {} MiB chunks and {} reusable buffers.",
        pipeline_bench::DEFAULT_CHUNK_MIB,
        pipeline_bench::DEFAULT_BUFFER_COUNT
    );
}
