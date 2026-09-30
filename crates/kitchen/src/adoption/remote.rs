//! Repository identity read from a checkout's Git remotes.
//!
//! Kitchen keeps nothing in a working tree, so a checkout is identified by the
//! GitHub `owner/name` of one remote: the push destination of the current
//! branch's tracked upstream, else `origin`. `git remote -v` reports URLs after
//! `insteadOf` and `pushInsteadOf` rewriting, and remotes live in the common
//! Git configuration, so every worktree of a repository reports the same
//! identity. Other remotes never choose the identity; a checkout whose chosen
//! remote does not name exactly one GitHub repository is refused.
use super::installer::check_path;
use crate::{
    contracts::Repository,
    git::{GitLimits, GitReadError, run_raw},
    house::HouseError,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Most remote URL lines read from one checkout.
const MAX_REMOTE_LINES: usize = 128;
/// Hosts whose URLs name a GitHub repository.
const GITHUB_HOSTS: [&str; 2] = ["github.com", "ssh.github.com"];

/// A Git remote's name, as `git remote` lists it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemoteName(String);
impl RemoteName {
    /// Accept a name `git` printed: non-empty, bounded, without whitespace or
    /// control characters.
    fn new(name: &str) -> Option<Self> {
        (!name.is_empty()
            && name.len() <= 255
            && !name.chars().any(|c| c.is_whitespace() || c.is_control()))
        .then(|| Self(name.to_owned()))
    }
    /// The name as Git spells it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for RemoteName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One remote and the GitHub repository a URL of it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIdentity {
    /// The remote.
    pub remote: RemoteName,
    /// The repository its URL names, in lowercase `owner/name` form.
    pub repository: Repository,
}

/// The remotes of a checkout, with the one that identifies it singled out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutRemotes {
    /// The push destination of the current branch's tracked upstream, else of
    /// `origin`. This is the only remote that decides the checkout's identity.
    pub selected: RemoteIdentity,
    /// Every other GitHub repository named by a fetch or push URL of another
    /// remote. They cannot choose the identity; they can only make it
    /// ambiguous when they belong to a different house.
    pub others: Vec<RemoteIdentity>,
}

/// The remotes of the checkout containing `start`.
///
/// The identifying remote is the one the current branch tracks (its push URL,
/// after `pushInsteadOf` rewriting), or `origin` when the branch tracks no
/// remote or `HEAD` is detached. A fork checkout therefore identifies itself by
/// the repository its branch pushes to, not by every remote it happens to have.
///
/// # Errors
/// [`HouseError::InvalidInput`] for a relative path,
/// [`HouseError::RedirectedPath`] for a path through a symbolic link,
/// [`HouseError::RepositoryUnidentified`] when `start` is not inside a
/// checkout, there is no such remote, or its push URLs do not name exactly one
/// GitHub repository, and [`HouseError::Git`] when `git` cannot be run within
/// its bounds.
pub fn checkout_remotes(start: &Path) -> Result<CheckoutRemotes, HouseError> {
    let listing = git(start, &["remote", "-v"])?;
    let lines: Vec<&str> = listing.lines().collect();
    if lines.len() > MAX_REMOTE_LINES {
        return Err(HouseError::InvalidInput);
    }
    let selected = selected_remote(start)?;
    let mut push_urls: BTreeSet<Repository> = BTreeSet::new();
    let mut others: Vec<RemoteIdentity> = Vec::new();
    for line in lines {
        let Some((name, rest)) = line.split_once('\t') else {
            continue;
        };
        let (url, is_push) = match (rest.strip_suffix(" (fetch)"), rest.strip_suffix(" (push)")) {
            (Some(url), _) => (url, false),
            (None, Some(url)) => (url, true),
            (None, None) => continue,
        };
        let (Some(remote), Some(repository)) = (RemoteName::new(name), parse_remote_url(url))
        else {
            continue;
        };
        if remote == selected {
            if is_push {
                push_urls.insert(repository);
            }
        } else {
            let identity = RemoteIdentity { remote, repository };
            if !others.contains(&identity) {
                others.push(identity);
            }
        }
    }
    let mut push_urls = push_urls.into_iter();
    match (push_urls.next(), push_urls.next()) {
        (Some(repository), None) => Ok(CheckoutRemotes {
            selected: RemoteIdentity {
                remote: selected,
                repository,
            },
            others,
        }),
        _ => Err(HouseError::RepositoryUnidentified),
    }
}

