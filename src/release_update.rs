use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::ptr::{null, null_mut};
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};
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

const UPDATE_BACKUP_PARTIAL_FILE: &str = "previous.exe.partial";

const UPDATE_INSTALL_CANDIDATE_SUFFIX: &str = ".networkcopy-update.partial";

const UPDATE_ROLLBACK_CANDIDATE_SUFFIX: &str = ".networkcopy-rollback.partial";

const UPDATE_HANDOFF_FILE: &str = "handoff-plan.bin";

const UPDATE_HANDOFF_MAGIC: [u8; 4] = *b"NCH1";

const UPDATE_HANDOFF_VERSION: u16 = 1;

const UPDATE_HANDOFF_MAX_BYTES: usize = 1024 * 1024;

const UPDATE_HANDOFF_MAX_PATH_UTF16_UNITS: usize = 32_767;

const UPDATE_HANDOFF_DIGEST_BYTES: usize = 32;

const UPDATE_HANDOFF_PARTIAL_SUFFIX: &str = ".partial";

const UPDATE_STARTUP_MARKER_FILE: &str = "startup-ok.marker";

const UPDATE_PARTIAL_SUFFIX: &str = ".partial";

const UPDATE_DOWNLOAD_BUFFER_SIZE: usize = 64 * 1024;

const UPDATE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

const UPDATE_PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(120);

pub const UPDATE_HANDOFF_WAIT_ARGUMENT: &str = "--update-handoff-wait";

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
pub struct UpdateHandoffPlan {
    pub parent_process_id: u32,

    pub artifact_kind: ReleaseArtifactKind,

    pub naming: UpdateInstallNaming,

    pub current_executable: PathBuf,

    pub install_path: PathBuf,

    pub staging_directory: PathBuf,

    pub staged_executable: PathBuf,

    pub backup_executable: PathBuf,

    pub handoff_plan: PathBuf,

    pub startup_marker: PathBuf,

    pub expected_size: u64,

    pub expected_sha256_hex: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParentProcessWaitOutcome {
    AlreadyExited,

    Exited,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateHandoffWaitReport {
    pub handoff: UpdateHandoffPlan,

    pub parent_wait: ParentProcessWaitOutcome,

    pub installation: PreparedUpdateInstallation,

    pub publication: UpdateInstallationPublication,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpdateHandoffLaunchReport {
    pub helper_process_id: u32,

    pub parent_process_id: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedUpdateInstallation {
    pub backup_executable: PathBuf,

    pub install_candidate: PathBuf,

    pub backup_size: u64,

    pub backup_sha256_hex: String,

    pub candidate_size: u64,

    pub candidate_sha256_hex: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateInstallationPublication {
    PublishedSideBySide { installed_executable: PathBuf },

    ReplacedInPlace { installed_executable: PathBuf },
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

pub fn write_update_handoff_plan(
    plan: &UpdateInstallPlan,
    verified: &VerifiedStagedUpdate,
    parent_process_id: u32,
) -> io::Result<UpdateHandoffPlan> {
    let handoff = build_update_handoff_plan(plan, verified, parent_process_id)?;

    let staged_metadata = fs::metadata(&handoff.staged_executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "verified staged executable {:?} could not be inspected before handoff: {error}",
                handoff.staged_executable,
            ),
        )
    })?;

    if !staged_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "verified staged executable is not a regular file: {:?}",
                handoff.staged_executable,
            ),
        ));
    }

    if staged_metadata.len() != handoff.expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "verified staged executable contains {} bytes, expected {}",
                staged_metadata.len(),
                handoff.expected_size,
            ),
        ));
    }

    let encoded = encode_update_handoff_plan(&handoff)?;

    let partial_handoff = partial_handoff_plan_path(plan);

    remove_file_if_exists(&partial_handoff)?;

    remove_file_if_exists(&plan.handoff_plan)?;

    match write_update_handoff_bytes(&partial_handoff, &plan.handoff_plan, &encoded) {
        Ok(()) => {}

        Err(error) => {
            if let Err(cleanup_error) = remove_file_if_exists(&partial_handoff) {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "{error}; incomplete handoff file {:?} also could not be removed: \
                         {cleanup_error}",
                        partial_handoff,
                    ),
                ));
            }

            return Err(error);
        }
    }

    let decoded = read_update_handoff_plan(&plan.handoff_plan)?;

    if decoded != handoff {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "written update handoff plan did not round-trip exactly",
        ));
    }

    Ok(handoff)
}

pub fn read_update_handoff_plan(path: &Path) -> io::Result<UpdateHandoffPlan> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update handoff path must be absolute",
        ));
    }

    let file = fs::File::open(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("update handoff file {path:?} could not be opened: {error}"),
        )
    })?;

    let mut reader = file.take((UPDATE_HANDOFF_MAX_BYTES + 1) as u64);

    let mut encoded = Vec::new();

    reader.read_to_end(&mut encoded).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("update handoff file {path:?} could not be read: {error}"),
        )
    })?;

    if encoded.len() > UPDATE_HANDOFF_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("update handoff file exceeds the {UPDATE_HANDOFF_MAX_BYTES}-byte limit"),
        ));
    }

    let handoff = decode_update_handoff_plan(&encoded)?;

    if handoff.handoff_plan != path {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update handoff file was loaded from {path:?}, but its encoded path is {:?}",
                handoff.handoff_plan,
            ),
        ));
    }

    Ok(handoff)
}

pub fn run_update_handoff_wait_mode(
    handoff_path: &Path,
    expected_artifact_kind: ReleaseArtifactKind,
) -> io::Result<UpdateHandoffWaitReport> {
    let handoff = read_update_handoff_plan(handoff_path)?;

    if handoff.artifact_kind != expected_artifact_kind {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update handoff targets {}, but this helper expects {}",
                handoff.artifact_kind.label(),
                expected_artifact_kind.label(),
            ),
        ));
    }

    validate_update_helper_identity(&handoff)?;

    // Acquire the process object before hashing. Once this handle is open, the
    // later wait remains attached to that process object rather than merely
    // checking the numeric PID again.
    let parent_process = open_parent_process(handoff.parent_process_id)?;

    verify_update_handoff_executable(&handoff)?;

    let parent_wait = match parent_process {
        Some(parent_process) => {
            wait_for_parent_process_exit(&parent_process, UPDATE_PARENT_EXIT_TIMEOUT)?;

            ParentProcessWaitOutcome::Exited
        }

        None => ParentProcessWaitOutcome::AlreadyExited,
    };

    complete_update_handoff_after_parent_exit(handoff, parent_wait)
}

fn complete_update_handoff_after_parent_exit(
    handoff: UpdateHandoffPlan,
    parent_wait: ParentProcessWaitOutcome,
) -> io::Result<UpdateHandoffWaitReport> {
    let installation = prepare_update_installation_files(&handoff)?;

    let publication = publish_prepared_update_installation(&handoff, &installation)?;

    Ok(UpdateHandoffWaitReport {
        handoff,

        parent_wait,

        installation,

        publication,
    })
}

pub fn launch_update_handoff_wait_helper(
    handoff_path: &Path,
    expected_artifact_kind: ReleaseArtifactKind,
) -> io::Result<UpdateHandoffLaunchReport> {
    let handoff = read_update_handoff_plan(handoff_path)?;

    if handoff.artifact_kind != expected_artifact_kind {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update handoff targets {}, but this launcher expects {}",
                handoff.artifact_kind.label(),
                expected_artifact_kind.label(),
            ),
        ));
    }

    let current_process_id = process::id();

    if handoff.parent_process_id != current_process_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update handoff records parent process {}, but the running application is process \
                 {current_process_id}",
                handoff.parent_process_id,
            ),
        ));
    }

    let metadata = fs::metadata(&handoff.staged_executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "staged update helper {:?} could not be inspected before launch: {error}",
                handoff.staged_executable,
            ),
        )
    })?;

    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "staged update helper is not a regular file: {:?}",
                handoff.staged_executable,
            ),
        ));
    }

    if metadata.len() != handoff.expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "staged update helper contains {} bytes, but the handoff requires {}",
                metadata.len(),
                handoff.expected_size,
            ),
        ));
    }

    let mut command = build_update_handoff_wait_command(&handoff);

    let child = command.spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "staged update helper {:?} could not be launched: {error}",
                handoff.staged_executable,
            ),
        )
    })?;

    Ok(UpdateHandoffLaunchReport {
        helper_process_id: child.id(),

        parent_process_id: current_process_id,
    })
}

