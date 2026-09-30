//! GitHub App credentials: installation tokens minted per effect scope,
//! refreshed before expiry, and refused where the app is not installed.
//! Simulated: an in-memory GitHub App API, a fake `gh`, and a fake `curl`.
//! Test keys are generated with `openssl` at test time; no live GitHub App or
//! real private key is used.
#![cfg(unix)]

mod common;

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use kitchen::{
    BackendId, CredentialId, Error, HolderId, HouseId, TaskId,
    adoption::HouseRegistry,
    contracts::{
        AttemptNumber, BranchName, Claimant, Clock, CommitId, EffectExecutor, EffectFailure,
        EffectRequest, ExternalRef, GitHubAction, GitHubEffect, GitHubMutation, Grant,
        IdempotencyKey, IssueNumber, Lookup, Permission, PostingBudget, Repository, Text,
        Timestamp, Trigger, UncertainReason,
    },
    house::{
        ApprovedWrite, CredentialKind, FORGE_BINDING_SCHEMA, ForgeBinding, ForgeCredential,
        ForgeError, ForgeKind, HouseConfig, HouseError, apply_approved, bind_forge,
        credential_path, forge_binding,
    },
    integrations::github::{
        Access, AppApi, AppAuth, AppId, AppPermission, AppRequest, AppResponse, AppTokens,
        CredentialFile, CredentialRef, CurlApi, GhCli, GitHubApp, GitHubExecutor,
        GitHubMutationTransport, HouseScope, InstallationId, Installed, IntegrationError,
        ReadLimits, TokenScope,
    },
};
use ring::signature::{self, KeyPair, UnparsedPublicKey};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const APP_ID: u64 = 4242;
const INSTALLATION: u64 = 77;
const SLUG: &str = "kitchn-app";
const LOGIN: &str = "kitchn-app[bot]";
const HOUR: Duration = Duration::from_secs(60 * 60);
const TIMEOUT: Duration = Duration::from_secs(10);

/// A settable clock shared by the token source and the fake API.
struct TestClock(AtomicU64);
impl TestClock {
    fn at(seconds: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(seconds * 1000)))
    }
    fn advance(&self, by: Duration) -> TestResult {
        self.0
            .fetch_add(u64::try_from(by.as_millis())?, Ordering::SeqCst);
        Ok(())
    }
    fn seconds(&self) -> u64 {
        self.0.load(Ordering::SeqCst) / 1000
    }
}
impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.0.load(Ordering::SeqCst))
    }
}

/// A PEM RSA key made by `openssl` for this test only, in PKCS#1 (as GitHub
/// issues app keys) or PKCS#8, with its public key for checking signatures.
struct TestKey {
    pem: String,
    public: Vec<u8>,
}

fn test_key(pkcs1: bool) -> TestResult<TestKey> {
    let attempts: &[&[&str]] = if pkcs1 {
        // OpenSSL 3 writes PKCS#8 unless told otherwise; LibreSSL has no flag.
        &[&["genrsa", "-traditional", "2048"], &["genrsa", "2048"]]
    } else {
        &[&[
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
        ]]
    };
    let label = if pkcs1 {
        "RSA PRIVATE KEY"
    } else {
        "PRIVATE KEY"
    };
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    for args in attempts {
        let output = Command::new("openssl").args(*args).output()?;
        let pem = String::from_utf8(output.stdout)?;
        let Some(body) = pem
            .trim()
            .strip_prefix(begin.as_str())
            .and_then(|rest| rest.strip_suffix(end.as_str()))
        else {
            continue;
        };
        let der = STANDARD
            .decode(body.split_ascii_whitespace().collect::<String>())
            .map_err(|error| error.to_string())?;
        let pair = if pkcs1 {
            signature::RsaKeyPair::from_der(&der)
        } else {
            signature::RsaKeyPair::from_pkcs8(&der)
        }
        .map_err(|error| error.to_string())?;
        return Ok(TestKey {
            public: pair.public_key().as_ref().to_vec(),
            pem,
        });
    }
    Err(format!("openssl produced no {label}").into())
}

fn write_private(path: &Path, contents: &str) -> TestResult {
    fs::create_dir_all(path.parent().ok_or("no parent")?)?;
    fs::write(path, contents)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    Jwt,
    Installation,
}

#[derive(Debug, Clone)]
struct Call {
    method: &'static str,
    endpoint: String,
    auth: Auth,
    body: Option<Value>,
}