/// The repository named by `origin`'s fetch URL, if every rewritten push URL
/// names that same repository. CLI defaults use this before reading a binding.
///
/// # Errors
/// [`HouseError::CheckoutRepositoryMismatch`] when a push destination names
/// another repository; otherwise the path, Git, and unidentified errors of
/// [`checkout_remotes`].
pub fn origin_repository(start: &Path) -> Result<Repository, HouseError> {
    fn repositories(urls: &str) -> Result<BTreeSet<Repository>, HouseError> {
        let mut repositories = BTreeSet::new();
        let mut count = 0;
        for url in urls.lines() {
            count += 1;
            if count > MAX_REMOTE_LINES {
                return Err(HouseError::RepositoryUnidentified);
            }
            repositories.insert(parse_remote_url(url).ok_or(HouseError::RepositoryUnidentified)?);
        }
        (!repositories.is_empty())
            .then_some(repositories)
            .ok_or(HouseError::RepositoryUnidentified)
    }

    // Git expands insteadOf and pushInsteadOf in get-url. Read both sides,
    // including all configured URLs, before permitting a registry lookup.
    let fetch = repositories(&git(start, &["remote", "get-url", "--all", "origin"])?)?;
    let push = repositories(&git(
        start,
        &["remote", "get-url", "--push", "--all", "origin"],
    )?)?;
    let mut fetch = fetch.into_iter();
    let repository = match (fetch.next(), fetch.next()) {
        (Some(repository), None) => repository,
        _ => return Err(HouseError::RepositoryUnidentified),
    };
    if push.iter().any(|destination| destination != &repository) {
        return Err(HouseError::CheckoutRepositoryMismatch);
    }
    Ok(repository)
}

/// The remote whose push destination identifies the checkout.
fn selected_remote(start: &Path) -> Result<RemoteName, HouseError> {
    let branch = optional_git(start, &["symbolic-ref", "--quiet", "HEAD"])?;
    let tracked = match branch.as_deref().map(str::trim) {
        Some(reference) if reference.starts_with("refs/heads/") => optional_git(
            start,
            &["for-each-ref", "--format=%(upstream:remotename)", reference],
        )?,
        _ => None,
    };
    // A local upstream branch is reported as ".": it names no remote.
    tracked
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != ".")
        .map_or_else(
            || RemoteName::new("origin"),
            |name| Some(RemoteName::new(name)).flatten(),
        )
        .ok_or(HouseError::RepositoryUnidentified)
}

/// The top level of the worktree containing `start`.
///
/// # Errors
/// As [`checkout_remotes`]; a bare repository or a path outside any
/// checkout is [`HouseError::RepositoryUnidentified`].
pub fn checkout_root(start: &Path) -> Result<PathBuf, HouseError> {
    let output = git(start, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(output.trim_end_matches('\n'));
    if !root.is_absolute() {
        return Err(HouseError::RepositoryUnidentified);
    }
    Ok(root)
}

/// Return the checked out commit only when tracked and untracked files are clean.
///
/// # Errors
/// A dirty checkout is refused; Git reads are bounded by the shared runner.
pub fn clean_checkout_head(start: &Path) -> Result<crate::contracts::CommitId, HouseError> {
    let status = git(
        start,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    if !status.is_empty() {
        return Err(HouseError::DirtyCheckout);
    }
    let head = git(start, &["rev-parse", "--verify", "HEAD"])?;
    crate::contracts::CommitId::new(head.trim())
        .map_err(|_| HouseError::Git(GitReadError::Malformed))
}

/// The current checkout's local branch name, or `None` for detached HEAD.
///
/// # Errors
/// Git reads retain the path and execution bounds used by other checkout reads.
pub fn checkout_branch(start: &Path) -> Result<Option<String>, HouseError> {
    Ok(
        optional_git(start, &["symbolic-ref", "--quiet", "--short", "HEAD"])?
            .map(|branch| branch.trim_end_matches('\n').to_owned()),
    )
}

/// Normalize a GitHub remote URL to lowercase `owner/name`.
///
/// Accepts `https://`, `http://`, `ssh://`, `git://`, `git+ssh://` and
/// `ssh+git://` URLs with optional user and port, and scp-like
/// `user@github.com:owner/name` forms, each with an optional `.git` suffix or
/// trailing slash. Returns `None` for any other host, a local path, a path
/// that is not exactly `owner/name`, or a URL containing a backslash, percent
/// escape, whitespace or control character.
#[must_use]
pub fn parse_remote_url(url: &str) -> Option<Repository> {
    // Parsers disagree about where the host ends when the authority holds a
    // backslash, whitespace, a control character or a percent escape, and this
    // result feeds an authority decision: refuse them outright.
    if url
        .chars()
        .any(|c| c == '\\' || c == '%' || c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git"
        ) {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let host = match host.split_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                host
            }
            Some(_) => return None,
            None => host,
        };
        (host, path)
    } else {
        // scp-like syntax: `[user@]host:path`, where the host has no slash.
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        (
            authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host),
            path,
        )
    };
    if !GITHUB_HOSTS
        .iter()
        .any(|known| host.eq_ignore_ascii_case(known))
    {
        return None;
    }
    let path = path.strip_prefix('/').unwrap_or(path);
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    if name.contains('/') {
        return None;
    }
    Repository::new(&format!(
        "{}/{}",
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    ))
    .ok()
}

