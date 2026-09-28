//! Self-update from the latest GitHub release.
//!
//! Release contract: the tag is `v<semver>` and the release carries one asset per
//! target named `kitchen-<target-triple>.tar.gz`. The archive contains the
//! executable at any depth, and GitHub must report a SHA-256 digest for the asset.

use std::{
    ffi::OsStr,
    fmt,
    io::{self, Read, Write},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/lemarier/kitchen/releases/latest";
const BINARY_NAME: &str = env!("CARGO_BIN_NAME");
const TARGET: &str = env!("KITCHEN_TARGET");

const RELEASE_LIMIT: u64 = 1024 * 1024;
const ARCHIVE_LIMIT: u64 = 128 * 1024 * 1024;
const UNPACKED_LIMIT: u64 = 512 * 1024 * 1024;
const API_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Result of a completed update check.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The running version matches the latest release.
    UpToDate { version: Version },
    /// The running version is newer than the latest release; nothing is downgraded.
    AheadOfRelease { current: Version, latest: Version },
    /// The executable was replaced with the latest release.
    Updated { from: Version, to: Version },
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UpToDate { version } => write!(f, "{BINARY_NAME} {version} is up to date"),
            Self::AheadOfRelease { current, latest } => write!(
                f,
                "{BINARY_NAME} {current} is newer than the latest release {latest}; not updating"
            ),
            Self::Updated { from, to } => write!(f, "updated {BINARY_NAME} {from} -> {to}"),
        }
    }
}

/// Reasons an update did not complete. The running executable is unchanged
/// unless the error is [`UpdateError::Install`] from the final replacement.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("running version is not a semantic version: {0}")]
    CurrentVersion(#[from] semver::Error),
    #[error("no published release found")]
    NoRelease,
    #[error("request to {url} failed: {source}")]
    Http {
        url: String,
        source: Box<ureq::Error>,
    },
    #[error("latest release response is not valid: {0}")]
    ReleaseFormat(#[from] serde_json::Error),
    #[error("release tag {tag:?} is not a semantic version")]
    ReleaseTag { tag: String },
    #[error("latest release has no asset {name}")]
    MissingAsset { name: String },
    #[error("release asset {name} has no SHA-256 digest")]
    MissingDigest { name: String },
    #[error("release asset {name} does not match its SHA-256 digest")]
    DigestMismatch { name: String },
    #[error("release archive is not readable: {0}")]
    Archive(#[source] io::Error),
    #[error("release archive does not contain {name}")]
    BinaryMissing { name: String },
    #[error("release archive contains more than one {name}")]
    BinaryDuplicate { name: String },
    #[error("release archive contains an empty {name}")]
    BinaryEmpty { name: String },
    #[error("could not replace {path}: {source}")]
    Install { path: PathBuf, source: io::Error },
}

/// Replaces the running executable when the latest GitHub release is newer.
pub fn run() -> Result<Outcome, UpdateError> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    match prepare(&agent(true), LATEST_RELEASE_URL, &current, TARGET)? {
        Prepared::Skip(outcome) => Ok(outcome),
        Prepared::Install { version, binary } => {
            install(&binary)?;
            Ok(Outcome::Updated {
                from: current,
                to: version,
            })
        }
    }
}

fn agent(https_only: bool) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(https_only)
        .build()
        .new_agent()
}

#[derive(Debug)]
enum Prepared {
    Skip(Outcome),
    Install { version: Version, binary: Vec<u8> },
}

fn prepare(
    agent: &ureq::Agent,
    latest_url: &str,
    current: &Version,
    target: &str,
) -> Result<Prepared, UpdateError> {
    let body = match fetch(agent, latest_url, API_TIMEOUT, RELEASE_LIMIT) {
        Err(UpdateError::Http { source, .. })
            if matches!(*source, ureq::Error::StatusCode(404)) =>
        {
            return Err(UpdateError::NoRelease);
        }
        result => result?,
    };
    let release = Release::parse(&body)?;
    if release.version == *current {
        return Ok(Prepared::Skip(Outcome::UpToDate {
            version: release.version,
        }));
    }
    if release.version < *current {
        return Ok(Prepared::Skip(Outcome::AheadOfRelease {
            current: current.clone(),
            latest: release.version,
        }));
    }
    let (asset, digest) = release.asset_for(target)?;
    let archive = fetch(
        agent,
        &asset.browser_download_url,
        DOWNLOAD_TIMEOUT,
        ARCHIVE_LIMIT,
    )?;
    digest.verify(&archive, &asset.name)?;
    let binary = extract_binary(
        &archive,
        &format!("{BINARY_NAME}{}", std::env::consts::EXE_SUFFIX),
    )?;
    Ok(Prepared::Install {
        version: release.version,
        binary,
    })
}

