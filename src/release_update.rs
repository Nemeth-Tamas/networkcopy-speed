use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::Duration;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/Nemeth-Tamas/networkcopy-speed/releases/latest";

const RELEASE_PAGE_PREFIX: &str = "https://github.com/Nemeth-Tamas/networkcopy-speed/releases/";

const REQUEST_USER_AGENT: &str = concat!("networkcopy-speed/", env!("CARGO_PKG_VERSION"),);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

const RELEASE_DOWNLOAD_PREFIX: &str =
    "https://github.com/Nemeth-Tamas/networkcopy-speed/releases/download/";

const SHA256_DIGEST_PREFIX: &str = "sha256:";

const SHA256_HEX_LENGTH: usize = 64;

const UPDATE_DATA_DIRECTORY: &str = "NetworkCopy Speed Edition";

const UPDATE_STAGING_DIRECTORY: &str = "Updates";

const UPDATE_BACKUP_FILE: &str = "previous.exe";

const UPDATE_HANDOFF_FILE: &str = "handoff-plan.txt";

const UPDATE_STARTUP_MARKER_FILE: &str = "startup-ok.marker";

const UPDATE_PARTIAL_SUFFIX: &str = ".partial";

const UPDATE_DOWNLOAD_BUFFER_SIZE: usize = 64 * 1024;

const UPDATE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseArtifactKind {
    Manager,

    Agent,

    Cli,

    GuiHungarian,

    GuiEnglish,
}

impl ReleaseArtifactKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Manager => "Manager",

            Self::Agent => "Agent",

            Self::Cli => "CLI",

            Self::GuiHungarian => "GUI-HU",

            Self::GuiEnglish => "GUI-EN",
        }
    }

    const fn storage_key(self) -> &'static str {
        match self {
            Self::Manager => "manager",

            Self::Agent => "agent",

            Self::Cli => "cli",

            Self::GuiHungarian => "gui-hu",

            Self::GuiEnglish => "gui-en",
        }
    }

    fn expected_asset_names(self, version: StableVersion) -> [String; 2] {
        let role = self.label();

        [
            format!(
                "NetworkCopy-Speed-v{}.{}.{}-{role}-Windows-x64.exe",
                version.major, version.minor, version.patch,
            ),
            format!("NetworkCopy-Speed-{role}-Windows-x64.exe",),
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseAssetInfo {
    pub id: u64,

    pub name: String,

    pub state: String,

    pub size: u64,

    pub digest: Option<String>,

    pub download_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedReleaseAsset {
    pub id: u64,

    pub name: String,

    pub size: u64,

    pub sha256_hex: String,

    pub download_url: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateInstallNaming {
    PublishedAssetName,

    PreserveCurrentName,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateInstallPlan {
    pub artifact_kind: ReleaseArtifactKind,

    pub selected_asset: SelectedReleaseAsset,

    pub naming: UpdateInstallNaming,

    pub current_executable: PathBuf,

    pub install_path: PathBuf,

    pub staging_directory: PathBuf,

    pub staged_executable: PathBuf,

    pub backup_executable: PathBuf,

    pub handoff_plan: PathBuf,

    pub startup_marker: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedStagedUpdate {
    pub executable: PathBuf,

    pub size: u64,

    pub sha256_hex: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseInfo {
    pub tag_name: String,

    pub name: String,

    pub html_url: String,

    pub assets: Vec<ReleaseAssetInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseCheck {
    pub current_version: String,

    pub latest: ReleaseInfo,

    pub update_available: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct StableVersion {
    major: u64,

    minor: u64,

    patch: u64,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct GitHubReleaseAssetResponse {
    id: u64,

    name: String,

    state: String,

    size: u64,

    digest: Option<String>,

    browser_download_url: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct GitHubReleaseResponse {
    tag_name: String,

    name: Option<String>,

    html_url: String,

    #[serde(default)]
    assets: Vec<GitHubReleaseAssetResponse>,
}

pub fn check_latest(current_version: &str) -> io::Result<ReleaseCheck> {
    let current = parse_stable_version(current_version)?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(REQUEST_TIMEOUT)
        .timeout_read(REQUEST_TIMEOUT)
        .timeout_write(REQUEST_TIMEOUT)
        .build();

    let response = agent
        .get(LATEST_RELEASE_API)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", REQUEST_USER_AGENT)
        .call()
        .map_err(map_request_error)?;

    let body = response.into_string().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("GitHub release response could not be read: {error}"),
        )
    })?;

    let response = parse_release_response(&body)?;

    validate_release_url(&response.html_url)?;

    let latest = parse_stable_version(&response.tag_name)?;

    let assets = response
        .assets
        .into_iter()
        .map(|asset| ReleaseAssetInfo {
            id: asset.id,

            name: asset.name,

            state: asset.state,

            size: asset.size,

            digest: asset.digest,

            download_url: asset.browser_download_url,
        })
        .collect();

    let display_name = response
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| response.tag_name.clone());

    Ok(ReleaseCheck {
        current_version: current_version.to_string(),

        latest: ReleaseInfo {
            tag_name: response.tag_name,

            name: display_name,

            html_url: response.html_url,

            assets,
        },

        update_available: latest > current,
    })
}

pub fn plan_current_update(
    release: &ReleaseInfo,
    artifact_kind: ReleaseArtifactKind,
) -> io::Result<UpdateInstallPlan> {
    let current_executable = env::current_exe().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("the current executable path could not be determined: {error}",),
        )
    })?;

    let local_app_data = env::var_os("LOCALAPPDATA").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "LOCALAPPDATA is unavailable for update staging",
        )
    })?;

    build_update_install_plan(
        &current_executable,
        Path::new(&local_app_data),
        release,
        artifact_kind,
    )
}

pub fn download_and_stage_update(plan: &UpdateInstallPlan) -> io::Result<VerifiedStagedUpdate> {
    validate_update_staging_plan(plan)?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(UPDATE_DOWNLOAD_TIMEOUT)
        .timeout_read(UPDATE_DOWNLOAD_TIMEOUT)
        .timeout_write(UPDATE_DOWNLOAD_TIMEOUT)
        .build();

    let response = agent
        .get(&plan.selected_asset.download_url)
        .set("Accept", "application/octet-stream")
        .set("User-Agent", REQUEST_USER_AGENT)
        .call()
        .map_err(map_download_request_error)?;

    validate_download_content_length(response.header("Content-Length"), plan.selected_asset.size)?;

    let mut reader = response.into_reader();

    stage_update_from_reader(plan, &mut reader)
}

fn stage_update_from_reader<R: Read>(
    plan: &UpdateInstallPlan,
    reader: &mut R,
) -> io::Result<VerifiedStagedUpdate> {
    validate_update_staging_plan(plan)?;

    prepare_update_staging_directory(plan)?;

    let partial_executable = partial_staged_executable_path(plan);

    match write_verified_staged_update(plan, &partial_executable, reader) {
        Ok(staged) => Ok(staged),

        Err(error) => {
            if let Err(cleanup_error) = remove_file_if_exists(&partial_executable) {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "{error}; incomplete update file {:?} also could not be removed: \
                         {cleanup_error}",
                        partial_executable,
                    ),
                ));
            }

            Err(error)
        }
    }
}

