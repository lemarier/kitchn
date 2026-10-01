//! GitHub App installation tokens as forge credentials.
//!
//! A house can write as a GitHub App instead of a person. Its binding names
//! the app and one installation; the app's private key stays in the house's
//! private credential file, read only when a token is minted. [`AppTokens`]
//! signs a short RS256 JWT with that key, checks through
//! `GET /repos/{repository}/installation` that the bound installation of this
//! app covers the repository, and mints an installation token limited to that
//! repository and the permissions a [`TokenScope`] names. It then verifies the
//! token through `GET /installation/repositories`, since an installation token
//! cannot read `/user`.
//!
//! Tokens are cached per scope and replaced once less than [`REFRESH_MARGIN`]
//! of their life is left, which is longer than any single bounded call, so a
//! token never expires during the request it was fetched for. Minting and
//! refreshing only happen before a request is sent and never resend one: a
//! refused or failed mint leaves the effect unsubmitted, and a retry after an
//! uncertain submission still goes through core reconciliation.
//!
//! [`AppApi`] is the HTTP boundary for these three calls. [`CurlApi`] sends
//! them with `curl`, passing the JWT or token in a configuration read from
//! stdin, never in its arguments.

use std::{
    collections::BTreeMap,
    fmt,
    num::NonZeroU64,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::{rand::SystemRandom, signature};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{CredentialFile, CredentialRef, GitHubAction, GitHubMutation, IntegrationError};
use crate::contracts::{Clock, Repository, Timestamp};

/// A token with less life left than this is replaced before use. It exceeds
/// the longest bounded provider call, so a fetched token outlives its request.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
/// JWT lifetime; GitHub accepts at most ten minutes.
const JWT_LIFETIME: Duration = Duration::from_secs(9 * 60);
/// JWT issue time is backdated this much to tolerate clock drift.
const JWT_BACKDATE: Duration = Duration::from_secs(60);
/// Largest App API response accepted, in bytes.
const RESPONSE_LIMIT: usize = 64 * 1024;

/// A GitHub App's numeric ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AppId(NonZeroU64);

/// The numeric ID of one installation of a GitHub App on an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstallationId(NonZeroU64);

macro_rules! numeric_id {
    ($name:ident) => {
        impl $name {
            /// Build the ID from GitHub's number.
            ///
            /// # Errors
            /// [`IntegrationError::InvalidInput`] for zero.
            pub fn new(id: u64) -> Result<Self, IntegrationError> {
                NonZeroU64::new(id)
                    .map(Self)
                    .ok_or(IntegrationError::InvalidInput)
            }

            /// GitHub's number.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}
numeric_id!(AppId);
numeric_id!(InstallationId);

/// The GitHub App a house writes as, and the installation it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubApp {
    /// The app.
    pub app_id: AppId,
    /// Its installation on the account owning the house's repositories.
    pub installation: InstallationId,
}

/// A repository permission an installation token can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AppPermission {
    /// `contents`: branches, commits, and merges.
    Contents,
    /// `issues`: issues, their comments, labels, and relationships.
    Issues,
    /// `pull_requests`: pull requests.
    PullRequests,
    /// `checks`: check runs on commits.
    Checks,
    /// `statuses`: commit statuses.
    Statuses,
    /// `administration`: branch-protection requirements.
    Administration,
}

impl AppPermission {
    /// GitHub's name for the permission.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Contents => "contents",
            Self::Issues => "issues",
            Self::PullRequests => "pull_requests",
            Self::Checks => "checks",
            Self::Statuses => "statuses",
            Self::Administration => "administration",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            Self::Contents,
            Self::Issues,
            Self::PullRequests,
            Self::Checks,
            Self::Statuses,
            Self::Administration,
        ]
        .into_iter()
        .find(|permission| permission.as_str() == name)
    }
}

/// Access level of an [`AppPermission`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    /// `read`.
    Read,
    /// `write`, which includes read.
    Write,
}

