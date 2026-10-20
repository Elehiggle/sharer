//! Non-blocking GitHub release checks derived from package repository metadata.

use std::{io::Read as _, sync::mpsc, time::Duration};

use anyhow::{Context as _, Result};
use reqwest::{StatusCode, blocking::Client, header};
use semver::Version;
use serde::Deserialize;
use sharer::upload::ensure_tls_provider;
use url::Url;

const MAX_RELEASE_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(super) enum UpdateCheck {
    Available(UpdateInfo),
    Current,
    Failed(String),
}

#[derive(Debug)]
pub(super) struct UpdateInfo {
    pub(super) version: String,
    pub(super) url: String,
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

pub(super) fn spawn_check(
    tor_proxy: Option<String>,
    require_tor: bool,
) -> Result<mpsc::Receiver<UpdateCheck>> {
    let (sender, receiver) = mpsc::channel();

    if require_tor && tor_proxy.is_none() {
        let _ = sender.send(UpdateCheck::Failed(
            "Skipped because Tor is required but unavailable".to_owned(),
        ));

        return Ok(receiver);
    }

    std::thread::Builder::new()
        .name("sharer-update-check".to_owned())
        .spawn(move || {
            let result = check(tor_proxy.as_deref());
            let event = match result {
                Ok(Some(info)) => UpdateCheck::Available(info),
                Ok(None) => UpdateCheck::Current,
                Err(error) => UpdateCheck::Failed(format!("{error:#}")),
            };

            let _ = sender.send(event);
        })
        .context("failed to start update checker")?;

    Ok(receiver)
}

fn check(tor_proxy: Option<&str>) -> Result<Option<UpdateInfo>> {
    let (endpoint, release_url) = release_urls(env!("CARGO_PKG_REPOSITORY"))?;

    ensure_tls_provider();
    let builder = Client::builder()
        .timeout(Duration::from_secs(8))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none());
    let builder = if let Some(proxy) = tor_proxy {
        builder.proxy(reqwest::Proxy::all(proxy).context("invalid Tor proxy URL")?)
    } else {
        builder
    };
    let client = builder.build().context("failed to create update client")?;
    let response = client
        .get(endpoint)
        .header(header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(
            header::USER_AGENT,
            concat!("sharer/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .context("GitHub release check failed")?;

    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    response
        .error_for_status_ref()
        .context("GitHub release check returned an error")?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub release check returned an unexpected status: {}",
        response.status()
    );
    let limit = u64::try_from(MAX_RELEASE_RESPONSE_BYTES).unwrap_or(u64::MAX);
    let mut body = Vec::with_capacity(4_096);

    response
        .take(limit.saturating_add(1))
        .read_to_end(&mut body)
        .context("failed to read GitHub release response")?;
    anyhow::ensure!(
        body.len() <= MAX_RELEASE_RESPONSE_BYTES,
        "GitHub release response was too large"
    );
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("package version is not semantic versioning")?;

    release_update(&body, &current, &release_url)
}

fn release_urls(repository: &str) -> Result<(Url, Url)> {
    let repository = Url::parse(repository).context("package repository URL is invalid")?;
    let repository_path = repository.path().trim_matches('/').to_owned();

    anyhow::ensure!(
        repository.scheme() == "https"
            && repository.host_str() == Some("github.com")
            && repository.username().is_empty()
            && repository.password().is_none()
            && repository.port().is_none()
            && repository.query().is_none()
            && repository.fragment().is_none()
            && repository_path.split('/').count() == 2
            && repository_path
                .split('/')
                .all(|segment| !segment.is_empty()),
        "package repository must identify a GitHub owner and repository"
    );
    let mut release_url = repository.clone();

    release_url.set_path(&format!("/{repository_path}/releases/latest"));
    let mut endpoint = repository;

    endpoint
        .set_host(Some("api.github.com"))
        .context("could not select the GitHub API host")?;
    endpoint.set_path(&format!("/repos/{repository_path}/releases/latest"));

    Ok((endpoint, release_url))
}

fn release_update(body: &[u8], current: &Version, release_url: &Url) -> Result<Option<UpdateInfo>> {
    let release = serde_json::from_slice::<LatestRelease>(body)
        .context("GitHub release response was invalid")?;
    let latest = Version::parse(release.tag_name.trim_start_matches('v'))
        .context("latest release tag is not semantic versioning")?;

    // The API response supplies version data only. Browser destinations come from
    // package metadata, so response fields cannot select a local or external URL.
    Ok((latest > *current).then_some(UpdateInfo {
        version: latest.to_string(),
        url: release_url.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{release_update, release_urls};
    use semver::Version;

    use super::{UpdateCheck, spawn_check};

    #[test]
    fn required_tor_without_a_proxy_skips_the_network_check() {
        let receiver = spawn_check(None, true).unwrap();
        let event = receiver.try_recv().unwrap();

        assert!(
            matches!(event, UpdateCheck::Failed(message) if message.contains("Tor is required"))
        );
    }

    #[test]
    fn release_destinations_come_from_package_repository() {
        let (endpoint, release_url) = release_urls("https://github.com/example/sharer").unwrap();

        assert_eq!(
            endpoint.as_str(),
            "https://api.github.com/repos/example/sharer/releases/latest"
        );
        assert_eq!(
            release_url.as_str(),
            "https://github.com/example/sharer/releases/latest"
        );
    }

    #[test]
    fn response_urls_cannot_change_update_destination() {
        let (_, release_url) = release_urls("https://github.com/example/sharer").unwrap();
        let current = Version::parse("0.1.0").unwrap();

        for response_url in [
            "file:///tmp/update.desktop",
            "javascript:alert(1)",
            "https://github.com.example.invalid/example/sharer/releases/latest",
            "https://github.com@evil.example/update",
            "https://user:password@github.com/example/sharer/releases/latest",
            "https://github.com:8443/example/sharer/releases/latest",
            "https://github.com/other/project/releases/latest",
            "https://github.com/example/sharer/releases/tag/v0.2.0",
        ] {
            let body = serde_json::to_vec(&serde_json::json!({
                "tag_name": "v0.2.0",
                "html_url": response_url,
            }))
            .unwrap();
            let update = release_update(&body, &current, &release_url)
                .unwrap()
                .unwrap();

            assert_eq!(update.version, "0.2.0");
            assert_eq!(update.url, release_url.as_str());
        }
    }

    #[test]
    fn release_version_controls_notification() {
        let (_, release_url) = release_urls("https://github.com/example/sharer").unwrap();
        let current = Version::parse("0.2.0").unwrap();

        for tag in ["v0.1.9", "v0.2.0", "v0.2.0-beta.1"] {
            let body = serde_json::to_vec(&serde_json::json!({ "tag_name": tag })).unwrap();

            assert!(
                release_update(&body, &current, &release_url)
                    .unwrap()
                    .is_none()
            );
        }

        assert!(release_update(br#"{"tag_name":"invalid"}"#, &current, &release_url).is_err());
        assert!(release_update(b"not JSON", &current, &release_url).is_err());
    }

    #[test]
    fn package_repository_must_identify_a_trusted_https_github_repository() {
        for repository in [
            "http://github.com/example/sharer",
            "https://github.com.example.invalid/example/sharer",
            "https://github.com@evil.example/example/sharer",
            "https://user:password@github.com/example/sharer",
            "https://github.com:8443/example/sharer",
            "https://github.com/example",
            "https://github.com/example//sharer",
            "https://github.com/example/sharer/releases",
            "https://github.com/example/sharer?query=1",
            "https://github.com/example/sharer#fragment",
        ] {
            assert!(release_urls(repository).is_err(), "accepted {repository}");
        }
    }
}
