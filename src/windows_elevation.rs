use std::ffi::{OsStr, c_void};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};

type Bool = i32;
type Dword = u32;
type Handle = *mut c_void;

const TOKEN_QUERY: Dword = 0x0008;

const TOKEN_ELEVATION_CLASS: Dword = 20;

const SW_SHOWNORMAL: i32 = 1;

#[repr(C)]
#[derive(Default)]
struct TokenElevation {
    token_is_elevated: Dword,
}

struct OwnedHandle(Handle);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = close_handle(self.0);
        }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "GetCurrentProcess"]
    fn get_current_process() -> Handle;

    #[link_name = "CloseHandle"]
    fn close_handle(handle: Handle) -> Bool;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    #[link_name = "OpenProcessToken"]
    fn open_process_token(process: Handle, desired_access: Dword, token: *mut Handle) -> Bool;

    #[link_name = "GetTokenInformation"]
    fn get_token_information(
        token: Handle,
        information_class: Dword,
        information: *mut c_void,
        information_length: Dword,
        returned_length: *mut Dword,
    ) -> Bool;
}

#[link(name = "shell32")]
unsafe extern "system" {
    #[link_name = "ShellExecuteW"]
    fn shell_execute_w(
        window: Handle,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show_command: i32,
    ) -> Handle;
}

pub fn is_elevated() -> io::Result<bool> {
    let mut token = null_mut();

    let opened = unsafe { open_process_token(get_current_process(), TOKEN_QUERY, &mut token) };

    if opened == 0 {
        return Err(io::Error::last_os_error());
    }

    let token = OwnedHandle(token);

    let mut elevation = TokenElevation::default();

    let mut returned_length = 0;

    let information_length = u32::try_from(size_of::<TokenElevation>())
        .expect("TokenElevation size fits in a Windows DWORD");

    let queried = unsafe {
        get_token_information(
            token.0,
            TOKEN_ELEVATION_CLASS,
            (&mut elevation as *mut TokenElevation).cast(),
            information_length,
            &mut returned_length,
        )
    };

    if queried == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(elevation.token_is_elevated != 0)
}

pub fn relaunch_elevated(argument: &OsStr) -> io::Result<()> {
    let executable = std::env::current_exe()?;

    let operation = wide(OsStr::new("runas"));

    let executable = wide(executable.as_os_str());

    let parameters = wide(argument);

    let result = unsafe {
        shell_execute_w(
            null_mut(),
            operation.as_ptr(),
            executable.as_ptr(),
            parameters.as_ptr(),
            null(),
            SW_SHOWNORMAL,
        )
    } as isize;

    if result <= 32 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Windows elevation request failed with ShellExecuteW result {result}",),
        ));
    }

    Ok(())
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::wide;
    use std::ffi::OsStr;

    #[test]
    fn wide_string_is_nul_terminated() {
        assert_eq!(
            wide(OsStr::new("runas",),),
            [
                b'r' as u16,
                b'u' as u16,
                b'n' as u16,
                b'a' as u16,
                b's' as u16,
                0,
            ],
        );
    }
}