impl Access {
    /// GitHub's name for the level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// One repository and the permissions a token for it carries. Every
/// installation token also carries `metadata: read`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TokenScope {
    repository: Repository,
    permissions: BTreeMap<AppPermission, Access>,
}

impl TokenScope {
    /// Repository contents write access for a checked Git branch push.
    #[must_use]
    pub fn for_push(repository: &Repository) -> Self {
        Self {
            repository: repository.clone(),
            permissions: [(AppPermission::Contents, Access::Write)]
                .into_iter()
                .collect(),
        }
    }
    /// Read access for a repository. Check runs, commit statuses, and branch
    /// protection need separate permissions requested only for those reads.
    #[must_use]
    pub fn for_read(repository: &Repository, extra: Option<AppPermission>) -> Self {
        use AppPermission::{Contents, Issues, PullRequests};
        let mut permissions: BTreeMap<_, _> = [Contents, Issues, PullRequests]
            .into_iter()
            .map(|permission| (permission, Access::Read))
            .collect();
        if let Some(extra) = extra {
            permissions.insert(extra, Access::Read);
        }
        Self {
            repository: repository.clone(),
            permissions,
        }
    }

    /// The scope a GitHub effect needs: its repository, write access for the
    /// change, and read access for what inspecting it reads.
    ///
    /// These mappings follow GitHub's documented fine-grained permissions and
    /// have not been checked against a live installation.
    #[must_use]
    pub fn for_mutation(mutation: &GitHubMutation) -> Self {
        use AppPermission::{Contents, Issues, PullRequests};
        use GitHubAction as Action;
        let permissions: &[(AppPermission, Access)] = match mutation.action {
            Action::CloseIssue { .. }
            | Action::PostComment { .. }
            | Action::SetLabel { .. }
            | Action::CreateLabel { .. }
            | Action::CreateIssue { .. }
            | Action::LinkSubIssue { .. }
            | Action::LinkDependency { .. } => &[(Issues, Access::Write)],
            Action::MergePullRequest { .. } => {
                &[(Contents, Access::Write), (PullRequests, Access::Read)]
            }
            Action::OpenPullRequest { .. } => {
                &[(PullRequests, Access::Write), (Contents, Access::Read)]
            }
            Action::ReviewPullRequest { .. } => {
                &[(PullRequests, Access::Write), (Contents, Access::Read)]
            }
            Action::ReplyToReviewThread { .. } | Action::ResolveReviewThread { .. } => {
                &[(PullRequests, Access::Write)]
            }
        };
        Self {
            repository: mutation.repository.clone(),
            permissions: permissions.iter().copied().collect(),
        }
    }

    /// The repository-scoped token for posting and verifying a gate review.
    #[must_use]
    pub fn for_review(repository: Repository) -> Self {
        Self {
            repository,
            permissions: [
                (AppPermission::PullRequests, Access::Write),
                (AppPermission::Contents, Access::Read),
            ]
            .into(),
        }
    }

    /// Minimal token for GraphQL review-thread readback.
    #[must_use]
    pub fn for_thread_read(repository: &Repository) -> Self {
        Self {
            repository: repository.clone(),
            permissions: [(AppPermission::PullRequests, Access::Read)].into(),
        }
    }

    /// Read-only scope for one repository's issues, pull requests, and refs.
    #[must_use]
    pub fn for_reads(repository: Repository) -> Self {
        Self {
            repository,
            permissions: [
                (AppPermission::Contents, Access::Read),
                (AppPermission::Issues, Access::Read),
                (AppPermission::PullRequests, Access::Read),
            ]
            .into(),
        }
    }

    /// The only repository the token reaches.
    #[must_use]
    pub const fn repository(&self) -> &Repository {
        &self.repository
    }

