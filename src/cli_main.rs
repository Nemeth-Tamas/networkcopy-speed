use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::io;
use std::net::{
    Ipv6Addr,
    SocketAddr,
    SocketAddrV6,
    TcpListener,
};
use std::path::PathBuf;

pub fn run_cli() {
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
        "send-auto" => run_send_auto(&mut arguments),

        "receive-auto" => run_receive_auto(&mut arguments),

        "send" => run_send(&mut arguments),

        "receive" => run_receive(&mut arguments),
        "bench-copy" => run_bench_copy(&mut arguments),
        "bench-hash" => run_hash_bench(&mut arguments),
        "probe-compression" => run_compression_probe(&mut arguments),
        "probe-storage-media" => {
            run_storage_media_probe(&mut arguments)
        }
        "bench-zstd-dictionary" => run_zstd_dictionary_bench(&mut arguments),
        "bench-tiny-file-writes" => {
            run_tiny_file_write_bench(&mut arguments)
        }
        "bench-fixed-dedup" => {
            run_fixed_block_dedup_bench(&mut arguments)
        }
        "bench-cdc-dedup" => {
            run_content_defined_dedup_bench(
                &mut arguments,
            )
        }
        "bench-dedup-matrix" => {
            run_dedup_comparison_bench(
                &mut arguments,
            )
        }
        "bench-cdc-index" => {
            run_cdc_basis_index_bench(
                &mut arguments,
            )
        }
        "bench-cdc-reconstruct" => {
            run_cdc_reconstruction_bench(
                &mut arguments,
            )
        }
        "bench-cdc-folder-plan" => {
            run_cdc_folder_plan_bench(
                &mut arguments,
            )
        }
        "bench-pipeline" => run_bench_pipeline(&mut arguments),
        "probe-iocp" => run_iocp_probe(&mut arguments),
        "probe-overlapped-read" => run_overlapped_read_probe(&mut arguments),
        "bench-blocking-read" => run_blocking_read_bench(&mut arguments),
        "bench-iocp-read-ahead" => run_iocp_read_ahead_bench(&mut arguments),
        "bench-iocp-copy" => run_iocp_copy_bench(&mut arguments),
        "bench-scan" => run_manifest_scan_bench(&mut arguments),
        "probe-control" => run_control_plane_probe(&mut arguments),
        "bench-multistream-copy" => {
            run_multistream_copy_bench(
                &mut arguments,
            )
        }
        "bench-multistream-update" => {
            run_multistream_update_bench(
                &mut arguments,
            )
        }
        "bench-striped-file" => {
            run_striped_file_bench(
                &mut arguments,
            )
        }

        "management-agent" => {
            run_management_agent(
                &mut arguments,
            )
        }

        "management-discover" => {
            run_management_discover(
                &mut arguments,
            )
        }

        "management-direct-candidates" => {
            run_management_direct_candidates(
                &mut arguments,
            )
        }

        "management-direct-discover" => {
            run_management_direct_discover(
                &mut arguments,
            )
        }

        "management-notification-test" => {
            run_management_notification_test(
                &mut arguments,
            )
        }

        "management-update-check" => {
            run_management_update_check(
                &mut arguments,
            )
        }

        "management-hello" => {
            run_management_hello(
                &mut arguments,
            )
        }

        "management-roots" => {
            run_management_roots(
                &mut arguments,
            )
        }

        "management-list" => {
            run_management_list(
                &mut arguments,
            )
        }

        "management-prepare-receive" => {
            run_management_prepare_receive(
                &mut arguments,
            )
        }

        "management-transfer" => {
            run_management_transfer(
                &mut arguments,
            )
        }

        "management-start-send" => {
            run_management_start_send(
                &mut arguments,
            )
        }

        "management-snapshot" => {
            run_management_snapshot(
                &mut arguments,
            )
        }

        "management-job-status" => {
            run_management_job_status(
                &mut arguments,
            )
        }

        "management-cancel" => {
            run_management_cancel(
                &mut arguments,
            )
        }

        "direct-interfaces" => run_direct_interfaces(&mut arguments),

        "direct-address" => run_direct_address(&mut arguments),

        "direct-discovery-receive" => run_direct_discovery_receive(&mut arguments),

        "direct-discovery-receive-auto" => run_direct_discovery_receive_auto(&mut arguments),

        "direct-discovery-send" => run_direct_discovery_send(&mut arguments),

        "direct-discovery-send-auto" => run_direct_discovery_send_auto(&mut arguments),

        "direct-tcp-receive" => run_direct_tcp_receive(&mut arguments),

        "direct-tcp-send" => run_direct_tcp_send(&mut arguments),

        "direct-receive" => run_direct_receive(&mut arguments),

        "direct-send" => run_direct_send(&mut arguments),

        "version" | "--version" | "-V" => {
            println!("NetworkCopy Speed Edition {}", env!("CARGO_PKG_VERSION"));

            Ok(())
        }

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

fn run_management_agent(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    crate::management_agent::run()?;

    Ok(())
}

fn run_management_direct_candidates(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    let report =
        crate::management_direct::discover_candidates()?;

    println!(
        "NetworkCopy Speed Edition direct management candidates",
    );

    println!();

    println!(
        "  Usable direct interfaces: {}",
        report.candidates.len(),
    );

    println!(
        "  Routed Ethernet rejected: {}",
        report.routed_interface_indices.len(),
    );

    println!(
        "  Addressless interfaces:   {}",
        report
            .addressless_interface_indices
            .len(),
    );

    for candidate in &report.candidates {
        println!();

        println!(
            "Interface {}",
            candidate.interface_index,
        );

        match candidate.ipv6_endpoint {
            Some(endpoint) => {
                println!(
                    "  IPv6 management: {}",
                    endpoint,
                );
            }

            None => {
                println!(
                    "  IPv6 management: unavailable",
                );
            }
        }

        match candidate.ipv4_endpoint {
            Some(endpoint) => {
                println!(
                    "  IPv4 management: {}",
                    endpoint,
                );
            }

            None => {
                println!(
                    "  IPv4 management: unavailable",
                );
            }
        }

        match candidate.preferred_endpoint() {
            Some(endpoint) => {
                println!(
                    "  Preferred:       {}",
                    endpoint,
                );
            }

            None => {
                println!(
                    "  Preferred:       unavailable",
                );
            }
        }
    }

    if !report.routed_interface_indices.is_empty() {
        println!();

        println!(
            "Rejected routed Ethernet interface indexes: {}",
            format_interface_indices(
                &report.routed_interface_indices,
            ),
        );
    }

    if !report
        .addressless_interface_indices
        .is_empty()
    {
        println!();

        println!(
            "Direct interfaces without link-local/APIPA addresses: {}",
            format_interface_indices(
                &report
                    .addressless_interface_indices,
            ),
        );
    }

    if report.candidates.is_empty() {
        println!();

        println!(
            "No usable gateway-free Direct Link management interface is currently available.",
        );
    }

    Ok(())
}

fn run_management_direct_discover(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    let agents =
        crate::management_direct::discover_agents()?;

    println!(
        "NetworkCopy Speed Edition Direct Link management discovery",
    );

    println!();

    println!(
        "  Management agents found: {}",
        agents.len(),
    );

    for discovered in agents {
        println!();

        println!(
            "  Computer:        {}",
            discovered.agent.hostname,
        );

        println!(
            "  Interface index: {}",
            discovered.interface_index,
        );

        println!(
            "  Local endpoint:  {}",
            discovered.local_endpoint,
        );

        println!(
            "  Control endpoint: {}",
            discovered.agent.endpoint,
        );

        println!(
            "  Protocol:        {}",
            discovered
                .agent
                .protocol_version,
        );

        println!(
            "  State:           {}",
            discovered.agent.state.label(),
        );

        println!(
            "  Capabilities:    {}{}",
            if discovered
                .agent
                .capabilities
                .can_send()
            {
                "sender"
            } else {
                ""
            },
            if discovered
                .agent
                .capabilities
                .can_send()
                && discovered
                    .agent
                    .capabilities
                    .can_receive()
            {
                ", receiver"
            } else if discovered
                .agent
                .capabilities
                .can_receive()
            {
                "receiver"
            } else {
                ""
            },
        );
    }

    Ok(())
}

fn run_management_notification_test(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let kind_argument = required_argument(
        arguments,
        "notification kind: info, warning, or error",
    )?;

    let kind_text =
        kind_argument.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "notification kind must contain valid Unicode",
            )
        })?;

    let kind = match kind_text {
        "info" | "information" => {
            crate::windows_notification::NotificationKind::Information
        }

        "warning" => {
            crate::windows_notification::NotificationKind::Warning
        }

        "error" => {
            crate::windows_notification::NotificationKind::Error
        }

        unknown => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown notification kind {unknown:?}; expected info, warning, or error",
                ),
            )
            .into());
        }
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    let body = format!(
        "This is a NetworkCopy {} notification test.",
        kind.label(),
    );

    println!(
        "Showing Windows {} notification...",
        kind.label(),
    );

    crate::windows_notification::show_blocking(
        kind,
        "NetworkCopy notification test",
        &body,
    )?;

    println!("Notification test finished.");

    Ok(())
}

