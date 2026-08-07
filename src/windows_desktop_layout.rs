use crate::desktop_layout::{
    DESKTOP_LAYOUT_FORMAT_VERSION, DesktopItemKind, DesktopLayoutItem, DesktopLayoutSnapshot,
    DesktopMonitor, DesktopPoint, DesktopRect, MAX_DESKTOP_LAYOUT_ITEMS,
    MAX_DESKTOP_LAYOUT_MONITORS,
};
use crate::destination_layout::validate_windows_path_component;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::mem::size_of;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use windows::Win32::Foundation::{LPARAM, POINT, RECT};
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
    IShellView, IShellWindows, KF_FLAG_DEFAULT, SHGetKnownFolderPath, SID_STopLevelBrowser,
    SIGDN_PARENTRELATIVEPARSING, SVGIO_ALLVIEW, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::core::{BOOL, Error as WindowsError, Interface, PWSTR};

const FOLDER_FLAG_AUTO_ARRANGE_MASK: u32 = 0x0000_0001;

const MONITOR_INFO_PRIMARY_MASK: u32 = 0x0000_0001;

const FILE_ATTRIBUTE_REPARSE_POINT_MASK: u32 = 0x0000_0400;

const SELECT_AND_POSITION_ITEMS_FLAGS: u32 = 0x0000_0080;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopLayoutRestoreOutcome {
    Applied,

    SkippedDestinationNotDesktop,

    SkippedDisplayMismatch,

    SkippedAutoArrangeMismatch,

    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopLayoutRestoreReport {
    pub outcome: DesktopLayoutRestoreOutcome,

    pub matched_items: usize,

    pub missing_items: usize,
}

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

pub fn is_current_desktop_path(path: &Path) -> io::Result<bool> {
    if path.as_os_str().is_empty() {
        return Ok(false);
    }

    let selected = fs::canonicalize(path)?;

    let desktop = fs::canonicalize(current_desktop_path()?)?;

    Ok(selected == desktop)
}

pub fn capture_current_desktop_layout() -> io::Result<DesktopLayoutSnapshot> {
    let capture_thread = thread::Builder::new()
        .name("networkcopy-desktop-capture".to_string())
        .spawn(capture_on_sta_thread)?;

    capture_thread
        .join()
        .map_err(|_| io::Error::other("Windows Desktop capture thread panicked"))?
}

pub fn restore_current_desktop_layout(
    snapshot: &DesktopLayoutSnapshot,
) -> io::Result<DesktopLayoutRestoreReport> {
    snapshot.validate()?;

    let snapshot = snapshot.clone();

    let restore_thread = thread::Builder::new()
        .name("networkcopy-desktop-restore".to_string())
        .spawn(move || restore_on_sta_thread(&snapshot))?;

    restore_thread
        .join()
        .map_err(|_| io::Error::other("Windows Desktop restore thread panicked"))?
}

fn restore_on_sta_thread(
    snapshot: &DesktopLayoutSnapshot,
) -> io::Result<DesktopLayoutRestoreReport> {
    let _dpi_awareness = ThreadDpiAwareness::per_monitor_v2()?;

    let _apartment = ComApartment::initialize()?;

    let current_monitors = capture_monitors()?;

    let desktop_path = current_desktop_path()?;

    let folder_view = current_desktop_folder_view()?;

    let (_, current_auto_arrange) = capture_view_settings(&folder_view)?;

    if snapshot.auto_arrange != current_auto_arrange {
        return Ok(DesktopLayoutRestoreReport {
            outcome: DesktopLayoutRestoreOutcome::SkippedAutoArrangeMismatch,

            matched_items: 0,

            missing_items: snapshot.items.len(),
        });
    }

    let planned_positions = if snapshot.auto_arrange {
        HashMap::new()
    } else {
        let Some(positions) = plan_desktop_positions(snapshot, &current_monitors) else {
            return Ok(DesktopLayoutRestoreReport {
                outcome: DesktopLayoutRestoreOutcome::SkippedDisplayMismatch,

                matched_items: 0,

                missing_items: snapshot.items.len(),
            });
        };

        positions
    };

    let mut view_mode = FOLDERVIEWMODE::default();

    let mut current_icon_size = 0_i32;

    unsafe { folder_view.GetViewModeAndIconSize(&mut view_mode, &mut current_icon_size) }.map_err(
        |error| {
            windows_error(
                "failed to read current Desktop view mode before restore",
                error,
            )
        },
    )?;

    let target_icon_size = i32::try_from(snapshot.icon_size).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "captured Desktop icon size cannot be represented by Windows",
        )
    })?;

    if current_icon_size != target_icon_size {
        unsafe { folder_view.SetViewModeAndIconSize(view_mode, target_icon_size) }
            .map_err(|error| windows_error("failed to restore Desktop icon size", error))?;
    }

    let shell_view: IShellView = folder_view
        .cast()
        .map_err(|error| windows_error("Desktop folder view did not expose IShellView", error))?;

    let targets =
        collect_restore_targets(&folder_view, &desktop_path, snapshot, &planned_positions)?;

    let matched_items = targets.len();

    let missing_items = snapshot.items.len().saturating_sub(matched_items);

    if !snapshot.auto_arrange && !targets.is_empty() {
        let target_count = u32::try_from(targets.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Desktop restore target count cannot be represented",
            )
        })?;

        let pidls: Vec<*const ITEMIDLIST> =
            targets.iter().map(|target| target.pidl.as_ptr()).collect();

        let positions: Vec<POINT> = targets.iter().map(|target| target.position).collect();

        unsafe {
            folder_view.SelectAndPositionItems(
                target_count,
                pidls.as_ptr(),
                Some(positions.as_ptr()),
                SELECT_AND_POSITION_ITEMS_FLAGS,
            )
        }
        .map_err(|error| windows_error("failed to restore Desktop icon positions", error))?;
    }

    unsafe { shell_view.SaveViewState() }.map_err(|error| {
        windows_error("failed to persist the restored Desktop view state", error)
    })?;

    Ok(DesktopLayoutRestoreReport {
        outcome: DesktopLayoutRestoreOutcome::Applied,

        matched_items,

        missing_items,
    })
}

