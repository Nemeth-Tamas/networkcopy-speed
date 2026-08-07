use crate::desktop_layout::{
    DESKTOP_LAYOUT_FORMAT_VERSION, DesktopItemKind, DesktopLayoutItem, DesktopLayoutSnapshot,
    DesktopMonitor, DesktopPoint, DesktopRect, MAX_DESKTOP_LAYOUT_ITEMS,
    MAX_DESKTOP_LAYOUT_MONITORS,
};
use crate::destination_layout::validate_windows_path_component;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::mem::size_of;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, IServiceProvider,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor,
    MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, FOLDERID_Desktop, FOLDERVIEWMODE, IFolderView2, IShellBrowser, IShellItem,
    IShellWindows, KF_FLAG_DEFAULT, SHGetKnownFolderPath, SID_STopLevelBrowser,
    SIGDN_PARENTRELATIVEPARSING, SVGIO_ALLVIEW, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::core::{BOOL, Error as WindowsError, Interface, PWSTR};

const FOLDER_FLAG_AUTO_ARRANGE_MASK: u32 = 0x0000_0001;

const MONITOR_INFO_PRIMARY_MASK: u32 = 0x0000_0001;

const FILE_ATTRIBUTE_REPARSE_POINT_MASK: u32 = 0x0000_0400;

pub fn current_desktop_path() -> io::Result<PathBuf> {
    let raw_path = unsafe { SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None) }
        .map_err(|error| {
            windows_error(
                "failed to resolve the current Windows Desktop folder",
                error,
            )
        })?;

    let raw_path = OwnedCoTaskWide::new(raw_path, "Windows Desktop folder path")?;

    let path = raw_path.to_string("Windows Desktop folder path")?;

    if path.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows returned an empty Desktop folder path",
        ));
    }

    Ok(PathBuf::from(path))
}

pub fn capture_current_desktop_layout() -> io::Result<DesktopLayoutSnapshot> {
    let capture_thread = thread::Builder::new()
        .name("networkcopy-desktop-capture".to_string())
        .spawn(capture_on_sta_thread)?;

    capture_thread
        .join()
        .map_err(|_| io::Error::other("Windows Desktop capture thread panicked"))?
}

fn capture_on_sta_thread() -> io::Result<DesktopLayoutSnapshot> {
    let _dpi_awareness = ThreadDpiAwareness::per_monitor_v2()?;

    let _apartment = ComApartment::initialize()?;

    let desktop_path = current_desktop_path()?;

    let folder_view = current_desktop_folder_view()?;

    let (icon_size, auto_arrange) = capture_view_settings(&folder_view)?;

    let monitors = capture_monitors()?;

    let items = capture_desktop_items(&folder_view, &desktop_path)?;

    let snapshot = DesktopLayoutSnapshot {
        version: DESKTOP_LAYOUT_FORMAT_VERSION,

        icon_size,

        auto_arrange,

        monitors,

        items,
    };

    snapshot.validate()?;

    Ok(snapshot)
}

fn current_desktop_folder_view() -> io::Result<IFolderView2> {
    let shell_windows: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }
        .map_err(|error| windows_error("failed to connect to Windows ShellWindows", error))?;

    let desktop_location = VARIANT::from(CSIDL_DESKTOP as i32);

    let empty_root = VARIANT::default();

    let mut shell_window = 0_i32;

    let dispatch = unsafe {
        shell_windows.FindWindowSW(
            &desktop_location,
            &empty_root,
            SWC_DESKTOP,
            &mut shell_window,
            SWFO_NEEDDISPATCH,
        )
    }
    .map_err(|error| {
        windows_error(
            "failed to locate the live Windows Desktop Shell window",
            error,
        )
    })?;

    let service_provider: IServiceProvider = dispatch.cast().map_err(|error| {
        windows_error(
            "Windows Desktop Shell window did not expose IServiceProvider",
            error,
        )
    })?;

    let shell_browser: IShellBrowser =
        unsafe { service_provider.QueryService(&SID_STopLevelBrowser) }
            .map_err(|error| windows_error("failed to query the Desktop Shell browser", error))?;

    let shell_view = unsafe { shell_browser.QueryActiveShellView() }
        .map_err(|error| windows_error("failed to query the active Desktop Shell view", error))?;

    let folder_view: IFolderView2 = shell_view
        .cast()
        .map_err(|error| windows_error("Desktop Shell view did not expose IFolderView2", error))?;

    Ok(folder_view)
}