/// GitHub's side of the app: where it is installed, what it may grant, and
/// the tokens it minted.
struct Remote {
    /// Repository full name to (installation, app, slug).
    installations: BTreeMap<String, (u64, u64, String)>,
    /// Permissions the app was granted by its installation.
    allowed: BTreeMap<String, String>,
    /// Token to (repository, expiry in Unix seconds).
    tokens: BTreeMap<String, (String, u64)>,
    calls: Vec<Call>,
    /// Status every mint answers instead of a token.
    mint_status: Option<u16>,
    /// Extra permissions every mint claims to grant.
    extra_grant: Option<(String, String)>,
    lifetime: Duration,
}

impl Remote {
    fn new() -> Self {
        Self {
            installations: [(
                "acme/app".to_owned(),
                (INSTALLATION, APP_ID, SLUG.to_owned()),
            )]
            .into(),
            allowed: [
                ("issues", "write"),
                ("contents", "write"),
                ("pull_requests", "write"),
            ]
            .into_iter()
            .map(|(name, access)| (name.to_owned(), access.to_owned()))
            .collect(),
            tokens: BTreeMap::new(),
            calls: Vec::new(),
            mint_status: None,
            extra_grant: None,
            lifetime: HOUR,
        }
    }

    fn mints(&self) -> Vec<&Call> {
        self.calls
            .iter()
            .filter(|call| call.endpoint.ends_with("/access_tokens"))
            .collect()
    }
}

#[derive(Clone)]
struct FakeApi {
    remote: Arc<Mutex<Remote>>,
    public: Vec<u8>,
    clock: Arc<TestClock>,
}

fn respond(status: u16, body: &Value) -> Result<AppResponse, IntegrationError> {
    Ok(AppResponse {
        status,
        body: body.to_string().into_bytes(),
    })
}

impl FakeApi {
    /// Whether `jwt` is a current RS256 JWT for the app, signed by its key.
    fn jwt_valid(&self, jwt: &str) -> bool {
        let parts: Vec<_> = jwt.split('.').collect();
        let [header, claims, signed] = parts.as_slice() else {
            return false;
        };
        let Ok(signed) = URL_SAFE_NO_PAD.decode(signed) else {
            return false;
        };
        let message = format!("{header}.{claims}");
        if UnparsedPublicKey::new(&signature::RSA_PKCS1_2048_8192_SHA256, &self.public)
            .verify(message.as_bytes(), &signed)
            .is_err()
        {
            return false;
        }
        let decode = |part: &str| -> Option<Value> {
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok()
        };
        let (Some(header), Some(claims)) = (decode(header), decode(claims)) else {
            return false;
        };
        let now = self.clock.seconds();
        let number = |name: &str| claims.get(name).and_then(Value::as_u64);
        header == json!({"alg":"RS256","typ":"JWT"})
            && number("iss") == Some(APP_ID)
            && number("iat").is_some_and(|iat| iat <= now)
            && number("exp").is_some_and(|exp| exp > now)
            && matches!((number("iat"), number("exp")), (Some(iat), Some(exp)) if exp - iat <= 600)
    }
}