fn run_management_update_check(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let open_release =
        match arguments.next() {
            None => false,

            Some(argument)
                if argument == "--open" =>
            {
                true
            }

            Some(argument) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "unexpected argument: {}; expected --open or no argument",
                        argument.to_string_lossy(),
                    ),
                )
                .into());
            }
        };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    println!(
        "NetworkCopy Speed Edition update check",
    );

    println!();

    let check =
        crate::release_update::check_latest(
            env!("CARGO_PKG_VERSION"),
        )?;

    println!(
        "  Current build:  {}",
        check.current_version,
    );

    println!(
        "  Latest stable: {}",
        check.latest.tag_name,
    );

    println!(
        "  Release name:  {}",
        check.latest.name,
    );

    println!(
        "  Status:        {}",
        if check.update_available {
            "newer stable release available"
        } else {
            "current build is up to date or newer"
        },
    );

    if open_release {
        println!();

        println!(
            "Opening the latest stable release page...",
        );

        crate::release_update::open_release_page(
            &check.latest.html_url,
        )?;
    }

    Ok(())
}

fn format_interface_indices(
    interface_indices: &[u32],
) -> String {
    interface_indices
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn run_management_transfer(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let sender_agent =
        parse_socket_address(
            &required_argument(
                arguments,
                "sender management agent address",
            )?,
            "sender management agent address",
        )?;

    let receiver_agent =
        parse_socket_address(
            &required_argument(
                arguments,
                "receiver management agent address",
            )?,
            "receiver management agent address",
        )?;

    let source_root =
        required_argument(
            arguments,
            "sender source directory",
        )?
        .into_string()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "sender source must contain valid Unicode",
            )
        })?;

    let destination_root =
        required_argument(
            arguments,
            "receiver destination directory",
        )?
        .into_string()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "receiver destination must contain valid Unicode",
            )
        })?;

    let mut optional_arguments =
        arguments.collect::<Vec<_>>();

    let update_existing =
        optional_arguments
            .last()
            .is_some_and(|value| {
                value == "--update"
                    || value == "update"
            });

    if update_existing {
        optional_arguments.pop();
    }

    if let Some(extra) =
        optional_arguments.get(2)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    let worker_count =
        match optional_arguments.first() {
            Some(value) => {
                parse_usize_count(
                    value,
                    "scan worker count",
                )?
            }

            None => {
                manifest_scan::
                    default_worker_count()
            }
        };

    let calibration_mib =
        match optional_arguments.get(1) {
            Some(value) => {
                parse_u64_count(
                    value,
                    "calibration MiB",
                )?
            }

            None => {
                calibrated_transfer::
                    DEFAULT_CALIBRATION_MIB
            }
        };

    let record =
        crate::management_orchestration::
            start_transfer(
                crate::management_orchestration::
                    ManagedTransferRequest {
                        sender_agent,

                        receiver_agent,

                        source_root,

                        destination_root,

                        update_existing,

                        worker_count,

                        calibration_mib,
                    },
            )?;

    println!(
        "NetworkCopy Speed Edition managed transfer started"
    );

    println!();

    println!(
        "  Sender agent:       {}",
        record.sender_agent,
    );

    println!(
        "  Sender job ID:      {}",
        record.sender_job_id,
    );

    println!(
        "  Source:             {}",
        record.source_root,
    );

    println!();

    println!(
        "  Receiver agent:     {}",
        record.receiver_agent,
    );

    println!(
        "  Receiver job ID:    {}",
        record.receiver_job_id,
    );

    println!(
        "  Payload endpoint:   {}",
        record.receiver_payload,
    );

    println!(
        "  Destination:        {}",
        record.destination_root,
    );

    println!();

    println!(
        "  Update mode:        {}",
        if record.update_existing {
            "enabled"
        } else {
            "disabled"
        },
    );

    println!(
        "  Scan workers:       {}",
        record.worker_count,
    );

    println!(
        "  Calibration:        {} MiB per matrix run",
        record.calibration_mib,
    );

    println!(
        "  Manager required:   no"
    );

    println!(
        "  State:              endpoints running independently"
    );

    Ok(())
}

