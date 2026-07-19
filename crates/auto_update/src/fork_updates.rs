//! zn-zed fork: serves nightly app updates from the fork's GitHub Releases
//! instead of the official zed.dev release endpoint.
//!
//! CI (`.github/workflows/fork-release.yml`) tags each release as
//! `nightly-<cargo_version>-<full_commit_sha>` and uploads the Windows
//! installer as `Zed-<arch>.exe`. This module maps the latest such release to
//! the `ReleaseAsset` shape the auto-updater expects, returning a version of
//! the form `<cargo_version>+nightly.<sha>` so that the nightly branch of
//! `check_if_fetched_version_is_newer` (which compares the last dot-segment of
//! the build metadata against the running commit SHA) works unchanged.

use anyhow::{Context as _, Result};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl};
use smol::io::AsyncReadExt;
use std::sync::Arc;

use crate::ReleaseAsset;

pub const FORK_REPO: &str = "zharinov-nikita/zn-zed";

#[derive(serde::Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(serde::Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

/// Fetches the latest release of the fork from the GitHub API. Version pins
/// are not supported — only the latest release is ever served.
pub async fn get_release_asset(
    http_client: &Arc<HttpClientWithUrl>,
    asset: &str,
    os: &str,
    arch: &str,
) -> Result<ReleaseAsset> {
    let url = format!("https://api.github.com/repos/{FORK_REPO}/releases/latest");
    let mut response = http_client
        .get(&url, AsyncBody::default(), true)
        .await
        .context("error fetching latest fork release from GitHub")?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;

    anyhow::ensure!(
        response.status().is_success(),
        "failed to fetch fork release: {:?}",
        String::from_utf8_lossy(&body),
    );

    let release: GithubRelease = serde_json::from_slice(&body).with_context(|| {
        format!(
            "error deserializing fork release {:?}",
            String::from_utf8_lossy(&body),
        )
    })?;

    let (version, sha) = parse_tag(&release.tag_name)?;
    let asset_name = asset_name(asset, os, arch)?;
    let asset = release
        .assets
        .iter()
        .find(|github_asset| github_asset.name == asset_name)
        .with_context(|| {
            format!(
                "fork release {} has no asset named {asset_name}",
                release.tag_name
            )
        })?;

    Ok(ReleaseAsset {
        version: format!("{version}+nightly.{sha}"),
        url: asset.browser_download_url.clone(),
    })
}

/// Parses a fork release tag of the form
/// `nightly-<cargo_version>-<full_commit_sha>` into `(version, sha)`.
fn parse_tag(tag: &str) -> Result<(&str, &str)> {
    let rest = tag
        .strip_prefix("nightly-")
        .with_context(|| format!("unexpected fork release tag {tag:?}"))?;
    let (version, sha) = rest
        .rsplit_once('-')
        .with_context(|| format!("unexpected fork release tag {tag:?}"))?;
    anyhow::ensure!(
        sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "fork release tag {tag:?} does not end in a full commit sha",
    );
    Ok((version, sha))
}

fn asset_name(asset: &str, os: &str, arch: &str) -> Result<String> {
    match (asset, os) {
        // Matches the installer name produced by script/bundle-windows.ps1.
        ("zed", "windows") => Ok(format!("Zed-{arch}.exe")),
        ("zed-remote-server", _) => {
            anyhow::bail!("the fork does not serve remote server binaries")
        }
        _ => anyhow::bail!("unsupported asset {asset:?} for os {os:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn test_parse_tag() {
        let tag = format!("nightly-0.198.0-{SHA}");
        assert_eq!(parse_tag(&tag).unwrap(), ("0.198.0", SHA));

        assert!(parse_tag(&format!("v0.198.0-{SHA}")).is_err());
        assert!(parse_tag("nightly-0.198.0").is_err());
        assert!(parse_tag("nightly-0.198.0-abc123").is_err());
        assert!(parse_tag(&format!("nightly-0.198.0-{}", "z".repeat(40))).is_err());
    }

    #[test]
    fn test_parsed_tag_produces_version_with_sha_as_last_build_segment() {
        let tag = format!("nightly-0.198.0-{SHA}");
        let (version, sha) = parse_tag(&tag).unwrap();
        let version: semver::Version = format!("{version}+nightly.{sha}").parse().unwrap();
        assert_eq!(version.build.as_str().rsplit('.').next(), Some(SHA));
    }

    #[test]
    fn test_asset_name() {
        assert_eq!(
            asset_name("zed", "windows", "x86_64").unwrap(),
            "Zed-x86_64.exe"
        );
        assert!(asset_name("zed", "macos", "aarch64").is_err());
        assert!(asset_name("zed-remote-server", "windows", "x86_64").is_err());
    }
}