fn validate_update_staging_plan(plan: &UpdateInstallPlan) -> io::Result<()> {
    if !plan.staging_directory.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update staging directory must be absolute",
        ));
    }

    if plan.selected_asset.size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "selected update asset has an invalid zero-byte size",
        ));
    }

    if plan.selected_asset.sha256_hex.len() != SHA256_HEX_LENGTH
        || !plan
            .selected_asset
            .sha256_hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "selected update asset SHA-256 must contain exactly 64 lowercase hexadecimal characters",
        ));
    }

    validate_asset_download_url(&plan.selected_asset.download_url)?;

    let asset_path = Path::new(&plan.selected_asset.name);

    if asset_path.components().count() != 1
        || asset_path.file_name() != Some(OsStr::new(&plan.selected_asset.name))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "selected update asset name must be a single file name",
        ));
    }

    let expected_staged_executable = plan.staging_directory.join(&plan.selected_asset.name);

    if plan.staged_executable != expected_staged_executable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "staged executable path {:?} does not match the selected asset path {:?}",
                plan.staged_executable, expected_staged_executable,
            ),
        ));
    }

    let reserved_paths = [
        (
            &plan.backup_executable,
            UPDATE_BACKUP_FILE,
            "backup executable",
        ),
        (&plan.handoff_plan, UPDATE_HANDOFF_FILE, "handoff plan"),
        (
            &plan.startup_marker,
            UPDATE_STARTUP_MARKER_FILE,
            "startup marker",
        ),
    ];

    for (actual, file_name, label) in reserved_paths {
        let expected = plan.staging_directory.join(file_name);

        if actual != &expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} path {actual:?} does not match the expected path {expected:?}",),
            ));
        }
    }

    Ok(())
}