fn run_management_start_send(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let endpoint =
        parse_socket_address(
            &required_argument(
                arguments,
                "sender management agent address",
            )?,
            "sender management agent address",
        )?;

    let receiver_address =
        parse_socket_address(
            &required_argument(
                arguments,
                "receiver payload address",
            )?,
            "receiver payload address",
        )?;

    let source_root =
        required_argument(
            arguments,
            "sender source directory",
        )?
        .into_string()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "sender source must contain valid Unicode",
            )
        })?;

    let worker_count =
        match arguments.next() {
            Some(value) => {
                parse_usize_count(
                    &value,
                    "scan worker count",
                )?
            }

            None => {
                manifest_scan::
                    default_worker_count()
            }
        };

    let calibration_mib =
        match arguments.next() {
            Some(value) => {
                parse_u64_count(
                    &value,
                    "calibration MiB",
                )?
            }

            None => {
                calibrated_transfer::
                    DEFAULT_CALIBRATION_MIB
            }
        };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    let job =
        management_control::start_send(
            endpoint,
            receiver_address,
            &source_root,
            worker_count,
            calibration_mib,
        )?;

    println!(
        "NetworkCopy Speed Edition sender started"
    );

    println!(
        "  Agent:         {endpoint}"
    );

    println!(
        "  Job ID:        {}",
        job.job_id,
    );

    println!(
        "  Receiver:      {}",
        job.receiver_address,
    );

    println!(
        "  Source:        {}",
        job.source_root,
    );

    println!(
        "  Scan workers:  {}",
        job.worker_count,
    );

    println!(
        "  Calibration:   {} MiB per matrix run",
        job.calibration_mib,
    );

    println!(
        "  State:         sender running"
    );

    Ok(())
}

fn run_management_prepare_receive(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let endpoint =
        parse_socket_address(
            &required_argument(
                arguments,
                "management agent address",
            )?,
            "management agent address",
        )?;

    let destination_root =
        required_argument(
            arguments,
            "receiver destination directory",
        )?
        .into_string()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "receiver destination must contain valid Unicode",
            )
        })?;

    let update_existing =
        match arguments.next() {
            None => false,

            Some(value)
                if value == "--update"
                    || value == "update" =>
            {
                true
            }

            Some(value) => {
                return Err(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "unexpected argument: {}",
                            value.to_string_lossy(),
                        ),
                    )
                    .into(),
                );
            }
        };

    if let Some(extra) = arguments.next() {
        return Err(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unexpected extra argument: {}",
                    extra.to_string_lossy(),
                ),
            )
            .into(),
        );
    }

    let job =
        management_control::prepare_receive(
            endpoint,
            &destination_root,
            update_existing,
        )?;

    println!(
        "NetworkCopy Speed Edition receiver prepared"
    );

    println!(
        "  Agent:         {endpoint}"
    );

    println!(
        "  Job ID:        {}",
        job.job_id,
    );

    println!(
        "  Receiver:      {}",
        SocketAddr::new(
            endpoint.ip(),
            job.transfer_port,
        ),
    );

    println!(
        "  Destination:   {}",
        job.destination_root,
    );

    println!(
        "  Update mode:   {}",
        if job.update_existing {
            "enabled"
        } else {
            "disabled"
        },
    );

    println!(
        "  State:         receiver waiting for sender"
    );

    Ok(())
}

fn run_management_snapshot(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let endpoint =
        parse_socket_address(
            &required_argument(
                arguments,
                "management agent address",
            )?,
            "management agent address",
        )?;

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    let snapshot =
        management_control::
            agent_snapshot(endpoint)?;

    println!(
        "NetworkCopy Speed Edition agent snapshot"
    );

    println!(
        "  Agent:         {endpoint}"
    );

    println!();

    match snapshot.active {
        Some(active) => {
            println!("Active job");

            println!(
                "  Role:          {}",
                active.role.label(),
            );

            println!(
                "  Job ID:        {}",
                active.job_id,
            );

            println!(
                "  Phase:         {}",
                active.phase,
            );

            println!(
                "  Completed:     {} bytes",
                active.completed,
            );

            if active.total == 0 {
                println!(
                    "  Total:         unknown"
                );
            } else {
                let percent =
                    active.completed
                        .min(active.total)
                        as f64
                        / active.total
                            as f64
                        * 100.0;

                println!(
                    "  Total:         {} bytes",
                    active.total,
                );

                println!(
                    "  Progress:      {percent:.2}%"
                );
            }

            println!(
                "  Cancelling:    {}",
                if active.cancel_requested {
                    "yes"
                } else {
                    "no"
                },
            );
        }

        None => {
            println!(
                "Active job:      none"
            );
        }
    }

    println!();

    match snapshot.latest_result {
        Some(result) => {
            println!("Latest result");

            println!(
                "  Role:          {}",
                result.role.label(),
            );

            println!(
                "  Job ID:        {}",
                result.job_id,
            );

            println!(
                "  Outcome:       {}",
                result.outcome.label(),
            );

            println!(
                "  Files:         {}",
                result.files,
            );

            println!(
                "  Logical data:  {} bytes",
                result.logical_bytes,
            );

            println!(
                "  Wire data:     {} bytes",
                result.wire_bytes,
            );

            println!(
                "  Data streams:  {}",
                result.data_stream_count,
            );

            if !result.message.is_empty() {
                println!(
                    "  Message:       {}",
                    result.message,
                );
            }
        }

        None => {
            println!(
                "Latest result:   none"
            );
        }
    }

    Ok(())
}

fn run_management_job_status(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let endpoint =
        parse_socket_address(
            &required_argument(
                arguments,
                "management agent address",
            )?,
            "management agent address",
        )?;

    if let Some(extra) = arguments.next() {
        return Err(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unexpected extra argument: {}",
                    extra.to_string_lossy(),
                ),
            )
            .into(),
        );
    }

    let status =
        management_control::job_status(
            endpoint,
        )?;

    println!(
        "NetworkCopy Speed Edition management job status"
    );

    println!(
        "  Agent:         {endpoint}"
    );

    println!(
        "  State:         {}",
        status.phase.label(),
    );

    if let Some(job_id) =
        status.job_id
    {
        println!(
            "  Job ID:        {job_id}"
        );
    }

    if let Some(transfer_port) =
        status.transfer_port
    {
        println!(
            "  Receiver:      {}",
            SocketAddr::new(
                endpoint.ip(),
                transfer_port,
            ),
        );
    }

    if let Some(destination) =
        status.destination_root
    {
        println!(
            "  Destination:   {destination}"
        );

        println!(
            "  Update mode:   {}",
            if status.update_existing {
                "enabled"
            } else {
                "disabled"
            },
        );
    }

    Ok(())
}