fn build_update_handoff_wait_command(handoff: &UpdateHandoffPlan) -> Command {
    let mut command = Command::new(&handoff.staged_executable);

    command
        .arg(UPDATE_HANDOFF_WAIT_ARGUMENT)
        .arg(&handoff.handoff_plan)
        .current_dir(&handoff.staging_directory);

    command
}

pub fn prepare_update_installation_files(
    handoff: &UpdateHandoffPlan,
) -> io::Result<PreparedUpdateInstallation> {
    validate_update_handoff_plan(handoff)?;

    let backup_partial = update_backup_partial_path(&handoff.staging_directory);

    let install_candidate = update_install_candidate_path(handoff)?;

    validate_update_installation_paths(handoff, &backup_partial, &install_candidate)?;

    remove_file_if_exists(&backup_partial)?;

    remove_file_if_exists(&install_candidate)?;

    let backup_copy = copy_verified_file(&handoff.current_executable, &backup_partial, None)?;

    if let Err(error) =
        crate::windows_file_replace::replace(&backup_partial, &handoff.backup_executable)
    {
        if let Err(cleanup_error) = remove_file_if_exists(&backup_partial) {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "{error}; incomplete backup {:?} also could not be removed: \
                     {cleanup_error}",
                    backup_partial,
                ),
            ));
        }

        return Err(io::Error::new(
            error.kind(),
            format!(
                "verified update backup {:?} could not be published: {error}",
                handoff.backup_executable,
            ),
        ));
    }

    let published_backup =
        inspect_file_digest(&handoff.backup_executable, "published update backup")?;

    if published_backup != backup_copy {
        let mismatch = io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "published update backup {:?} did not match its verified partial copy",
                handoff.backup_executable,
            ),
        );

        if let Err(cleanup_error) = remove_file_if_exists(&handoff.backup_executable) {
            return Err(io::Error::new(
                mismatch.kind(),
                format!("{mismatch}; invalid backup also could not be removed: {cleanup_error}",),
            ));
        }

        return Err(mismatch);
    }

    let candidate_copy = copy_verified_file(
        &handoff.staged_executable,
        &install_candidate,
        Some((handoff.expected_size, handoff.expected_sha256_hex.as_str())),
    )?;

    Ok(PreparedUpdateInstallation {
        backup_executable: handoff.backup_executable.clone(),

        install_candidate,

        backup_size: backup_copy.size,

        backup_sha256_hex: backup_copy.sha256_hex,

        candidate_size: candidate_copy.size,

        candidate_sha256_hex: candidate_copy.sha256_hex,
    })
}

fn publish_prepared_update_installation(
    handoff: &UpdateHandoffPlan,
    installation: &PreparedUpdateInstallation,
) -> io::Result<UpdateInstallationPublication> {
    validate_update_handoff_plan(handoff)?;

    if installation.backup_executable != handoff.backup_executable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "prepared backup path {:?} does not match the handoff backup path {:?}",
                installation.backup_executable, handoff.backup_executable,
            ),
        ));
    }

    let expected_candidate = update_install_candidate_path(handoff)?;

    if installation.install_candidate != expected_candidate {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "prepared install candidate {:?} does not match the expected path {:?}",
                installation.install_candidate, expected_candidate,
            ),
        ));
    }

    verify_prepared_installation_file(
        &installation.backup_executable,
        "prepared update backup",
        installation.backup_size,
        &installation.backup_sha256_hex,
    )?;

    if installation.candidate_size != handoff.expected_size
        || installation.candidate_sha256_hex != handoff.expected_sha256_hex
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "prepared install-candidate report does not match the handoff size and SHA-256",
        ));
    }

    verify_prepared_installation_file(
        &installation.install_candidate,
        "prepared update install candidate",
        handoff.expected_size,
        &handoff.expected_sha256_hex,
    )?;

    match handoff.naming {
        UpdateInstallNaming::PreserveCurrentName => {
            publish_custom_named_update(handoff, installation)
        }

        UpdateInstallNaming::PublishedAssetName => {
            publish_officially_named_update(handoff, installation)
        }
    }
}

fn publish_officially_named_update(
    handoff: &UpdateHandoffPlan,
    installation: &PreparedUpdateInstallation,
) -> io::Result<UpdateInstallationPublication> {
    if handoff.install_path == handoff.current_executable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "official-name publication cannot replace the current executable in place",
        ));
    }

    match fs::metadata(&handoff.install_path) {
        Ok(_) => {
            if let Err(error) = verify_prepared_installation_file(
                &handoff.install_path,
                "existing official update executable",
                handoff.expected_size,
                &handoff.expected_sha256_hex,
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "official update destination {:?} already exists and does not match the \
                         verified update: {error}",
                        handoff.install_path,
                    ),
                ));
            }

            remove_file_if_exists(&installation.install_candidate)?;

            return Ok(UpdateInstallationPublication::PublishedSideBySide {
                installed_executable: handoff.install_path.clone(),
            });
        }

        Err(error) if error.kind() == io::ErrorKind::NotFound => {}

        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "official update destination {:?} could not be inspected: {error}",
                    handoff.install_path,
                ),
            ));
        }
    }

    crate::windows_file_replace::move_new(&installation.install_candidate, &handoff.install_path)?;

    if let Err(error) = verify_prepared_installation_file(
        &handoff.install_path,
        "published official update executable",
        handoff.expected_size,
        &handoff.expected_sha256_hex,
    ) {
        if let Err(cleanup_error) = remove_file_if_exists(&handoff.install_path) {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "{error}; invalid published executable {:?} also could not be removed: \
                     {cleanup_error}",
                    handoff.install_path,
                ),
            ));
        }

        return Err(error);
    }

    Ok(UpdateInstallationPublication::PublishedSideBySide {
        installed_executable: handoff.install_path.clone(),
    })
}

fn publish_custom_named_update(
    handoff: &UpdateHandoffPlan,
    installation: &PreparedUpdateInstallation,
) -> io::Result<UpdateInstallationPublication> {
    let rollback_candidate = update_rollback_candidate_path(handoff)?;

    validate_custom_replacement_paths(handoff, installation, &rollback_candidate)?;

    verify_prepared_installation_file(
        &handoff.current_executable,
        "custom-name executable before replacement",
        installation.backup_size,
        &installation.backup_sha256_hex,
    )?;

    crate::windows_file_replace::replace(&installation.install_candidate, &handoff.install_path)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "verified custom-name update candidate {:?} could not replace {:?}: {error}",
                    installation.install_candidate, handoff.install_path,
                ),
            )
        })?;

    if let Err(verification_error) = verify_prepared_installation_file(
        &handoff.install_path,
        "installed custom-name update executable",
        handoff.expected_size,
        &handoff.expected_sha256_hex,
    ) {
        return match rollback_custom_named_update(handoff, installation) {
            Ok(()) => Err(io::Error::new(
                verification_error.kind(),
                format!(
                    "{verification_error}; the previous custom-named executable was restored \
                     from its verified backup",
                ),
            )),

            Err(rollback_error) => Err(io::Error::new(
                verification_error.kind(),
                format!(
                    "{verification_error}; automatic restoration of the previous executable \
                     also failed: {rollback_error}",
                ),
            )),
        };
    }

    Ok(UpdateInstallationPublication::ReplacedInPlace {
        installed_executable: handoff.install_path.clone(),
    })
}

fn rollback_custom_named_update(
    handoff: &UpdateHandoffPlan,
    installation: &PreparedUpdateInstallation,
) -> io::Result<()> {
    let rollback_candidate = update_rollback_candidate_path(handoff)?;

    validate_custom_replacement_paths(handoff, installation, &rollback_candidate)?;

    verify_prepared_installation_file(
        &installation.backup_executable,
        "custom-name rollback backup",
        installation.backup_size,
        &installation.backup_sha256_hex,
    )?;

    remove_file_if_exists(&rollback_candidate)?;

    copy_verified_file(
        &installation.backup_executable,
        &rollback_candidate,
        Some((
            installation.backup_size,
            installation.backup_sha256_hex.as_str(),
        )),
    )?;

    crate::windows_file_replace::replace(&rollback_candidate, &handoff.install_path).map_err(
        |error| {
            io::Error::new(
                error.kind(),
                format!(
                    "verified rollback candidate {:?} could not restore custom-named executable \
                 {:?}: {error}",
                    rollback_candidate, handoff.install_path,
                ),
            )
        },
    )?;

    verify_prepared_installation_file(
        &handoff.install_path,
        "restored custom-name executable",
        installation.backup_size,
        &installation.backup_sha256_hex,
    )
}