fn prepare_update_staging_directory(plan: &UpdateInstallPlan) -> io::Result<()> {
    fs::create_dir_all(&plan.staging_directory).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "update staging directory {:?} could not be created: {error}",
                plan.staging_directory,
            ),
        )
    })?;

    let partial_executable = partial_staged_executable_path(plan);

    for path in [
        &plan.staged_executable,
        &partial_executable,
        &plan.backup_executable,
        &plan.handoff_plan,
        &plan.startup_marker,
    ] {
        remove_file_if_exists(path)?;
    }

    Ok(())
}

fn write_verified_staged_update<R: Read>(
    plan: &UpdateInstallPlan,
    partial_executable: &Path,
    reader: &mut R,
) -> io::Result<VerifiedStagedUpdate> {
    let mut partial_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(partial_executable)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "partial update file {:?} could not be created: {error}",
                    partial_executable,
                ),
            )
        })?;

    let mut hasher = Sha256::new();

    let mut total_size = 0_u64;

    let mut buffer = [0_u8; UPDATE_DOWNLOAD_BUFFER_SIZE];

    loop {
        let read = reader.read(&mut buffer).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("update download could not be read: {error}"),
            )
        })?;

        if read == 0 {
            break;
        }

        total_size = total_size.checked_add(read as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "update download size overflowed u64",
            )
        })?;

        if total_size > plan.selected_asset.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "update download exceeded its expected size of {} bytes",
                    plan.selected_asset.size,
                ),
            ));
        }

        partial_file.write_all(&buffer[..read]).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "partial update file {:?} could not be written: {error}",
                    partial_executable,
                ),
            )
        })?;

        hasher.update(&buffer[..read]);
    }

    if total_size != plan.selected_asset.size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update download contained {total_size} bytes, but GitHub reported {} bytes",
                plan.selected_asset.size,
            ),
        ));
    }

    let digest = hasher.finalize();

    let actual_sha256_hex = encode_lower_hex(&digest);

    if actual_sha256_hex != plan.selected_asset.sha256_hex {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update download SHA-256 mismatch: expected {}, received {actual_sha256_hex}",
                plan.selected_asset.sha256_hex,
            ),
        ));
    }

    partial_file.flush().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "partial update file {:?} could not be flushed: {error}",
                partial_executable,
            ),
        )
    })?;

    partial_file.sync_all().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "partial update file {:?} could not be synchronized to disk: {error}",
                partial_executable,
            ),
        )
    })?;

    drop(partial_file);

    fs::rename(partial_executable, &plan.staged_executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "verified update file {:?} could not be renamed to {:?}: {error}",
                partial_executable, plan.staged_executable,
            ),
        )
    })?;

    Ok(VerifiedStagedUpdate {
        executable: plan.staged_executable.clone(),

        size: total_size,

        sha256_hex: actual_sha256_hex,
    })
}

fn partial_staged_executable_path(plan: &UpdateInstallPlan) -> PathBuf {
    plan.staging_directory.join(format!(
        "{}{UPDATE_PARTIAL_SUFFIX}",
        plan.selected_asset.name,
    ))
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),

        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),

        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("stale update artifact {path:?} could not be removed: {error}",),
        )),
    }
}

fn validate_download_content_length(
    content_length: Option<&str>,
    expected_size: u64,
) -> io::Result<()> {
    let Some(content_length) = content_length else {
        return Ok(());
    };

    let content_length = content_length.trim().parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("update download returned an invalid Content-Length header: {error}",),
        )
    })?;

    if content_length != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update download Content-Length is {content_length} bytes, but GitHub reported \
                 {expected_size} bytes",
            ),
        ));
    }

    Ok(())
}

fn encode_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(bytes.len() * 2);

    for &byte in bytes {
        encoded.push(char::from(HEX[(byte >> 4) as usize]));

        encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
    }

    encoded
}

fn map_download_request_error(error: ureq::Error) -> io::Error {
    match error {
        ureq::Error::Status(status, _response) => io::Error::other(format!(
            "GitHub update download returned HTTP status {status}",
        )),

        ureq::Error::Transport(error) => {
            io::Error::other(format!("GitHub update download failed: {error}",))
        }
    }
}