impl AppApi for FakeApi {
    fn send(
        &self,
        request: &AppRequest<'_>,
        _timeout: Duration,
    ) -> Result<AppResponse, IntegrationError> {
        let mut remote = self.remote.lock().map_err(|_| IntegrationError::Unknown)?;
        let (auth, secret) = match request.auth {
            AppAuth::Jwt(value) => (Auth::Jwt, value),
            AppAuth::Installation(value) => (Auth::Installation, value),
        };
        remote.calls.push(Call {
            method: request.method,
            endpoint: request.endpoint.clone(),
            auth,
            body: request.body.clone(),
        });
        let now = self.clock.seconds();
        if auth == Auth::Jwt && !self.jwt_valid(secret) {
            return respond(
                401,
                &json!({"message":"A JSON web token could not be decoded"}),
            );
        }
        let endpoint = request.endpoint.as_str();
        if let Some(repository) = endpoint
            .strip_prefix("repos/")
            .and_then(|rest| rest.strip_suffix("/installation"))
        {
            return match remote.installations.get(repository) {
                Some((id, app, slug)) if auth == Auth::Jwt => respond(
                    200,
                    &json!({"id": id, "app_id": app, "app_slug": slug, "account": {"login": "acme"}}),
                ),
                _ => respond(404, &json!({"message":"Not Found"})),
            };
        }
        if let Some(id) = endpoint
            .strip_prefix("app/installations/")
            .and_then(|rest| rest.strip_suffix("/access_tokens"))
        {
            if let Some(status) = remote.mint_status {
                return respond(status, &json!({"message":"refused"}));
            }
            let body = request.body.clone().unwrap_or_default();
            let names: Vec<_> = body["repositories"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let [name] = names.as_slice() else {
                return respond(422, &json!({"message":"one repository"}));
            };
            let Some(repository) = remote
                .installations
                .iter()
                .find(|(full, (installation, ..))| {
                    installation.to_string() == id && full.ends_with(&format!("/{name}"))
                })
                .map(|(full, _)| full.clone())
            else {
                return respond(422, &json!({"message":"not accessible"}));
            };
            let mut granted = serde_json::Map::new();
            for (permission, access) in body["permissions"].as_object().into_iter().flatten() {
                let access = access.as_str().unwrap_or_default();
                let allowed = remote.allowed.get(permission).map(String::as_str);
                if !(allowed == Some(access) || allowed == Some("write") && access == "read") {
                    return respond(422, &json!({"message":"permission not granted"}));
                }
                granted.insert(permission.clone(), json!(access));
            }
            granted.insert("metadata".into(), json!("read"));
            if let Some((name, access)) = &remote.extra_grant {
                granted.insert(name.clone(), json!(access));
            }
            let token = format!("ghs_fake_{}", remote.tokens.len() + 1);
            let expires = now + remote.lifetime.as_secs();
            remote
                .tokens
                .insert(token.clone(), (repository.clone(), expires));
            let expires_at = time::OffsetDateTime::from_unix_timestamp(
                i64::try_from(expires).map_err(|_| IntegrationError::Unknown)?,
            )
            .map_err(|_| IntegrationError::Unknown)?
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| IntegrationError::Unknown)?;
            return respond(
                201,
                &json!({
                    "token": token,
                    "expires_at": expires_at,
                    "permissions": granted,
                    "repositories": [{"full_name": repository}],
                }),
            );
        }
        if endpoint == "installation/repositories" && auth == Auth::Installation {
            return match remote.tokens.get(secret) {
                Some((repository, expires)) if *expires > now => respond(
                    200,
                    &json!({"total_count": 1, "repositories": [{"full_name": repository}]}),
                ),
                _ => respond(401, &json!({"message":"Bad credentials"})),
            };
        }
        respond(404, &json!({"message":"Not Found"}))
    }
}

fn credential_ref(requester: &str) -> TestResult<CredentialRef> {
    Ok(CredentialRef::new(
        HouseId::new("acme")?,
        CredentialId::new("github")?,
        ExternalRef::new(requester)?,
    ))
}

fn app() -> TestResult<GitHubApp> {
    Ok(GitHubApp {
        app_id: AppId::new(APP_ID)?,
        installation: InstallationId::new(INSTALLATION)?,
    })
}

/// An app token source over a key file in `directory`, a fake API, and a
/// clock at an arbitrary fixed start.
struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    remote: Arc<Mutex<Remote>>,
    clock: Arc<TestClock>,
    api: FakeApi,
    key: PathBuf,
}

impl Fixture {
    fn new(pkcs1: bool) -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let key = test_key(pkcs1)?;
        let path = root.join("app.pem");
        write_private(&path, &key.pem)?;
        let remote = Arc::new(Mutex::new(Remote::new()));
        let clock = TestClock::at(1_900_000_000);
        let api = FakeApi {
            remote: Arc::clone(&remote),
            public: key.public,
            clock: Arc::clone(&clock),
        };
        Ok(Self {
            _directory: directory,
            root,
            remote,
            clock,
            api,
            key: path,
        })
    }

    fn tokens(&self) -> TestResult<AppTokens> {
        Ok(AppTokens::new(
            app()?,
            CredentialFile::new(credential_ref(LOGIN)?, self.key.clone())?,
            self.api.clone(),
            Arc::clone(&self.clock) as Arc<dyn Clock + Send + Sync>,
        ))
    }

    fn calls(&self) -> TestResult<Vec<Call>> {
        Ok(self.remote.lock().map_err(|_| "poisoned")?.calls.clone())
    }

    fn remote(&self) -> TestResult<std::sync::MutexGuard<'_, Remote>> {
        Ok(self.remote.lock().map_err(|_| "poisoned")?)
    }
}

fn comment_on(repository: &str) -> TestResult<GitHubMutation> {
    Ok(GitHubMutation {
        repository: Repository::new(repository)?,
        action: GitHubAction::PostComment {
            issue: IssueNumber::new(1)?,
            body: Text::new("approved")?,
        },
    })
}

fn permissions(scope: &TokenScope) -> Vec<(AppPermission, Access)> {
    scope.permissions().collect()
}