fn validate_custom_replacement_paths(
    handoff: &UpdateHandoffPlan,
    installation: &PreparedUpdateInstallation,
    rollback_candidate: &Path,
) -> io::Result<()> {
    if handoff.naming != UpdateInstallNaming::PreserveCurrentName {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "custom-name replacement requires PreserveCurrentName naming",
        ));
    }

    if handoff.install_path != handoff.current_executable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "custom-name install path {:?} does not match the current executable {:?}",
                handoff.install_path, handoff.current_executable,
            ),
        ));
    }

    if rollback_candidate.parent() != handoff.install_path.parent() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "custom-name rollback candidate must be beside the installed executable",
        ));
    }

    for (reserved, label) in [
        (&handoff.current_executable, "current executable"),
        (&handoff.staged_executable, "staged helper"),
        (&handoff.backup_executable, "published backup"),
        (&handoff.handoff_plan, "handoff plan"),
        (&handoff.startup_marker, "startup marker"),
        (&installation.install_candidate, "install candidate"),
    ] {
        if rollback_candidate == reserved {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("custom-name rollback candidate aliases the {label}"),
            ));
        }
    }

    Ok(())
}

fn update_rollback_candidate_path(handoff: &UpdateHandoffPlan) -> io::Result<PathBuf> {
    let install_parent = handoff.install_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "custom-name install path has no parent directory",
        )
    })?;

    let install_file_name = handoff.install_path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "custom-name install path has no file name",
        )
    })?;

    let mut rollback_file_name = install_file_name.to_os_string();

    rollback_file_name.push(UPDATE_ROLLBACK_CANDIDATE_SUFFIX);

    Ok(install_parent.join(rollback_file_name))
}

fn verify_prepared_installation_file(
    path: &Path,
    label: &str,
    expected_size: u64,
    expected_sha256_hex: &str,
) -> io::Result<()> {
    let inspected = inspect_file_digest(path, label)?;

    if inspected.size != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{label} {path:?} contains {} bytes, expected {expected_size}",
                inspected.size,
            ),
        ));
    }

    if inspected.sha256_hex != expected_sha256_hex {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{label} {path:?} has SHA-256 {}, expected {expected_sha256_hex}",
                inspected.sha256_hex,
            ),
        ));
    }

    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VerifiedFileCopy {
    size: u64,

    sha256_hex: String,
}

fn update_backup_partial_path(staging_directory: &Path) -> PathBuf {
    staging_directory.join(UPDATE_BACKUP_PARTIAL_FILE)
}

fn update_install_candidate_path(handoff: &UpdateHandoffPlan) -> io::Result<PathBuf> {
    let install_parent = handoff.install_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "update install path has no parent directory",
        )
    })?;

    let install_file_name = handoff.install_path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "update install path has no file name",
        )
    })?;

    let mut candidate_file_name = install_file_name.to_os_string();

    candidate_file_name.push(UPDATE_INSTALL_CANDIDATE_SUFFIX);

    Ok(install_parent.join(candidate_file_name))
}

fn validate_update_installation_paths(
    handoff: &UpdateHandoffPlan,
    backup_partial: &Path,
    install_candidate: &Path,
) -> io::Result<()> {
    if !backup_partial.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "partial update backup path must be absolute",
        ));
    }

    if !install_candidate.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update install-candidate path must be absolute",
        ));
    }

    if backup_partial.parent() != Some(handoff.staging_directory.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "partial update backup must be directly inside the staging directory",
        ));
    }

    if install_candidate.parent() != handoff.install_path.parent() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update install candidate must be beside the final install path",
        ));
    }

    for (reserved, label) in [
        (&handoff.current_executable, "current executable"),
        (&handoff.install_path, "final install path"),
        (&handoff.staged_executable, "staged executable"),
        (&handoff.backup_executable, "published backup"),
        (&handoff.handoff_plan, "handoff plan"),
        (&handoff.startup_marker, "startup marker"),
    ] {
        if install_candidate == reserved {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("update install candidate aliases the {label}"),
            ));
        }
    }

    for (reserved, label) in [
        (&handoff.staged_executable, "staged executable"),
        (&handoff.backup_executable, "published backup"),
        (&handoff.handoff_plan, "handoff plan"),
        (&handoff.startup_marker, "startup marker"),
    ] {
        if backup_partial == reserved {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("partial update backup aliases the {label}"),
            ));
        }
    }

    if backup_partial == install_candidate {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "partial update backup aliases the install candidate",
        ));
    }

    Ok(())
}

fn copy_verified_file(
    source: &Path,
    destination: &Path,
    expected: Option<(u64, &str)>,
) -> io::Result<VerifiedFileCopy> {
    match copy_verified_file_inner(source, destination, expected) {
        Ok(copy) => Ok(copy),

        Err(error) => {
            if let Err(cleanup_error) = remove_file_if_exists(destination) {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "{error}; incomplete copied file {:?} also could not be removed: \
                         {cleanup_error}",
                        destination,
                    ),
                ));
            }

            Err(error)
        }
    }
}

fn copy_verified_file_inner(
    source: &Path,
    destination: &Path,
    expected: Option<(u64, &str)>,
) -> io::Result<VerifiedFileCopy> {
    let mut source_file = fs::File::open(source).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("update source file {source:?} could not be opened: {error}"),
        )
    })?;

    let source_metadata = source_file.metadata().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("update source file {source:?} could not be inspected: {error}"),
        )
    })?;

    if !source_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("update source is not a regular file: {source:?}"),
        ));
    }

    let source_size = source_metadata.len();

    if source_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("update source file is empty: {source:?}"),
        ));
    }

    if let Some((expected_size, _expected_sha256_hex)) = expected
        && source_size != expected_size
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update source file {source:?} contains {source_size} bytes, expected \
                 {expected_size}",
            ),
        ));
    }

    let mut destination_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("update copy destination {destination:?} could not be created: {error}",),
            )
        })?;

    let mut hasher = Sha256::new();

    let mut total_size = 0_u64;

    let mut buffer = [0_u8; UPDATE_DOWNLOAD_BUFFER_SIZE];

    loop {
        let read = source_file.read(&mut buffer).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("update source file {source:?} could not be read: {error}"),
            )
        })?;

        if read == 0 {
            break;
        }

        total_size = total_size.checked_add(read as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "copied update file size overflowed u64",
            )
        })?;

        if let Some((expected_size, _expected_sha256_hex)) = expected
            && total_size > expected_size
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "update source file {source:?} exceeded its expected size of \
                     {expected_size} bytes",
                ),
            ));
        }

        destination_file
            .write_all(&buffer[..read])
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "update copy destination {destination:?} could not be written: \
                         {error}",
                    ),
                )
            })?;

        hasher.update(&buffer[..read]);
    }

    if total_size != source_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update source file {source:?} yielded {total_size} bytes after reporting \
                 {source_size}",
            ),
        ));
    }

    let final_source_size = source_file
        .metadata()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "update source file {source:?} could not be re-inspected after copying: \
                 {error}",
                ),
            )
        })?
        .len();

    if final_source_size != source_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update source file {source:?} changed size while being copied: \
                 {source_size} became {final_source_size}",
            ),
        ));
    }

    let sha256_hex = encode_lower_hex(&hasher.finalize());

    if let Some((_expected_size, expected_sha256_hex)) = expected
        && sha256_hex != expected_sha256_hex
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "copied update file SHA-256 mismatch: expected {expected_sha256_hex}, \
                 received {sha256_hex}",
            ),
        ));
    }

    destination_file.flush().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("update copy destination {destination:?} could not be flushed: {error}",),
        )
    })?;

    destination_file.sync_all().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "update copy destination {destination:?} could not be synchronized: \
                 {error}",
            ),
        )
    })?;

    drop(destination_file);

    let copied = VerifiedFileCopy {
        size: total_size,

        sha256_hex,
    };

    let inspected = inspect_file_digest(destination, "copied update file")?;

    if inspected != copied {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("copied update file {destination:?} did not match the bytes written to it",),
        ));
    }

    Ok(copied)
}