fn build_update_install_plan(
    current_executable: &Path,
    local_app_data: &Path,
    release: &ReleaseInfo,
    artifact_kind: ReleaseArtifactKind,
) -> io::Result<UpdateInstallPlan> {
    if !current_executable.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "current executable path must be absolute",
        ));
    }

    if !local_app_data.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "LOCALAPPDATA path must be absolute",
        ));
    }

    let current_parent = current_executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "current executable path has no parent directory",
        )
    })?;

    let current_file_name = current_executable.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "current executable path has no file name",
        )
    })?;

    let selected_asset = select_release_asset(release, artifact_kind)?;

    let release_version = parse_stable_version(&release.tag_name)?;

    let normalized_version = format!(
        "v{}.{}.{}",
        release_version.major, release_version.minor, release_version.patch,
    );

    let staging_directory = local_app_data
        .join(UPDATE_DATA_DIRECTORY)
        .join(UPDATE_STAGING_DIRECTORY)
        .join(artifact_kind.storage_key())
        .join(normalized_version);

    let officially_named = is_official_release_file_name(current_file_name, artifact_kind);

    let naming = if officially_named {
        UpdateInstallNaming::PublishedAssetName
    } else {
        UpdateInstallNaming::PreserveCurrentName
    };

    let install_path = match naming {
        UpdateInstallNaming::PublishedAssetName => current_parent.join(&selected_asset.name),

        UpdateInstallNaming::PreserveCurrentName => current_executable.to_path_buf(),
    };

    let staged_executable = staging_directory.join(&selected_asset.name);

    let backup_executable = staging_directory.join(UPDATE_BACKUP_FILE);

    let handoff_plan = staging_directory.join(UPDATE_HANDOFF_FILE);

    let startup_marker = staging_directory.join(UPDATE_STARTUP_MARKER_FILE);

    Ok(UpdateInstallPlan {
        artifact_kind,

        selected_asset,

        naming,

        current_executable: current_executable.to_path_buf(),

        install_path,

        staging_directory,

        staged_executable,

        backup_executable,

        handoff_plan,

        startup_marker,
    })
}

fn is_official_release_file_name(file_name: &OsStr, artifact_kind: ReleaseArtifactKind) -> bool {
    let Some(file_name) = file_name.to_str() else {
        return false;
    };

    let role = artifact_kind.label();

    let unversioned = format!("NetworkCopy-Speed-{role}-Windows-x64.exe",);

    if file_name.eq_ignore_ascii_case(&unversioned) {
        return true;
    }

    let lowercase = file_name.to_ascii_lowercase();

    let prefix = "networkcopy-speed-v";

    let suffix = format!("-{}-windows-x64.exe", role.to_ascii_lowercase(),);

    let Some(version) = lowercase
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(&suffix))
    else {
        return false;
    };

    is_strict_release_version(version)
}

fn is_strict_release_version(value: &str) -> bool {
    let mut parts = value.split('.');

    let valid_component = |component: Option<&str>| {
        component.is_some_and(|component| {
            !component.is_empty()
                && component.bytes().all(|byte| byte.is_ascii_digit())
                && component.parse::<u64>().is_ok()
        })
    };

    valid_component(parts.next())
        && valid_component(parts.next())
        && valid_component(parts.next())
        && parts.next().is_none()
}

pub fn select_release_asset(
    release: &ReleaseInfo,
    artifact_kind: ReleaseArtifactKind,
) -> io::Result<SelectedReleaseAsset> {
    let version = parse_stable_version(&release.tag_name)?;

    let expected_names = artifact_kind.expected_asset_names(version);

    let mut selected = None;

    for expected_name in &expected_names {
        let matching = release
            .assets
            .iter()
            .filter(|asset| asset.name == *expected_name)
            .collect::<Vec<_>>();

        match matching.as_slice() {
            [] => {}

            [asset] => {
                selected = Some(*asset);

                break;
            }

            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "GitHub release {} contains more than one asset named {expected_name:?}",
                        release.tag_name,
                    ),
                ));
            }
        }
    }

    let asset = selected.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "GitHub release {} does not contain the {} executable; expected {:?} or {:?}",
                release.tag_name,
                artifact_kind.label(),
                expected_names[0],
                expected_names[1],
            ),
        )
    })?;

    if asset.id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("release asset {:?} has an invalid zero ID", asset.name,),
        ));
    }

    if asset.state != "uploaded" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "release asset {:?} is in state {:?}, not \"uploaded\"",
                asset.name, asset.state,
            ),
        ));
    }

    if asset.size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "release asset {:?} has an invalid zero-byte size",
                asset.name,
            ),
        ));
    }

    validate_asset_download_url(&asset.download_url)?;

    let digest = asset.digest.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "release asset {:?} does not provide a GitHub SHA-256 digest",
                asset.name,
            ),
        )
    })?;

    let sha256_hex = parse_sha256_digest(digest)?;

    Ok(SelectedReleaseAsset {
        id: asset.id,

        name: asset.name.clone(),

        size: asset.size,

        sha256_hex,

        download_url: asset.download_url.clone(),
    })
}