    /// The permissions requested, besides the implicit `metadata: read`.
    pub fn permissions(&self) -> impl Iterator<Item = (AppPermission, Access)> + '_ {
        self.permissions
            .iter()
            .map(|(name, access)| (*name, *access))
    }

    fn mint_body(&self) -> Result<Value, IntegrationError> {
        let (_, name) = split(&self.repository)?;
        let permissions: serde_json::Map<_, _> = self
            .permissions()
            .map(|(name, access)| (name.as_str().to_owned(), json!(access.as_str())))
            .collect();
        Ok(json!({ "repositories": [name], "permissions": permissions }))
    }

    /// Whether GitHub granted exactly the requested permissions, ignoring
    /// the implicit `metadata: read`.
    fn granted_exactly(&self, granted: &BTreeMap<String, String>) -> bool {
        let mut rest = 0_usize;
        for (name, access) in granted {
            if name == "metadata" && access == "read" {
                continue;
            }
            let requested = AppPermission::parse(name)
                .and_then(|permission| self.permissions.get(&permission))
                .is_some_and(|expected| expected.as_str() == access);
            if !requested {
                return false;
            }
            rest += 1;
        }
        rest == self.permissions.len()
    }
}

/// How an App API request authenticates. Its value never appears in `Debug`.
#[derive(Clone, Copy)]
pub enum AppAuth<'a> {
    /// The app's signed JWT.
    Jwt(&'a str),
    /// An installation token.
    Installation(&'a str),
}

impl AppAuth<'_> {
    /// The bearer value.
    #[must_use]
    pub const fn secret(&self) -> &str {
        match self {
            Self::Jwt(value) | Self::Installation(value) => value,
        }
    }
}

impl fmt::Debug for AppAuth<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Jwt(_) => "Jwt([private])",
            Self::Installation(_) => "Installation([private])",
        })
    }
}

/// One App API request, relative to `https://api.github.com/`.
#[derive(Debug)]
pub struct AppRequest<'a> {
    /// `GET` or `POST`.
    pub method: &'static str,
    /// Relative endpoint, without a leading slash.
    pub endpoint: String,
    /// Credential.
    pub auth: AppAuth<'a>,
    /// JSON body, when the method sends one.
    pub body: Option<Value>,
}

/// An App API response.
#[derive(Debug)]
pub struct AppResponse {
    /// HTTP status.
    pub status: u16,
    /// Response body, at most [`AppApi`]'s byte bound.
    pub body: Vec<u8>,
}

/// HTTP boundary for the GitHub App API.
pub trait AppApi {
    /// Send one request within `timeout` and return its status and body.
    ///
    /// # Errors
    /// Transport failures, timeouts, and oversized responses; HTTP error
    /// statuses are responses, not errors.
    fn send(
        &self,
        request: &AppRequest<'_>,
        timeout: Duration,
    ) -> Result<AppResponse, IntegrationError>;
}

/// [`AppApi`] over an installed `curl`, pinned by absolute path. The
/// credential and body travel in a configuration on stdin, never in argv.
#[derive(Debug, Clone)]
pub struct CurlApi {
    executable: PathBuf,
}

impl CurlApi {
    /// Select the `curl` binary without running it.
    ///
    /// # Errors
    /// Refuses a relative path.
    pub fn new(executable: PathBuf) -> Result<Self, IntegrationError> {
        if !executable.is_absolute() {
            return Err(IntegrationError::InvalidInput);
        }
        Ok(Self { executable })
    }
}

/// Quote a value for a curl configuration file.
fn curl_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