fn inspect_file_digest(path: &Path, label: &str) -> io::Result<VerifiedFileCopy> {
    let mut file = fs::File::open(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{label} {path:?} could not be opened: {error}"),
        )
    })?;

    let metadata = file.metadata().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{label} {path:?} could not be inspected: {error}"),
        )
    })?;

    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} is not a regular file: {path:?}"),
        ));
    }

    let mut hasher = Sha256::new();

    let mut total_size = 0_u64;

    let mut buffer = [0_u8; UPDATE_DOWNLOAD_BUFFER_SIZE];

    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{label} {path:?} could not be read: {error}"),
            )
        })?;

        if read == 0 {
            break;
        }

        total_size = total_size.checked_add(read as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{label} size overflowed u64"),
            )
        })?;

        hasher.update(&buffer[..read]);
    }

    if total_size != metadata.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{label} {path:?} yielded {total_size} bytes after reporting {}",
                metadata.len(),
            ),
        ));
    }

    Ok(VerifiedFileCopy {
        size: total_size,

        sha256_hex: encode_lower_hex(&hasher.finalize()),
    })
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

    let partial_handoff = partial_handoff_plan_path(plan);

    let partial_backup = update_backup_partial_path(&plan.staging_directory);

    for path in [
        &plan.staged_executable,
        &partial_executable,
        &plan.backup_executable,
        &partial_backup,
        &plan.handoff_plan,
        &partial_handoff,
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

fn build_update_handoff_plan(
    plan: &UpdateInstallPlan,
    verified: &VerifiedStagedUpdate,
    parent_process_id: u32,
) -> io::Result<UpdateHandoffPlan> {
    validate_update_staging_plan(plan)?;

    if parent_process_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update handoff parent process ID must not be zero",
        ));
    }

    if verified.executable != plan.staged_executable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "verified staged executable {:?} does not match the planned path {:?}",
                verified.executable, plan.staged_executable,
            ),
        ));
    }

    if verified.size != plan.selected_asset.size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "verified update size {} does not match the planned size {}",
                verified.size, plan.selected_asset.size,
            ),
        ));
    }

    if verified.sha256_hex != plan.selected_asset.sha256_hex {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "verified update SHA-256 does not match the selected release asset",
        ));
    }

    let handoff = UpdateHandoffPlan {
        parent_process_id,

        artifact_kind: plan.artifact_kind,

        naming: plan.naming,

        current_executable: plan.current_executable.clone(),

        install_path: plan.install_path.clone(),

        staging_directory: plan.staging_directory.clone(),

        staged_executable: plan.staged_executable.clone(),

        backup_executable: plan.backup_executable.clone(),

        handoff_plan: plan.handoff_plan.clone(),

        startup_marker: plan.startup_marker.clone(),

        expected_size: verified.size,

        expected_sha256_hex: verified.sha256_hex.clone(),
    };

    validate_update_handoff_plan(&handoff)?;

    Ok(handoff)
}

fn validate_update_handoff_plan(plan: &UpdateHandoffPlan) -> io::Result<()> {
    if plan.parent_process_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update handoff parent process ID must not be zero",
        ));
    }

    if plan.expected_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update handoff expected size must not be zero",
        ));
    }

    decode_sha256_hex(&plan.expected_sha256_hex)?;

    for (path, label) in [
        (&plan.current_executable, "current executable"),
        (&plan.install_path, "install path"),
        (&plan.staging_directory, "staging directory"),
        (&plan.staged_executable, "staged executable"),
        (&plan.backup_executable, "backup executable"),
        (&plan.handoff_plan, "handoff plan"),
        (&plan.startup_marker, "startup marker"),
    ] {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("update handoff {label} path must be absolute: {path:?}"),
            ));
        }
    }

    if plan.staged_executable.parent() != Some(plan.staging_directory.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update handoff staged executable is not directly inside its staging directory",
        ));
    }

    let staged_file_name = plan.staged_executable.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "update handoff staged executable has no file name",
        )
    })?;

    if !is_official_release_file_name(staged_file_name, plan.artifact_kind) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update handoff staged executable {:?} is not an official {} release filename",
                staged_file_name,
                plan.artifact_kind.label(),
            ),
        ));
    }

    let expected_backup = plan.staging_directory.join(UPDATE_BACKUP_FILE);

    if plan.backup_executable != expected_backup {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update handoff backup path {:?} does not match {:?}",
                plan.backup_executable, expected_backup,
            ),
        ));
    }

    let expected_handoff = plan.staging_directory.join(UPDATE_HANDOFF_FILE);

    if plan.handoff_plan != expected_handoff {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update handoff plan path {:?} does not match {:?}",
                plan.handoff_plan, expected_handoff,
            ),
        ));
    }

    let expected_marker = plan.staging_directory.join(UPDATE_STARTUP_MARKER_FILE);

    if plan.startup_marker != expected_marker {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update handoff startup marker path {:?} does not match {:?}",
                plan.startup_marker, expected_marker,
            ),
        ));
    }

    match plan.naming {
        UpdateInstallNaming::PublishedAssetName => {
            if plan.current_executable.parent() != plan.install_path.parent() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "published-name update must install beside the current executable",
                ));
            }

            if plan.install_path.file_name() != Some(staged_file_name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "published-name update install filename must match the staged release filename",
                ));
            }
        }

        UpdateInstallNaming::PreserveCurrentName => {
            if plan.install_path != plan.current_executable {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "custom-name update must preserve the current executable path",
                ));
            }
        }
    }

    if plan.staged_executable == plan.current_executable
        || plan.staged_executable == plan.install_path
        || plan.staged_executable == plan.backup_executable
        || plan.staged_executable == plan.handoff_plan
        || plan.staged_executable == plan.startup_marker
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update handoff staged executable aliases another reserved path",
        ));
    }

    Ok(())
}

fn encode_update_handoff_plan(plan: &UpdateHandoffPlan) -> io::Result<Vec<u8>> {
    validate_update_handoff_plan(plan)?;

    let digest = decode_sha256_hex(&plan.expected_sha256_hex)?;

    let mut encoded = Vec::new();

    encoded.extend_from_slice(&UPDATE_HANDOFF_MAGIC);

    encoded.extend_from_slice(&UPDATE_HANDOFF_VERSION.to_le_bytes());

    encoded.extend_from_slice(&0_u16.to_le_bytes());

    encoded.extend_from_slice(&plan.parent_process_id.to_le_bytes());

    encoded.push(encode_artifact_kind(plan.artifact_kind));

    encoded.push(encode_install_naming(plan.naming));

    encoded.extend_from_slice(&0_u16.to_le_bytes());

    encoded.extend_from_slice(&plan.expected_size.to_le_bytes());

    encoded.extend_from_slice(&digest);

    write_update_handoff_path(&mut encoded, &plan.current_executable)?;

    write_update_handoff_path(&mut encoded, &plan.install_path)?;

    write_update_handoff_path(&mut encoded, &plan.staging_directory)?;

    write_update_handoff_path(&mut encoded, &plan.staged_executable)?;

    write_update_handoff_path(&mut encoded, &plan.backup_executable)?;

    write_update_handoff_path(&mut encoded, &plan.handoff_plan)?;

    write_update_handoff_path(&mut encoded, &plan.startup_marker)?;

    if encoded.len() > UPDATE_HANDOFF_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "encoded update handoff plan exceeds the {UPDATE_HANDOFF_MAX_BYTES}-byte limit"
            ),
        ));
    }

    Ok(encoded)
}