fn same_monitor_environment(expected: &[DesktopMonitor], current: &[DesktopMonitor]) -> bool {
    expected.len() == current.len()
        && expected
            .iter()
            .all(|monitor| current.iter().any(|candidate| candidate == monitor))
}

fn plan_desktop_positions(
    snapshot: &DesktopLayoutSnapshot,
    current_monitors: &[DesktopMonitor],
) -> Option<HashMap<String, DesktopPoint>> {
    if snapshot.monitors.len() != current_monitors.len() || snapshot.monitors.is_empty() {
        return None;
    }

    if same_monitor_environment(&snapshot.monitors, current_monitors) {
        return Some(
            snapshot
                .items
                .iter()
                .map(|item| (item.name.clone(), item.position))
                .collect(),
        );
    }

    let source_order = ordered_monitor_indices(&snapshot.monitors)?;

    let target_order = ordered_monitor_indices(current_monitors)?;

    if source_order.len() != target_order.len() {
        return None;
    }

    let source_virtual = virtual_monitor_bounds(&snapshot.monitors)?;

    let target_virtual = virtual_monitor_bounds(current_monitors)?;

    let mut monitor_mapping = HashMap::with_capacity(source_order.len());

    for (source_index, target_index) in source_order.into_iter().zip(target_order) {
        monitor_mapping.insert(source_index, target_index);
    }

    let icon_extent = i32::try_from(snapshot.icon_size).ok()?;

    let mut positions = HashMap::with_capacity(snapshot.items.len());

    for item in &snapshot.items {
        let source_monitor_index =
            monitor_for_desktop_point(item.position, &snapshot.monitors, source_virtual)?;

        let target_monitor_index = *monitor_mapping.get(&source_monitor_index)?;

        let source_monitor = snapshot.monitors[source_monitor_index];

        let target_monitor = current_monitors[target_monitor_index];

        let mapped = map_desktop_point(
            item.position,
            source_monitor,
            target_monitor,
            source_virtual,
            target_virtual,
            icon_extent,
        );

        positions.insert(item.name.clone(), mapped);
    }

    Some(positions)
}

