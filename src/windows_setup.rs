use std::env;
use std::ffi::OsString;
use std::io;
use std::mem::size_of;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const FIREWALL_RULE_PREFIX: &str = "NetworkCopy Speed Edition TCP";

pub fn prepare_receiver(bind_address: SocketAddr) -> io::Result<()> {
    require_administrator()?;

    ensure_firewall_rule(bind_address.port())
}

fn require_administrator() -> io::Result<()> {
    if is_process_elevated()? {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "receiver commands require an elevated terminal so NetworkCopy can configure Windows Firewall; reopen PowerShell with Run as administrator and rerun the command",
    ))
}

fn is_process_elevated() -> io::Result<bool> {
    let mut token: HANDLE = null_mut();

    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };

    if opened == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };

    let information_length = u32::try_from(size_of::<TOKEN_ELEVATION>())
        .map_err(|_| io::Error::other("token elevation structure size cannot be represented"))?;

    let mut returned_length = 0_u32;

    let queried = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            information_length,
            &mut returned_length,
        )
    };

    let query_error = if queried == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };

    let closed = unsafe { CloseHandle(token) };

    if let Some(error) = query_error {
        return Err(error);
    }

    if closed == 0 {
        return Err(io::Error::last_os_error());
    }

    if returned_length < information_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an incomplete token elevation structure",
        ));
    }

    Ok(elevation.TokenIsElevated != 0)
}

fn ensure_firewall_rule(port: u16) -> io::Result<()> {
    let executable = env::current_exe()?;

    let rule_name = firewall_rule_name(port);

    remove_existing_rule(&rule_name)?;

    let output = Command::new("netsh")
        .args(["advfirewall", "firewall", "add", "rule"])
        .args(firewall_rule_arguments(&rule_name, &executable, port))
        .output()?;

    if !output.status.success() {
        return Err(command_failure(
            "creating the Windows Firewall rule",
            &output.stdout,
            &output.stderr,
        ));
    }

    println!("Windows Firewall rule ready");

    println!("  Rule:          {rule_name}");

    println!("  Program:       {}", executable.display());

    println!("  Inbound TCP:   {port}");

    println!("  Remote scope:  local subnet");

    println!();

    Ok(())
}

fn remove_existing_rule(rule_name: &str) -> io::Result<()> {
    let output = Command::new("netsh")
        .args(["advfirewall", "firewall", "delete", "rule"])
        .arg(format!("name={rule_name}"))
        .output()?;

    // A nonzero exit status is expected when
    // this is the first run and the rule does
    // not exist yet. The following add command
    // is the authoritative operation.
    let _ = output;

    Ok(())
}

fn firewall_rule_name(port: u16) -> String {
    format!("{FIREWALL_RULE_PREFIX} {port}")
}

fn firewall_rule_arguments(rule_name: &str, executable: &Path, port: u16) -> Vec<OsString> {
    let mut program = OsString::from("program=");

    program.push(executable.as_os_str());

    vec![
        OsString::from(format!("name={rule_name}")),
        OsString::from("dir=in"),
        OsString::from("action=allow"),
        OsString::from("protocol=TCP"),
        OsString::from(format!("localport={port}")),
        program,
        OsString::from("profile=any"),
        OsString::from("remoteip=localsubnet"),
        OsString::from("enable=yes"),
    ]
}

fn command_failure(action: &str, stdout: &[u8], stderr: &[u8]) -> io::Error {
    let stderr = String::from_utf8_lossy(stderr);

    let stdout = String::from_utf8_lossy(stdout);

    let details = if !stderr.trim().is_empty() {
        stderr.trim()
    } else if !stdout.trim().is_empty() {
        stdout.trim()
    } else {
        "netsh returned no diagnostic output"
    };

    io::Error::other(format!("failed while {action}: {details}"))
}

#[cfg(test)]
mod tests {
    use super::{firewall_rule_arguments, firewall_rule_name};
    use std::path::Path;

    #[test]
    fn firewall_rule_is_scoped_to_program_port_and_subnet() {
        let rule_name = firewall_rule_name(7337);

        assert_eq!(rule_name, "NetworkCopy Speed Edition TCP 7337");

        let arguments = firewall_rule_arguments(
            &rule_name,
            Path::new(r"C:\Program Files\NetworkCopy\networkcopy-speed.exe"),
            7337,
        );

        let rendered: Vec<String> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        assert!(rendered.contains(&"localport=7337".to_string(),));

        assert!(rendered.contains(&"protocol=TCP".to_string(),));

        assert!(rendered.contains(&"profile=any".to_string(),));

        assert!(rendered.contains(&"remoteip=localsubnet".to_string(),));

        assert!(
            rendered.iter().any(|argument| {
                argument.starts_with("program=C:\\Program Files\\NetworkCopy")
            },)
        );
    }
}