fn fetch(
    agent: &ureq::Agent,
    url: &str,
    timeout: Duration,
    limit: u64,
) -> Result<Vec<u8>, UpdateError> {
    let http = |source| UpdateError::Http {
        url: url.to_owned(),
        source: Box::new(source),
    };
    agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(http)?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(http)
}

#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

#[derive(Debug)]
struct Release {
    version: Version,
    assets: Vec<Asset>,
}

impl Release {
    fn parse(body: &[u8]) -> Result<Self, UpdateError> {
        let json: ReleaseJson = serde_json::from_slice(body)?;
        let tag = json.tag_name.strip_prefix('v').unwrap_or(&json.tag_name);
        let version = Version::parse(tag).map_err(|_| UpdateError::ReleaseTag {
            tag: json.tag_name.clone(),
        })?;
        Ok(Self {
            version,
            assets: json.assets,
        })
    }

    fn asset_for(&self, target: &str) -> Result<(&Asset, Sha256Digest), UpdateError> {
        let name = format!("{BINARY_NAME}-{target}.tar.gz");
        let asset = self
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .ok_or_else(|| UpdateError::MissingAsset { name: name.clone() })?;
        let digest = asset
            .digest
            .as_deref()
            .and_then(|digest| digest.parse().ok())
            .ok_or(UpdateError::MissingDigest { name })?;
        Ok((asset, digest))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Sha256Digest([u8; 32]);

impl FromStr for Sha256Digest {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s.strip_prefix("sha256:").ok_or(())?;
        // `from_str_radix` alone would accept a leading `+`.
        if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(());
        }
        let mut bytes = [0; 32];
        for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().as_chunks::<2>().0) {
            let pair = std::str::from_utf8(pair).map_err(|_| ())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| ())?;
        }
        Ok(Self(bytes))
    }
}

impl Sha256Digest {
    fn verify(&self, bytes: &[u8], name: &str) -> Result<(), UpdateError> {
        if Sha256::digest(bytes).as_slice() == self.0 {
            Ok(())
        } else {
            Err(UpdateError::DigestMismatch {
                name: name.to_owned(),
            })
        }
    }
}

fn extract_binary(archive: &[u8], name: &str) -> Result<Vec<u8>, UpdateError> {
    let unpacked = flate2::read::GzDecoder::new(archive).take(UNPACKED_LIMIT);
    let mut archive = tar::Archive::new(unpacked);
    let mut found = None;
    for entry in archive.entries().map_err(UpdateError::Archive)? {
        let mut entry = entry.map_err(UpdateError::Archive)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(UpdateError::Archive)?;
        if path.file_name() != Some(OsStr::new(name)) {
            continue;
        }
        if found.is_some() {
            return Err(UpdateError::BinaryDuplicate {
                name: name.to_owned(),
            });
        }
        let mut binary = Vec::new();
        entry
            .read_to_end(&mut binary)
            .map_err(UpdateError::Archive)?;
        found = Some(binary);
    }
    match found {
        None => Err(UpdateError::BinaryMissing {
            name: name.to_owned(),
        }),
        Some(binary) if binary.is_empty() => Err(UpdateError::BinaryEmpty {
            name: name.to_owned(),
        }),
        Some(binary) => Ok(binary),
    }
}

