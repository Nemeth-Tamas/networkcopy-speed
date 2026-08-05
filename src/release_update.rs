use serde::Deserialize;
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
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
        parse_release_response, parse_sha256_digest, parse_stable_version, select_release_asset,
        validate_release_url,
    };
    use std::io;

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
}
