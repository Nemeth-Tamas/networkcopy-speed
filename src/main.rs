mod copy_bench;

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

fn print_usage(program: &OsStr) {
    let program = program.to_string_lossy();

    println!("NetworkCopy Speed Edition");
    println!();
    println!("Usage:");
    println!("  {program} bench-copy <source> <destination> [buffer-mib]");
    println!();
    println!(
        "The buffer defaults to {} MiB.",
        copy_bench::DEFAULT_BUFFER_MIB
    );
}