fn decode_update_handoff_plan(encoded: &[u8]) -> io::Result<UpdateHandoffPlan> {
    if encoded.len() > UPDATE_HANDOFF_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "encoded update handoff plan exceeds the {UPDATE_HANDOFF_MAX_BYTES}-byte limit"
            ),
        ));
    }

    let mut reader = UpdateHandoffReader::new(encoded);

    let magic = reader.read_array::<4>("handoff magic")?;

    if magic != UPDATE_HANDOFF_MAGIC {
        return Err(invalid_handoff("update handoff magic is invalid"));
    }

    let version = reader.read_u16("handoff version")?;

    if version != UPDATE_HANDOFF_VERSION {
        return Err(invalid_handoff(format!(
            "unsupported update handoff version {version}; expected {UPDATE_HANDOFF_VERSION}",
        )));
    }

    let header_reserved = reader.read_u16("header reserved value")?;

    if header_reserved != 0 {
        return Err(invalid_handoff(
            "update handoff header reserved value is not zero",
        ));
    }

    let parent_process_id = reader.read_u32("parent process ID")?;

    let artifact_kind = decode_artifact_kind(reader.read_u8("artifact kind")?)?;

    let naming = decode_install_naming(reader.read_u8("install naming")?)?;

    let naming_reserved = reader.read_u16("naming reserved value")?;

    if naming_reserved != 0 {
        return Err(invalid_handoff(
            "update handoff naming reserved value is not zero",
        ));
    }

    let expected_size = reader.read_u64("expected executable size")?;

    let digest = reader.read_array::<UPDATE_HANDOFF_DIGEST_BYTES>("SHA-256 digest")?;

    let plan = UpdateHandoffPlan {
        parent_process_id,

        artifact_kind,

        naming,

        current_executable: reader.read_path("current executable")?,

        install_path: reader.read_path("install path")?,

        staging_directory: reader.read_path("staging directory")?,

        staged_executable: reader.read_path("staged executable")?,

        backup_executable: reader.read_path("backup executable")?,

        handoff_plan: reader.read_path("handoff plan")?,

        startup_marker: reader.read_path("startup marker")?,

        expected_size,

        expected_sha256_hex: encode_lower_hex(&digest),
    };

    reader.finish()?;

    validate_update_handoff_plan(&plan)?;

    Ok(plan)
}

fn write_update_handoff_bytes(
    partial_path: &Path,
    final_path: &Path,
    encoded: &[u8],
) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(partial_path)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "partial update handoff file {partial_path:?} could not be created: {error}"
                ),
            )
        })?;

    file.write_all(encoded).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("partial update handoff file {partial_path:?} could not be written: {error}"),
        )
    })?;

    file.flush().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("partial update handoff file {partial_path:?} could not be flushed: {error}"),
        )
    })?;

    file.sync_all().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "partial update handoff file {partial_path:?} could not be synchronized: {error}"
            ),
        )
    })?;

    drop(file);

    fs::rename(partial_path, final_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "update handoff file {partial_path:?} could not be renamed to \
                 {final_path:?}: {error}"
            ),
        )
    })
}

fn write_update_handoff_path(encoded: &mut Vec<u8>, path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("update handoff path must be absolute: {path:?}"),
        ));
    }

    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();

    if units.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update handoff path must not be empty",
        ));
    }

    if units.len() > UPDATE_HANDOFF_MAX_PATH_UTF16_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "update handoff path contains {} UTF-16 units, exceeding the {}-unit limit",
                units.len(),
                UPDATE_HANDOFF_MAX_PATH_UTF16_UNITS,
            ),
        ));
    }

    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update handoff path contains a null UTF-16 unit",
        ));
    }

    let unit_count = u32::try_from(units.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "update handoff path length cannot be represented as u32",
        )
    })?;

    encoded.extend_from_slice(&unit_count.to_le_bytes());

    for unit in units {
        encoded.extend_from_slice(&unit.to_le_bytes());
    }

    Ok(())
}

fn encode_artifact_kind(kind: ReleaseArtifactKind) -> u8 {
    match kind {
        ReleaseArtifactKind::Manager => 1,

        ReleaseArtifactKind::Agent => 2,

        ReleaseArtifactKind::Cli => 3,

        ReleaseArtifactKind::GuiHungarian => 4,

        ReleaseArtifactKind::GuiEnglish => 5,
    }
}

fn decode_artifact_kind(code: u8) -> io::Result<ReleaseArtifactKind> {
    match code {
        1 => Ok(ReleaseArtifactKind::Manager),

        2 => Ok(ReleaseArtifactKind::Agent),

        3 => Ok(ReleaseArtifactKind::Cli),

        4 => Ok(ReleaseArtifactKind::GuiHungarian),

        5 => Ok(ReleaseArtifactKind::GuiEnglish),

        _ => Err(invalid_handoff(format!(
            "unknown update handoff artifact kind {code}"
        ))),
    }
}

fn encode_install_naming(naming: UpdateInstallNaming) -> u8 {
    match naming {
        UpdateInstallNaming::PublishedAssetName => 1,

        UpdateInstallNaming::PreserveCurrentName => 2,
    }
}

fn decode_install_naming(code: u8) -> io::Result<UpdateInstallNaming> {
    match code {
        1 => Ok(UpdateInstallNaming::PublishedAssetName),

        2 => Ok(UpdateInstallNaming::PreserveCurrentName),

        _ => Err(invalid_handoff(format!(
            "unknown update handoff install naming {code}"
        ))),
    }
}

fn decode_sha256_hex(hex: &str) -> io::Result<[u8; UPDATE_HANDOFF_DIGEST_BYTES]> {
    if hex.len() != SHA256_HEX_LENGTH
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update handoff SHA-256 must contain exactly 64 lowercase hexadecimal characters",
        ));
    }

    let bytes = hex.as_bytes();

    let mut digest = [0_u8; UPDATE_HANDOFF_DIGEST_BYTES];

    for (index, output) in digest.iter_mut().enumerate() {
        let high = decode_hex_nibble(bytes[index * 2]).ok_or_else(|| {
            invalid_handoff("update handoff SHA-256 contains an invalid character")
        })?;

        let low = decode_hex_nibble(bytes[index * 2 + 1]).ok_or_else(|| {
            invalid_handoff("update handoff SHA-256 contains an invalid character")
        })?;

        *output = high << 4 | low;
    }

    Ok(digest)
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),

        b'a'..=b'f' => Some(byte - b'a' + 10),

        _ => None,
    }
}

fn partial_handoff_plan_path(plan: &UpdateInstallPlan) -> PathBuf {
    plan.staging_directory.join(format!(
        "{UPDATE_HANDOFF_FILE}{UPDATE_HANDOFF_PARTIAL_SUFFIX}"
    ))
}

fn invalid_handoff(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

struct UpdateHandoffReader<'a> {
    encoded: &'a [u8],

    offset: usize,
}

impl<'a> UpdateHandoffReader<'a> {
    fn new(encoded: &'a [u8]) -> Self {
        Self { encoded, offset: 0 }
    }

    fn read_array<const LENGTH: usize>(&mut self, label: &str) -> io::Result<[u8; LENGTH]> {
        let end = self
            .offset
            .checked_add(LENGTH)
            .ok_or_else(|| invalid_handoff(format!("update handoff {label} offset overflowed")))?;

        let bytes = self
            .encoded
            .get(self.offset..end)
            .ok_or_else(|| invalid_handoff(format!("update handoff {label} is truncated")))?;

        let mut output = [0_u8; LENGTH];

        output.copy_from_slice(bytes);

        self.offset = end;

        Ok(output)
    }

    fn read_u8(&mut self, label: &str) -> io::Result<u8> {
        Ok(self.read_array::<1>(label)?[0])
    }

