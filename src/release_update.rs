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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseInfo {
    pub tag_name: String,

    pub name: String,

    pub html_url: String,
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
struct GitHubReleaseResponse {
    tag_name: String,

    name: Option<String>,

    html_url: String,
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
        },

        update_available: latest > current,
    })
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
        GitHubReleaseResponse, parse_release_response, parse_stable_version, validate_release_url,
    };

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
}