impl AppApi for CurlApi {
    fn send(
        &self,
        request: &AppRequest<'_>,
        timeout: Duration,
    ) -> Result<AppResponse, IntegrationError> {
        if !request
            .endpoint
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/-_.".contains(&byte))
            || !request.auth.secret().bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(IntegrationError::InvalidInput);
        }
        let mut config = [
            format!("url = \"https://api.github.com/{}\"", request.endpoint),
            format!("request = \"{}\"", request.method),
            "header = \"Accept: application/vnd.github+json\"".to_owned(),
            "header = \"X-GitHub-Api-Version: 2022-11-28\"".to_owned(),
            "header = \"User-Agent: kitchn\"".to_owned(),
            format!(
                "header = {}",
                curl_quote(&format!("Authorization: Bearer {}", request.auth.secret()))
            ),
            "proto = \"=https\"".to_owned(),
            "silent".to_owned(),
            format!("max-time = {}", timeout.as_secs().max(1)),
            format!("max-filesize = {RESPONSE_LIMIT}"),
            "write-out = \"\\n%{http_code}\"".to_owned(),
        ]
        .join("\n");
        if let Some(body) = &request.body {
            let body = serde_json::to_string(body).map_err(|_| IntegrationError::InvalidInput)?;
            config.push_str("\nheader = \"Content-Type: application/json\"\ndata-binary = ");
            config.push_str(&curl_quote(&body));
        }
        config.push('\n');
        let output = super::process::run(
            &self.executable,
            &["-q".into(), "--config".into(), "-".into()],
            config.as_bytes(),
            &[],
            timeout,
            RESPONSE_LIMIT + 16,
        )?;
        match output.code {
            Some(0) => {}
            Some(28) => return Err(IntegrationError::Timeout),
            Some(63) => return Err(IntegrationError::LimitExceeded),
            _ => return Err(IntegrationError::Unavailable),
        }
        let split = output
            .stdout
            .iter()
            .rposition(|byte| *byte == b'\n')
            .ok_or(IntegrationError::Unknown)?;
        let (body, status) = output.stdout.split_at(split);
        let status = std::str::from_utf8(status.get(1..).unwrap_or_default())
            .ok()
            .and_then(|code| code.trim().parse::<u16>().ok())
            .filter(|code| (100..=599).contains(code))
            .ok_or(IntegrationError::Unknown)?;
        Ok(AppResponse {
            status,
            body: body.to_vec(),
        })
    }
}

/// Whether the bound installation covers a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// The bound installation of this app covers it.
    Yes,
    /// The app is not installed there, or through another installation.
    No,
}

/// A minted installation token and when it expires. Never printed.
struct Minted {
    token: String,
    expires: Timestamp,
}

/// Mints, caches, and refreshes installation tokens for one bound app.
pub struct AppTokens {
    app: GitHubApp,
    key: CredentialFile,
    api: Box<dyn AppApi + Send + Sync>,
    clock: Arc<dyn Clock + Send + Sync>,
    cache: Mutex<BTreeMap<TokenScope, Minted>>,
}

impl fmt::Debug for AppTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppTokens")
            .field("app", &self.app)
            .finish_non_exhaustive()
    }
}