    fn read_u16(&mut self, label: &str) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.read_array::<2>(label)?))
    }

    fn read_u32(&mut self, label: &str) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>(label)?))
    }

    fn read_u64(&mut self, label: &str) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.read_array::<8>(label)?))
    }

    fn read_path(&mut self, label: &str) -> io::Result<PathBuf> {
        let unit_count = usize::try_from(self.read_u32(&format!("{label} UTF-16 unit count"))?)
            .map_err(|_| {
                invalid_handoff(format!(
                    "update handoff {label} UTF-16 unit count cannot be represented"
                ))
            })?;

        if unit_count == 0 {
            return Err(invalid_handoff(format!(
                "update handoff {label} path is empty"
            )));
        }

        if unit_count > UPDATE_HANDOFF_MAX_PATH_UTF16_UNITS {
            return Err(invalid_handoff(format!(
                "update handoff {label} path contains {unit_count} UTF-16 units, \
                 exceeding the {UPDATE_HANDOFF_MAX_PATH_UTF16_UNITS}-unit limit",
            )));
        }

        let mut units = Vec::with_capacity(unit_count);

        for _ in 0..unit_count {
            units.push(self.read_u16(&format!("{label} UTF-16 contents"))?);
        }

        if units.contains(&0) {
            return Err(invalid_handoff(format!(
                "update handoff {label} path contains a null UTF-16 unit"
            )));
        }

        Ok(PathBuf::from(OsString::from_wide(&units)))
    }

    fn finish(self) -> io::Result<()> {
        if self.offset != self.encoded.len() {
            return Err(invalid_handoff(format!(
                "update handoff contains {} trailing byte(s)",
                self.encoded.len() - self.offset,
            )));
        }

        Ok(())
    }
}

fn validate_update_helper_identity(handoff: &UpdateHandoffPlan) -> io::Result<()> {
    let current_executable = env::current_exe().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("update helper executable path could not be determined: {error}"),
        )
    })?;

    let actual = fs::canonicalize(&current_executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "update helper executable {:?} could not be canonicalized: {error}",
                current_executable,
            ),
        )
    })?;

    let expected = fs::canonicalize(&handoff.staged_executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "planned staged executable {:?} could not be canonicalized: {error}",
                handoff.staged_executable,
            ),
        )
    })?;

    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "update helper is running from {:?}, but the handoff requires {:?}",
                actual, expected,
            ),
        ));
    }

    Ok(())
}

fn verify_update_handoff_executable(handoff: &UpdateHandoffPlan) -> io::Result<()> {
    let mut file = fs::File::open(&handoff.staged_executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "staged update executable {:?} could not be opened for handoff verification: \
                 {error}",
                handoff.staged_executable,
            ),
        )
    })?;

    let metadata = file.metadata().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "staged update executable {:?} could not be inspected: {error}",
                handoff.staged_executable,
            ),
        )
    })?;

    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "staged update executable is not a regular file: {:?}",
                handoff.staged_executable,
            ),
        ));
    }

    if metadata.len() != handoff.expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "staged update executable contains {} bytes, but the handoff requires {}",
                metadata.len(),
                handoff.expected_size,
            ),
        ));
    }

    let mut hasher = Sha256::new();

    let mut buffer = [0_u8; UPDATE_DOWNLOAD_BUFFER_SIZE];

    let mut total_size = 0_u64;

    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "staged update executable {:?} could not be read for SHA-256 verification: \
                     {error}",
                    handoff.staged_executable,
                ),
            )
        })?;

        if read == 0 {
            break;
        }

        total_size = total_size.checked_add(read as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "staged update helper byte count overflowed u64",
            )
        })?;

        if total_size > handoff.expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "staged update executable exceeded the expected {} bytes while being verified",
                    handoff.expected_size,
                ),
            ));
        }

        hasher.update(&buffer[..read]);
    }

    if total_size != handoff.expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "staged update executable yielded {total_size} bytes, but the handoff requires {}",
                handoff.expected_size,
            ),
        ));
    }

    let actual_sha256_hex = encode_lower_hex(&hasher.finalize());

    if actual_sha256_hex != handoff.expected_sha256_hex {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "staged update executable SHA-256 mismatch: expected {}, received \
                 {actual_sha256_hex}",
                handoff.expected_sha256_hex,
            ),
        ));
    }

    Ok(())
}

#[derive(Debug)]
struct OwnedProcessHandle(HANDLE);

impl Drop for OwnedProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn open_parent_process(parent_process_id: u32) -> io::Result<Option<OwnedProcessHandle>> {
    if parent_process_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update handoff parent process ID must not be zero",
        ));
    }

    if parent_process_id == process::id() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update helper cannot wait for its own process",
        ));
    }

    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, parent_process_id) };

    if handle.is_null() {
        let error = io::Error::last_os_error();

        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(None);
        }

        return Err(io::Error::new(
            error.kind(),
            format!(
                "parent process {parent_process_id} could not be opened for synchronization: \
                 {error}",
            ),
        ));
    }

    Ok(Some(OwnedProcessHandle(handle)))
}

