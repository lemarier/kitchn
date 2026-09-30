//! Explicit, previewed changes to a house's standing authority.
use clap::Args;
use kitchen::{
    HouseId,
    adoption::{HouseRegistry, RepositoryMatch},
    contracts::{Grant, GrantScope, Permission, Repository},
    house::{HouseConfig, HouseError, Workflow, forge_binding, workflow_grant_permissions},
};
use std::{
    collections::BTreeSet,
    io::{self, Write},
    path::PathBuf,
};

#[derive(Args)]
pub struct GrantArgs {
    /// A named workflow's authority set.
    #[arg(
        long,
        required_unless_present = "permission",
        conflicts_with = "permission"
    )]
    workflow: Option<Workflow>,
    /// One standing permission (for example launch-worker).
    #[arg(long)]
    permission: Option<Permission>,
    /// External registry directory (default: ~/.kitchn).
    #[arg(long)]
    registry: Option<PathBuf>,
    /// House to change; inferred only when the registry has one house.
    #[arg(long)]
    house: Option<HouseId>,
    /// Repository to scope the grant to; inferred only when the house has one.
    #[arg(long)]
    repository: Option<Repository>,
    /// Grant house scope where supported, or revoke matching authority across the house.
    #[arg(long)]
    house_wide: bool,
    /// Show the exact change without writing or prompting.
    #[arg(long, conflicts_with = "yes")]
    preview: bool,
    /// Apply the displayed change without an interactive confirmation.
    #[arg(long)]
    yes: bool,
}

