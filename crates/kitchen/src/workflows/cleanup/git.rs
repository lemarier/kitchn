//! A bounded, read-only Git reader for worktree preservation evidence.
//!
//! Every call goes through the shared bounded runner in `crate::git`, so it
//! has a deadline and an output limit and ignores the caller's `GIT_DIR`-style
//! overrides. Nothing here writes to the repository.

use std::{
    fmt,
    io::{self, BufRead, BufReader, Read},
    path::Path,
    str::FromStr,
};

use serde::{Serialize, Serializer};

use super::CleanupError;
pub use crate::git::{GitLimits, GitReadError};
use crate::{
    contracts::CommitId,
    git::{run, run_raw, run_with},
};

/// Most ignored paths one inspection lists; more makes the worktree
/// unreadable, which retains it.
pub const MAX_IGNORED_PATHS: usize = 256;
/// Longest single `git ls-files` record accepted while scanning the index.
const MAX_RECORD_BYTES: u64 = 8192;

/// Longest remote name accepted.
const MAX_REMOTE_NAME_BYTES: usize = 64;

/// The name of a Git remote whose remote-tracking refs prove a commit is
/// pushed: the house's forge remote, not a local mirror or a person's backup.
/// Validated on construction, so it is safe as part of a ref pattern. Names
/// with a slash are not accepted here, but a differently configured remote
/// whose name starts with this one followed by `/` would still match its refs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteName(String);

impl RemoteName {
    /// Accept `name` if it is 1 to 64 ASCII letters, digits, `-`, `_`, or `.`
    /// and does not start with `-` or `.` or contain `..`.
    ///
    /// # Errors
    /// Returns [`CleanupError::InvalidRemote`] for anything else, including
    /// pattern characters and path separators.
    pub fn new(name: &str) -> Result<Self, CleanupError> {
        let valid = !name.is_empty()
            && name.len() <= MAX_REMOTE_NAME_BYTES
            && !name.starts_with(['-', '.'])
            && !name.contains("..")
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if valid {
            Ok(Self(name.to_owned()))
        } else {
            Err(CleanupError::InvalidRemote)
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RemoteName {
    type Err = CleanupError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

impl fmt::Display for RemoteName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// An operation Git left unfinished in a worktree. Its progress lives only in
/// the worktree's Git directory and is lost when the worktree is removed, and
/// `HEAD` during it may say nothing about the commits the operation holds, so
/// the worktree is kept and the operation reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GitOperation {
    /// A rebase stopped for a conflict, an `edit` or `break`, or a failed `exec`.
    Rebase,
    /// `git am` stopped on a patch that did not apply.
    ApplyMailbox,
    /// A merge stopped before its commit.
    Merge,
    /// A single cherry-pick stopped.
    CherryPick,
    /// A single revert stopped.
    Revert,
    /// A cherry-pick or revert of several commits stopped part way.
    Sequence,
    /// A bisect that was started and not reset.
    Bisect,
}

impl GitOperation {
    /// The stable kebab-case name, as serialized.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rebase => "rebase",
            Self::ApplyMailbox => "apply-mailbox",
            Self::Merge => "merge",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Sequence => "sequence",
            Self::Bisect => "bisect",
        }
    }
}

impl fmt::Display for GitOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for GitOperation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// What Git reports about one worktree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeState {
    /// The checked-out commit.
    pub head: CommitId,
    /// Whether this is a linked worktree rather than a repository's main checkout.
    pub linked: bool,
    /// Whether the worktree is locked (`git worktree lock`).
    pub locked: bool,
    /// Tracked paths with staged or unstaged changes, including conflicts.
    pub tracked_changes: u32,
    /// Untracked, non-ignored paths.
    pub untracked_files: u32,
    /// Paths Git ignores, from every ignore source, as `git ls-files` lists
    /// them: a wholly ignored directory appears once with a trailing `/`.
    pub ignored: Vec<String>,
    /// Tracked files marked assume-unchanged or skip-worktree, whose edits
    /// `git status` does not report.
    pub hidden_tracked: u32,
    /// Whether `HEAD` is detached rather than on a branch. Commits made on a
    /// detached `HEAD` and then left are remembered only by this worktree's
    /// reflog.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub detached_head: bool,
    /// The operation Git left unfinished in this worktree, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<GitOperation>,
    /// Whether `HEAD` has commits that no remote-tracking ref of a configured
    /// remote contains.
    pub unpushed_commits: bool,
}