#[test]
fn effects_map_to_the_narrowest_permissions_they_need() -> TestResult {
    let comment = TokenScope::for_mutation(&comment_on("acme/app")?);
    assert_eq!(comment.repository().as_str(), "acme/app");
    assert_eq!(
        permissions(&comment),
        [(AppPermission::Issues, Access::Write)]
    );
    let merge = TokenScope::for_mutation(&GitHubMutation {
        repository: Repository::new("acme/app")?,
        action: GitHubAction::MergePullRequest {
            number: IssueNumber::new(2)?,
            expected_head: CommitId::new("4f2a9c1e0b7d3a5f6c8e9d0a1b2c3d4e5f6a7b8c")?,
            expected_base: BranchName::new("main")?,
            expected_base_commit: None,
            method: kitchen::integrations::github::MergeMethod::Squash,
        },
    });
    assert_eq!(
        permissions(&merge),
        [
            (AppPermission::Contents, Access::Write),
            (AppPermission::PullRequests, Access::Read)
        ]
    );
    Ok(())
}

#[test]
fn a_token_is_minted_for_one_repository_and_verified_through_its_installation() -> TestResult {
    for pkcs1 in [true, false] {
        let fixture = Fixture::new(pkcs1)?;
        let tokens = fixture.tokens()?;
        let scope = TokenScope::for_mutation(&comment_on("acme/app")?);
        let token = tokens.token(&credential_ref(LOGIN)?, &scope, TIMEOUT)?;
        assert_eq!(token, "ghs_fake_1");

        let calls = fixture.calls()?;
        let shape: Vec<_> = calls
            .iter()
            .map(|call| (call.method, call.endpoint.as_str(), call.auth))
            .collect();
        assert_eq!(
            shape,
            [
                ("GET", "repos/acme/app/installation", Auth::Jwt),
                ("POST", "app/installations/77/access_tokens", Auth::Jwt),
                ("GET", "installation/repositories", Auth::Installation),
            ],
            "pkcs1: {pkcs1}"
        );
        // Only the target repository and the effect's permissions.
        assert_eq!(
            calls.get(1).and_then(|call| call.body.clone()),
            Some(json!({"repositories": ["app"], "permissions": {"issues": "write"}}))
        );
    }
    Ok(())
}

#[test]
fn a_token_is_reused_until_it_nears_expiry_and_then_replaced() -> TestResult {
    let fixture = Fixture::new(true)?;
    let tokens = fixture.tokens()?;
    let credential = credential_ref(LOGIN)?;
    let scope = TokenScope::for_mutation(&comment_on("acme/app")?);
    assert_eq!(tokens.token(&credential, &scope, TIMEOUT)?, "ghs_fake_1");

    // 55 minutes in, exactly the refresh margin is left: still reused only
    // while strictly more than the margin remains.
    fixture.clock.advance(Duration::from_secs(54 * 60))?;
    assert_eq!(tokens.token(&credential, &scope, TIMEOUT)?, "ghs_fake_1");
    fixture.clock.advance(Duration::from_secs(60))?;
    assert_eq!(tokens.token(&credential, &scope, TIMEOUT)?, "ghs_fake_2");
    assert_eq!(fixture.remote()?.mints().len(), 2);

    // An effect needing the same scope shares the refreshed token.
    let close = TokenScope::for_mutation(&GitHubMutation {
        repository: Repository::new("acme/app")?,
        action: GitHubAction::CloseIssue {
            repository: Repository::new("acme/app")?,
            number: IssueNumber::new(1)?,
            reason: kitchen::integrations::github::CloseReason::Completed,
        },
    });
    assert_eq!(tokens.token(&credential, &close, TIMEOUT)?, "ghs_fake_2");
    assert_eq!(fixture.remote()?.mints().len(), 2);
    Ok(())
}

#[test]
fn a_warm_token_is_never_handed_to_another_credential_reference() -> TestResult {
    let fixture = Fixture::new(true)?;
    let tokens = fixture.tokens()?;
    let scope = TokenScope::for_mutation(&comment_on("acme/app")?);
    assert_eq!(
        tokens.token(&credential_ref(LOGIN)?, &scope, TIMEOUT)?,
        "ghs_fake_1"
    );
    // The cache now holds a token for this scope; another reference still
    // gets nothing from it.
    assert_eq!(
        tokens.token(&credential_ref("other-app[bot]")?, &scope, TIMEOUT),
        Err(IntegrationError::ScopeMismatch)
    );
    assert_eq!(fixture.remote()?.mints().len(), 1);
    Ok(())
}