fn capture_view_settings(folder_view: &IFolderView2) -> io::Result<(u32, bool)> {
    let folder_flags = unsafe { folder_view.GetCurrentFolderFlags() }
        .map_err(|error| windows_error("failed to read Desktop folder-view flags", error))?;

    let auto_arrange = folder_flags & FOLDER_FLAG_AUTO_ARRANGE_MASK != 0;

    let mut view_mode = FOLDERVIEWMODE::default();

    let mut icon_size = 0_i32;

    unsafe { folder_view.GetViewModeAndIconSize(&mut view_mode, &mut icon_size) }
        .map_err(|error| windows_error("failed to read Desktop icon size", error))?;

    let icon_size = u32::try_from(icon_size).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Windows returned negative Desktop icon size {icon_size}",),
        )
    })?;

    Ok((icon_size, auto_arrange))
}

fn capture_desktop_items(
    folder_view: &IFolderView2,
    desktop_path: &Path,
) -> io::Result<Vec<DesktopLayoutItem>> {
    let item_count = unsafe { folder_view.ItemCount(SVGIO_ALLVIEW) }
        .map_err(|error| windows_error("failed to enumerate Desktop Shell items", error))?;

    if item_count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Windows returned negative Desktop item count {item_count}",),
        ));
    }

    let mut items = Vec::new();

    for index in 0..item_count {
        let shell_item: IShellItem = unsafe { folder_view.GetItem(index) }.map_err(|error| {
            windows_error(
                &format!("failed to read Desktop Shell item {index}",),
                error,
            )
        })?;

        let name = shell_item_name(&shell_item, index)?;

        let Some(kind) = classify_physical_desktop_child(desktop_path, &name)? else {
            continue;
        };

        let pidl = unsafe { folder_view.Item(index) }.map_err(|error| {
            windows_error(
                &format!("failed to obtain Desktop item {index} identifier",),
                error,
            )
        })?;

        let pidl = OwnedPidl::new(pidl, index)?;

        let position =
            unsafe { folder_view.GetItemPosition(pidl.as_ptr()) }.map_err(|error| {
                windows_error(
                    &format!("failed to read position for Desktop item {name:?}",),
                    error,
                )
            })?;

        items.push(DesktopLayoutItem {
            name,

            kind,

            position: DesktopPoint {
                x: position.x,

                y: position.y,
            },
        });

        if items.len() > MAX_DESKTOP_LAYOUT_ITEMS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "live Desktop contains more than the supported {MAX_DESKTOP_LAYOUT_ITEMS} transferable items",
                ),
            ));
        }
    }

    Ok(items)
}

fn shell_item_name(shell_item: &IShellItem, index: i32) -> io::Result<String> {
    let raw_name =
        unsafe { shell_item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING) }.map_err(|error| {
            windows_error(
                &format!("failed to read parsing name for Desktop Shell item {index}",),
                error,
            )
        })?;

    let raw_name = OwnedCoTaskWide::new(raw_name, "Desktop item parsing name")?;

    raw_name.to_string("Desktop item parsing name")
}

fn classify_physical_desktop_child(
    desktop_path: &Path,
    name: &str,
) -> io::Result<Option<DesktopItemKind>> {
    if validate_windows_path_component(name, "desktop item name").is_err() {
        return Ok(None);
    }

    let path = desktop_path.join(name);

    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,

        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Explorer's Desktop view is a merged Shell namespace.
            // Virtual items and Public Desktop items are deliberately
            // omitted because this migration metadata belongs only to
            // the current user's physical Desktop folder.
            return Ok(None);
        }

        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("failed to inspect Desktop item {}: {error}", path.display(),),
            ));
        }
    };

    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_MASK != 0 {
        // The transfer engine deliberately rejects filesystem
        // reparse points, so layout metadata must not promise a
        // position for an item that will not be transferred.
        return Ok(None);
    }

    let file_type = metadata.file_type();

    if file_type.is_dir() {
        Ok(Some(DesktopItemKind::Directory))
    } else if file_type.is_file() {
        Ok(Some(DesktopItemKind::File))
    } else {
        Ok(None)
    }
}

fn capture_monitors() -> io::Result<Vec<DesktopMonitor>> {
    let mut context = MonitorCaptureContext {
        monitors: Vec::new(),

        error: None,
    };

    let result = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(capture_monitor),
            LPARAM((&mut context as *mut MonitorCaptureContext) as isize),
        )
    };

    if let Some(error) = context.error {
        return Err(error);
    }

    result
        .ok()
        .map_err(|error| windows_error("failed to enumerate Windows display monitors", error))?;

    if context.monitors.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows reported no active display monitors",
        ));
    }

    Ok(context.monitors)
}

struct MonitorCaptureContext {
    monitors: Vec<DesktopMonitor>,

    error: Option<io::Error>,
}