/// Inspect the worktree whose top level is `path`. A commit counts as pushed
/// only when a remote-tracking ref of one of `remotes` contains it; with no
/// remotes, nothing is pushed.
///
/// # Errors
/// Returns a [`GitReadError`] when any bounded call fails; callers must treat
/// that as "retain", never as clean.
pub fn inspect_worktree(
    path: &Path,
    limits: &GitLimits,
    remotes: &[RemoteName],
) -> Result<WorktreeState, GitReadError> {
    if !path.is_absolute() || !path.is_dir() {
        return Err(GitReadError::InvalidPath);
    }
    let layout = run(
        path,
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
            "--show-toplevel",
        ],
        limits,
    )?;
    let mut lines = layout.lines();
    let (Some(git_dir), Some(common_dir), Some(top_level), None) =
        (lines.next(), lines.next(), lines.next(), lines.next())
    else {
        return Err(GitReadError::Malformed);
    };
    let canonical = |value: &Path| value.canonicalize().map_err(|_| GitReadError::Malformed);
    if canonical(Path::new(top_level))? != canonical(path)? {
        return Err(GitReadError::NotCheckoutRoot);
    }
    let git_dir = canonical(Path::new(git_dir))?;
    let linked = git_dir != canonical(Path::new(common_dir))?;
    // A lock marker that cannot be read counts as a failure, not as unlocked.
    let locked = linked && state_file_exists(&git_dir, "locked")?;
    let operation = in_progress_operation(&git_dir)?;

    let head = run(
        path,
        ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        limits,
    )?;
    let head = CommitId::new(head.trim_end()).map_err(|_| GitReadError::Malformed)?;
    // Exit 0 names the branch `HEAD` is on, 1 means it is detached; anything
    // else is a failure, never "attached".
    let (symbolic, _) = run_raw(path, ["symbolic-ref", "--quiet", "HEAD"], limits)?;
    let detached_head = match symbolic.code() {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(GitReadError::Failed),
    };

    let status = run(
        path,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        limits,
    )?;
    let (tracked_changes, untracked_files) = count_status(&status)?;
    let ignored = list_ignored(path, limits)?;
    let hidden_tracked = count_hidden_tracked(path, limits)?;

    // `--remotes=<name>` matches `refs/remotes/<name>/*`, so a remote such as
    // `origin-backup` does not match `origin`. Git allows a remote's name to
    // hold a slash, and then a remote `origin/backup` would match too. With no
    // remotes the `--not` list is empty, so `HEAD` itself is listed: nothing
    // counts as pushed.
    let mut unpushed_args = vec![
        "rev-list".to_owned(),
        "--max-count=1".to_owned(),
        "HEAD".to_owned(),
        "--not".to_owned(),
    ];
    unpushed_args.extend(remotes.iter().map(|remote| format!("--remotes={remote}")));
    let unpushed = run(path, unpushed_args, limits)?;
    Ok(WorktreeState {
        head,
        linked,
        locked,
        tracked_changes,
        untracked_files,
        ignored,
        hidden_tracked,
        detached_head,
        operation,
        unpushed_commits: !unpushed.trim().is_empty(),
    })
}

/// Whether `name` exists in the Git directory, without following symlinks. A
/// path that cannot be examined is an error, never "absent".
fn state_file_exists(git_dir: &Path, name: &str) -> Result<bool, GitReadError> {
    match git_dir.join(name).symlink_metadata() {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(GitReadError::Failed),
    }
}