fn ordered_monitor_indices(monitors: &[DesktopMonitor]) -> Option<Vec<usize>> {
    let primary_indices: Vec<usize> = monitors
        .iter()
        .enumerate()
        .filter_map(|(index, monitor)| monitor.primary.then_some(index))
        .collect();

    if primary_indices.len() != 1 {
        return None;
    }

    let primary_index = primary_indices[0];

    let primary = monitors[primary_index];

    let primary_center = monitor_center_twice(primary);

    let mut secondary: Vec<usize> = (0..monitors.len())
        .filter(|index| *index != primary_index)
        .collect();

    secondary.sort_by_key(|index| {
        let center = monitor_center_twice(monitors[*index]);

        let dx = center.0 - primary_center.0;

        let dy = center.1 - primary_center.1;

        (
            monitor_direction_sector(dx, dy),
            dx.abs().saturating_add(dy.abs()),
            dx,
            dy,
        )
    });

    let mut ordered = Vec::with_capacity(monitors.len());

    ordered.push(primary_index);

    ordered.extend(secondary);

    Some(ordered)
}

fn monitor_center_twice(monitor: DesktopMonitor) -> (i64, i64) {
    (
        i64::from(monitor.bounds.left) + i64::from(monitor.bounds.right),
        i64::from(monitor.bounds.top) + i64::from(monitor.bounds.bottom),
    )
}

fn monitor_direction_sector(dx: i64, dy: i64) -> u8 {
    if dx.abs() >= dy.abs() {
        if dx < 0 { 0 } else { 2 }
    } else if dy < 0 {
        1
    } else {
        3
    }
}

fn virtual_monitor_bounds(monitors: &[DesktopMonitor]) -> Option<DesktopRect> {
    let first = monitors.first()?;

    let mut bounds = first.bounds;

    for monitor in &monitors[1..] {
        bounds.left = bounds.left.min(monitor.bounds.left);

        bounds.top = bounds.top.min(monitor.bounds.top);

        bounds.right = bounds.right.max(monitor.bounds.right);

        bounds.bottom = bounds.bottom.max(monitor.bounds.bottom);
    }

    Some(bounds)
}

fn monitor_for_desktop_point(
    point: DesktopPoint,
    monitors: &[DesktopMonitor],
    virtual_bounds: DesktopRect,
) -> Option<usize> {
    let mut nearest = None;

    for (index, monitor) in monitors.iter().enumerate() {
        let bounds = translate_to_view_space(monitor.bounds, virtual_bounds);

        if point_in_rect(point, bounds) {
            return Some(index);
        }

        let distance = point_distance_squared(point, bounds);

        match nearest {
            Some((best_distance, _)) if best_distance <= distance => {}

            _ => {
                nearest = Some((distance, index));
            }
        }
    }

    nearest.map(|(_, index)| index)
}

fn translate_to_view_space(rect: DesktopRect, virtual_bounds: DesktopRect) -> DesktopRect {
    DesktopRect {
        left: rect.left - virtual_bounds.left,

        top: rect.top - virtual_bounds.top,

        right: rect.right - virtual_bounds.left,

        bottom: rect.bottom - virtual_bounds.top,
    }
}

fn point_in_rect(point: DesktopPoint, rect: DesktopRect) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

