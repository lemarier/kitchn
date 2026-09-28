//! Repository identity read from a checkout's Git remotes.
//!
//! Kitchen keeps nothing in a working tree, so a checkout is identified by the
//! GitHub `owner/name` its remotes name. `git remote -v` reports fetch and push
//! URLs after `insteadOf` and `pushInsteadOf` rewriting, so every worktree of a
//! repository reports the same identities. A remote that does not name a GitHub
//! repository contributes nothing; a checkout with no such remote is refused.
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

/// The GitHub repositories named by the fetch and push URLs of the checkout
/// containing `start`, in lowercase `owner/name` form.
///
/// # Errors
/// [`HouseError::InvalidInput`] for a relative path,
/// [`HouseError::RedirectedPath`] for a path through a symbolic link,
/// [`HouseError::RepositoryUnidentified`] when `start` is not inside a
/// checkout or no remote names a GitHub repository, and [`HouseError::Git`]
/// when `git` cannot be run within its bounds.
pub fn remote_repositories(start: &Path) -> Result<BTreeSet<Repository>, HouseError> {
    let listing = git(start, &["remote", "-v"])?;
    let lines: Vec<&str> = listing.lines().collect();
    if lines.len() > MAX_REMOTE_LINES {
        return Err(HouseError::InvalidInput);
    }
    let repositories: BTreeSet<Repository> = lines
        .iter()
        .filter_map(|line| {
            let (_, rest) = line.split_once('\t')?;
            let url = rest
                .strip_suffix(" (fetch)")
                .or_else(|| rest.strip_suffix(" (push)"))?;
            parse_remote_url(url)
        })
        .collect();
    if repositories.is_empty() {
        return Err(HouseError::RepositoryUnidentified);
    }
    Ok(repositories)
}

/// The top level of the worktree containing `start`.
///
/// # Errors
/// As [`remote_repositories`]; a bare repository or a path outside any
/// checkout is [`HouseError::RepositoryUnidentified`].
pub fn checkout_root(start: &Path) -> Result<PathBuf, HouseError> {
    let output = git(start, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(output.trim_end_matches('\n'));
    if !root.is_absolute() {
        return Err(HouseError::RepositoryUnidentified);
    }
    Ok(root)
}

/// Normalize a GitHub remote URL to lowercase `owner/name`.
///
/// Accepts `https://`, `http://`, `ssh://`, `git://`, `git+ssh://` and
/// `ssh+git://` URLs with optional user and port, and scp-like
/// `user@github.com:owner/name` forms, each with an optional `.git` suffix or
/// trailing slash. Returns `None` for any other host, a local path, or a path
/// that is not exactly `owner/name`.
#[must_use]
pub fn parse_remote_url(url: &str) -> Option<Repository> {
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
    if !status.success() {
        return Err(HouseError::RepositoryUnidentified);
    }
    Ok(output)
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
        ] {
            assert_eq!(parsed(url), None, "{url}");
        }
    }

    #[test]
    fn a_path_outside_any_checkout_is_unidentified() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        assert!(matches!(
            remote_repositories(&root),
            Err(HouseError::RepositoryUnidentified)
        ));
        assert!(matches!(
            remote_repositories(&root.join("missing")),
            Err(HouseError::RepositoryUnidentified)
        ));
        assert!(matches!(
            remote_repositories(Path::new("relative")),
            Err(HouseError::InvalidInput)
        ));
        Ok(())
    }
}
