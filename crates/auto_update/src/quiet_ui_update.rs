//! Where this fork looks for its own updates.
//!
//! The nightly build publishes a dmg to a release at a fixed tag, and beside it
//! a small manifest naming the commit that dmg was built from. That is all the
//! updater needs: the commit says whether the build on the other end is a
//! different one from the one running, and the url says where it is.
//!
//! Everything after that is Zed's own: the download, the mount, the install,
//! the "Restart to update" button. This module only answers the question Zed
//! normally asks zed.dev.

use anyhow::{Context as _, Result};
use http_client::{AsyncBody, HttpClient};
use serde::Deserialize;
use std::sync::Arc;

use crate::ReleaseAsset;

/// The manifest the build publishes next to the dmg. A plain asset rather than
/// the GitHub API: no token, no rate limit, no user agent to get right.
const MANIFEST_URL: &str =
    "https://github.com/ArthurBrussee/zed/releases/download/quiet-ui-latest/quiet-ui-latest.json";

#[derive(Deserialize)]
struct Manifest {
    /// The commit the dmg was built from.
    sha: String,
    /// Where the dmg is. Named here rather than assumed, so the release can
    /// move without stranding every installed copy.
    url: String,
}

/// The fork's own latest build, shaped as the release Zed's updater expects.
///
/// The version carries the commit in its build metadata, which is where the
/// nightly-channel comparison looks for it: a build from a different commit is
/// a newer one, whatever the version numbers say. The fork's version number
/// never moves, so the commit is the only thing that can answer this.
pub(crate) async fn fetch_release(
    http: Arc<dyn HttpClient>,
    installed_version: &semver::Version,
) -> Result<ReleaseAsset> {
    let mut response = http
        .get(MANIFEST_URL, AsyncBody::default(), true)
        .await
        .context("fetching the quiet-ui release manifest")?;
    anyhow::ensure!(
        response.status().is_success(),
        "quiet-ui release manifest: {:?}",
        response.status()
    );

    let mut body = Vec::new();
    smol::io::AsyncReadExt::read_to_end(response.body_mut(), &mut body).await?;
    let manifest: Manifest = serde_json::from_slice(&body).with_context(|| {
        format!(
            "reading the quiet-ui release manifest: {:?}",
            String::from_utf8_lossy(&body)
        )
    })?;

    let mut version = installed_version.clone();
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::new(&format!("quiet-ui.{}", manifest.sha))
        .context("the manifest's commit is not usable as version metadata")?;

    Ok(ReleaseAsset {
        version: version.to_string(),
        url: manifest.url,
    })
}