unsafe extern "system" fn capture_monitor(
    monitor: HMONITOR,
    _device_context: HDC,
    _monitor_rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let context_pointer = data.0 as *mut MonitorCaptureContext;

    if context_pointer.is_null() {
        return false.into();
    }

    let context = unsafe { &mut *context_pointer };

    if context.monitors.len() >= MAX_DESKTOP_LAYOUT_MONITORS {
        context.error = Some(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Windows reported more than the supported {MAX_DESKTOP_LAYOUT_MONITORS} monitors",
            ),
        ));

        return false.into();
    }

    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,

        ..Default::default()
    };

    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        context.error = Some(windows_error(
            "failed to read monitor geometry",
            WindowsError::from_thread(),
        ));

        return false.into();
    }

    let mut dpi_x = 0_u32;

    let mut dpi_y = 0_u32;

    if let Err(error) =
        unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
    {
        context.error = Some(windows_error("failed to read monitor DPI", error));

        return false.into();
    }

    context.monitors.push(DesktopMonitor {
        bounds: desktop_rect(info.rcMonitor),

        work_area: desktop_rect(info.rcWork),

        dpi_x,

        dpi_y,

        primary: info.dwFlags & MONITOR_INFO_PRIMARY_MASK != 0,
    });

    true.into()
}

fn desktop_rect(rect: RECT) -> DesktopRect {
    DesktopRect {
        left: rect.left,

        top: rect.top,

        right: rect.right,

        bottom: rect.bottom,
    }
}

fn windows_error(context: &str, error: WindowsError) -> io::Error {
    io::Error::other(format!("{context}: {error}"))
}

struct ThreadDpiAwareness {
    previous: *mut c_void,
}

impl ThreadDpiAwareness {
    fn per_monitor_v2() -> io::Result<Self> {
        let previous = unsafe {
            SetThreadDpiAwarenessContext(
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        };

        if previous.is_invalid() {
            return Err(windows_error(
                "failed to enable per-monitor DPI awareness for Desktop capture",
                WindowsError::from_thread(),
            ));
        }

        Ok(Self {
            previous: previous.0,
        })
    }
}

impl Drop for ThreadDpiAwareness {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(
                DPI_AWARENESS_CONTEXT(self.previous),
            );
        }
    }
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> io::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(|error| {
                windows_error("failed to initialize Desktop capture COM apartment", error)
            })?;

        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

struct OwnedPidl(*mut ITEMIDLIST);

impl OwnedPidl {
    fn new(pidl: *mut ITEMIDLIST, index: i32) -> io::Result<Self> {
        if pidl.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Windows returned a null identifier for Desktop item {index}",),
            ));
        }

        Ok(Self(pidl))
    }

    const fn as_ptr(&self) -> *const ITEMIDLIST {
        self.0.cast_const()
    }
}

impl Drop for OwnedPidl {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(Some(self.0.cast::<c_void>() as *const c_void));
        }
    }
}

struct OwnedCoTaskWide(PWSTR);

impl OwnedCoTaskWide {
    fn new(value: PWSTR, description: &str) -> io::Result<Self> {
        if value.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Windows returned a null {description}",),
            ));
        }

        Ok(Self(value))
    }

    fn to_string(&self, description: &str) -> io::Result<String> {
        unsafe { self.0.to_string() }.map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{description} is not valid UTF-16: {error}",),
            )
        })
    }
}

impl Drop for OwnedCoTaskWide {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(Some(self.0.as_ptr().cast::<c_void>() as *const c_void));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{capture_current_desktop_layout, current_desktop_path};

    #[test]
    #[ignore = "queries the live Windows Explorer desktop"]
    fn live_desktop_capture_is_valid() {
        let desktop_path = current_desktop_path().unwrap();

        let snapshot = capture_current_desktop_layout().unwrap();

        snapshot.validate().unwrap();

        println!("Desktop path: {}", desktop_path.display(),);

        println!("Icon size: {} px", snapshot.icon_size,);

        println!("Auto Arrange: {}", snapshot.auto_arrange,);

        println!("Monitors: {}", snapshot.monitors.len(),);

        for (index, monitor) in snapshot.monitors.iter().enumerate() {
            println!(
                "  monitor {index}: bounds=({}, {})..({}, {}), work=({}, {})..({}, {}), dpi={}x{}, primary={}",
                monitor.bounds.left,
                monitor.bounds.top,
                monitor.bounds.right,
                monitor.bounds.bottom,
                monitor.work_area.left,
                monitor.work_area.top,
                monitor.work_area.right,
                monitor.work_area.bottom,
                monitor.dpi_x,
                monitor.dpi_y,
                monitor.primary,
            );
        }

        println!("Transferable Desktop items: {}", snapshot.items.len(),);

        for item in &snapshot.items {
            println!(
                "  {:?}: {:?} at ({}, {})",
                item.kind, item.name, item.position.x, item.position.y,
            );
        }
    }
}