pub fn run(args: GrantArgs, revoke: bool) -> Result<(String, bool), kitchen::Error> {
    let root = match args.registry {
        Some(path) => super::house::canonical_root(path)?,
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(HouseError::InvalidInput)?
            .join(".kitchn"),
    };
    let registry = HouseRegistry::new(root)?;
    let house_id = match args.house {
        Some(id) => id,
        None => {
            let listing = registry.houses()?;
            if let Some(repository) = &args.repository {
                let claims: Vec<_> = listing
                    .available
                    .iter()
                    .filter(|house| house.repositories.contains(repository))
                    .collect();
                if claims.len() != 1 {
                    return Err(HouseError::HouseSelection.into());
                }
                claims[0].house.clone()
            } else if listing.available.len() == 1 && listing.unavailable.is_empty() {
                listing.available[0].house.clone()
            } else {
                current_selection(&registry)?.0
            }
        }
    };
    let current = registry.load(&house_id)?;
    let repository = match args.repository {
        Some(repository) if current.repositories.contains(&repository) => repository,
        Some(_) => return Err(HouseError::HouseSelection.into()),
        None if current.repositories.len() == 1 => current
            .repositories
            .iter()
            .next()
            .cloned()
            .ok_or(HouseError::InvalidInput)?,
        None => {
            let (selected_house, repository) = current_selection(&registry)?;
            if selected_house != house_id {
                return Err(HouseError::HouseSelection.into());
            }
            repository
        }
    };
    let permissions = selected_permissions(args.workflow, args.permission)?;
    let requested = if revoke {
        current
            .grants
            .union(&current.policy_limits)
            .filter(|grant| {
                permissions.contains(&grant.permission)
                    && (args.house_wide
                        || matches!(&grant.scope, GrantScope::Repository(scope) if scope == &repository))
            })
            .cloned()
            .collect()
    } else {
        match (args.workflow, args.permission) {
            (Some(workflow), None) => {
                workflow_grants(&registry, &current, workflow, &repository, args.house_wide)?
            }
            (None, Some(permission)) => BTreeSet::from([one_grant(
                &registry,
                &current,
                permission,
                &repository,
                args.house_wide,
            )?]),
            _ => return Err(HouseError::InvalidInput.into()),
        }
    };
    let mut next = current.clone();
    let mut changed = Vec::new();
    for grant in requested {
        let (standing, limit) = if revoke {
            let standing = next.grants.remove(&grant);
            let limit = next.policy_limits.remove(&grant);
            (standing, limit)
        } else {
            let standing = next.grants.insert(grant.clone());
            let limit = next.policy_limits.insert(grant.clone());
            (standing, limit)
        };
        if standing || limit {
            changed.push((grant, standing, limit));
        }
    }
    next.validate()?;
    let verb = if revoke { "Revoke" } else { "Grant" };
    let mut preview = format!(
        "{verb} authority for house {} in {}:\n",
        current.house,
        registry.root().display()
    );
    if changed.is_empty() {
        preview.push_str("  No standing grants change.\n");
    }
    for (grant, standing, limit) in &changed {
        let targets = if grant.targets.is_empty() {
            String::new()
        } else {
            format!(
                " | targets {}",
                grant
                    .targets
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        preview.push_str(&format!(
            "  {} | {} | {} | {}{} | {}{}\n",
            grant.permission,
            grant.scope,
            grant.destination,
            grant.credential,
            targets,
            if *standing { "standing" } else { "" },
            if *limit {
                if *standing {
                    ", policy limit"
                } else {
                    "policy limit"
                }
            } else {
                ""
            },
        ));
    }
    if revoke && !args.house_wide {
        let retained: BTreeSet<_> = current
            .grants
            .union(&current.policy_limits)
            .filter(|grant| {
                matches!(grant.scope, GrantScope::House) && permissions.contains(&grant.permission)
            })
            .map(|grant| grant.permission.to_string())
            .collect();
        if !retained.is_empty() {
            preview.push_str(&format!(
                "  Retained house-scoped authority for {}. Use --house-wide to revoke matching authority across all repositories.\n",
                retained.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    preview.push_str("No workers, schedules, or external effects are started.\n");
    if args.preview || changed.is_empty() {
        return Ok((preview, true));
    }
    if !args.yes {
        let mut stderr = io::stderr().lock();
        write!(stderr, "{preview}Apply this change? [y/N]: ").map_err(HouseError::from)?;
        stderr.flush().map_err(HouseError::from)?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(HouseError::from)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            return Ok(("Declined. No authority changed.".into(), true));
        }
    }
    registry.configure_grants(&current, &next)?;
    Ok((format!("{preview}Applied."), true))
}

fn current_selection(registry: &HouseRegistry) -> Result<(HouseId, Repository), kitchen::Error> {
    let cwd = std::env::current_dir().map_err(HouseError::from)?;
    Ok(match registry.resolve_repository(&cwd)? {
        RepositoryMatch::Bound(binding) => (binding.house, binding.repository),
        RepositoryMatch::Unbound { house, repository } => (house, repository),
    })
}

fn workflow_grants(
    registry: &HouseRegistry,
    house: &HouseConfig,
    workflow: Workflow,
    repository: &Repository,
    house_wide: bool,
) -> Result<BTreeSet<Grant>, kitchen::Error> {
    selected_permissions(Some(workflow), None)?
        .iter()
        .map(|permission| one_grant(registry, house, *permission, repository, house_wide))
        .collect()
}

fn selected_permissions(
    workflow: Option<Workflow>,
    permission: Option<Permission>,
) -> Result<Vec<Permission>, kitchen::Error> {
    match (workflow, permission) {
        (Some(workflow), None) => workflow_grant_permissions(workflow)
            .map(<[Permission]>::to_vec)
            .ok_or_else(|| HouseError::InvalidInput.into()),
        (None, Some(permission)) => Ok(vec![permission]),
        _ => Err(HouseError::InvalidInput.into()),
    }
}

fn one_grant(
    registry: &HouseRegistry,
    house: &HouseConfig,
    permission: Permission,
    repository: &Repository,
    house_wide: bool,
) -> Result<Grant, kitchen::Error> {
    use Permission::*;
    // A Roger destination has no house binding to infer here; target-scoped
    // authority also needs named targets the command does not accept.
    if permission.is_target_scoped() || permission == AskHuman {
        return Err(HouseError::InvalidInput.into());
    }
    if permission == Merge {
        return Err(HouseError::MergeGrantNeedsGate.into());
    }
    let forge = matches!(
        permission,
        PostComment
            | EditLabels
            | CreateIssue
            | CloseIssue
            | EditIssueRelationships
            | PushBranch
            | OpenPullRequest
            | ReviewPullRequest
            | RequestReview
            | Merge
            | Publish
    );
    let (destination, credential) = if forge {
        let binding = forge_binding(registry, &house.house)?;
        (binding.backend, binding.credential)
    } else {
        let binding = house.backend.as_ref().ok_or(HouseError::InvalidInput)?;
        (binding.backend.clone(), binding.credential.clone())
    };
    let repository_scoped = matches!(
        permission,
        LaunchWorker
            | PostComment
            | EditLabels
            | CreateIssue
            | CloseIssue
            | EditIssueRelationships
            | PushBranch
            | OpenPullRequest
            | ReviewPullRequest
            | RequestReview
            | Merge
            | Publish
    );
    Ok(if !house_wide || repository_scoped {
        Grant::repository(permission, repository.clone(), destination, credential)
    } else {
        Grant::house(permission, destination, credential)
    })
}