fn point_distance_squared(point: DesktopPoint, rect: DesktopRect) -> i64 {
    let x = i64::from(point.x);

    let y = i64::from(point.y);

    let left = i64::from(rect.left);

    let top = i64::from(rect.top);

    let right = i64::from(rect.right.saturating_sub(1));

    let bottom = i64::from(rect.bottom.saturating_sub(1));

    let dx = if x < left {
        left - x
    } else if x > right {
        x - right
    } else {
        0
    };

    let dy = if y < top {
        top - y
    } else if y > bottom {
        y - bottom
    } else {
        0
    };

    dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
}

fn map_desktop_point(
    point: DesktopPoint,
    source_monitor: DesktopMonitor,
    target_monitor: DesktopMonitor,
    source_virtual: DesktopRect,
    target_virtual: DesktopRect,
    icon_extent: i32,
) -> DesktopPoint {
    let source_work = translate_to_view_space(source_monitor.work_area, source_virtual);

    let target_work = translate_to_view_space(target_monitor.work_area, target_virtual);

    let mapped_x = scale_axis(
        point.x,
        source_work.left,
        source_work.right,
        target_work.left,
        target_work.right,
    );

    let mapped_y = scale_axis(
        point.y,
        source_work.top,
        source_work.bottom,
        target_work.top,
        target_work.bottom,
    );

    DesktopPoint {
        x: clamp_icon_axis(mapped_x, target_work.left, target_work.right, icon_extent),

        y: clamp_icon_axis(mapped_y, target_work.top, target_work.bottom, icon_extent),
    }
}

fn scale_axis(
    value: i32,
    source_start: i32,
    source_end: i32,
    target_start: i32,
    target_end: i32,
) -> i32 {
    let source_span = i64::from(
        source_end
            .saturating_sub(source_start)
            .saturating_sub(1)
            .max(1),
    );

    let target_span = i64::from(
        target_end
            .saturating_sub(target_start)
            .saturating_sub(1)
            .max(1),
    );

    let source_relative = i64::from(value.saturating_sub(source_start)).clamp(0, source_span);

    let scaled = source_relative.saturating_mul(target_span) / source_span;

    i32::try_from(i64::from(target_start).saturating_add(scaled)).unwrap_or_else(|_| {
        if scaled.is_negative() {
            i32::MIN
        } else {
            i32::MAX
        }
    })
}

fn clamp_icon_axis(value: i32, work_start: i32, work_end: i32, icon_extent: i32) -> i32 {
    let work_size = work_end.saturating_sub(work_start);

    let extent = icon_extent.clamp(1, work_size.max(1));

    let maximum = work_end.saturating_sub(extent).max(work_start);

    value.clamp(work_start, maximum)
}

struct RestoreTarget {
    pidl: OwnedPidl,

    position: POINT,
}

