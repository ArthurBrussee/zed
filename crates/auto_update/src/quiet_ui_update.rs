//! Answers the questions Zed's updater normally asks zed.dev, from the manifests the fork's
//! nightly build publishes beside its dmg: which app build is current, and which remote server
//! this client can talk to.

use anyhow::{Context as _, Result};
use http_client::{AsyncBody, HttpClient};
use release_channel::ReleaseChannel;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

use crate::ReleaseAsset;

/// One release at a fixed tag, replaced in place by every build. Release assets rather than the
/// GitHub API, so no token or rate limit applies.
const RELEASE_DOWNLOAD_BASE: &str =
    "https://github.com/ArthurBrussee/zed/releases/download/quiet-ui-latest";

fn asset_url(name: &str) -> String {
    format!("{RELEASE_DOWNLOAD_BASE}/{name}")
}

/// A quiet-ui build is the Dev channel bundled in release: what the nightly ships. A debug build
/// is someone's own `cargo run`, which wants Zed's normal behaviour.
pub(crate) fn is_quiet_ui_build(release_channel: ReleaseChannel) -> bool {
    release_channel == ReleaseChannel::Dev && !cfg!(debug_assertions)
}

#[derive(Deserialize)]
struct Manifest {
    sha: String,
    url: String,
}

/// What the nightly publishes beside each remote server binary. The URL is fixed by the asset's
/// name, so only the commit needs saying.
#[derive(Deserialize)]
struct ServerManifest {
    sha: String,
}

async fn fetch_manifest<T: DeserializeOwned>(http: &Arc<dyn HttpClient>, url: &str) -> Result<T> {
    let mut response = http
        .get(url, AsyncBody::default(), true)
        .await
        .with_context(|| format!("fetching {url}"))?;
    anyhow::ensure!(response.status().is_success(), "{url}: {:?}", response.status());

    let mut body = Vec::new();
    smol::io::AsyncReadExt::read_to_end(response.body_mut(), &mut body).await?;
    serde_json::from_slice(&body)
        .with_context(|| format!("reading {url}: {:?}", String::from_utf8_lossy(&body)))
}

/// The commit goes in the version's build metadata, which is what the Nightly comparison reads.
pub(crate) async fn fetch_release(
    http: Arc<dyn HttpClient>,
    installed_version: &semver::Version,
) -> Result<ReleaseAsset> {
    let manifest: Manifest = fetch_manifest(&http, &asset_url("quiet-ui-latest.json"))
        .await
        .context("fetching the quiet-ui release manifest")?;

    let mut version = installed_version.clone();
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::new(&format!("quiet-ui.{}", manifest.sha))
        .context("the manifest's commit is not usable as version metadata")?;

    Ok(ReleaseAsset {
        version: version.to_string(),
        url: manifest.url,
    })
}

/// The remote server a given client can actually talk to: the one built from its own commit.
/// Client and server are two halves of one build and speak each other's protocol only when they
/// came from the same commit, so a mismatch is refused here, by name, rather than left to fail
/// later as an RPC error that says nothing about why.
pub(crate) async fn fetch_remote_server_release(
    http: Arc<dyn HttpClient>,
    os: &str,
    arch: &str,
    client_sha: Option<String>,
) -> Result<ReleaseAsset> {
    let client_sha = client_sha.context(
        "this Zed does not know which commit it was built from, so it cannot ask for a remote \
         server to match it",
    )?;
    let asset = format!("zed-remote-server-{os}-{arch}");
    let manifest: ServerManifest = fetch_manifest(&http, &asset_url(&format!("{asset}.json")))
        .await
        .with_context(|| format!("fetching the quiet-ui {os} {arch} remote server manifest"))?;
    anyhow::ensure!(
        manifest.sha == client_sha,
        "the quiet-ui release has a {os} {arch} remote server built from {}, but this Zed is {}. \
         The nightly publishes both together, so wait for the next build or update Zed.",
        short_sha(&manifest.sha),
        short_sha(&client_sha),
    );

    Ok(ReleaseAsset {
        version: remote_server_version(&client_sha),
        url: asset_url(&format!("{asset}.gz")),
    })
}

/// Names the cached download and the binary installed on the box, so a client only ever runs a
/// server from its own commit and an update does not inherit the last one.
fn remote_server_version(sha: &str) -> String {
    format!("quiet-ui-{}", short_sha(sha))
}

/// Matches `AppCommitSha::short`, which is what names the binary on the box.
fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_server_is_named_after_the_commit_it_was_built_from() {
        assert_eq!(
            remote_server_version("4287bc6eca0197bc2b27918a3ce3678bc50e58e5"),
            "quiet-ui-4287bc6"
        );
        // A sha shorter than the abbreviation is used as it stands.
        assert_eq!(remote_server_version("4287bc"), "quiet-ui-4287bc");
    }
}