fn run_management_cancel(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let endpoint =
        parse_socket_address(
            &required_argument(
                arguments,
                "management agent address",
            )?,
            "management agent address",
        )?;

    let job_id =
        parse_u64_count(
            &required_argument(
                arguments,
                "management job ID",
            )?,
            "management job ID",
        )?;

    if let Some(extra) = arguments.next() {
        return Err(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unexpected extra argument: {}",
                    extra.to_string_lossy(),
                ),
            )
            .into(),
        );
    }

    let cancelled =
        management_control::cancel_job(
            endpoint,
            job_id,
        )?;

    println!(
        "NetworkCopy Speed Edition management job cancelled"
    );

    println!(
        "  Agent:         {endpoint}"
    );

    println!(
        "  Job ID:        {cancelled}"
    );

    println!(
        "  State:         idle"
    );

    Ok(())
}

fn run_management_list(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let endpoint =
        parse_socket_address(
            &required_argument(
                arguments,
                "management agent address",
            )?,
            "management agent address",
        )?;

    let remote_path =
        required_argument(
            arguments,
            "remote directory path",
        )?
        .into_string()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote directory path must contain valid Unicode",
            )
        })?;

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    println!(
        "NetworkCopy Speed Edition remote directory"
    );

    println!(
        "  Agent:       {endpoint}"
    );

    println!(
        "  Directory:   {remote_path}"
    );

    println!();

    println!(
        "WARNING: management mode is currently unauthenticated."
    );

    println!(
        "Use it only on a known, trusted local network."
    );

    println!();

    let entries =
        management_control::list_directory(
            endpoint,
            &remote_path,
        )?;

    println!(
        "Entries: {}",
        entries.len(),
    );

    println!();

    println!(
        "  Type            Size        Modified  Name"
    );

    for entry in entries {
        let size = match entry.kind {
            management_directory::
                ManagementEntryKind::File =>
            {
                entry.size.to_string()
            }

            _ => "-".to_string(),
        };

        let modified =
            entry.modified_unix_seconds
                .map(|value| {
                    value.to_string()
                })
                .unwrap_or_else(|| {
                    "-".to_string()
                });

        println!(
            "  {:<6} {:>14} {:>15}  {}",
            entry.kind.label(),
            size,
            modified,
            entry.name,
        );
    }

    Ok(())
}

fn run_management_roots(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let endpoint =
        parse_socket_address(
            &required_argument(
                arguments,
                "management agent address",
            )?,
            "management agent address",
        )?;

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    println!(
        "NetworkCopy Speed Edition remote roots"
    );

    println!();

    println!(
        "WARNING: management mode is currently unauthenticated."
    );

    println!(
        "Use it only on a known, trusted local network."
    );

    println!();

    let roots =
        management_control::list_roots(
            endpoint,
        )?;

    println!(
        "Available roots: {}",
        roots.len(),
    );

    for root in roots {
        println!(
            "  {}",
            root.path,
        );
    }

    Ok(())
}

fn run_management_hello(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let endpoint =
        parse_socket_address(
            &required_argument(
                arguments,
                "management agent address",
            )?,
            "management agent address",
        )?;

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    println!(
        "NetworkCopy Speed Edition management hello"
    );

    println!();

    println!(
        "WARNING: management mode is currently unauthenticated."
    );

    println!(
        "Use it only on a known, trusted local network."
    );

    println!();

    let response =
        management_control::hello(
            endpoint,
        )?;

    let capabilities =
        match (
            response.capabilities.can_send(),
            response.capabilities.can_receive(),
        ) {
            (true, true) => {
                "sender, receiver"
            }

            (true, false) => "sender",

            (false, true) => "receiver",

            (false, false) => "none",
        };

    println!(
        "Management agent replied"
    );

    println!(
        "  Computer:      {}",
        response.hostname,
    );

    println!(
        "  Application:   {}",
        response.application_version,
    );

    println!(
        "  Protocol:      {}",
        response.protocol_version,
    );

    println!(
        "  State:         {}",
        response.state.label(),
    );

    println!(
        "  Capabilities:  {capabilities}"
    );

    Ok(())
}

fn run_management_discover(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    println!(
        "NetworkCopy Speed Edition management discovery"
    );

    println!();

    println!(
        "WARNING: management mode is currently unauthenticated."
    );

    println!(
        "Use it only on a known, trusted local network."
    );

    println!();

    let agents =
        management_discovery::discover()?;

    println!(
        "Discovered agents: {}",
        agents.len(),
    );

    for agent in agents {
        let capabilities =
            match (
                agent.capabilities.can_send(),
                agent.capabilities.can_receive(),
            ) {
                (true, true) => {
                    "sender, receiver"
                }

                (true, false) => "sender",

                (false, true) => "receiver",

                (false, false) => {
                    "none"
                }
            };

        println!();

        println!(
            "  Computer:      {}",
            agent.hostname,
        );

        println!(
            "  Control:       {}",
            agent.endpoint,
        );

        println!(
            "  Protocol:      {}",
            agent.protocol_version,
        );

        println!(
            "  State:         {}",
            agent.state.label(),
        );

        println!(
            "  Capabilities:  {capabilities}"
        );
    }

    Ok(())
}

fn run_direct_receive(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let destination_root =
        PathBuf::from(required_argument(arguments, "destination root directory")?);

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    let firewall_address = SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::UNSPECIFIED,
        direct_address::DIRECT_TRANSFER_PORT,
        0,
        0,
    ));

    windows_setup::prepare_receiver(firewall_address)?;

    windows_setup::prepare_discovery_receiver(direct_discovery::DISCOVERY_PORT)?;

    let report = direct_transfer::receive_once(&destination_root)?;

    report.print();

    Ok(())
}