fn collect_restore_targets(
    folder_view: &IFolderView2,
    desktop_path: &Path,
    snapshot: &DesktopLayoutSnapshot,
    planned_positions: &HashMap<String, DesktopPoint>,
) -> io::Result<Vec<RestoreTarget>> {
    let expected: HashMap<&str, &DesktopLayoutItem> = snapshot
        .items
        .iter()
        .map(|item| (item.name.as_str(), item))
        .collect();

    let item_count = unsafe { folder_view.ItemCount(SVGIO_ALLVIEW) }.map_err(|error| {
        windows_error(
            "failed to enumerate Desktop Shell items during restore",
            error,
        )
    })?;

    if item_count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Windows returned negative Desktop item count {item_count} during restore",),
        ));
    }

    let mut targets = Vec::new();

    for index in 0..item_count {
        let shell_item: IShellItem = unsafe { folder_view.GetItem(index) }.map_err(|error| {
            windows_error(
                &format!("failed to read Desktop Shell item {index} during restore",),
                error,
            )
        })?;

        let name = shell_item_name(&shell_item, index)?;

        let Some(kind) = classify_physical_desktop_child(desktop_path, &name)? else {
            continue;
        };

        let Some(expected_item) = expected.get(name.as_str()) else {
            continue;
        };

        if expected_item.kind != kind {
            continue;
        }

        let Some(position) = planned_positions.get(&name) else {
            continue;
        };

        let pidl = unsafe { folder_view.Item(index) }.map_err(|error| {
            windows_error(
                &format!("failed to obtain Desktop item {index} identifier during restore",),
                error,
            )
        })?;

        let pidl = OwnedPidl::new(pidl, index)?;

        targets.push(RestoreTarget {
            pidl,

            position: POINT {
                x: position.x,

                y: position.y,
            },
        });
    }

    Ok(targets)
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

        let position = unsafe { folder_view.GetItemPosition(pidl.as_ptr()) }.map_err(|error| {
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
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

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
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT(self.previous));
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
    use super::{
        DesktopLayoutRestoreOutcome, capture_current_desktop_layout, current_desktop_path,
        plan_desktop_positions, point_in_rect, restore_current_desktop_layout,
        same_monitor_environment, translate_to_view_space, virtual_monitor_bounds,
    };
    use crate::desktop_layout::{
        DESKTOP_LAYOUT_FORMAT_VERSION, DesktopItemKind, DesktopLayoutItem, DesktopLayoutSnapshot,
        DesktopMonitor, DesktopPoint, DesktopRect,
    };

    fn synthetic_compact_monitor_environment(source: &[DesktopMonitor]) -> Vec<DesktopMonitor> {
        let primary = source
            .iter()
            .find(|monitor| monitor.primary)
            .expect("source Desktop must have a primary monitor");

        let origin_x = primary.bounds.left;

        let origin_y = primary.bounds.top;

        let scale_coordinate = |value: i32, origin: i32| -> i32 {
            let relative = i64::from(value) - i64::from(origin);

            i32::try_from(relative.saturating_mul(3) / 4).unwrap()
        };

        source
            .iter()
            .map(|monitor| DesktopMonitor {
                bounds: DesktopRect {
                    left: scale_coordinate(monitor.bounds.left, origin_x),

                    top: scale_coordinate(monitor.bounds.top, origin_y),

                    right: scale_coordinate(monitor.bounds.right, origin_x),

                    bottom: scale_coordinate(monitor.bounds.bottom, origin_y),
                },

                work_area: DesktopRect {
                    left: scale_coordinate(monitor.work_area.left, origin_x),

                    top: scale_coordinate(monitor.work_area.top, origin_y),

                    right: scale_coordinate(monitor.work_area.right, origin_x),

                    bottom: scale_coordinate(monitor.work_area.bottom, origin_y),
                },

                dpi_x: monitor.dpi_x.saturating_mul(3) / 2,

                dpi_y: monitor.dpi_y.saturating_mul(3) / 2,

                primary: monitor.primary,
            })
            .collect()
    }

    fn planned_position_is_visible(position: DesktopPoint, monitors: &[DesktopMonitor]) -> bool {
        let Some(virtual_bounds) = virtual_monitor_bounds(monitors) else {
            return false;
        };

        monitors.iter().any(|monitor| {
            let work_area = translate_to_view_space(monitor.work_area, virtual_bounds);

            point_in_rect(position, work_area)
        })
    }

    #[test]
    fn desktop_positions_scale_between_resolutions() {
        let snapshot = DesktopLayoutSnapshot {
            version: DESKTOP_LAYOUT_FORMAT_VERSION,

            icon_size: 48,

            auto_arrange: false,

            monitors: vec![DesktopMonitor {
                bounds: DesktopRect {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },

                work_area: DesktopRect {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1040,
                },

                dpi_x: 96,
                dpi_y: 96,
                primary: true,
            }],

            items: vec![DesktopLayoutItem {
                name: "middle.txt".to_string(),

                kind: DesktopItemKind::File,

                position: DesktopPoint { x: 960, y: 520 },
            }],
        };

        let current = [DesktopMonitor {
            bounds: DesktopRect {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1440,
            },

            work_area: DesktopRect {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1380,
            },

            dpi_x: 144,
            dpi_y: 144,
            primary: true,
        }];

        let positions = plan_desktop_positions(&snapshot, &current).unwrap();

        let position = positions.get("middle.txt").unwrap();

        assert!(
            (1279..=1281).contains(&position.x),
            "unexpected mapped x {}",
            position.x,
        );

        assert!(
            (689..=691).contains(&position.y),
            "unexpected mapped y {}",
            position.y,
        );
    }

    #[test]
    fn desktop_positions_preserve_monitor_sides() {
        let source_left = DesktopMonitor {
            bounds: DesktopRect {
                left: -1920,
                top: 360,
                right: 0,
                bottom: 1440,
            },

            work_area: DesktopRect {
                left: -1920,
                top: 360,
                right: 0,
                bottom: 1392,
            },

            dpi_x: 96,
            dpi_y: 96,
            primary: false,
        };

        let source_primary = DesktopMonitor {
            bounds: DesktopRect {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1440,
            },

            work_area: DesktopRect {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1392,
            },

            dpi_x: 120,
            dpi_y: 120,
            primary: true,
        };

        let source_right = DesktopMonitor {
            bounds: DesktopRect {
                left: 2560,
                top: 360,
                right: 4480,
                bottom: 1440,
            },

            work_area: DesktopRect {
                left: 2560,
                top: 360,
                right: 4480,
                bottom: 1392,
            },

            dpi_x: 96,
            dpi_y: 96,
            primary: false,
        };

        let snapshot = DesktopLayoutSnapshot {
            version: DESKTOP_LAYOUT_FORMAT_VERSION,

            icon_size: 48,

            auto_arrange: false,

            monitors: vec![source_right, source_primary, source_left],

            items: vec![
                DesktopLayoutItem {
                    name: "left.txt".to_string(),

                    kind: DesktopItemKind::File,

                    position: DesktopPoint { x: 200, y: 600 },
                },
                DesktopLayoutItem {
                    name: "primary.txt".to_string(),

                    kind: DesktopItemKind::File,

                    position: DesktopPoint { x: 2500, y: 600 },
                },
                DesktopLayoutItem {
                    name: "right.txt".to_string(),

                    kind: DesktopItemKind::File,

                    position: DesktopPoint { x: 4700, y: 600 },
                },
            ],
        };

        let target_left = DesktopMonitor {
            bounds: DesktopRect {
                left: -1280,
                top: 200,
                right: 0,
                bottom: 1224,
            },

            work_area: DesktopRect {
                left: -1280,
                top: 200,
                right: 0,
                bottom: 1176,
            },

            dpi_x: 120,
            dpi_y: 120,
            primary: false,
        };

        let target_primary = DesktopMonitor {
            bounds: DesktopRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },

            work_area: DesktopRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1032,
            },

            dpi_x: 96,
            dpi_y: 96,
            primary: true,
        };

        let target_right = DesktopMonitor {
            bounds: DesktopRect {
                left: 1920,
                top: 200,
                right: 3280,
                bottom: 1280,
            },

            work_area: DesktopRect {
                left: 1920,
                top: 200,
                right: 3280,
                bottom: 1232,
            },

            dpi_x: 144,
            dpi_y: 144,
            primary: false,
        };

        let positions =
            plan_desktop_positions(&snapshot, &[target_primary, target_left, target_right])
                .unwrap();

        let left = positions.get("left.txt").unwrap();

        let primary = positions.get("primary.txt").unwrap();

        let right = positions.get("right.txt").unwrap();

        assert!(left.x < 1280);

        assert!(primary.x >= 1280 && primary.x < 3200,);

        assert!(right.x >= 3200);
    }

    #[test]
    fn desktop_positions_are_clamped_visible() {
        let snapshot = DesktopLayoutSnapshot {
            version: DESKTOP_LAYOUT_FORMAT_VERSION,

            icon_size: 48,

            auto_arrange: false,

            monitors: vec![DesktopMonitor {
                bounds: DesktopRect {
                    left: 0,
                    top: 0,
                    right: 3840,
                    bottom: 2160,
                },

                work_area: DesktopRect {
                    left: 0,
                    top: 0,
                    right: 3840,
                    bottom: 2100,
                },

                dpi_x: 96,
                dpi_y: 96,
                primary: true,
            }],

            items: vec![DesktopLayoutItem {
                name: "edge.txt".to_string(),

                kind: DesktopItemKind::File,

                position: DesktopPoint {
                    x: 900_000,
                    y: 900_000,
                },
            }],
        };

        let current = [DesktopMonitor {
            bounds: DesktopRect {
                left: 0,
                top: 0,
                right: 1280,
                bottom: 720,
            },

            work_area: DesktopRect {
                left: 0,
                top: 0,
                right: 1280,
                bottom: 680,
            },

            dpi_x: 96,
            dpi_y: 96,
            primary: true,
        }];

        let positions = plan_desktop_positions(&snapshot, &current).unwrap();

        let position = positions.get("edge.txt").unwrap();

        assert!((0..=1232).contains(&position.x),);

        assert!((0..=632).contains(&position.y),);
    }

    #[test]
    fn different_monitor_counts_are_not_mapped() {
        let snapshot = DesktopLayoutSnapshot {
            version: DESKTOP_LAYOUT_FORMAT_VERSION,

            icon_size: 48,

            auto_arrange: false,

            monitors: vec![DesktopMonitor {
                bounds: DesktopRect {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },

                work_area: DesktopRect {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1040,
                },

                dpi_x: 96,
                dpi_y: 96,
                primary: true,
            }],

            items: Vec::new(),
        };

        let current = [
            snapshot.monitors[0],
            DesktopMonitor {
                bounds: DesktopRect {
                    left: 1920,
                    top: 0,
                    right: 3840,
                    bottom: 1080,
                },

                work_area: DesktopRect {
                    left: 1920,
                    top: 0,
                    right: 3840,
                    bottom: 1040,
                },

                dpi_x: 96,
                dpi_y: 96,
                primary: false,
            },
        ];

        assert!(plan_desktop_positions(&snapshot, &current,).is_none(),);
    }

    #[test]
    fn matching_monitor_environment_ignores_enumeration_order() {
        let primary = DesktopMonitor {
            bounds: DesktopRect {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1440,
            },

            work_area: DesktopRect {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1392,
            },

            dpi_x: 120,
            dpi_y: 120,
            primary: true,
        };

        let secondary = DesktopMonitor {
            bounds: DesktopRect {
                left: -1920,
                top: 360,
                right: 0,
                bottom: 1440,
            },

            work_area: DesktopRect {
                left: -1920,
                top: 360,
                right: 0,
                bottom: 1392,
            },

            dpi_x: 96,
            dpi_y: 96,
            primary: false,
        };

        assert!(same_monitor_environment(
            &[primary, secondary],
            &[secondary, primary],
        ));

        let mut changed = secondary;

        changed.dpi_x = 120;

        assert!(!same_monitor_environment(
            &[primary, secondary],
            &[primary, changed],
        ));
    }

    #[test]
    #[ignore = "captures the live Desktop and previews a synthetic cross-display migration"]
    fn live_desktop_migration_preview_is_visible() {
        let snapshot = capture_current_desktop_layout().unwrap();

        let target_monitors = synthetic_compact_monitor_environment(&snapshot.monitors);

        let positions = plan_desktop_positions(&snapshot, &target_monitors)
            .expect("synthetic receiver topology should be mappable");

        assert_eq!(positions.len(), snapshot.items.len(),);

        println!();
        println!("NetworkCopy Desktop migration dry run",);

        println!("  Items:           {}", snapshot.items.len(),);

        println!("  Source monitors: {}", snapshot.monitors.len(),);

        println!("  Target monitors: {}", target_monitors.len(),);

        println!();
        println!("Source monitor environment:");

        for (index, monitor) in snapshot.monitors.iter().enumerate() {
            println!(
                "  {index}: bounds=({}, {})..({}, {}), work=({}, {})..({}, {}), dpi={}x{}, primary={}",
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

        println!();
        println!("Synthetic receiver environment:");

        for (index, monitor) in target_monitors.iter().enumerate() {
            println!(
                "  {index}: bounds=({}, {})..({}, {}), work=({}, {})..({}, {}), dpi={}x{}, primary={}",
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

        println!();
        println!("Planned icon migration:");

        let mut moved_items = 0_usize;

        for item in &snapshot.items {
            let planned = *positions
                .get(&item.name)
                .expect("captured item must have a planned position");

            if planned != item.position {
                moved_items += 1;
            }

            assert!(
                planned_position_is_visible(planned, &target_monitors,),
                "planned Desktop position for {:?} is outside every target work area: ({}, {})",
                item.name,
                planned.x,
                planned.y,
            );

            println!(
                "  {:?}: {:?}: ({}, {}) -> ({}, {})",
                item.kind, item.name, item.position.x, item.position.y, planned.x, planned.y,
            );
        }

        println!();
        println!(
            "Dry run complete: {} / {} items would move",
            moved_items,
            snapshot.items.len(),
        );

        println!("All planned positions are inside a target monitor work area.",);
    }

    #[test]
    #[ignore = "reapplies and verifies the current live Windows Desktop layout"]
    fn live_desktop_restore_is_idempotent() {
        let before = capture_current_desktop_layout().unwrap();

        println!();
        println!("NetworkCopy live Desktop restore acceptance");
        println!("  Captured items: {}", before.items.len());
        println!("  Icon size:      {} px", before.icon_size);
        println!("  Auto Arrange:   {}", before.auto_arrange);
        println!("  Monitors:       {}", before.monitors.len());

        let report = restore_current_desktop_layout(&before).unwrap();

        println!();
        println!(
            "Restore report: {:?}, {} matched / {} missing",
            report.outcome, report.matched_items, report.missing_items,
        );

        assert_eq!(report.outcome, DesktopLayoutRestoreOutcome::Applied,);

        assert_eq!(
            report.matched_items,
            before.items.len(),
            "every captured physical Desktop item should be found during restore",
        );

        assert_eq!(
            report.missing_items, 0,
            "same-machine restore should not lose any captured Desktop items",
        );

        // Explorer applies view changes synchronously through the Shell API,
        // but give its view a moment to settle before independently capturing
        // the resulting state again.
        std::thread::sleep(std::time::Duration::from_millis(250));

        let after = capture_current_desktop_layout().unwrap();

        assert_eq!(
            after.icon_size, before.icon_size,
            "Desktop icon size changed after idempotent restore",
        );

        assert_eq!(
            after.auto_arrange, before.auto_arrange,
            "Desktop Auto Arrange state changed after idempotent restore",
        );

        assert!(
            same_monitor_environment(&before.monitors, &after.monitors,),
            "monitor environment changed during live Desktop restore acceptance",
        );

        let mut before_items = before.items.clone();

        let mut after_items = after.items.clone();

        before_items.sort_by(|left, right| left.name.cmp(&right.name));

        after_items.sort_by(|left, right| left.name.cmp(&right.name));

        assert_eq!(
            after_items, before_items,
            "Desktop item positions or identities changed after idempotent restore",
        );

        println!();
        println!(
            "Verified {} Desktop items unchanged after live restore.",
            after_items.len(),
        );

        println!("Live Desktop restore acceptance passed.",);
    }

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