#[test]
fn an_app_missing_from_the_repository_is_refused_before_any_token_is_minted() -> TestResult {
    let fixture = Fixture::new(true)?;
    let tokens = fixture.tokens()?;
    let credential = credential_ref(LOGIN)?;
    let other = Repository::new("acme/other")?;
    assert_eq!(
        tokens.installed(&credential, &other, TIMEOUT)?,
        Installed::No
    );
    let scope = TokenScope::for_mutation(&comment_on("acme/other")?);
    assert_eq!(
        tokens.token(&credential, &scope, TIMEOUT),
        Err(IntegrationError::ScopeMismatch)
    );
    assert!(fixture.remote()?.mints().is_empty());

    // Installed there, but through another installation of the app.
    fixture
        .remote()?
        .installations
        .insert("acme/other".into(), (INSTALLATION + 1, APP_ID, SLUG.into()));
    assert_eq!(
        tokens.installed(&credential, &other, TIMEOUT)?,
        Installed::No
    );
    assert!(fixture.remote()?.mints().is_empty());

    // A binding whose requester is not the app's bot login is refused.
    assert_eq!(
        tokens.installed(&credential_ref("someone-else[bot]")?, &other, TIMEOUT),
        Err(IntegrationError::ScopeMismatch)
    );
    Ok(())
}

#[test]
fn a_grant_other_than_the_request_is_refused_and_not_cached() -> TestResult {
    let fixture = Fixture::new(true)?;
    let tokens = fixture.tokens()?;
    let credential = credential_ref(LOGIN)?;
    let scope = TokenScope::for_mutation(&comment_on("acme/app")?);

    fixture.remote()?.extra_grant = Some(("administration".into(), "write".into()));
    assert_eq!(
        tokens.token(&credential, &scope, TIMEOUT),
        Err(IntegrationError::ScopeMismatch)
    );
    // The app lacks the permission: GitHub refuses the mint.
    fixture.remote()?.extra_grant = None;
    fixture.remote()?.allowed.remove("issues");
    assert_eq!(
        tokens.token(&credential, &scope, TIMEOUT),
        Err(IntegrationError::ScopeMismatch)
    );
    // A token about to expire is not used.
    fixture
        .remote()?
        .allowed
        .insert("issues".into(), "write".into());
    fixture.remote()?.lifetime = Duration::from_secs(4 * 60);
    assert_eq!(
        tokens.token(&credential, &scope, TIMEOUT),
        Err(IntegrationError::Unknown)
    );
    // An outage recovers on the next call, which mints afresh.
    fixture.remote()?.lifetime = HOUR;
    fixture.remote()?.mint_status = Some(502);
    assert_eq!(
        tokens.token(&credential, &scope, TIMEOUT),
        Err(IntegrationError::Unavailable)
    );
    fixture.remote()?.mint_status = None;
    assert!(
        tokens
            .token(&credential, &scope, TIMEOUT)?
            .starts_with("ghs_fake_")
    );
    Ok(())
}

#[test]
fn an_invalid_or_foreign_key_sends_nothing_valid() -> TestResult {
    let fixture = Fixture::new(true)?;
    let credential = credential_ref(LOGIN)?;
    let scope = TokenScope::for_mutation(&comment_on("acme/app")?);

    write_private(&fixture.key, "not a key")?;
    assert_eq!(
        fixture.tokens()?.token(&credential, &scope, TIMEOUT),
        Err(IntegrationError::InvalidInput)
    );
    assert!(fixture.calls()?.is_empty());

    // A well-formed key of another app signs a JWT GitHub rejects.
    write_private(&fixture.key, &test_key(true)?.pem)?;
    assert_eq!(
        fixture.tokens()?.token(&credential, &scope, TIMEOUT),
        Err(IntegrationError::ScopeMismatch)
    );
    assert!(fixture.remote()?.mints().is_empty());

    // The key file answers only to its own credential reference.
    assert_eq!(
        fixture
            .tokens()?
            .token(&credential_ref("other-app[bot]")?, &scope, TIMEOUT),
        Err(IntegrationError::ScopeMismatch)
    );
    Ok(())
}

/// A fake `gh` that logs each call's token and arguments and serves one
/// issue's comments, keeping a posted comment. With `lose` present, a post
/// lands but its response is lost.
fn fake_gh(directory: &Path) -> TestResult<PathBuf> {
    let gh = directory.join("gh");
    let dir = directory.display();
    common::executable::write_executable(
        &gh,
        format!(
            r#"#!/bin/sh
printf '%s %s\n' "$GH_TOKEN" "$*" >> '{dir}/log'
case "$*" in
  *"--method POST repos/acme/app/issues/1/comments"*)
    cat > '{dir}/posted'
    [ -f '{dir}/lose' ] && exit 1
    printf 'HTTP/2.0 201 Created\r\n\r\n{{}}'
    ;;
  *"--method GET repos/acme/app/issues/1/comments"*)
    if [ -f '{dir}/posted' ]; then
      printf '[{{"id":1,"user":{{"login":"{LOGIN}"}},"html_url":"https://github.com/acme/app/issues/1#issuecomment-1",'
      sed 's/^{{//' '{dir}/posted'
      printf ']'
    else
      printf '[]'
    fi
    ;;
  *) exit 2 ;;