fn run_direct_send(arguments: &mut impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let source_root = PathBuf::from(required_argument(arguments, "source root directory")?);

    let worker_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,

        None => manifest_scan::default_worker_count(),
    };

    let calibration_mib = match arguments.next() {
        Some(value) => parse_u64_count(&value, "calibration MiB")?,

        None => calibrated_transfer::DEFAULT_CALIBRATION_MIB,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    manifest_scan::validate_worker_count(worker_count)?;

    let calibration_bytes = network_calibration::bytes_from_mib(calibration_mib)?;

    println!("Direct-link calibration payload: {calibration_mib} MiB per matrix run");

    let report = direct_transfer::send(&source_root, worker_count, calibration_bytes)?;

    report.print();

    Ok(())
}

fn run_direct_tcp_receive(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    let firewall_address = SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::UNSPECIFIED,
        direct_address::DIRECT_TRANSFER_PORT,
        0,
        0,
    ));

    windows_setup::prepare_receiver(firewall_address)?;

    windows_setup::prepare_discovery_receiver(direct_discovery::DISCOVERY_PORT)?;

    direct_tcp::receive_once()?;

    Ok(())
}

fn run_direct_tcp_send(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    direct_tcp::send_once()?;

    Ok(())
}

fn run_direct_discovery_receive_auto(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    windows_setup::prepare_discovery_receiver(direct_discovery::DISCOVERY_PORT)?;

    direct_discovery::receive_all()?;

    Ok(())
}

fn run_direct_discovery_send_auto(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    direct_discovery::discover_all()?;

    Ok(())
}

fn run_direct_discovery_receive(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let value = required_argument(arguments, "interface index")?;

    let parsed = parse_u64_count(&value, "interface index")?;

    let interface_index = u32::try_from(parsed).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index is larger than u32",
        )
    })?;

    if interface_index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index must not be zero",
        )
        .into());
    }

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    windows_setup::prepare_discovery_receiver(direct_discovery::DISCOVERY_PORT)?;

    direct_discovery::receive(interface_index)?;

    Ok(())
}

fn run_direct_discovery_send(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let value = required_argument(arguments, "interface index")?;

    let parsed = parse_u64_count(&value, "interface index")?;

    let interface_index = u32::try_from(parsed).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index is larger than u32",
        )
    })?;

    if interface_index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index must not be zero",
        )
        .into());
    }

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    direct_discovery::discover(interface_index)?;

    Ok(())
}

fn run_direct_address(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let value = required_argument(arguments, "interface index")?;

    let parsed = parse_u64_count(&value, "interface index")?;

    let interface_index = u32::try_from(parsed).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index is larger than u32",
        )
    })?;

    if interface_index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index must not be zero",
        )
        .into());
    }

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    direct_address::print_addresses(interface_index)?;

    Ok(())
}

fn run_direct_interfaces(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    direct_link::print_inventory()?;

    Ok(())
}

fn run_storage_media_probe(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let path = PathBuf::from(
        required_argument(
            arguments,
            "storage path",
        )?,
    );

    if let Some(extra) = arguments.next() {
        return Err(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unexpected extra argument: {}",
                    extra.to_string_lossy(),
                ),
            )
            .into(),
        );
    }

    let report =
        storage_media::inspect_path(&path)?;

    println!(
        "NetworkCopy Speed Edition storage media probe",
    );

    println!(
        "  Path:          {}",
        path.display(),
    );

    match report.drive_letter {
        Some(letter) => {
            println!(
                "  Drive:         {letter}:",
            );
        }

        None => {
            println!(
                "  Drive:         unavailable",
            );
        }
    }

    match report.incurs_seek_penalty() {
        Some(true) => {
            println!(
                "  Seek penalty:  yes",
            );
        }

        Some(false) => {
            println!(
                "  Seek penalty:  no",
            );
        }

        None => {
            println!(
                "  Seek penalty:  unknown",
            );
        }
    }

    println!(
        "  Storage class: {}",
        report.kind,
    );

    Ok(())
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

fn run_zstd_dictionary_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(required_argument(arguments, "root directory")?);

    let dictionary_kib = match arguments.next() {
        Some(value) => parse_usize_count(&value, "dictionary size in KiB")?,

        None => zstd_dictionary_bench::DEFAULT_DICTIONARY_KIB,
    };

    let level = match arguments.next() {
        Some(value) => parse_compression_level(&value)?,

        None => compression_probe::DEFAULT_LEVEL,
    };

    let worker_count = match arguments.next() {
        Some(value) => parse_usize_count(&value, "scanner worker count")?,

        None => manifest_scan::default_worker_count(),
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    zstd_dictionary_bench::validate_dictionary_kib(dictionary_kib)?;
    manifest_scan::validate_worker_count(worker_count)?;

    println!("NetworkCopy Speed Edition shared Zstandard dictionary benchmark");
    println!("  Root:            {}", root.display());
    println!("  Dictionary:      {dictionary_kib} KiB");
    println!("  Zstandard level: {level}");
    println!("  Scanner workers: {worker_count}");
    println!();

    let report =
        zstd_dictionary_bench::run(&root, worker_count, dictionary_kib, level)?;

    report.print();
    Ok(())
}