/// The operation Git left unfinished in the worktree whose Git directory is
/// `git_dir`, judged from the state files Git keeps there (the same ones its
/// shell prompt reads). These files are per worktree, so another worktree's
/// operation does not show here. When several are present, the first in this
/// order is reported.
///
/// An unfinished operation says nothing through `git status`, and during it
/// `HEAD` can sit on a pushed base while the branch's own commits wait in the
/// operation's to-do list, so the pushed check on `HEAD` alone cannot vouch for
/// them. The branch itself survives the worktree's removal; the operation's
/// progress does not.
fn in_progress_operation(git_dir: &Path) -> Result<Option<GitOperation>, GitReadError> {
    if state_file_exists(git_dir, "rebase-merge")? {
        return Ok(Some(GitOperation::Rebase));
    }
    if state_file_exists(git_dir, "rebase-apply")? {
        // `rebasing` marks an old-style rebase; `applying` or nothing, `am`.
        return Ok(Some(
            if state_file_exists(git_dir, "rebase-apply/rebasing")? {
                GitOperation::Rebase
            } else {
                GitOperation::ApplyMailbox
            },
        ));
    }
    for (name, operation) in [
        ("MERGE_HEAD", GitOperation::Merge),
        ("CHERRY_PICK_HEAD", GitOperation::CherryPick),
        ("REVERT_HEAD", GitOperation::Revert),
        ("sequencer", GitOperation::Sequence),
        ("BISECT_LOG", GitOperation::Bisect),
    ] {
        if state_file_exists(git_dir, name)? {
            return Ok(Some(operation));
        }
    }
    Ok(None)
}

/// Count tracked and untracked entries in `git status --porcelain=v1 -z`
/// output. Renames and copies carry their source path as an extra field.
fn count_status(output: &str) -> Result<(u32, u32), GitReadError> {
    let mut tracked: u32 = 0;
    let mut untracked: u32 = 0;
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    while let Some(entry) = fields.next() {
        let code = entry.get(..2).ok_or(GitReadError::Malformed)?;
        if entry.get(2..3) != Some(" ") {
            return Err(GitReadError::Malformed);
        }
        if code == "??" {
            untracked = untracked.saturating_add(1);
            continue;
        }
        tracked = tracked.saturating_add(1);
        if code.contains(['R', 'C']) && fields.next().is_none() {
            return Err(GitReadError::Malformed);
        }
    }
    Ok((tracked, untracked))
}