esac
"#
        ),
    )?;
    Ok(gh)
}

/// Lines of the fake `gh` log as (token, method).
fn gh_calls(directory: &Path) -> TestResult<Vec<(String, String)>> {
    let log = match fs::read_to_string(directory.join("log")) {
        Ok(log) => log,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    Ok(log
        .lines()
        .map(|line| {
            let token = line.split(' ').next().unwrap_or_default().to_owned();
            let method = line
                .split(' ')
                .skip_while(|word| *word != "--method")
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            (token, method)
        })
        .collect())
}

fn house_scope() -> TestResult<HouseScope> {
    Ok(HouseScope::new(
        HouseId::new("acme")?,
        [Repository::new("acme/app")?],
        ExternalRef::new(LOGIN)?,
        credential_ref(LOGIN)?,
        PostingBudget::new(5)?,
        [Permission::PostComment],
    )?)
}

#[test]
fn a_refresh_between_a_lost_response_and_its_reconciliation_posts_once() -> TestResult {
    let fixture = Fixture::new(true)?;
    let gh = fake_gh(&fixture.root)?;
    let executor = GitHubExecutor::new(
        BackendId::new("github")?,
        house_scope()?,
        GhCli::app(gh, fixture.tokens()?)?,
        ReadLimits::default(),
    );
    let effect = executor.effect(comment_on("acme/app")?)?;
    let request = EffectRequest::new(
        HouseId::new("acme")?,
        BackendId::new("github")?,
        CredentialId::new("github")?,
        TaskId::new("task-1")?,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new("comment-1")?),
        effect.into(),
    );

    // The post lands but its response is lost.
    fs::write(fixture.root.join("lose"), "")?;
    assert_eq!(
        executor.execute(&request),
        Err(EffectFailure::Uncertain(UncertainReason::Transport))
    );
    fs::remove_file(fixture.root.join("lose"))?;

    // The token nears expiry before core reconciles the effect.
    fixture.clock.advance(Duration::from_secs(58 * 60))?;
    assert!(matches!(executor.lookup(&request)?, Lookup::Applied(_)));
    // A retry of the settled effect finds it instead of posting again.
    assert!(executor.execute(&request).is_ok());

    let calls = gh_calls(&fixture.root)?;
    let posts: Vec<_> = calls
        .iter()
        .filter(|(_, method)| method == "POST")
        .collect();
    assert_eq!(posts.len(), 1, "{calls:?}");
    assert_eq!(
        calls,
        [
            ("ghs_fake_1".to_owned(), "GET".to_owned()),
            ("ghs_fake_1".to_owned(), "POST".to_owned()),
            ("ghs_fake_2".to_owned(), "GET".to_owned()),
            ("ghs_fake_2".to_owned(), "GET".to_owned()),
        ]
    );
    assert_eq!(fixture.remote()?.mints().len(), 2);
    Ok(())
}

#[test]
fn a_failed_refresh_leaves_the_effect_unsubmitted() -> TestResult {
    let fixture = Fixture::new(true)?;
    let gh = fake_gh(&fixture.root)?;
    let executor = GitHubExecutor::new(
        BackendId::new("github")?,
        house_scope()?,
        GhCli::app(gh, fixture.tokens()?)?,
        ReadLimits::default(),
    );
    let request = EffectRequest::new(
        HouseId::new("acme")?,
        BackendId::new("github")?,
        CredentialId::new("github")?,
        TaskId::new("task-1")?,
        AttemptNumber::FIRST,
        IdempotencyKey::from_ref(ExternalRef::new("comment-1")?),
        executor.effect(comment_on("acme/app")?)?.into(),
    );
    fixture.remote()?.mint_status = Some(500);
    assert!(matches!(
        executor.execute(&request),
        Err(EffectFailure::NotApplied(_))
    ));
    assert!(gh_calls(&fixture.root)?.is_empty());
    Ok(())
}

// The approved-write hook with an app binding.