fn run_tiny_file_write_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source_root =
        PathBuf::from(required_argument(arguments, "source root directory")?);

    let output_root =
        PathBuf::from(required_argument(arguments, "empty output directory")?);

    let max_workers = match arguments.next() {
        Some(value) => parse_usize_count(&value, "maximum worker count")?,

        None => tiny_file_write_bench::default_max_workers(),
    };

    let scan_workers = match arguments.next() {
        Some(value) => parse_usize_count(&value, "scanner worker count")?,

        None => manifest_scan::default_worker_count(),
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    tiny_file_write_bench::validate_max_workers(max_workers)?;
    manifest_scan::validate_worker_count(scan_workers)?;

    println!("NetworkCopy Speed Edition tiny-file write calibration");
    println!("  Source:          {}", source_root.display());
    println!("  Output:          {}", output_root.display());
    println!("  Maximum workers: {max_workers}");
    println!("  Scanner workers: {scan_workers}");
    println!();

    let report = tiny_file_write_bench::run(
        &source_root,
        &output_root,
        max_workers,
        scan_workers,
    )?;

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

fn run_fixed_block_dedup_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let basis = PathBuf::from(required_argument(
        arguments,
        "basis file",
    )?);

    let candidate = PathBuf::from(required_argument(
        arguments,
        "candidate file",
    )?);

    let block_kib = match arguments.next() {
        Some(value) => {
            parse_usize_count(
                &value,
                "fixed dedup block size in KiB",
            )?
        }

        None => {
            fixed_block_dedup_bench::DEFAULT_BLOCK_KIB
        }
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    fixed_block_dedup_bench::validate_block_kib(
        block_kib,
    )?;

    println!(
        "NetworkCopy Speed Edition fixed-block dedup benchmark",
    );
    println!("  Basis:       {}", basis.display());
    println!("  Candidate:   {}", candidate.display());
    println!("  Block size:  {block_kib} KiB");
    println!();

    let report = fixed_block_dedup_bench::run(
        &basis,
        &candidate,
        block_kib,
    )?;

    report.print();

    Ok(())
}

fn run_content_defined_dedup_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let basis = PathBuf::from(required_argument(
        arguments,
        "basis file",
    )?);

    let candidate = PathBuf::from(required_argument(
        arguments,
        "candidate file",
    )?);

    let average_kib = match arguments.next() {
        Some(value) => {
            parse_usize_count(
                &value,
                "content-defined average chunk size in KiB",
            )?
        }

        None => {
            content_defined_dedup_bench::
                DEFAULT_AVERAGE_KIB
        }
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    content_defined_dedup_bench::
        validate_average_kib(average_kib)?;

    println!(
        "NetworkCopy Speed Edition content-defined dedup benchmark",
    );

    println!("  Basis:          {}", basis.display());

    println!(
        "  Candidate:      {}",
        candidate.display(),
    );

    println!(
        "  Target average: {average_kib} KiB",
    );

    println!();

    let report =
        content_defined_dedup_bench::run(
            &basis,
            &candidate,
            average_kib,
        )?;

    report.print();

    Ok(())
}

fn run_dedup_comparison_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let basis = PathBuf::from(required_argument(
        arguments,
        "basis file",
    )?);

    let candidate = PathBuf::from(required_argument(
        arguments,
        "candidate file",
    )?);

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    println!(
        "NetworkCopy Speed Edition deduplication comparison matrix",
    );

    println!("  Basis:     {}", basis.display());

    println!(
        "  Candidate: {}",
        candidate.display(),
    );

    println!(
        "  Sizes:     4, 16, 64, 256 KiB",
    );

    println!();

    let report = dedup_comparison_bench::run(
        &basis,
        &candidate,
    )?;

    report.print();

    Ok(())
}

fn run_cdc_basis_index_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let basis = PathBuf::from(required_argument(
        arguments,
        "basis file",
    )?);

    let average_kib = match arguments.next() {
        Some(value) => {
            parse_usize_count(
                &value,
                "content-defined average chunk size in KiB",
            )?
        }

        None => {
            content_defined_dedup_bench::
                DEFAULT_AVERAGE_KIB
        }
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    content_defined_dedup_bench::
        validate_average_kib(average_kib)?;

    println!(
        "NetworkCopy Speed Edition receiver basis index benchmark",
    );

    println!("  Basis:          {}", basis.display());

    println!(
        "  Target average: {average_kib} KiB",
    );

    println!();

    let report = cdc_basis_index::run(
        &basis,
        average_kib,
    )?;

    report.print();

    Ok(())
}

fn run_cdc_reconstruction_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let basis = PathBuf::from(required_argument(
        arguments,
        "basis file",
    )?);

    let candidate =
        PathBuf::from(required_argument(
            arguments,
            "candidate file",
        )?);

    let output =
        PathBuf::from(required_argument(
            arguments,
            "reconstructed output file",
        )?);

    let average_kib = match arguments.next() {
        Some(value) => {
            parse_usize_count(
                &value,
                "content-defined average chunk size in KiB",
            )?
        }

        None => {
            content_defined_dedup_bench::
                DEFAULT_AVERAGE_KIB
        }
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    content_defined_dedup_bench::
        validate_average_kib(average_kib)?;

    println!(
        "NetworkCopy Speed Edition CDC reconstruction benchmark",
    );

    println!(
        "  Basis:          {}",
        basis.display(),
    );

    println!(
        "  Candidate:      {}",
        candidate.display(),
    );

    println!(
        "  Output:         {}",
        output.display(),
    );

    println!(
        "  Target average: {average_kib} KiB",
    );

    println!();

    let report =
        cdc_reconstruction_bench::run(
            &basis,
            &candidate,
            &output,
            average_kib,
        )?;

    report.print();

    Ok(())
}

fn run_cdc_folder_plan_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source_root =
        PathBuf::from(required_argument(
            arguments,
            "source root directory",
        )?);

    let destination_root =
        PathBuf::from(required_argument(
            arguments,
            "destination root directory",
        )?);

    let average_kib =
        match arguments.next() {
            Some(value) => {
                parse_usize_count(
                    &value,
                    "content-defined average chunk size in KiB",
                )?
            }

            None => {
                content_defined_dedup_bench::
                    DEFAULT_AVERAGE_KIB
            }
        };

    let minimum_file_mib =
        match arguments.next() {
            Some(value) => {
                parse_usize_count(
                    &value,
                    "minimum candidate size in MiB",
                )?
            }

            None => {
                folder_dedup_bench::
                    DEFAULT_MINIMUM_FILE_MIB
            }
        };

    let maximum_literal_mib =
        match arguments.next() {
            Some(value) => {
                parse_usize_count(
                    &value,
                    "maximum literal staging in MiB",
                )?
            }

            None => {
                folder_dedup_bench::
                    DEFAULT_MAXIMUM_LITERAL_MIB
            }
        };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    content_defined_dedup_bench::
        validate_average_kib(
            average_kib,
        )?;

    folder_dedup_bench::
        validate_limits(
            minimum_file_mib,
            maximum_literal_mib,
        )?;

    println!(
        "NetworkCopy Speed Edition bounded folder CDC planner",
    );

    println!(
        "  Source:          {}",
        source_root.display(),
    );

    println!(
        "  Destination:     {}",
        destination_root.display(),
    );

    println!(
        "  Target average:  {average_kib} KiB",
    );

    println!(
        "  Minimum file:    {minimum_file_mib} MiB",
    );

    println!(
        "  Literal ceiling: {maximum_literal_mib} MiB",
    );

    println!();

    let report =
        folder_dedup_bench::run(
            &source_root,
            &destination_root,
            average_kib,
            minimum_file_mib,
            maximum_literal_mib,
        )?;

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

fn run_blocking_read_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source = PathBuf::from(
        required_argument(
            arguments,
            "source path",
        )?,
    );

    let chunk_mib =
        match arguments.next() {
            Some(value) => {
                parse_buffer_mib(&value)?
            }

            None => {
                iocp_copy::DEFAULT_CHUNK_MIB
            }
        };

    if let Some(extra) = arguments.next() {
        return Err(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unexpected extra argument: {}",
                    extra.to_string_lossy(),
                ),
            )
            .into(),
        );
    }

    pipeline_bench::validate_config(
        chunk_mib,
        1,
    )?;

    println!(
        "NetworkCopy Speed Edition blocking sequential source-read benchmark",
    );

    println!(
        "  Source:      {}",
        source.display(),
    );

    println!(
        "  Chunk:       {chunk_mib} MiB",
    );

    println!();

    let report =
        iocp_copy::run_blocking_read(
            &source,
            chunk_mib,
        )?;

    report.print();

    Ok(())
}