fn install(binary: &[u8]) -> Result<(), UpdateError> {
    let exe = std::env::current_exe().map_err(|source| UpdateError::Install {
        path: PathBuf::from(BINARY_NAME),
        source,
    })?;
    let failed = |source| UpdateError::Install {
        path: exe.clone(),
        source,
    };
    let dir = exe
        .parent()
        .ok_or_else(|| failed(io::Error::from(io::ErrorKind::NotFound)))?;
    // Stage beside the executable so the replacement stays on one filesystem.
    let mut staged = tempfile::Builder::new()
        .prefix(".kitchen-update-")
        .tempfile_in(dir)
        .map_err(failed)?;
    staged.write_all(binary).map_err(failed)?;
    staged.as_file().sync_all().map_err(failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))
            .map_err(failed)?;
    }
    self_replace::self_replace(staged.path()).map_err(failed)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        io::{BufRead, BufReader},
        net::TcpListener,
        sync::Mutex,
    };

    use super::*;

    const TARGET: &str = "x86_64-unknown-linux-gnu";

    fn archive(entries: &[(&str, &[u8])]) -> Result<Vec<u8>, io::Error> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(u64::try_from(data.len()).map_err(io::Error::other)?);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, *data)?;
        }
        builder.into_inner()?.finish()
    }

    fn digest_of(bytes: &[u8]) -> String {
        let hex: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("sha256:{hex}")
    }

    fn asset_name() -> String {
        format!("{BINARY_NAME}-{TARGET}.tar.gz")
    }

    fn release_json(tag: &str, base: &str, digest: Option<&str>) -> Vec<u8> {
        serde_json::json!({
            "tag_name": tag,
            "assets": [{
                "name": asset_name(),
                "browser_download_url": format!("{base}/download"),
                "digest": digest,
            }],
        })
        .to_string()
        .into_bytes()
    }

    /// Serves canned responses over plain HTTP and records requested paths.
    struct Server {
        base: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        fn start(
            routes: impl FnOnce(&str) -> HashMap<&'static str, (u16, Vec<u8>)>,
        ) -> Result<Self, io::Error> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let base = format!("http://{}", listener.local_addr()?);
            let routes = routes(&base);
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            // Detached: the thread ends with the test process.
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
                    let mut header = String::new();
                    while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                        header.clear();
                    }
                    let (status, body) = routes
                        .get(path.as_str())
                        .cloned()
                        .unwrap_or((404, Vec::new()));
                    seen.lock().unwrap_or_else(|e| e.into_inner()).push(path);
                    let mut stream = &stream;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(&body);
                }
            });
            Ok(Self { base, requests })
        }

        fn latest(&self) -> String {
            format!("{}/latest", self.base)
        }

        fn requests(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    fn release_server(
        tag: &str,
        archive: Vec<u8>,
        digest: Option<String>,
    ) -> Result<Server, io::Error> {
        let tag = tag.to_owned();
        Server::start(move |base| {
            HashMap::from([
                (
                    "/latest",
                    (200, release_json(&tag, base, digest.as_deref())),
                ),
                ("/download", (200, archive)),
            ])
        })
    }

    #[test]
    fn newer_release_is_downloaded_verified_and_unpacked() -> Result<(), Box<dyn std::error::Error>>
    {
        let archive = archive(&[(
            &format!("{BINARY_NAME}-{TARGET}/{BINARY_NAME}"),
            b"new build",
        )])?;
        let server = release_server("v0.2.0", archive.clone(), Some(digest_of(&archive)))?;
        let prepared = prepare(
            &agent(false),
            &server.latest(),
            &Version::new(0, 1, 0),
            TARGET,
        )?;
        let Prepared::Install { version, binary } = prepared else {
            return Err(format!("expected install, got {prepared:?}").into());
        };
        assert_eq!(version, Version::new(0, 2, 0));
        assert_eq!(binary, b"new build");
        assert_eq!(server.requests(), ["/latest", "/download"]);
        Ok(())
    }

    #[test]
    fn same_or_older_release_is_not_downloaded() -> Result<(), Box<dyn std::error::Error>> {
        for (tag, expected) in [
            (
                "v0.2.0",
                Outcome::UpToDate {
                    version: Version::new(0, 2, 0),
                },
            ),
            (
                "0.1.9",
                Outcome::AheadOfRelease {
                    current: Version::new(0, 2, 0),
                    latest: Version::new(0, 1, 9),
                },
            ),
        ] {
            let server = release_server(tag, Vec::new(), None)?;
            let prepared = prepare(
                &agent(false),
                &server.latest(),
                &Version::new(0, 2, 0),
                TARGET,
            )?;
            let Prepared::Skip(outcome) = prepared else {
                return Err(format!("expected skip, got {prepared:?}").into());
            };
            assert_eq!(outcome, expected);
            assert_eq!(server.requests(), ["/latest"]);
        }
        Ok(())
    }

    #[test]
    fn tampered_archive_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let archive = archive(&[(BINARY_NAME, b"new build")])?;
        let server = release_server("v0.2.0", archive, Some(digest_of(b"something else")))?;
        let result = prepare(
            &agent(false),
            &server.latest(),
            &Version::new(0, 1, 0),
            TARGET,
        );
        assert!(
            matches!(result, Err(UpdateError::DigestMismatch { ref name }) if *name == asset_name()),
            "{result:?}"
        );
        Ok(())
    }

    #[test]
    fn release_without_digest_or_target_asset_is_rejected() -> Result<(), Box<dyn std::error::Error>>
    {
        let server = release_server("v0.2.0", Vec::new(), None)?;
        let result = prepare(
            &agent(false),
            &server.latest(),
            &Version::new(0, 1, 0),
            TARGET,
        );
        assert!(
            matches!(result, Err(UpdateError::MissingDigest { .. })),
            "{result:?}"
        );

        let result = prepare(
            &agent(false),
            &server.latest(),
            &Version::new(0, 1, 0),
            "riscv64gc-unknown-linux-gnu",
        );
        assert!(
            matches!(result, Err(UpdateError::MissingAsset { .. })),
            "{result:?}"
        );
        assert_eq!(server.requests(), ["/latest", "/latest"]);
        Ok(())
    }

    #[test]
    fn missing_release_and_http_failures_are_reported() -> Result<(), Box<dyn std::error::Error>> {
        let server = Server::start(|_| {
            HashMap::from([
                ("/oversized", (200, vec![b' '; 2 * 1024 * 1024])),
                ("/broken", (500, Vec::new())),
            ])
        })?;
        let result = prepare(
            &agent(false),
            &server.latest(),
            &Version::new(0, 1, 0),
            TARGET,
        );
        assert!(matches!(result, Err(UpdateError::NoRelease)), "{result:?}");

        for (path, expected) in [
            ("/oversized", "larger than request limit"),
            ("/broken", "500"),
        ] {
            let result = prepare(
                &agent(false),
                &format!("{}{path}", server.base),
                &Version::new(0, 1, 0),
                TARGET,
            );
            let Err(error @ UpdateError::Http { .. }) = result else {
                return Err(format!("expected HTTP error, got {result:?}").into());
            };
            assert!(error.to_string().contains(expected), "{error}");
        }
        Ok(())
    }

    #[test]
    fn production_agent_refuses_plain_http() -> Result<(), Box<dyn std::error::Error>> {
        let server = release_server("v0.2.0", Vec::new(), None)?;
        let result = prepare(
            &agent(true),
            &server.latest(),
            &Version::new(0, 1, 0),
            TARGET,
        );
        assert!(
            matches!(result, Err(UpdateError::Http { .. })),
            "{result:?}"
        );
        assert!(server.requests().is_empty());
        Ok(())
    }

    #[test]
    fn release_tags_must_be_semantic_versions() -> Result<(), String> {
        for (tag, expected) in [
            ("v1.2.3", Some(Version::new(1, 2, 3))),
            ("1.2.3", Some(Version::new(1, 2, 3))),
            ("latest", None),
            ("v1.2", None),
        ] {
            let body = release_json(tag, "http://unused", None);
            match (Release::parse(&body), expected) {
                (Ok(release), Some(version)) => assert_eq!(release.version, version),
                (Err(UpdateError::ReleaseTag { tag: reported }), None) => assert_eq!(reported, tag),
                (result, _) => return Err(format!("{tag}: unexpected {result:?}")),
            }
        }
        assert!(matches!(
            Release::parse(b"{}"),
            Err(UpdateError::ReleaseFormat(_))
        ));
        Ok(())
    }

    #[test]
    fn digests_require_sha256_and_64_hex_characters() {
        let valid = digest_of(b"x");
        assert!(valid.parse::<Sha256Digest>().is_ok());
        assert!(
            valid
                .to_uppercase()
                .replace("SHA256", "sha256")
                .parse::<Sha256Digest>()
                .is_ok()
        );
        for invalid in [
            valid.replace("sha256:", "sha512:"),
            valid.replace("sha256:", ""),
            valid[..valid.len() - 1].to_owned(),
            format!("{valid}0"),
            valid.replace(&valid[7..9], "zz"),
            format!("sha256:{}", "é".repeat(32)),
            format!("sha256:{}", "+f".repeat(32)),
        ] {
            assert_eq!(invalid.parse::<Sha256Digest>(), Err(()), "{invalid}");
        }
    }

    #[test]
    fn archive_must_contain_exactly_one_non_empty_binary() -> Result<(), Box<dyn std::error::Error>>
    {
        let only_readme = archive(&[("README.md", b"docs")])?;
        assert!(matches!(
            extract_binary(&only_readme, "kitchen"),
            Err(UpdateError::BinaryMissing { .. })
        ));

        let lookalike = archive(&[("kitchen-helper", b"no"), ("dir/kitchen", b"yes")])?;
        assert_eq!(extract_binary(&lookalike, "kitchen")?, b"yes");

        let duplicate = archive(&[("a/kitchen", b"one"), ("b/kitchen", b"two")])?;
        assert!(matches!(
            extract_binary(&duplicate, "kitchen"),
            Err(UpdateError::BinaryDuplicate { .. })
        ));

        let empty = archive(&[("kitchen", b"")])?;
        assert!(matches!(
            extract_binary(&empty, "kitchen"),
            Err(UpdateError::BinaryEmpty { .. })
        ));

        assert!(matches!(
            extract_binary(b"not gzip", "kitchen"),
            Err(UpdateError::Archive(_))
        ));
        Ok(())
    }
}