fn house_config() -> TestResult<HouseConfig> {
    let app = Repository::new("acme/app")?;
    let other = Repository::new("acme/other")?;
    let kitchen = CommitId::new("4f2a9c1e0b7d3a5f6c8e9d0a1b2c3d4e5f6a7b8c")?;
    Ok(HouseConfig {
        schema: 1,
        house: HouseId::new("acme")?,
        kitchen: kitchen.clone(),
        guidance: kitchen,
        repositories: [app.clone(), other.clone()].into(),
        posting_destinations: [app.clone(), other.clone()].into(),
        required_reviewers: Default::default(),
        required_checks: Default::default(),
        policy_limits: [app, other]
            .into_iter()
            .map(|repository| -> TestResult<Grant> {
                Ok(Grant::repository(
                    Permission::PostComment,
                    repository,
                    BackendId::new("github")?,
                    CredentialId::new("github")?,
                ))
            })
            .collect::<TestResult<_>>()?,
        grants: Default::default(),
        agents: None,
        stack_tool: None,
        schedules: None,
        merge_readiness: Default::default(),
        disk_pressure: None,
        follow_up: None,
        backend: None,
        graduation: Default::default(),
        tick: None,
    })
}

fn app_binding(requester: &str) -> TestResult<ForgeBinding> {
    Ok(ForgeBinding {
        schema: FORGE_BINDING_SCHEMA,
        house: HouseId::new("acme")?,
        forge: ForgeKind::GitHub,
        backend: BackendId::new("github")?,
        requester: ExternalRef::new(requester)?,
        credential: CredentialId::new("github")?,
        credential_kind: CredentialKind::GitHubApp(app()?),
        posting_budget: PostingBudget::new(5)?,
    })
}

/// A preview that builds one comment effect on its repository.
struct Comment {
    repository: Repository,
    applied: Cell<u32>,
}
impl ApprovedWrite for Comment {
    type Digest = String;
    type Report = GitHubEffect;
    fn repository(&self) -> &Repository {
        &self.repository
    }
    fn digest(&self) -> kitchen::Result<String> {
        Ok("sha256:aaaa".to_owned())
    }
    fn apply<T: GitHubMutationTransport>(
        &self,
        forge: &GitHubExecutor<T>,
        _: &String,
        _: &Claimant,
    ) -> kitchen::Result<GitHubEffect> {
        self.applied.set(self.applied.get() + 1);
        Ok(forge.effect(GitHubMutation {
            repository: self.repository.clone(),
            action: GitHubAction::PostComment {
                issue: IssueNumber::new(1)?,
                body: Text::new("approved")?,
            },
        })?)
    }
}

#[test]
fn an_app_binding_is_stored_with_its_kind_and_needs_a_bot_login() -> TestResult {
    let fixture = Fixture::new(true)?;
    let registry = HouseRegistry::new(fixture.root.join("registry"))?;
    registry.initialize(&house_config()?)?;
    assert!(matches!(
        bind_forge(&registry, &app_binding("kitchn-app")?),
        Err(ForgeError::House(HouseError::InvalidInput))
    ));
    bind_forge(&registry, &app_binding(LOGIN)?)?;
    let stored = fs::read_to_string(fixture.root.join("registry/private/acme/forge.json"))?;
    let stored: Value = serde_json::from_str(&stored)?;
    assert_eq!(
        stored["credentialKind"],
        json!({"gitHubApp": {"appId": APP_ID, "installation": INSTALLATION}})
    );
    assert!(!stored.to_string().contains("PRIVATE KEY"));
    assert_eq!(
        forge_binding(&registry, &HouseId::new("acme")?)?,
        app_binding(LOGIN)?
    );
    Ok(())
}

#[test]
fn a_token_binding_is_stored_without_a_kind() -> TestResult {
    let fixture = Fixture::new(true)?;
    let registry = HouseRegistry::new(fixture.root.join("registry"))?;
    registry.initialize(&house_config()?)?;
    let binding = ForgeBinding {
        requester: ExternalRef::new("acme-bot")?,
        credential_kind: CredentialKind::Token,
        ..app_binding(LOGIN)?
    };
    bind_forge(&registry, &binding)?;
    let stored: Value = serde_json::from_str(&fs::read_to_string(
        fixture.root.join("registry/private/acme/forge.json"),
    )?)?;
    assert_eq!(stored.get("credentialKind"), None);
    Ok(())
}

fn apply_as_app(
    fixture: &Fixture,
    registry: &HouseRegistry,
    write: &Comment,
) -> kitchen::Result<GitHubEffect> {
    let gh = fake_gh(&fixture.root).map_err(|_| HouseError::InvalidInput)?;
    let api = fixture.api.clone();
    let clock = Arc::clone(&fixture.clock);
    apply_approved(
        registry,
        &HouseId::new("acme")?,
        write,
        &"sha256:aaaa".to_owned(),
        &Claimant {
            holder: HolderId::new("session-1")?,
            trigger: Trigger::Interactive,
            consumer: None,
        },
        |credential| match credential {
            ForgeCredential::App { app, key } => GhCli::app(
                gh,
                AppTokens::new(app, key, api, clock as Arc<dyn Clock + Send + Sync>),
            )
            .map_err(ForgeError::Integration),
            ForgeCredential::Token(_) => {
                Err(ForgeError::Integration(IntegrationError::ScopeMismatch))
            }
        },
    )
}

