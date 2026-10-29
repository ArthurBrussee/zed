//! Answers the questions Zed's updater normally asks zed.dev, from the manifests the fork's
//! nightly build publishes beside its dmg: which app build is current, and which remote server
//! this client can talk to.

use anyhow::{Context as _, Result};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Method, RedirectPolicy, Request};
use release_channel::ReleaseChannel;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

use crate::ReleaseAsset;

/// One release at a fixed tag, replaced in place by every build. Release assets rather than the
/// GitHub API, so no token or rate limit applies.
const RELEASE_DOWNLOAD_BASE: &str =
    "https://github.com/ArthurBrussee/zed/releases/download/quiet-ui-latest";
/// Only asked when no server matches, to say which builds there are.
const RELEASE_API_URL: &str =
    "https://api.github.com/repos/ArthurBrussee/zed/releases/tags/quiet-ui-latest";

fn asset_url(name: &str) -> String {
    format!("{RELEASE_DOWNLOAD_BASE}/{name}")
}

/// A quiet-ui build is the Dev channel bundled in release: what the nightly ships. A debug build
/// is someone's own `cargo run`, which wants Zed's normal behaviour.
pub fn is_quiet_ui_build(release_channel: ReleaseChannel) -> bool {
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

#[derive(Deserialize)]
struct ReleaseListing {
    assets: Vec<ReleaseListingAsset>,
}

#[derive(Deserialize)]
struct ReleaseListingAsset {
    name: String,
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
///
/// The nightly publishes each server under its commit as well as under the fixed name, and the
/// fixed name moves an hour before the dmg does, so the commit's own asset is asked for first.
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
    let client_short = short_sha(&client_sha);
    let asset = format!("zed-remote-server-{os}-{arch}");

    let own = asset_url(&format!("{asset}-{client_short}.gz"));
    let url = if asset_exists(&http, &own).await? {
        own
    } else {
        let latest = fetch_manifest::<ServerManifest>(&http, &asset_url(&format!("{asset}.json")))
            .await
            .map(|manifest| manifest.sha)
            .inspect_err(|error| log::warn!("quiet-ui remote server: {error:#}"))
            .ok();
        if latest.as_deref() == Some(client_sha.as_str()) {
            asset_url(&format!("{asset}.gz"))
        } else {
            let available = available_builds(&http, &asset).await;
            anyhow::bail!(
                "{}",
                refusal(
                    os,
                    arch,
                    &client_short,
                    latest.as_deref().map(short_sha).as_deref(),
                    &available
                )
            );
        }
    };

    Ok(ReleaseAsset {
        version: remote_server_version(&client_sha),
        url,
    })
}

async fn asset_exists(http: &Arc<dyn HttpClient>, url: &str) -> Result<bool> {
    let request = Request::builder()
        .method(Method::HEAD)
        .uri(url)
        .follow_redirects(RedirectPolicy::FollowAll)
        .body(AsyncBody::default())?;
    let response = http
        .send(request)
        .await
        .with_context(|| format!("asking for {url}"))?;
    Ok(response.status().is_success())
}

/// The commits the release has a server for, newest last as the release lists them. Empty when
/// the listing cannot be had, which only costs the refusal its list.
async fn available_builds(http: &Arc<dyn HttpClient>, asset: &str) -> Vec<String> {
    match fetch_manifest::<ReleaseListing>(http, RELEASE_API_URL).await {
        Ok(listing) => builds_in_listing(asset, listing.assets.iter().map(|a| a.name.as_str())),
        Err(error) => {
            log::warn!("quiet-ui remote server: listing the release: {error:#}");
            Vec::new()
        }
    }
}

fn builds_in_listing<'a>(asset: &str, names: impl Iterator<Item = &'a str>) -> Vec<String> {
    names
        .filter_map(|name| {
            let sha = name.strip_prefix(asset)?.strip_prefix('-')?.strip_suffix(".gz")?;
            (sha.len() == 7 && sha.chars().all(|c| c.is_ascii_hexdigit())).then(|| sha.to_string())
        })
        .collect()
}