impl AppTokens {
    /// Bind the app, its private key file, the API, and a clock. Nothing is
    /// read or sent until a token is needed.
    pub fn new(
        app: GitHubApp,
        key: CredentialFile,
        api: impl AppApi + Send + Sync + 'static,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Self {
        Self {
            app,
            key,
            api: Box::new(api),
            clock,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// The bound app.
    #[must_use]
    pub const fn app(&self) -> GitHubApp {
        self.app
    }

    /// Whether the bound installation covers `repository`, read with the
    /// app's JWT from `GET /repos/{repository}/installation`.
    ///
    /// # Errors
    /// [`IntegrationError::ScopeMismatch`] when the credential is not the
    /// bound one or the installation belongs to another app or identity;
    /// key, transport, and response failures.
    pub fn installed(
        &self,
        credential: &CredentialRef,
        repository: &Repository,
        timeout: Duration,
    ) -> Result<Installed, IntegrationError> {
        let deadline = Deadline::new(timeout);
        let jwt = self.jwt(credential)?;
        self.check_installed(credential, repository, &jwt, &deadline)
    }

    /// An installation token for `scope`, from the cache while more than
    /// [`REFRESH_MARGIN`] of its life is left, otherwise freshly minted and
    /// verified. Never sends anything but the three App API calls.
    ///
    /// # Errors
    /// [`IntegrationError::ScopeMismatch`] when the installation does not
    /// cover the repository, belongs to another app or identity, or GitHub
    /// grants other permissions or repositories than requested; key,
    /// transport, and response failures.
    pub fn token(
        &self,
        credential: &CredentialRef,
        scope: &TokenScope,
        timeout: Duration,
    ) -> Result<String, IntegrationError> {
        // Checked before the cache: a warm token is never handed to a
        // credential reference other than the one this source is bound to.
        if credential != self.key.reference() {
            return Err(IntegrationError::ScopeMismatch);
        }
        let deadline = Deadline::new(timeout);
        let fresh_until = self.clock.now().saturating_add(REFRESH_MARGIN);
        if let Some(cached) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(scope)
            .filter(|minted| minted.expires > fresh_until)
        {
            return Ok(cached.token.clone());
        }
        let jwt = self.jwt(credential)?;
        match self.check_installed(credential, &scope.repository, &jwt, &deadline)? {
            Installed::Yes => {}
            Installed::No => return Err(IntegrationError::ScopeMismatch),
        }
        let minted = self.mint(scope, &jwt, &deadline)?;
        if minted.expires <= self.clock.now().saturating_add(REFRESH_MARGIN) {
            return Err(IntegrationError::Unknown);
        }
        self.verify(scope, &minted.token, &deadline)?;
        let token = minted.token.clone();
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(scope.clone(), minted);
        Ok(token)
    }

    fn check_installed(
        &self,
        credential: &CredentialRef,
        repository: &Repository,
        jwt: &str,
        deadline: &Deadline,
    ) -> Result<Installed, IntegrationError> {
        #[derive(Deserialize)]
        struct Installation {
            id: u64,
            app_id: u64,
            app_slug: String,
        }
        let response = self.api.send(
            &AppRequest {
                method: "GET",
                endpoint: format!("repos/{repository}/installation"),
                auth: AppAuth::Jwt(jwt),
                body: None,
            },
            deadline.remaining()?,
        )?;
        let installation: Installation = match response.status {
            200 => parse(&response.body)?,
            404 => return Ok(Installed::No),
            _ => return Err(refusal(&response)),
        };
        let login = format!("{}[bot]", installation.app_slug);
        if installation.app_id != self.app.app_id.get()
            || !login.eq_ignore_ascii_case(credential.requester().as_str())
        {
            return Err(IntegrationError::ScopeMismatch);
        }
        Ok(if installation.id == self.app.installation.get() {
            Installed::Yes
        } else {
            Installed::No
        })
    }

    fn mint(
        &self,
        scope: &TokenScope,
        jwt: &str,
        deadline: &Deadline,
    ) -> Result<Minted, IntegrationError> {
        #[derive(Deserialize)]
        struct Created {
            token: String,
            expires_at: String,
            permissions: BTreeMap<String, String>,
            repositories: Vec<Named>,
        }
        let response = self.api.send(
            &AppRequest {
                method: "POST",
                endpoint: format!("app/installations/{}/access_tokens", self.app.installation),
                auth: AppAuth::Jwt(jwt),
                body: Some(scope.mint_body()?),
            },
            deadline.remaining()?,
        )?;
        if response.status != 201 {
            return Err(refusal(&response));
        }
        let created: Created = parse(&response.body)?;
        if created.token.is_empty() || !created.token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(IntegrationError::Unknown);
        }
        if !scope.granted_exactly(&created.permissions)
            || !only(&created.repositories, &scope.repository)
        {
            return Err(IntegrationError::ScopeMismatch);
        }
        let expires = time::OffsetDateTime::parse(
            &created.expires_at,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| IntegrationError::Unknown)?;
        let millis = u64::try_from(expires.unix_timestamp_nanos() / 1_000_000)
            .map_err(|_| IntegrationError::Unknown)?;
        Ok(Minted {
            token: created.token,
            expires: Timestamp::from_unix_millis(millis),
        })
    }

    /// Verify the minted token's identity through its installation: it must
    /// reach exactly the requested repository.
    fn verify(
        &self,
        scope: &TokenScope,
        token: &str,
        deadline: &Deadline,
    ) -> Result<(), IntegrationError> {
        #[derive(Deserialize)]
        struct Listed {
            total_count: u64,
            repositories: Vec<Named>,
        }
        let response = self.api.send(
            &AppRequest {
                method: "GET",
                endpoint: "installation/repositories".to_owned(),
                auth: AppAuth::Installation(token),
                body: None,
            },
            deadline.remaining()?,
        )?;
        if response.status != 200 {
            return Err(refusal(&response));
        }
        let listed: Listed = parse(&response.body)?;
        if listed.total_count != 1 || !only(&listed.repositories, &scope.repository) {
            return Err(IntegrationError::ScopeMismatch);
        }
        Ok(())
    }

    /// Sign a short-lived RS256 JWT with the app's private key.
    fn jwt(&self, credential: &CredentialRef) -> Result<String, IntegrationError> {
        let pem = self.key.load_bytes(credential)?;
        let key = private_key(&pem)?;
        let now = self.clock.now().as_unix_millis() / 1000;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            json!({
                "iat": now.saturating_sub(JWT_BACKDATE.as_secs()),
                "exp": now.saturating_add(JWT_LIFETIME.as_secs()),
                "iss": self.app.app_id.get(),
            })
            .to_string(),
        );
        let message = format!("{header}.{claims}");
        let mut signature = vec![0; key.public().modulus_len()];
        key.sign(
            &signature::RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            message.as_bytes(),
            &mut signature,
        )
        .map_err(|_| IntegrationError::Unavailable)?;
        Ok(format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

/// Parse a PEM RSA private key: PKCS#1 (`RSA PRIVATE KEY`, as GitHub issues
/// them) or PKCS#8 (`PRIVATE KEY`). The error never carries key material.
fn private_key(pem: &[u8]) -> Result<signature::RsaKeyPair, IntegrationError> {
    let text = std::str::from_utf8(pem).map_err(|_| IntegrationError::InvalidInput)?;
    for (label, pkcs8) in [("RSA PRIVATE KEY", false), ("PRIVATE KEY", true)] {
        let begin = format!("-----BEGIN {label}-----");
        let end = format!("-----END {label}-----");
        let Some(body) = text
            .trim()
            .strip_prefix(begin.as_str())
            .and_then(|rest| rest.strip_suffix(end.as_str()))
        else {
            continue;
        };
        let encoded: String = body.split_ascii_whitespace().collect();
        let der = STANDARD
            .decode(encoded)
            .map_err(|_| IntegrationError::InvalidInput)?;
        let key = if pkcs8 {
            signature::RsaKeyPair::from_pkcs8(&der)
        } else {
            signature::RsaKeyPair::from_der(&der)
        };
        return key.map_err(|_| IntegrationError::InvalidInput);
    }
    Err(IntegrationError::InvalidInput)
}

#[derive(Deserialize)]
struct Named {
    full_name: String,
}

/// Whether `repositories` names exactly `repository`. GitHub names are
/// case-insensitive.
fn only(repositories: &[Named], repository: &Repository) -> bool {
    matches!(repositories, [one] if one.full_name.eq_ignore_ascii_case(repository.as_str()))
}

fn split(repository: &Repository) -> Result<(&str, &str), IntegrationError> {
    repository
        .as_str()
        .split_once('/')
        .ok_or(IntegrationError::InvalidInput)
}

fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, IntegrationError> {
    serde_json::from_slice(body).map_err(|_| IntegrationError::Unknown)
}

/// A refused App API call. A rate limit (429, or a 403 whose message says
/// so, as GitHub answers a secondary limit) is an outage to retry later;
/// other client errors are refusals, and anything else an outage.
fn refusal(response: &AppResponse) -> IntegrationError {
    match response.status {
        429 => IntegrationError::Unavailable,
        403 if mentions_rate_limit(&response.body) => IntegrationError::Unavailable,
        400..=499 => IntegrationError::HttpStatus(response.status),
        _ => IntegrationError::Unavailable,
    }
}

/// Whether a response body names a rate limit, in any case.
fn mentions_rate_limit(body: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"rate limit";
    body.windows(NEEDLE.len())
        .any(|window| window.eq_ignore_ascii_case(NEEDLE))
}

/// One deadline shared by the calls a token needs.
struct Deadline {
    started: Instant,
    timeout: Duration,
}

impl Deadline {
    fn new(timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            timeout,
        }
    }

    fn remaining(&self) -> Result<Duration, IntegrationError> {
        self.timeout
            .checked_sub(self.started.elapsed())
            .filter(|left| !left.is_zero())
            .ok_or(IntegrationError::Timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_values_are_quoted_so_they_cannot_add_options() {
        assert_eq!(curl_quote("plain"), "\"plain\"");
        assert_eq!(
            curl_quote("a\"b\\c\nurl = \"https://evil\""),
            "\"a\\\"b\\\\c\\nurl = \\\"https://evil\\\"\""
        );
    }

    #[test]
    fn keys_in_neither_pem_form_are_refused() {
        for pem in [
            &b""[..],
            b"not a key",
            b"-----BEGIN RSA PRIVATE KEY-----\n!!!\n-----END RSA PRIVATE KEY-----",
            b"-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----",
            b"-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----",
            b"\xff\xfe",
        ] {
            assert!(matches!(
                private_key(pem),
                Err(IntegrationError::InvalidInput)
            ));
        }
    }

    #[test]
    fn granted_permissions_must_match_the_request_exactly() -> Result<(), IntegrationError> {
        let scope = TokenScope {
            repository: Repository::new("acme/app").map_err(|_| IntegrationError::InvalidInput)?,
            permissions: [(AppPermission::Issues, Access::Write)].into(),
        };
        let granted = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(name, access)| ((*name).to_owned(), (*access).to_owned()))
                .collect()
        };
        assert!(scope.granted_exactly(&granted(&[("issues", "write"), ("metadata", "read")])));
        assert!(scope.granted_exactly(&granted(&[("issues", "write")])));
        assert!(!scope.granted_exactly(&granted(&[("issues", "read")])));
        assert!(!scope.granted_exactly(&granted(&[("issues", "write"), ("contents", "write")])));
        assert!(!scope.granted_exactly(&granted(&[("metadata", "write")])));
        assert!(!scope.granted_exactly(&granted(&[])));
        Ok(())
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::{AppResponse, IntegrationError, refusal};

    fn response(status: u16, body: &str) -> AppResponse {
        AppResponse {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn rate_limits_are_outages_and_other_client_errors_refusals() {
        assert_eq!(refusal(&response(429, "")), IntegrationError::Unavailable);
        assert_eq!(
            refusal(&response(
                403,
                r#"{"message":"You have exceeded a secondary Rate Limit."}"#
            )),
            IntegrationError::Unavailable
        );
        assert_eq!(
            refusal(&response(
                403,
                r#"{"message":"Resource not accessible by integration"}"#
            )),
            IntegrationError::HttpStatus(403)
        );
        assert_eq!(
            refusal(&response(422, r#"{"message":"rate"}"#)),
            IntegrationError::HttpStatus(422)
        );
        assert_eq!(refusal(&response(502, "")), IntegrationError::Unavailable);
    }
}