fn run_iocp_read_ahead_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source =
        PathBuf::from(
            required_argument(
                arguments,
                "source path",
            )?,
        );

    let chunk_mib =
        match arguments.next() {
            Some(value) => {
                parse_buffer_mib(&value)?
            }

            None => {
                iocp_copy::DEFAULT_CHUNK_MIB
            }
        };

    let operation_count =
        match arguments.next() {
            Some(value) => {
                parse_buffer_count(&value)?
            }

            None => {
                iocp_copy::DEFAULT_OPERATION_COUNT
            }
        };

    if let Some(extra) = arguments.next() {
        return Err(
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unexpected extra argument: {}",
                    extra.to_string_lossy(),
                ),
            )
            .into(),
        );
    }

    pipeline_bench::validate_config(
        chunk_mib,
        operation_count,
    )?;

    println!(
        "NetworkCopy Speed Edition native IOCP source read-ahead benchmark",
    );

    println!(
        "  Source:      {}",
        source.display(),
    );

    println!(
        "  Chunk:       {chunk_mib} MiB",
    );

    println!(
        "  Operations:  {operation_count}",
    );

    println!();

    let report =
        iocp_copy::run_read_ahead(
            &source,
            chunk_mib,
            operation_count,
        )?;

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

    windows_setup::prepare_receiver(bind_address)?;

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

    let socket_send_buffer_kib = match arguments.next() {
        Some(value) => Some(parse_u64_count(
            &value,
            "socket send buffer KiB",
        )?),

        None => None,
    };

    let socket_send_buffer_bytes = socket_send_buffer_kib
        .map(network_calibration::socket_buffer_bytes_from_kib)
        .transpose()?;

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

    match socket_send_buffer_kib {
        Some(buffer_kib) => {
            println!(
                "  Send buffer:  {buffer_kib} KiB requested",
            );
        }

        None => {
            println!("  Send buffer:  Windows default");
        }
    }

    println!("  Source:       generated memory buffers");

    println!();

    let report = match socket_send_buffer_bytes {
        Some(buffer_bytes) => {
            network_calibration::send_with_socket_send_buffer(
                receiver_address,
                total_bytes,
                data_stream_count,
                buffer_bytes,
            )?
        }

        None => network_calibration::send(
            receiver_address,
            total_bytes,
            data_stream_count,
        )?,
    };

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

    let socket_receive_buffer_kib = match arguments.next() {
        Some(value) => Some(parse_u64_count(
            &value,
            "socket receive buffer KiB",
        )?),

        None => None,
    };

    let socket_receive_buffer_bytes = socket_receive_buffer_kib
        .map(network_calibration::socket_buffer_bytes_from_kib)
        .transpose()?;

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy()),
        )
        .into());
    }

    windows_setup::prepare_receiver(bind_address)?;

    let listener = TcpListener::bind(bind_address)?;

    println!("NetworkCopy Speed Edition raw TCP receiver");

    println!("  Listening:    {}", listener.local_addr()?);

    match socket_receive_buffer_kib {
        Some(buffer_kib) => {
            println!(
                "  Receive buffer: {buffer_kib} KiB requested",
            );
        }

        None => {
            println!("  Receive buffer: Windows default");
        }
    }

    println!("  Destination:  discarded memory buffers");

    println!("  Mode:         one calibration, then exit");

    println!();

    let report = match socket_receive_buffer_bytes {
        Some(buffer_bytes) => {
            network_calibration::receive_once_with_socket_receive_buffer(
                listener,
                buffer_bytes,
            )?
        }

        None => network_calibration::receive_once(listener)?,
    };

    report.print("receive");

    Ok(())
}

fn run_send_auto(arguments: &mut impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let receiver_address = parse_socket_address(
        &required_argument(arguments, "receiver address")?,
        "receiver address",
    )?;

    let source_root = PathBuf::from(required_argument(arguments, "source root directory")?);

    let worker_count = match arguments.next() {
        Some(value) => parse_buffer_count(&value)?,

        None => manifest_scan::default_worker_count(),
    };

    let calibration_mib = match arguments.next() {
        Some(value) => parse_u64_count(&value, "calibration MiB")?,

        None => calibrated_transfer::DEFAULT_CALIBRATION_MIB,
    };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    manifest_scan::validate_worker_count(worker_count)?;

    let calibration_bytes = network_calibration::bytes_from_mib(calibration_mib)?;

    println!("NetworkCopy Speed Edition automatic calibrated sender");

    println!("  Receiver:       {receiver_address}");

    println!("  Source:         {}", source_root.display());

    println!("  Scan workers:   {worker_count}");

    println!("  Calibration:    {calibration_mib} MiB per matrix run");

    println!("  Stream tests:   1, 2, 4, 8");

    println!("  Selection:      smallest count within 90% of best");

    println!();

    let report = calibrated_transfer::send(
        receiver_address,
        &source_root,
        worker_count,
        calibration_bytes,
    )?;

    report.print();

    Ok(())
}

fn run_receive_auto(arguments: &mut impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let bind_address = parse_socket_address(
        &required_argument(arguments, "bind address")?,
        "bind address",
    )?;

    let destination_root =
        PathBuf::from(required_argument(arguments, "destination root directory")?);

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    windows_setup::prepare_receiver(bind_address)?;

    let listener = TcpListener::bind(bind_address)?;

    println!("NetworkCopy Speed Edition automatic calibrated receiver");

    println!("  Listening:      {}", listener.local_addr()?);

    println!("  Destination:    {}", destination_root.display());

    println!("  Sequence:       matrix, then one transfer");

    println!("  Stream tests:   1, 2, 4, 8");

    println!();

    let report = calibrated_transfer::receive_once(listener, &destination_root)?;

    report.print();

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
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
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
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        )
        .into());
    }

    windows_setup::prepare_receiver(bind_address)?;

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
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
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