fn refusal(
    os: &str,
    arch: &str,
    client_short: &str,
    latest_short: Option<&str>,
    available: &[String],
) -> String {
    let mut builds: Vec<&str> = available.iter().map(String::as_str).collect();
    if let Some(latest) = latest_short
        && !builds.contains(&latest)
    {
        builds.push(latest);
    }
    let has = if builds.is_empty() {
        "no build the release lists".to_string()
    } else {
        format!("builds {}", builds.join(", "))
    };
    format!(
        "the quiet-ui release has no {os} {arch} remote server built from {client_short}, which \
         is this Zed; it has {has}. Updating Zed fixes this."
    )
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
    use http_client::{FakeHttpClient, Response};

    #[test]
    fn a_remote_server_is_named_after_the_commit_it_was_built_from() {
        assert_eq!(
            remote_server_version("4287bc6eca0197bc2b27918a3ce3678bc50e58e5"),
            "quiet-ui-4287bc6"
        );
        // A sha shorter than the abbreviation is used as it stands.
        assert_eq!(remote_server_version("4287bc"), "quiet-ui-4287bc");
    }

    /// A release with two older servers under their commits and the latest under the fixed name,
    /// as the nightly leaves it between publishing the servers and the dmg.
    fn release() -> Arc<dyn HttpClient> {
        const ASSETS: [&str; 5] = [
            "zed-remote-server-linux-x86_64-6d6097e.gz",
            "zed-remote-server-linux-x86_64-f04bc31.gz",
            "zed-remote-server-linux-aarch64-f04bc31.gz",
            "zed-remote-server-linux-x86_64.gz",
            "zed-remote-server-linux-x86_64.json",
        ];
        FakeHttpClient::create(|request| async move {
            let url = request.uri().to_string();
            let respond = |status: u16, body: String| {
                Ok(Response::builder()
                    .status(status)
                    .body(AsyncBody::from(body))
                    .unwrap())
            };
            if url == RELEASE_API_URL {
                let assets: Vec<_> = ASSETS
                    .iter()
                    .map(|name| serde_json::json!({ "name": name }))
                    .collect();
                return respond(200, serde_json::json!({ "assets": assets }).to_string());
            }
            let Some(name) = url.strip_prefix(&format!("{RELEASE_DOWNLOAD_BASE}/")) else {
                return respond(404, String::new());
            };
            if !ASSETS.contains(&name) {
                return respond(404, String::new());
            }
            if name.ends_with(".json") {
                return respond(200, r#"{"sha":"aa11bb2ccdd","built_at":"x"}"#.to_string());
            }
            respond(200, String::new())
        })
    }

    fn resolve(client_sha: &str) -> Result<ReleaseAsset> {
        smol::block_on(fetch_remote_server_release(
            release(),
            "linux",
            "x86_64",
            Some(client_sha.to_string()),
        ))
    }

    #[test]
    fn each_client_finds_the_server_from_its_own_commit() {
        let current = resolve("f04bc31e0a").unwrap();
        assert_eq!(
            current.url,
            asset_url("zed-remote-server-linux-x86_64-f04bc31.gz")
        );
        assert_eq!(current.version, "quiet-ui-f04bc31");

        let older = resolve("6d6097ef7").unwrap();
        assert_eq!(
            older.url,
            asset_url("zed-remote-server-linux-x86_64-6d6097e.gz"),
            "a client that has not updated yet still finds its own"
        );

        // The fixed name serves a commit only its manifest vouches for.
        let latest = resolve("aa11bb2ccdd").unwrap();
        assert_eq!(latest.url, asset_url("zed-remote-server-linux-x86_64.gz"));
    }

    #[test]
    fn a_client_with_no_server_is_told_which_builds_there_are() {
        let error = resolve("0123456789").unwrap_err().to_string();
        assert_eq!(
            error,
            "the quiet-ui release has no linux x86_64 remote server built from 0123456, which \
             is this Zed; it has builds 6d6097e, f04bc31, aa11bb2. Updating Zed fixes this."
        );
    }
}