/// The paths Git ignores in the checkout at `path`, from `.gitignore` files,
/// `.git/info/exclude`, and the user's global excludes alike. A wholly ignored
/// directory is listed once, so build output does not flood the list.
fn list_ignored(path: &Path, limits: &GitLimits) -> Result<Vec<String>, GitReadError> {
    let listing = run(
        path,
        [
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
        limits,
    )?;
    let mut ignored: Vec<String> = listing
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    ignored.sort();
    ignored.dedup();
    if ignored.len() > MAX_IGNORED_PATHS {
        return Err(GitReadError::OutputTooLarge);
    }
    Ok(ignored)
}

/// How many tracked files carry the assume-unchanged or skip-worktree flag.
/// `git status` does not report edits to them. The index listing can be far
/// larger than the output limit, so it is filtered while it streams and only
/// the deadline bounds the scan.
pub(crate) fn count_hidden_tracked(path: &Path, limits: &GitLimits) -> Result<u32, GitReadError> {
    count_hidden_tracked_with_args(path, ["ls-files", "-v", "-z"], limits)
}

pub(crate) fn count_hidden_tracked_pinned(
    path: &Path,
    pinned: &str,
    limits: &GitLimits,
) -> Result<u32, GitReadError> {
    count_hidden_tracked_with_args(path, ["-c", pinned, "ls-files", "-v", "-z"], limits)
}

fn count_hidden_tracked_with_args<const N: usize>(
    path: &Path,
    args: [&str; N],
    limits: &GitLimits,
) -> Result<u32, GitReadError> {
    let (status, hidden) = run_with(path, args, limits, count_hidden)?;
    if !status.success() {
        return Err(GitReadError::Failed);
    }
    Ok(hidden)
}

/// Count `git ls-files -v -z` records whose tag is not `H` (an ordinary
/// cached file): `S` is skip-worktree, lowercase is assume-unchanged, and any
/// unknown tag is treated as hidden rather than as clean.
fn count_hidden(stdout: impl Read) -> io::Result<u32> {
    let mut reader = BufReader::new(stdout);
    let mut record = Vec::new();
    let mut hidden: u32 = 0;
    loop {
        record.clear();
        let read = (&mut reader)
            .take(MAX_RECORD_BYTES)
            .read_until(0, &mut record)?;
        if read == 0 {
            return Ok(hidden);
        }
        // A record is `<tag> <path>` ending in NUL; anything else is malformed.
        if record.last() != Some(&0) || record.get(1) != Some(&b' ') {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if record.first() != Some(&b'H') {
            hidden = hidden.saturating_add(1);
        }
    }
}

/// Whether `name`, a top-level entry of the checkout at `dir`, is ignored by
/// Git and holds only ignored content: no tracked file, and no untracked file
/// that Git does not ignore. A rule such as `name/*` with `!name/keep` ignores
/// the entry while leaving a file of someone's work inside it.
///
/// # Errors
/// Returns a [`GitReadError`] when a call fails; callers must treat that as
/// "not build output".
pub(super) fn ignored_untracked(
    dir: &Path,
    name: &str,
    limits: &GitLimits,
) -> Result<bool, GitReadError> {
    // `check-ignore` reads a leading `:` as pathspec magic, so a directory
    // named `:(top)target` would be checked as `target`. `./` prevents that.
    let relative = format!("./{name}/");
    let (ignored, _) = run_raw(
        dir,
        ["check-ignore", "--quiet", "--", relative.as_str()],
        limits,
    )?;
    match ignored.code() {
        Some(0) => {}
        Some(1) => return Ok(false),
        _ => return Err(GitReadError::Failed),
    }
    // Literal, so the name is neither magic nor a glob.
    let entry = format!("{name}/");
    let tracked = run(
        dir,
        [
            "--literal-pathspecs",
            "ls-files",
            "-z",
            "--",
            entry.as_str(),
        ],
        limits,
    )?;
    if !tracked.is_empty() {
        return Ok(false);
    }
    let unignored = run(
        dir,
        [
            "--literal-pathspecs",
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
            "--",
            entry.as_str(),
        ],
        limits,
    )?;
    Ok(unignored.is_empty())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A Git directory holding exactly `names`, each an empty file except
    /// those ending in `/`, which are directories.
    fn git_dir_with(names: &[&str]) -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        for name in names {
            if name.ends_with('/') {
                std::fs::create_dir_all(dir.path().join(name))?;
            } else {
                if let Some(parent) = Path::new(name).parent() {
                    std::fs::create_dir_all(dir.path().join(parent))?;
                }
                std::fs::write(dir.path().join(name), b"")?;
            }
        }
        Ok(dir)
    }

    #[test]
    fn each_state_file_names_its_operation() -> Result<(), Box<dyn std::error::Error>> {
        let kind = |names: &[&str]| -> Result<_, Box<dyn std::error::Error>> {
            Ok(in_progress_operation(git_dir_with(names)?.path())?)
        };
        assert_eq!(kind(&[])?, None);
        // Files Git keeps for every worktree do not count.
        assert_eq!(kind(&["HEAD", "index", "locked", "logs/"])?, None);
        assert_eq!(kind(&["rebase-merge/"])?, Some(GitOperation::Rebase));
        assert_eq!(
            kind(&["rebase-apply/rebasing"])?,
            Some(GitOperation::Rebase)
        );
        assert_eq!(
            kind(&["rebase-apply/applying"])?,
            Some(GitOperation::ApplyMailbox)
        );
        // A bare `rebase-apply` is `am` or a rebase; either way it is kept.
        assert_eq!(kind(&["rebase-apply/"])?, Some(GitOperation::ApplyMailbox));
        assert_eq!(kind(&["MERGE_HEAD"])?, Some(GitOperation::Merge));
        assert_eq!(kind(&["CHERRY_PICK_HEAD"])?, Some(GitOperation::CherryPick));
        assert_eq!(kind(&["REVERT_HEAD"])?, Some(GitOperation::Revert));
        assert_eq!(kind(&["sequencer/"])?, Some(GitOperation::Sequence));
        assert_eq!(kind(&["BISECT_LOG"])?, Some(GitOperation::Bisect));
        // A rebase that stops on a conflict also leaves `REBASE_HEAD` and
        // possibly `MERGE_HEAD`; the rebase is what is reported.
        assert_eq!(
            kind(&["rebase-merge/", "MERGE_HEAD", "BISECT_LOG"])?,
            Some(GitOperation::Rebase)
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_state_file_that_cannot_be_read_is_a_failure_not_a_clean_worktree()
    -> Result<(), Box<dyn std::error::Error>> {
        // A regular file where the Git directory should be: every lookup
        // beneath it fails with something other than "not found".
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("not-a-directory");
        std::fs::write(&file, b"")?;
        assert_eq!(in_progress_operation(&file), Err(GitReadError::Failed));
        Ok(())
    }

    #[test]
    fn remote_names_are_plain_and_bounded() {
        for name in [
            "origin",
            "upstream",
            "fork-2",
            "my_remote",
            "a.b",
            &"x".repeat(64),
        ] {
            assert_eq!(
                RemoteName::new(name).map(|n| n.to_string()),
                Ok(name.to_owned())
            );
        }
        assert_eq!("origin".parse(), RemoteName::new("origin"));
        for name in [
            "",
            "-origin",
            ".hidden",
            "a..b",
            "a/b",
            "or*",
            "or?gin",
            "or[i]gin",
            "with space",
            "new\nline",
            "café",
            &"x".repeat(65),
        ] {
            assert_eq!(
                RemoteName::new(name),
                Err(CleanupError::InvalidRemote),
                "{name:?}"
            );
        }
    }

    #[test]
    fn status_counts_tracked_untracked_and_renames() {
        assert_eq!(count_status(""), Ok((0, 0)));
        assert_eq!(
            count_status(" M a.rs\0?? new.txt\0R  b.rs\0old b.rs\0UU c.rs\0"),
            Ok((3, 1))
        );
    }

    #[test]
    fn only_ordinary_cached_files_are_not_hidden() {
        let index = |records: &[u8]| count_hidden(records);
        assert_eq!(index(b"").ok(), Some(0));
        assert_eq!(index(b"H a.rs\0H dir/b.rs\0").ok(), Some(0));
        // Assume-unchanged (lowercase), skip-worktree, and any unknown tag.
        assert_eq!(
            index(b"h a.rs\0S b.rs\0s c.rs\0H d.rs\0X e.rs\0").ok(),
            Some(4)
        );
    }

    #[test]
    fn malformed_index_records_are_errors() {
        let kind = |records: &[u8]| count_hidden(records).map_err(|error| error.kind());
        assert_eq!(kind(b"H"), Err(io::ErrorKind::InvalidData));
        assert_eq!(kind(b"H_a.rs\0"), Err(io::ErrorKind::InvalidData));
        let overlong = [b"H ".as_slice(), &vec![b'a'; 9000], b"\0"].concat();
        assert_eq!(kind(&overlong), Err(io::ErrorKind::InvalidData));
    }

    #[test]
    fn malformed_status_is_an_error() {
        assert_eq!(count_status("M"), Err(GitReadError::Malformed));
        assert_eq!(count_status("MMx.rs\0"), Err(GitReadError::Malformed));
        // A rename without its source path.
        assert_eq!(count_status("R  b.rs\0"), Err(GitReadError::Malformed));
    }

    #[test]
    fn relative_and_missing_paths_are_refused_without_running_git() {
        let limits = GitLimits {
            program: PathBuf::from("/nonexistent/git"),
            ..GitLimits::default()
        };
        assert_eq!(
            inspect_worktree(Path::new("relative"), &limits, &[]),
            Err(GitReadError::InvalidPath)
        );
        assert_eq!(
            inspect_worktree(Path::new("/nonexistent/worktree"), &limits, &[]),
            Err(GitReadError::InvalidPath)
        );
    }

    #[test]
    fn a_missing_program_is_a_spawn_failure() {
        let limits = GitLimits {
            program: PathBuf::from("/nonexistent/git"),
            ..GitLimits::default()
        };
        assert_eq!(
            inspect_worktree(&std::env::temp_dir(), &limits, &[]),
            Err(GitReadError::Spawn)
        );
    }
}
