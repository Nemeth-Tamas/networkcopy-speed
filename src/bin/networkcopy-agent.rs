use networkcopy_speed::management_agent;
use networkcopy_speed::management_control;
use networkcopy_speed::management_discovery::AgentState;
use networkcopy_speed::management_protocol::MANAGEMENT_CONTROL_PORT;
use networkcopy_speed::windows_elevation;
use std::env;
use std::ffi::{OsStr, c_void};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::windows::ffi::OsStrExt;
use std::process;
use std::ptr::null;
use std::thread;
use std::time::{Duration, Instant};

const ELEVATED_ARGUMENT: &str = "--elevated";

const RESTART_EVENT_NAME: &str = "Local\\NetworkCopySpeedEditionAgentRestartV1";

const RESTART_TIMEOUT: Duration = Duration::from_secs(10);

const RESTART_POLL_INTERVAL: Duration = Duration::from_millis(100);

const EVENT_MODIFY_STATE: u32 = 0x0002;

const SYNCHRONIZE: u32 = 0x0010_0000;

const INFINITE: u32 = 0xFFFF_FFFF;

const WAIT_OBJECT_0: u32 = 0;

const ERROR_FILE_NOT_FOUND: i32 = 2;

const ERROR_ALREADY_EXISTS: i32 = 183;

type Bool = i32;
type Dword = u32;
type Handle = *mut c_void;

struct OwnedHandle(Handle);

unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    fn raw(&self) -> Handle {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = close_handle(self.0);
        }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "CreateEventW"]
    fn create_event_w(
        event_attributes: *const c_void,
        manual_reset: Bool,
        initial_state: Bool,
        name: *const u16,
    ) -> Handle;

    #[link_name = "OpenEventW"]
    fn open_event_w(desired_access: Dword, inherit_handle: Bool, name: *const u16) -> Handle;

    #[link_name = "SetEvent"]
    fn set_event(event: Handle) -> Bool;

    #[link_name = "WaitForSingleObject"]
    fn wait_for_single_object(handle: Handle, milliseconds: Dword) -> Dword;

    #[link_name = "CloseHandle"]
    fn close_handle(handle: Handle) -> Bool;
}

fn main() {
    if let Err(error) = run() {
        eprintln!();

        eprintln!("NetworkCopy Agent could not start:");

        eprintln!("  {error}");

        eprintln!();

        eprintln!("Press Enter to close this window.");

        let mut input = String::new();

        let _ = io::stdin().read_line(&mut input);

        process::exit(1);
    }
}

fn run() -> io::Result<()> {
    validate_arguments()?;

    if !windows_elevation::is_elevated()? {
        println!("NetworkCopy Agent requires administrator permission.");

        println!("Opening the Windows elevation prompt...");

        windows_elevation::relaunch_elevated(OsStr::new(ELEVATED_ARGUMENT))?;

        return Ok(());
    }

    restart_existing_agent_if_needed()?;

    let restart_event = create_restart_event()?;

    start_restart_watcher(restart_event)?;

    println!("NetworkCopy Agent launcher");

    println!("  Administrator: yes");

    println!("  Restart mode:  enabled");

    println!();

    println!("Double-click this executable again to restart an idle agent.");

    println!("An agent with an active transfer will not be restarted.");

    println!();

    management_agent::run()
}

fn validate_arguments() -> io::Result<()> {
    let mut arguments = env::args_os();

    let _ = arguments.next();

    match arguments.next() {
        None => {}

        Some(argument) if argument == OsStr::new(ELEVATED_ARGUMENT) => {}

        Some(argument) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unexpected argument: {}", argument.to_string_lossy(),),
            ));
        }
    }

    if let Some(extra) = arguments.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected extra argument: {}", extra.to_string_lossy(),),
        ));
    }

    Ok(())
}

fn restart_existing_agent_if_needed() -> io::Result<()> {
    let Some(restart_event) = open_restart_event()? else {
        if let Ok(agent) = management_control::hello(local_agent_endpoint()) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "management agent {} is already running, but it was not started by networkcopy-agent.exe; close its existing console once, then launch this executable again",
                    agent.hostname,
                ),
            ));
        }

        return Ok(());
    };

    let agent =
        management_control::hello(
            local_agent_endpoint(),
        )
        .map_err(|error| {
            io::Error::other(format!(
                "an existing dedicated agent was detected, but its state could not be verified; refusing to restart it: {error}"
            ))
        })?;

    if agent.state == AgentState::Busy {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "management agent {} currently has an active transfer; restart was refused so the transfer remains untouched",
                agent.hostname,
            ),
        ));
    }

    println!("Existing idle NetworkCopy Agent detected.");

    println!("Requesting a clean process restart...");

    signal_restart_event(&restart_event)?;

    drop(restart_event);

    wait_for_restart_event_to_close()?;

    thread::sleep(RESTART_POLL_INTERVAL);

    println!("Previous agent stopped.");

    println!();

    Ok(())
}

fn local_agent_endpoint() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::LOCALHOST,
        MANAGEMENT_CONTROL_PORT,
    ))
}

fn create_restart_event() -> io::Result<OwnedHandle> {
    let name = wide(OsStr::new(RESTART_EVENT_NAME));

    let handle = unsafe { create_event_w(null(), 1, 0, name.as_ptr()) };

    let last_error = io::Error::last_os_error();

    if handle.is_null() {
        return Err(last_error);
    }

    let handle = OwnedHandle(handle);

    if last_error.raw_os_error() == Some(ERROR_ALREADY_EXISTS) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "NetworkCopy Agent restart event still belongs to another process",
        ));
    }

    Ok(handle)
}

fn open_restart_event() -> io::Result<Option<OwnedHandle>> {
    let name = wide(OsStr::new(RESTART_EVENT_NAME));

    let handle = unsafe { open_event_w(EVENT_MODIFY_STATE | SYNCHRONIZE, 0, name.as_ptr()) };

    if !handle.is_null() {
        return Ok(Some(OwnedHandle(handle)));
    }

    let error = io::Error::last_os_error();

    if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) {
        Ok(None)
    } else {
        Err(error)
    }
}

fn signal_restart_event(event: &OwnedHandle) -> io::Result<()> {
    let signalled = unsafe { set_event(event.raw()) };

    if signalled == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn wait_for_restart_event_to_close() -> io::Result<()> {
    let deadline = Instant::now()
        .checked_add(RESTART_TIMEOUT)
        .ok_or_else(|| io::Error::other("agent restart deadline overflowed"))?;

    loop {
        if open_restart_event()?.is_none() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the previous NetworkCopy Agent did not stop within ten seconds",
            ));
        }

        thread::sleep(RESTART_POLL_INTERVAL);
    }
}

fn start_restart_watcher(event: OwnedHandle) -> io::Result<()> {
    thread::Builder::new()
        .name("networkcopy-agent-restart".to_string())
        .spawn(move || {
            let wait_result = unsafe { wait_for_single_object(event.raw(), INFINITE) };

            if wait_result == WAIT_OBJECT_0 {
                println!();

                println!("Restart requested.");

                println!("Stopping the idle agent...");

                process::exit(0);
            }

            eprintln!();

            eprintln!("Agent restart watcher failed with wait result 0x{wait_result:08X}.");

            process::exit(1);
        })?;

    Ok(())
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}