fn run_multistream_update_bench(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn Error>> {
    let source_root =
        PathBuf::from(required_argument(
            arguments,
            "source root directory",
        )?);

    let destination_root =
        PathBuf::from(required_argument(
            arguments,
            "destination root directory",
        )?);

    let worker_count =
        match arguments.next() {
            Some(value) => {
                parse_buffer_count(
                    &value,
                )?
            }

            None => {
                manifest_scan::
                    default_worker_count()
            }
        };

    let data_stream_count =
        match arguments.next() {
            Some(value) => {
                parse_buffer_count(
                    &value,
                )?
            }

            None => {
                multistream_copy::
                    DEFAULT_DATA_STREAMS
            }
        };

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected extra argument: {}",
                extra.to_string_lossy(),
            ),
        )
        .into());
    }

    manifest_scan::
        validate_worker_count(
            worker_count,
        )?;

    control_plane::
        validate_data_stream_count(
            data_stream_count,
        )?;

    println!(
        "NetworkCopy Speed Edition protocol-v7 CDC update",
    );

    println!(
        "  Source:       {}",
        source_root.display(),
    );

    println!(
        "  Destination:  {}",
        destination_root.display(),
    );

    println!(
        "  Scan workers: {worker_count}",
    );

    println!(
        "  Data streams: {data_stream_count}",
    );

    println!();

    let report =
        multistream_copy::run_update(
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
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
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

fn parse_usize_count(value: &OsStr, description: &str) -> io::Result<usize> {
    let parsed = parse_u64_count(value, description)?;

    usize::try_from(parsed).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} is larger than usize"),
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
    println!("  {program} --version");
    println!("  {program} management-agent");
    println!("  {program} management-discover");
    println!(
        "  {program} management-notification-test <info|warning|error>"
    );
    println!(
        "  {program} management-update-check [--open]"
    );
    println!("  {program} management-hello <agent-address>");
    println!("  {program} management-roots <agent-address>");
    println!(
        "  {program} management-list <agent-address> <remote-directory>"
    );
    println!(
        "  {program} management-prepare-receive <agent-address> <destination-root> [--update]"
    );
    println!(
        "  {program} management-transfer <sender-agent> \
         <receiver-agent> <source-root> <destination-root> \
         [workers] [calibration-mib] [--update]"
    );
    println!(
        "  {program} management-start-send <sender-agent> \
         <receiver-address> <source-root> [workers] [calibration-mib]"
    );
    println!(
        "  {program} management-snapshot <agent-address>"
    );
    println!(
        "  {program} management-job-status <agent-address>"
    );
    println!(
        "  {program} management-cancel <agent-address> <job-id>"
    );
    println!("  {program} direct-interfaces");
    println!("  {program} direct-address <interface-index>");
    println!("  {program} direct-discovery-receive <interface-index>");
    println!("  {program} direct-discovery-receive-auto");
    println!("  {program} direct-discovery-send <interface-index>");
    println!("  {program} direct-discovery-send-auto");
    println!("  {program} direct-tcp-receive");
    println!("  {program} direct-tcp-send");
    println!("  {program} direct-receive <destination-root>");
    println!("  {program} direct-send <source-root> [workers] [calibration-mib]");
    println!("  {program} receive-auto <bind-address> <destination-root>");
    println!("  {program} send-auto <receiver-address> <source-root> [workers] [calibration-mib]");
    println!("  {program} bench-network-matrix-receive <bind-address>");
    println!("  {program} bench-network-matrix-send <receiver-address> [total-mib]");
    println!(
        "  {program} bench-network-receive <bind-address> \
         [receive-buffer-kib]"
    );
    println!(
        "  {program} bench-network-send <receiver-address> \
         [total-mib] [data-streams] [send-buffer-kib]"
    );
    println!("  {program} receive <bind-address> <destination-root>");
    println!("  {program} send <receiver-address> <source-root> [workers] [data-streams]");
    println!("  {program} bench-copy <source> <destination> [buffer-mib]");
    println!("  {program} bench-hash <source> [buffer-mib]");
    println!("  {program} probe-compression <source> [zstd-level]");
    println!(
        "  {program} probe-storage-media <path>"
    );
    println!(
        "  {program} bench-zstd-dictionary <root-directory> \
         [dictionary-kib] [zstd-level] [workers]"
    );
    println!(
        "  {program} bench-tiny-file-writes <source-root> <empty-output-root> \
         [max-workers] [scan-workers]"
    );
    println!(
        "  {program} bench-fixed-dedup <basis-file> \
         <candidate-file> [block-kib]"
    );
    println!(
        "  {program} bench-cdc-dedup <basis-file> \
         <candidate-file> [average-kib]"
    );
    println!(
        "  {program} bench-dedup-matrix <basis-file> \
         <candidate-file>"
    );
    println!(
        "  {program} bench-cdc-index <basis-file> \
         [average-kib]"
    );
    println!(
        "  {program} bench-cdc-reconstruct <basis-file> \
         <candidate-file> <output-file> [average-kib]"
    );
    println!(
        "  {program} bench-cdc-folder-plan <source-root> \
         <destination-root> [average-kib] [minimum-file-mib] \
         [maximum-literal-mib]"
    );
    println!("  {program} bench-pipeline <source> <destination> [chunk-mib] [buffers]");
    println!("  {program} probe-iocp");
    println!("  {program} probe-overlapped-read <source> [read-mib]");
    println!(
        "  {program} bench-blocking-read <source> \
         [chunk-mib]"
    );
    println!(
        "  {program} bench-iocp-read-ahead <source> \
         [chunk-mib] [operations]"
    );
    println!("  {program} bench-iocp-copy <source> <destination> [chunk-mib] [operations]");
    println!("  {program} bench-scan <root-directory> [workers]");
    println!("  {program} probe-control <root-directory> [workers] [data-streams]");
    println!(
        "  {program} bench-multistream-copy <source-root> <destination-root> \
         [workers] [data-streams]"
    );
    println!(
        "  {program} bench-multistream-update <source-root> \
         <destination-root> [workers] [data-streams]"
    );
    println!("  {program} bench-striped-file <source> <destination> [data-streams]");
    println!();
    println!(
        "The pipeline defaults to {} MiB chunks and {} reusable buffers.",
        pipeline_bench::DEFAULT_CHUNK_MIB,
        pipeline_bench::DEFAULT_BUFFER_COUNT
    );
}

#[cfg(test)]
mod tests {
    use super::format_interface_indices;

    #[test]
    fn interface_indices_are_formatted_for_diagnostics() {
        assert_eq!(
            format_interface_indices(&[3, 10, 42]),
            "3, 10, 42",
        );

        assert_eq!(
            format_interface_indices(&[]),
            "",
        );
    }
}