fn wait_for_parent_process_exit(
    parent_process: &OwnedProcessHandle,
    timeout: Duration,
) -> io::Result<()> {
    let timeout_milliseconds = u32::try_from(timeout.as_millis()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "update parent-process timeout cannot be represented in milliseconds",
        )
    })?;

    let wait_result = unsafe { WaitForSingleObject(parent_process.0, timeout_milliseconds) };

    match wait_result {
        WAIT_OBJECT_0 => Ok(()),

        WAIT_TIMEOUT => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "the original application did not exit within {} seconds",
                timeout.as_secs(),
            ),
        )),

        WAIT_FAILED => {
            let error = io::Error::last_os_error();

            Err(io::Error::new(
                error.kind(),
                format!("waiting for the original application failed: {error}"),
            ))
        }

        unexpected => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "waiting for the original application returned unexpected status \
                 0x{unexpected:08X}",
            ),
        )),
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
        GitHubReleaseResponse, ParentProcessWaitOutcome, ReleaseArtifactKind, ReleaseAssetInfo,
        ReleaseInfo, SelectedReleaseAsset, UPDATE_HANDOFF_WAIT_ARGUMENT, UpdateInstallNaming,
        UpdateInstallPlan, UpdateInstallationPublication, VerifiedStagedUpdate,
        build_update_handoff_plan, build_update_handoff_wait_command, build_update_install_plan,
        complete_update_handoff_after_parent_exit, decode_update_handoff_plan, encode_lower_hex,
        encode_update_handoff_plan, is_official_release_file_name, open_parent_process,
        parse_release_response, parse_sha256_digest, parse_stable_version,
        partial_staged_executable_path, prepare_update_installation_files,
        publish_prepared_update_installation, read_update_handoff_plan,
        rollback_custom_named_update, select_release_asset, stage_update_from_reader,
        validate_release_url, verify_update_handoff_executable, write_update_handoff_plan,
    };
    use sha2::{Digest, Sha256};
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

    fn test_sha256_hex(bytes: &[u8]) -> String {
        encode_lower_hex(&Sha256::digest(bytes))
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

            handoff_plan: staging_directory.join("handoff-plan.bin"),

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

    #[test]
    fn handoff_plan_round_trips_binary_windows_paths() {
        let sha256_hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        let (root, mut install_plan) = test_update_plan("handoff-roundtrip", 3, sha256_hex);

        install_plan.current_executable = root.join("Árvíztűrő Manager custom name.exe");

        install_plan.install_path = install_plan.current_executable.clone();

        install_plan.naming = UpdateInstallNaming::PreserveCurrentName;

        let verified = VerifiedStagedUpdate {
            executable: install_plan.staged_executable.clone(),

            size: 3,

            sha256_hex: sha256_hex.to_string(),
        };

        let handoff = build_update_handoff_plan(&install_plan, &verified, 1234).unwrap();

        let encoded = encode_update_handoff_plan(&handoff).unwrap();

        let decoded = decode_update_handoff_plan(&encoded).unwrap();

        assert_eq!(decoded, handoff);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handoff_plan_file_is_written_and_read_back() {
        let payload = b"abc";

        let sha256_hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        let (root, install_plan) =
            test_update_plan("handoff-file", payload.len() as u64, sha256_hex);

        let staged = stage_update_from_reader(&install_plan, &mut payload.as_slice()).unwrap();

        let written = write_update_handoff_plan(&install_plan, &staged, process::id()).unwrap();

        let read_back = read_update_handoff_plan(&install_plan.handoff_plan).unwrap();

        assert_eq!(read_back, written);

        assert_eq!(read_back.staged_executable, install_plan.staged_executable,);

        assert_eq!(read_back.expected_size, payload.len() as u64);

        assert_eq!(read_back.expected_sha256_hex, sha256_hex);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handoff_plan_rejects_mismatched_verified_update() {
        let sha256_hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        let (root, install_plan) = test_update_plan("handoff-mismatch", 3, sha256_hex);

        let verified = VerifiedStagedUpdate {
            executable: install_plan.staged_executable.clone(),

            size: 4,

            sha256_hex: sha256_hex.to_string(),
        };

        let error = build_update_handoff_plan(&install_plan, &verified, 1234).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handoff_plan_rejects_trailing_data() {
        let sha256_hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        let (root, install_plan) = test_update_plan("handoff-trailing", 3, sha256_hex);

        let verified = VerifiedStagedUpdate {
            executable: install_plan.staged_executable.clone(),

            size: 3,

            sha256_hex: sha256_hex.to_string(),
        };

        let handoff = build_update_handoff_plan(&install_plan, &verified, 1234).unwrap();

        let mut encoded = encode_update_handoff_plan(&handoff).unwrap();

        encoded.push(0);

        let error = decode_update_handoff_plan(&encoded).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        assert!(error.to_string().contains("trailing"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handoff_wait_rejects_current_process() {
        let error = match open_parent_process(process::id()) {
            Err(error) => error,

            Ok(_) => panic!("the update helper accepted its own process ID"),
        };

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        assert!(error.to_string().contains("own process"));
    }

    #[test]
    fn handoff_wait_treats_missing_process_as_exited() {
        let parent = open_parent_process(u32::MAX).unwrap();

        assert!(parent.is_none());
    }

    #[test]
    fn handoff_wait_reverifies_staged_executable() {
        let payload = b"abc";

        let sha256_hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        let (root, install_plan) = test_update_plan(
            "handoff-wait-verification",
            payload.len() as u64,
            sha256_hex,
        );

        let staged = stage_update_from_reader(&install_plan, &mut payload.as_slice()).unwrap();

        let handoff = build_update_handoff_plan(&install_plan, &staged, 1234).unwrap();

        verify_update_handoff_executable(&handoff).unwrap();

        fs::write(&handoff.staged_executable, b"abd").unwrap();

        let error = verify_update_handoff_executable(&handoff).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        assert!(error.to_string().contains("SHA-256 mismatch"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handoff_wait_command_targets_staged_manager_and_exact_plan() {
        let sha256_hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

        let (root, install_plan) = test_update_plan("handoff-wait-command", 3, sha256_hex);

        let verified = VerifiedStagedUpdate {
            executable: install_plan.staged_executable.clone(),

            size: 3,

            sha256_hex: sha256_hex.to_string(),
        };

        let handoff = build_update_handoff_plan(&install_plan, &verified, 1234).unwrap();

        let command = build_update_handoff_wait_command(&handoff);

        assert_eq!(command.get_program(), handoff.staged_executable.as_os_str(),);

        let arguments = command.get_args().collect::<Vec<_>>();

        assert_eq!(
            arguments,
            vec![
                OsStr::new(UPDATE_HANDOFF_WAIT_ARGUMENT),
                handoff.handoff_plan.as_os_str(),
            ],
        );

        assert_eq!(
            command.get_current_dir(),
            Some(handoff.staging_directory.as_path()),
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handoff_completion_publishes_official_name_after_parent_exit() {
        let current_payload = b"old official manager";

        let update_payload = b"new official manager";

        let update_sha256_hex = test_sha256_hex(update_payload);

        let (root, mut plan) = test_update_plan(
            "handoff-completion-official",
            update_payload.len() as u64,
            &update_sha256_hex,
        );

        let current_file_name = "NetworkCopy-Speed-v2.3.0-Manager-Windows-x64.exe";

        let install_file_name = plan.selected_asset.name.clone();

        plan.naming = UpdateInstallNaming::PublishedAssetName;

        plan.current_executable = root.join(current_file_name);

        plan.install_path = root.join(&install_file_name);

        fs::create_dir_all(&plan.staging_directory).unwrap();

        fs::write(&plan.current_executable, current_payload).unwrap();

        fs::write(&plan.staged_executable, update_payload).unwrap();

        let verified = VerifiedStagedUpdate {
            executable: plan.staged_executable.clone(),

            size: update_payload.len() as u64,

            sha256_hex: update_sha256_hex.clone(),
        };

        let handoff = build_update_handoff_plan(&plan, &verified, 1234).unwrap();

        let expected_handoff = handoff.clone();

        let report =
            complete_update_handoff_after_parent_exit(handoff, ParentProcessWaitOutcome::Exited)
                .unwrap();

        assert_eq!(report.handoff, expected_handoff);

        assert_eq!(report.parent_wait, ParentProcessWaitOutcome::Exited,);

        assert_eq!(
            report.publication,
            UpdateInstallationPublication::PublishedSideBySide {
                installed_executable: plan.install_path.clone(),
            },
        );

        assert_eq!(fs::read(&plan.current_executable).unwrap(), current_payload,);

        assert_eq!(fs::read(&plan.install_path).unwrap(), update_payload,);

        assert_eq!(
            fs::read(&report.installation.backup_executable).unwrap(),
            current_payload,
        );

        assert!(!report.installation.install_candidate.exists(),);

        assert_eq!(fs::read(&plan.staged_executable).unwrap(), update_payload,);

        assert_eq!(
            report.installation.backup_sha256_hex,
            test_sha256_hex(current_payload),
        );

        assert_eq!(report.installation.candidate_sha256_hex, update_sha256_hex,);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handoff_completion_replaces_custom_name_in_place() {
        let current_payload = b"old custom manager";

        let update_payload = b"new custom manager";

        let update_sha256_hex = test_sha256_hex(update_payload);

        let (root, mut plan) = test_update_plan(
            "handoff-completion-custom",
            update_payload.len() as u64,
            &update_sha256_hex,
        );

        plan.current_executable = root.join("Árvíztűrő custom Manager final definitely.exe");

        plan.install_path = plan.current_executable.clone();

        plan.naming = UpdateInstallNaming::PreserveCurrentName;

        fs::create_dir_all(&plan.staging_directory).unwrap();

        fs::write(&plan.current_executable, current_payload).unwrap();

        fs::write(&plan.staged_executable, update_payload).unwrap();

        let verified = VerifiedStagedUpdate {
            executable: plan.staged_executable.clone(),

            size: update_payload.len() as u64,

            sha256_hex: update_sha256_hex.clone(),
        };

        let handoff = build_update_handoff_plan(&plan, &verified, 1234).unwrap();

        let report =
            complete_update_handoff_after_parent_exit(handoff, ParentProcessWaitOutcome::Exited)
                .unwrap();

        assert_eq!(
            report.publication,
            UpdateInstallationPublication::ReplacedInPlace {
                installed_executable: plan.current_executable.clone(),
            },
        );

        assert_eq!(fs::read(&plan.current_executable).unwrap(), update_payload,);

        assert_eq!(
            fs::read(&report.installation.backup_executable).unwrap(),
            current_payload,
        );

        assert!(!report.installation.install_candidate.exists(),);

        assert_eq!(fs::read(&plan.staged_executable).unwrap(), update_payload,);

        assert_eq!(
            plan.current_executable.file_name().unwrap(),
            OsStr::new("Árvíztűrő custom Manager final definitely.exe"),
        );

        assert_eq!(report.installation.candidate_sha256_hex, update_sha256_hex,);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn custom_name_publication_rejects_changed_installed_executable() {
        let current_payload = b"old custom manager";

        let changed_payload = b"externally changed manager";

        let update_payload = b"new custom manager";

        let update_sha256_hex = test_sha256_hex(update_payload);

        let (root, plan) = test_update_plan(
            "custom-name-changed-before-publication",
            update_payload.len() as u64,
            &update_sha256_hex,
        );

        fs::create_dir_all(&plan.staging_directory).unwrap();

        fs::write(&plan.current_executable, current_payload).unwrap();

        fs::write(&plan.staged_executable, update_payload).unwrap();

        let verified = VerifiedStagedUpdate {
            executable: plan.staged_executable.clone(),

            size: update_payload.len() as u64,

            sha256_hex: update_sha256_hex,
        };

        let handoff = build_update_handoff_plan(&plan, &verified, 1234).unwrap();

        let installation = prepare_update_installation_files(&handoff).unwrap();

        fs::write(&plan.current_executable, changed_payload).unwrap();

        let error = publish_prepared_update_installation(&handoff, &installation).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        assert_eq!(fs::read(&plan.current_executable).unwrap(), changed_payload,);

        assert_eq!(
            fs::read(&installation.backup_executable).unwrap(),
            current_payload,
        );

        assert_eq!(
            fs::read(&installation.install_candidate).unwrap(),
            update_payload,
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn custom_name_rollback_restores_verified_backup() {
        let current_payload = b"old custom manager";

        let update_payload = b"new custom manager";

        let broken_payload = b"broken installed manager";

        let update_sha256_hex = test_sha256_hex(update_payload);

        let (root, plan) = test_update_plan(
            "custom-name-rollback",
            update_payload.len() as u64,
            &update_sha256_hex,
        );

        fs::create_dir_all(&plan.staging_directory).unwrap();

        fs::write(&plan.current_executable, current_payload).unwrap();

        fs::write(&plan.staged_executable, update_payload).unwrap();

        let verified = VerifiedStagedUpdate {
            executable: plan.staged_executable.clone(),

            size: update_payload.len() as u64,

            sha256_hex: update_sha256_hex,
        };

        let handoff = build_update_handoff_plan(&plan, &verified, 1234).unwrap();

        let installation = prepare_update_installation_files(&handoff).unwrap();

        fs::write(&plan.current_executable, broken_payload).unwrap();

        rollback_custom_named_update(&handoff, &installation).unwrap();

        assert_eq!(fs::read(&plan.current_executable).unwrap(), current_payload,);

        assert_eq!(
            fs::read(&installation.backup_executable).unwrap(),
            current_payload,
        );

        assert_eq!(
            fs::read(&installation.install_candidate).unwrap(),
            update_payload,
        );

        assert!(
            !root
                .join("current.exe.networkcopy-rollback.partial")
                .exists(),
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handoff_completion_preserves_conflicting_official_destination() {
        let current_payload = b"old official manager";

        let update_payload = b"new official manager";

        let conflicting_payload = b"unrelated existing executable";

        let update_sha256_hex = test_sha256_hex(update_payload);

        let (root, mut plan) = test_update_plan(
            "handoff-completion-conflict",
            update_payload.len() as u64,
            &update_sha256_hex,
        );

        plan.naming = UpdateInstallNaming::PublishedAssetName;

        plan.current_executable = root.join("NetworkCopy-Speed-v2.3.0-Manager-Windows-x64.exe");

        plan.install_path = root.join(&plan.selected_asset.name);

        fs::create_dir_all(&plan.staging_directory).unwrap();

        fs::write(&plan.current_executable, current_payload).unwrap();

        fs::write(&plan.staged_executable, update_payload).unwrap();

        fs::write(&plan.install_path, conflicting_payload).unwrap();

        let verified = VerifiedStagedUpdate {
            executable: plan.staged_executable.clone(),

            size: update_payload.len() as u64,

            sha256_hex: update_sha256_hex,
        };

        let handoff = build_update_handoff_plan(&plan, &verified, 1234).unwrap();

        let error =
            complete_update_handoff_after_parent_exit(handoff, ParentProcessWaitOutcome::Exited)
                .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        assert_eq!(fs::read(&plan.current_executable).unwrap(), current_payload,);

        assert_eq!(fs::read(&plan.install_path).unwrap(), conflicting_payload,);

        assert_eq!(fs::read(&plan.backup_executable).unwrap(), current_payload,);

        assert_eq!(
            fs::read(root.join(format!(
                "{}.networkcopy-update.partial",
                plan.selected_asset.name,
            )),)
            .unwrap(),
            update_payload,
        );

        assert_eq!(fs::read(&plan.staged_executable).unwrap(), update_payload,);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn installation_files_preserve_custom_name_without_publishing() {
        let current_payload = b"old custom manager";

        let update_payload = b"new custom manager";

        let update_sha256_hex = test_sha256_hex(update_payload);

        let (root, plan) = test_update_plan(
            "installation-custom-name",
            update_payload.len() as u64,
            &update_sha256_hex,
        );

        fs::create_dir_all(&plan.staging_directory).unwrap();

        fs::write(&plan.current_executable, current_payload).unwrap();

        fs::write(&plan.staged_executable, update_payload).unwrap();

        let verified = VerifiedStagedUpdate {
            executable: plan.staged_executable.clone(),

            size: update_payload.len() as u64,

            sha256_hex: update_sha256_hex.clone(),
        };

        let handoff = build_update_handoff_plan(&plan, &verified, 1234).unwrap();

        let prepared = prepare_update_installation_files(&handoff).unwrap();

        assert_eq!(
            prepared.install_candidate,
            root.join("current.exe.networkcopy-update.partial"),
        );

        assert_eq!(fs::read(&plan.current_executable).unwrap(), current_payload,);

        assert_eq!(
            fs::read(&prepared.backup_executable).unwrap(),
            current_payload,
        );

        assert_eq!(
            fs::read(&prepared.install_candidate).unwrap(),
            update_payload,
        );

        assert_eq!(fs::read(&plan.staged_executable).unwrap(), update_payload,);

        assert_eq!(prepared.backup_sha256_hex, test_sha256_hex(current_payload),);

        assert_eq!(prepared.candidate_sha256_hex, update_sha256_hex);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn installation_files_prepare_published_name_without_touching_old_executable() {
        let current_payload = b"old official manager";

        let update_payload = b"new official manager";

        let update_sha256_hex = test_sha256_hex(update_payload);

        let (root, mut plan) = test_update_plan(
            "installation-published-name",
            update_payload.len() as u64,
            &update_sha256_hex,
        );

        let current_file_name = "NetworkCopy-Speed-v2.3.0-Manager-Windows-x64.exe";

        let install_file_name = plan.selected_asset.name.clone();

        plan.naming = UpdateInstallNaming::PublishedAssetName;

        plan.current_executable = root.join(current_file_name);

        plan.install_path = root.join(&install_file_name);

        fs::create_dir_all(&plan.staging_directory).unwrap();

        fs::write(&plan.current_executable, current_payload).unwrap();

        fs::write(&plan.staged_executable, update_payload).unwrap();

        let verified = VerifiedStagedUpdate {
            executable: plan.staged_executable.clone(),

            size: update_payload.len() as u64,

            sha256_hex: update_sha256_hex,
        };

        let handoff = build_update_handoff_plan(&plan, &verified, 1234).unwrap();

        let prepared = prepare_update_installation_files(&handoff).unwrap();

        assert_eq!(
            prepared.install_candidate,
            root.join(format!("{install_file_name}.networkcopy-update.partial",)),
        );

        assert_eq!(fs::read(&plan.current_executable).unwrap(), current_payload,);

        assert!(!plan.install_path.exists());

        assert_eq!(
            fs::read(&prepared.backup_executable).unwrap(),
            current_payload,
        );

        assert_eq!(
            fs::read(&prepared.install_candidate).unwrap(),
            update_payload,
        );

        assert_eq!(fs::read(&plan.staged_executable).unwrap(), update_payload,);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn installation_files_reject_tampered_candidate_and_keep_backup() {
        let current_payload = b"old manager";

        let expected_update = b"expected update";

        let tampered_update = b"tampered update";

        assert_eq!(expected_update.len(), tampered_update.len());

        let expected_sha256_hex = test_sha256_hex(expected_update);

        let (root, plan) = test_update_plan(
            "installation-tampered",
            expected_update.len() as u64,
            &expected_sha256_hex,
        );

        fs::create_dir_all(&plan.staging_directory).unwrap();

        fs::write(&plan.current_executable, current_payload).unwrap();

        fs::write(&plan.staged_executable, tampered_update).unwrap();

        let verified = VerifiedStagedUpdate {
            executable: plan.staged_executable.clone(),

            size: expected_update.len() as u64,

            sha256_hex: expected_sha256_hex,
        };

        let handoff = build_update_handoff_plan(&plan, &verified, 1234).unwrap();

        let error = prepare_update_installation_files(&handoff).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        assert!(error.to_string().contains("SHA-256 mismatch"));

        assert_eq!(fs::read(&plan.current_executable).unwrap(), current_payload,);

        assert_eq!(fs::read(&plan.backup_executable).unwrap(), current_payload,);

        assert!(!root.join("current.exe.networkcopy-update.partial").exists(),);

        assert_eq!(fs::read(&plan.staged_executable).unwrap(), tampered_update,);

        let _ = fs::remove_dir_all(root);
    }
}