/// Run one bounded read-only `git` call in `start`.
fn git(start: &Path, args: &[&str]) -> Result<String, HouseError> {
    optional_git(start, args)?.ok_or(HouseError::RepositoryUnidentified)
}

/// As [`git`], but a command that exits unsuccessfully (for example, no
/// upstream configured) is `None` rather than an error.
fn optional_git(start: &Path, args: &[&str]) -> Result<Option<String>, HouseError> {
    if !start.is_absolute() {
        return Err(HouseError::InvalidInput);
    }
    check_path(start)?;
    if !start.is_dir() {
        return Err(HouseError::RepositoryUnidentified);
    }
    let (status, output) =
        run_raw(start, args, &GitLimits::default()).map_err(|error| match error {
            GitReadError::Malformed => HouseError::RepositoryUnidentified,
            other => HouseError::Git(other),
        })?;
    Ok(status.success().then_some(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(url: &str) -> Option<String> {
        parse_remote_url(url).map(|repository| repository.to_string())
    }

    #[test]
    fn github_urls_normalize_to_lowercase_owner_and_name() {
        for url in [
            "git@github.com:Lemarier/Kitchen.git",
            "git@github.com:lemarier/kitchen",
            "github.com:lemarier/kitchen.git",
            "ssh://git@github.com/lemarier/kitchen.git",
            "ssh://git@ssh.github.com:443/lemarier/kitchen.git",
            "git+ssh://git@github.com/lemarier/kitchen",
            "https://github.com/lemarier/kitchen",
            "https://github.com/lemarier/kitchen/",
            "https://x-access-token:secret@GitHub.com/lemarier/kitchen.git",
            "http://github.com:80/lemarier/kitchen.git",
            "git://github.com/lemarier/kitchen.git",
        ] {
            assert_eq!(parsed(url).as_deref(), Some("lemarier/kitchen"), "{url}");
        }
    }

    #[test]
    fn other_hosts_paths_and_shapes_are_unresolved() {
        for url in [
            "",
            "/srv/git/kitchen.git",
            "./kitchen",
            "file:///srv/git/lemarier/kitchen.git",
            "git@gitlab.com:lemarier/kitchen.git",
            "https://github.com.evil.example/lemarier/kitchen",
            "https://github.com/lemarier",
            "https://github.com/lemarier/kitchen/tree/main",
            "https://github.com:ssh/lemarier/kitchen",
            "git@github.com:-lemarier/kitchen",
            "git@github.com:lemarier/..",
            "https://github.com//kitchen",
            "some/dir:github.com/lemarier/kitchen",
            "https://evil.example\\@github.com/lemarier/kitchen",
            "https://github.com /lemarier/kitchen",
            "https://github.com\t/lemarier/kitchen",
            "https://github.com%2eevil.example/lemarier/kitchen",
            "https://github.com/lemarier/kit%63hen",
            "git@github.com:lemarier/kitchen\u{7f}",
            "https://github.com\u{0}.evil.example/lemarier/kitchen",
        ] {
            assert_eq!(parsed(url), None, "{url}");
        }
    }

    #[test]
    fn a_path_outside_any_checkout_is_unidentified() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        assert!(matches!(
            checkout_remotes(&root),
            Err(HouseError::RepositoryUnidentified)
        ));
        assert!(matches!(
            checkout_remotes(&root.join("missing")),
            Err(HouseError::RepositoryUnidentified)
        ));
        assert!(matches!(
            checkout_remotes(Path::new("relative")),
            Err(HouseError::InvalidInput)
        ));
        Ok(())
    }
}