#[test]
fn an_app_not_installed_on_the_write_repository_is_refused_by_name() -> TestResult {
    let fixture = Fixture::new(true)?;
    let registry = HouseRegistry::new(fixture.root.join("registry"))?;
    registry.initialize(&house_config()?)?;
    bind_forge(&registry, &app_binding(LOGIN)?)?;
    let key = fs::read_to_string(&fixture.key)?;
    write_private(&credential_path(&registry, &app_binding(LOGIN)?)?, &key)?;

    let write = Comment {
        repository: Repository::new("acme/other")?,
        applied: Cell::new(0),
    };
    let error = match apply_as_app(&fixture, &registry, &write) {
        Err(Error::Forge(error)) => error,
        other => return Err(format!("unexpected: {other:?}").into()),
    };
    assert!(matches!(
        &error,
        ForgeError::AppNotInstalled { repository, .. } if repository.as_str() == "acme/other"
    ));
    assert!(error.to_string().contains("acme/other"));
    assert_eq!(write.applied.get(), 0);
    assert!(fixture.remote()?.mints().is_empty());
    assert!(gh_calls(&fixture.root)?.is_empty());

    // Installed on the write's repository, the write goes ahead.
    let write = Comment {
        repository: Repository::new("acme/app")?,
        applied: Cell::new(0),
    };
    let effect = apply_as_app(&fixture, &registry, &write)?;
    assert_eq!(write.applied.get(), 1);
    assert_eq!(effect.requester.as_str(), LOGIN);
    Ok(())
}

/// A fake `curl` that records its arguments and configuration and prints
/// `response` followed by curl's write-out of the status, or exits `code`.
fn fake_curl(directory: &Path, response: &str, code: u8) -> TestResult<CurlApi> {
    let curl = directory.join("curl");
    let dir = directory.display();
    common::executable::write_executable(
        &curl,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{dir}/argv'\ncat > '{dir}/config'\nprintf '%s' '{response}'\nexit {code}\n"
        ),
    )?;
    Ok(CurlApi::new(curl)?)
}

#[test]
fn curl_gets_the_credential_on_stdin_and_reports_the_status() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let api = fake_curl(&root, "{\"id\":1}\n201", 0)?;
    let response = api.send(
        &AppRequest {
            method: "POST",
            endpoint: "app/installations/77/access_tokens".into(),
            auth: AppAuth::Jwt("header.claims.signature"),
            body: Some(json!({"repositories": ["app"]})),
        },
        TIMEOUT,
    )?;
    assert_eq!(response.status, 201);
    assert_eq!(response.body, b"{\"id\":1}");
    let argv = fs::read_to_string(root.join("argv"))?;
    assert_eq!(argv, "-q\n--config\n-\n");
    let config = fs::read_to_string(root.join("config"))?;
    assert!(config.contains("url = \"https://api.github.com/app/installations/77/access_tokens\""));
    assert!(config.contains("header = \"Authorization: Bearer header.claims.signature\""));
    assert!(config.contains("data-binary = \"{\\\"repositories\\\":[\\\"app\\\"]}\""));
    assert!(config.contains("proto = \"=https\""));

    // An endpoint that could inject configuration is refused unsent.
    let refused = api.send(
        &AppRequest {
            method: "GET",
            endpoint: "repos/acme/app\"\nurl = \"https://elsewhere".into(),
            auth: AppAuth::Jwt("jwt"),
            body: None,
        },
        TIMEOUT,
    );
    assert!(matches!(refused, Err(IntegrationError::InvalidInput)));

    // A timeout, an oversized reply, and an unparsable status stay errors.
    for (response, code, expected) in [
        ("", 28, IntegrationError::Timeout),
        ("", 63, IntegrationError::LimitExceeded),
        ("", 7, IntegrationError::Unavailable),
        ("{}\nabc", 0, IntegrationError::Unknown),
    ] {
        let api = fake_curl(&root, response, code)?;
        let result = api.send(
            &AppRequest {
                method: "GET",
                endpoint: "installation/repositories".into(),
                auth: AppAuth::Installation("ghs_token"),
                body: None,
            },
            TIMEOUT,
        );
        assert!(
            matches!(result, Err(error) if error == expected),
            "exit {code}"
        );
    }
    assert!(CurlApi::new(PathBuf::from("curl")).is_err());
    Ok(())
}