fn parse_sha256_digest(digest: &str) -> io::Result<String> {
    let hex = digest.strip_prefix(SHA256_DIGEST_PREFIX).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("release asset digest does not use the {SHA256_DIGEST_PREFIX:?} prefix",),
        )
    })?;

    if hex.len() != SHA256_HEX_LENGTH || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "release asset SHA-256 digest must contain exactly 64 hexadecimal characters",
        ));
    }

    Ok(hex.to_ascii_lowercase())
}

fn validate_asset_download_url(download_url: &str) -> io::Result<()> {
    if download_url.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "release asset download URL contains a null character",
        ));
    }

    if !download_url.starts_with(RELEASE_DOWNLOAD_PREFIX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("GitHub returned an unexpected release asset URL: {download_url}",),
        ));
    }

    Ok(())
}

pub fn open_release_page(release_url: &str) -> io::Result<()> {
    validate_release_url(release_url)?;

    let operation = wide(OsStr::new("open"));

    let target = wide(OsStr::new(release_url));

    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };

    let result_code = result as isize;

    if result_code <= 32 {
        return Err(io::Error::other(format!(
            "Windows could not open the release page; ShellExecuteW returned {result_code}",
        )));
    }

    Ok(())
}

fn parse_release_response(body: &str) -> io::Result<GitHubReleaseResponse> {
    serde_json::from_str(body).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("GitHub returned an invalid release response: {error}"),
        )
    })
}

fn parse_stable_version(value: &str) -> io::Result<StableVersion> {
    let value = value.trim();

    let value = value.strip_prefix('v').unwrap_or(value);

    let core = value.split(['-', '+']).next().unwrap_or_default();

    let mut parts = core.split('.');

    let major = parse_version_component(parts.next(), "major", value)?;

    let minor = parse_version_component(parts.next(), "minor", value)?;

    let patch = parse_version_component(parts.next(), "patch", value)?;

    if parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("version {value:?} has more than three numeric components"),
        ));
    }

    Ok(StableVersion {
        major,

        minor,

        patch,
    })
}

fn parse_version_component(component: Option<&str>, name: &str, original: &str) -> io::Result<u64> {
    let component = component.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("version {original:?} is missing its {name} component"),
        )
    })?;

    if component.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("version {original:?} has an empty {name} component"),
        ));
    }

    component.parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("version {original:?} has an invalid {name} component: {error}"),
        )
    })
}

fn validate_release_url(release_url: &str) -> io::Result<()> {
    if release_url.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "release URL contains a null character",
        ));
    }

    if !release_url.starts_with(RELEASE_PAGE_PREFIX) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("GitHub returned an unexpected release URL: {release_url}"),
        ));
    }

    Ok(())
}

