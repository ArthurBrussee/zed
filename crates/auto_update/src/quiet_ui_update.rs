//! Answers the question Zed's updater normally asks zed.dev, from the manifest the fork's nightly
//! build publishes beside its dmg.

use anyhow::{Context as _, Result};
use http_client::{AsyncBody, HttpClient};
use serde::Deserialize;
use std::sync::Arc;

use crate::ReleaseAsset;

/// A release asset rather than the GitHub API, so no token or rate limit applies.
const MANIFEST_URL: &str =
    "https://github.com/ArthurBrussee/zed/releases/download/quiet-ui-latest/quiet-ui-latest.json";

#[derive(Deserialize)]
struct Manifest {
    sha: String,
    url: String,
}

/// The commit goes in the version's build metadata, which is what the Nightly comparison reads.
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