fn map_request_error(error: ureq::Error) -> io::Error {
    match error {
        ureq::Error::Status(status, _response) => io::Error::other(format!(
            "GitHub release request returned HTTP status {status}",
        )),

        ureq::Error::Transport(error) => {
            io::Error::other(format!("GitHub release request failed: {error}",))
        }
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        GitHubReleaseResponse, ReleaseArtifactKind, ReleaseAssetInfo, ReleaseInfo,
        SelectedReleaseAsset, UpdateInstallNaming, UpdateInstallPlan, build_update_install_plan,
        is_official_release_file_name, parse_release_response, parse_sha256_digest,
        parse_stable_version, partial_staged_executable_path, select_release_asset,
        stage_update_from_reader, validate_release_url,
    };
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_release(assets: Vec<ReleaseAssetInfo>) -> ReleaseInfo {
        ReleaseInfo {
            tag_name: "v2.4.0".to_string(),

            name: "NetworkCopy v2.4.0".to_string(),

            html_url: "https://github.com/Nemeth-Tamas/networkcopy-speed/releases/tag/v2.4.0"
                .to_string(),

            assets,
        }
    }

    fn test_asset(name: &str, digest: Option<String>) -> ReleaseAssetInfo {
        ReleaseAssetInfo {
            id: 42,

            name: name.to_string(),

            state: "uploaded".to_string(),

            size: 12_345_678,

            digest,

            download_url: format!(
                "https://github.com/Nemeth-Tamas/networkcopy-speed/releases/download/v2.4.0/{name}",
            ),
        }
    }

    fn test_digest() -> String {
        format!("sha256:{}", "ab".repeat(32),)
    }

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn test_update_plan(
        test_name: &str,
        expected_size: u64,
        sha256_hex: &str,
    ) -> (PathBuf, UpdateInstallPlan) {
        let test_id = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);

        let root = std::env::temp_dir().join(format!(
            "networkcopy-speed-release-update-{test_name}-{}-{test_id}",
            process::id(),
        ));

        let staging_directory = root.join("stage");

        let asset_name = "NetworkCopy-Speed-v2.4.0-Manager-Windows-x64.exe";

        let plan = UpdateInstallPlan {
            artifact_kind: ReleaseArtifactKind::Manager,

            selected_asset: SelectedReleaseAsset {
                id: 42,

                name: asset_name.to_string(),

                size: expected_size,

                sha256_hex: sha256_hex.to_string(),

                download_url: format!(
                    "https://github.com/Nemeth-Tamas/networkcopy-speed/releases/download/v2.4.0/{asset_name}",
                ),
            },

            naming: UpdateInstallNaming::PreserveCurrentName,

            current_executable: root.join("current.exe"),

            install_path: root.join("current.exe"),

            staging_directory: staging_directory.clone(),

            staged_executable: staging_directory.join(asset_name),

            backup_executable: staging_directory.join("previous.exe"),

            handoff_plan: staging_directory.join("handoff-plan.txt"),

            startup_marker: staging_directory.join("startup-ok.marker"),
        };

        (root, plan)
    }

    #[test]
    fn stable_versions_ignore_dev_suffixes() {
        assert_eq!(
            parse_stable_version("2.3.0-dev").unwrap(),
            parse_stable_version("v2.3.0").unwrap(),
        );
    }

    #[test]
    fn stable_versions_compare_numerically() {
        assert!(parse_stable_version("v2.4.0").unwrap() > parse_stable_version("2.3.9").unwrap(),);

        assert!(parse_stable_version("v3.0.0").unwrap() > parse_stable_version("2.99.99").unwrap(),);
    }

    #[test]
    fn malformed_versions_are_rejected() {
        assert!(parse_stable_version("2.3").is_err(),);

        assert!(parse_stable_version("two.three.zero").is_err(),);
    }

    #[test]
    fn release_response_is_parsed() {
        let response = parse_release_response(
            r#"{
                    "tag_name": "v2.3.0",
                    "name": "NetworkCopy v2.3.0",
                    "html_url": "https://github.com/Nemeth-Tamas/networkcopy-speed/releases/tag/v2.3.0"
                }"#,
        )
        .unwrap();

        assert_eq!(
            response,
            GitHubReleaseResponse {
                tag_name: "v2.3.0".to_string(),

                name: Some("NetworkCopy v2.3.0".to_string(),),

                html_url: "https://github.com/Nemeth-Tamas/networkcopy-speed/releases/tag/v2.3.0"
                    .to_string(),

                assets: Vec::new(),
            },
        );
    }

    #[test]
    fn foreign_release_urls_are_rejected() {
        assert!(validate_release_url("https://example.com/releases/v9",).is_err(),);

        assert!(
            validate_release_url(
                "https://github.com/Nemeth-Tamas/networkcopy-speed/releases/tag/v2.3.0",
            )
            .is_ok(),
        );
    }

    #[test]
    fn selects_versioned_manager_asset() {
        let release = test_release(vec![test_asset(
            "NetworkCopy-Speed-v2.4.0-Manager-Windows-x64.exe",
            Some(test_digest()),
        )]);

        let selected = select_release_asset(&release, ReleaseArtifactKind::Manager).unwrap();

        assert_eq!(
            selected.name,
            "NetworkCopy-Speed-v2.4.0-Manager-Windows-x64.exe",
        );

        assert_eq!(selected.sha256_hex, "ab".repeat(32),);
    }

    #[test]
    fn accepts_unversioned_official_gui_asset() {
        let release = test_release(vec![test_asset(
            "NetworkCopy-Speed-GUI-EN-Windows-x64.exe",
            Some(test_digest()),
        )]);

        let selected = select_release_asset(&release, ReleaseArtifactKind::GuiEnglish).unwrap();

        assert_eq!(selected.name, "NetworkCopy-Speed-GUI-EN-Windows-x64.exe",);
    }

    #[test]
    fn versioned_asset_is_preferred_over_unversioned_alias() {
        let release = test_release(vec![
            test_asset(
                "NetworkCopy-Speed-GUI-HU-Windows-x64.exe",
                Some(test_digest()),
            ),
            test_asset(
                "NetworkCopy-Speed-v2.4.0-GUI-HU-Windows-x64.exe",
                Some(test_digest()),
            ),
        ]);

        let selected = select_release_asset(&release, ReleaseArtifactKind::GuiHungarian).unwrap();

        assert_eq!(
            selected.name,
            "NetworkCopy-Speed-v2.4.0-GUI-HU-Windows-x64.exe",
        );
    }

    #[test]
    fn wrong_application_asset_is_rejected() {
        let release = test_release(vec![test_asset(
            "NetworkCopy-Speed-v2.4.0-Agent-Windows-x64.exe",
            Some(test_digest()),
        )]);

        let error = select_release_asset(&release, ReleaseArtifactKind::Manager).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound,);

        assert!(error.to_string().contains("Manager"),);
    }

    #[test]
    fn missing_asset_digest_is_rejected() {
        let release = test_release(vec![test_asset(
            "NetworkCopy-Speed-v2.4.0-CLI-Windows-x64.exe",
            None,
        )]);

        let error = select_release_asset(&release, ReleaseArtifactKind::Cli).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);

        assert!(error.to_string().contains("SHA-256 digest"),);
    }

    #[test]
    fn malformed_sha256_digest_is_rejected() {
        assert!(parse_sha256_digest("sha256:not-a-real-digest",).is_err(),);

        assert!(parse_sha256_digest(&format!("sha256:{}", "12".repeat(32),),).is_ok(),);
    }

    #[test]
    fn official_manager_update_uses_published_asset_name() {
        let release = test_release(vec![test_asset(
            "NetworkCopy-Speed-v2.4.0-Manager-Windows-x64.exe",
            Some(test_digest()),
        )]);

        let plan = build_update_install_plan(
            Path::new(r"C:\Tools\NetworkCopy-Speed-v2.3.0-Manager-Windows-x64.exe"),
            Path::new(r"C:\Users\User\AppData\Local"),
            &release,
            ReleaseArtifactKind::Manager,
        )
        .unwrap();

        assert_eq!(plan.naming, UpdateInstallNaming::PublishedAssetName,);

        assert_eq!(
            plan.install_path,
            PathBuf::from(r"C:\Tools\NetworkCopy-Speed-v2.4.0-Manager-Windows-x64.exe",),
        );

        assert_eq!(
            plan.staged_executable,
            PathBuf::from(
                r"C:\Users\User\AppData\Local\NetworkCopy Speed Edition\Updates\manager\v2.4.0\NetworkCopy-Speed-v2.4.0-Manager-Windows-x64.exe",
            ),
        );
    }

    #[test]
    fn custom_manager_name_is_preserved() {
        let release = test_release(vec![test_asset(
            "NetworkCopy-Speed-v2.4.0-Manager-Windows-x64.exe",
            Some(test_digest()),
        )]);

        let plan = build_update_install_plan(
            Path::new(r"D:\Portable\Fast Copy Thing.exe"),
            Path::new(r"C:\Users\User\AppData\Local"),
            &release,
            ReleaseArtifactKind::Manager,
        )
        .unwrap();

        assert_eq!(plan.naming, UpdateInstallNaming::PreserveCurrentName,);

        assert_eq!(
            plan.install_path,
            PathBuf::from(r"D:\Portable\Fast Copy Thing.exe",),
        );

        assert_eq!(
            plan.backup_executable,
            PathBuf::from(
                r"C:\Users\User\AppData\Local\NetworkCopy Speed Edition\Updates\manager\v2.4.0\previous.exe",
            ),
        );
    }

    #[test]
    fn gui_languages_have_separate_staging_directories() {
        let hu_release = test_release(vec![test_asset(
            "NetworkCopy-Speed-v2.4.0-GUI-HU-Windows-x64.exe",
            Some(test_digest()),
        )]);

        let en_release = test_release(vec![test_asset(
            "NetworkCopy-Speed-v2.4.0-GUI-EN-Windows-x64.exe",
            Some(test_digest()),
        )]);

        let hu_plan = build_update_install_plan(
            Path::new(r"C:\Tools\gui-hu.exe"),
            Path::new(r"C:\Users\User\AppData\Local"),
            &hu_release,
            ReleaseArtifactKind::GuiHungarian,
        )
        .unwrap();

        let en_plan = build_update_install_plan(
            Path::new(r"C:\Tools\gui-en.exe"),
            Path::new(r"C:\Users\User\AppData\Local"),
            &en_release,
            ReleaseArtifactKind::GuiEnglish,
        )
        .unwrap();

        assert_ne!(hu_plan.staging_directory, en_plan.staging_directory,);

        assert!(
            hu_plan
                .staging_directory
                .ends_with(Path::new(r"gui-hu\v2.4.0",),),
        );

        assert!(
            en_plan
                .staging_directory
                .ends_with(Path::new(r"gui-en\v2.4.0",),),
        );
    }

    #[test]
    fn official_release_names_are_role_specific() {
        assert!(is_official_release_file_name(
            OsStr::new("NetworkCopy-Speed-v2.3.0-Agent-Windows-x64.exe",),
            ReleaseArtifactKind::Agent,
        ),);

        assert!(is_official_release_file_name(
            OsStr::new("networkcopy-speed-manager-windows-x64.exe",),
            ReleaseArtifactKind::Manager,
        ),);

        assert!(!is_official_release_file_name(
            OsStr::new("NetworkCopy-Speed-v2.3.0-Agent-Windows-x64.exe",),
            ReleaseArtifactKind::Manager,
        ),);

        assert!(!is_official_release_file_name(
            OsStr::new("NetworkCopy-Speed-v2.3-Agent-Windows-x64.exe",),
            ReleaseArtifactKind::Agent,
        ),);
    }

    #[test]
    fn update_plan_rejects_relative_paths() {
        let release = test_release(vec![test_asset(
            "NetworkCopy-Speed-v2.4.0-CLI-Windows-x64.exe",
            Some(test_digest()),
        )]);

        assert!(
            build_update_install_plan(
                Path::new("networkcopy-speed.exe",),
                Path::new(r"C:\Users\User\AppData\Local",),
                &release,
                ReleaseArtifactKind::Cli,
            )
            .is_err(),
        );

        assert!(
            build_update_install_plan(
                Path::new(r"C:\Tools\networkcopy-speed.exe",),
                Path::new("relative"),
                &release,
                ReleaseArtifactKind::Cli,
            )
            .is_err(),
        );
    }

    #[test]
    fn staged_update_writes_verified_executable() {
        let payload: &[u8] = b"abc";

        let expected_sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        let (root, plan) = test_update_plan("verified", payload.len() as u64, expected_sha256);

        fs::create_dir_all(&plan.staging_directory).unwrap();

        let partial_executable = partial_staged_executable_path(&plan);

        fs::write(&plan.staged_executable, b"stale executable").unwrap();

        fs::write(&partial_executable, b"stale partial").unwrap();

        fs::write(&plan.backup_executable, b"stale backup").unwrap();

        fs::write(&plan.handoff_plan, b"stale handoff").unwrap();

        fs::write(&plan.startup_marker, b"stale marker").unwrap();

        let mut reader = io::Cursor::new(payload);

        let staged = stage_update_from_reader(&plan, &mut reader).unwrap();

        assert_eq!(staged.executable, plan.staged_executable,);

        assert_eq!(staged.size, payload.len() as u64,);

        assert_eq!(staged.sha256_hex, expected_sha256,);

        assert_eq!(fs::read(&plan.staged_executable).unwrap(), payload,);

        assert!(!partial_executable.exists(),);

        assert!(!plan.backup_executable.exists(),);

        assert!(!plan.handoff_plan.exists(),);

        assert!(!plan.startup_marker.exists(),);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_update_rejects_short_body() {
        let payload: &[u8] = b"abc";

        let expected_sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        let (root, plan) = test_update_plan("short", 4, expected_sha256);

        let partial_executable = partial_staged_executable_path(&plan);

        let mut reader = io::Cursor::new(payload);

        let error = stage_update_from_reader(&plan, &mut reader).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);

        assert!(error.to_string().contains("contained 3 bytes"),);

        assert!(!partial_executable.exists(),);

        assert!(!plan.staged_executable.exists(),);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_update_rejects_oversized_body() {
        let payload: &[u8] = b"abc";

        let expected_sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        let (root, plan) = test_update_plan("oversized", 2, expected_sha256);

        let partial_executable = partial_staged_executable_path(&plan);

        let mut reader = io::Cursor::new(payload);

        let error = stage_update_from_reader(&plan, &mut reader).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);

        assert!(error.to_string().contains("exceeded its expected size"),);

        assert!(!partial_executable.exists(),);

        assert!(!plan.staged_executable.exists(),);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_update_rejects_digest_mismatch() {
        let payload: &[u8] = b"abc";

        let (root, plan) = test_update_plan("digest-mismatch", 3, &"00".repeat(32));

        let partial_executable = partial_staged_executable_path(&plan);

        let mut reader = io::Cursor::new(payload);

        let error = stage_update_from_reader(&plan, &mut reader).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData,);

        assert!(error.to_string().contains("SHA-256 mismatch"),);

        assert!(!partial_executable.exists(),);

        assert!(!plan.staged_executable.exists(),);

        fs::remove_dir_all(root).unwrap();
    }
}
