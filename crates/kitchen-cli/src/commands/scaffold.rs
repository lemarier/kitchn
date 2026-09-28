//! Preview-first repository creation and adoption.
use clap::Args;
use kitchen::{
    HouseId,
    adoption::{FileStatus, HouseRegistry},
    contracts::Repository,
    house::HouseError,
    scaffold::{ScaffoldError, TemplateName, VariableName, plan_repository},
};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    path::{Component, PathBuf},
};

#[derive(Args)]
pub struct ScaffoldArgs {
    /// Destination directory (created only after confirmation).
    #[arg(default_value = ".")]
    target: PathBuf,
    /// External house registry created by kitchen house init.
    #[arg(long)]
    registry: PathBuf,
    /// Template name in the selected house's pinned guidance snapshot.
    #[arg(long)]
    template: TemplateName,
    /// Required for an unbound repository; never inferred from available houses.
    #[arg(long)]
    house: Option<HouseId>,
    /// Forge owner/name; required for an unbound repository.
    #[arg(long)]
    repository: Option<Repository>,
    /// Template variable, repeatable as --set name=value.
    #[arg(long = "set")]
    variables: Vec<String>,
    /// Apply additions after printing the preview (non-interactive confirmation).
    #[arg(long, conflicts_with = "confirm")]
    yes: bool,
    /// Print the preview, then ask for explicit confirmation.
    #[arg(long)]
    confirm: bool,
}

pub fn run(args: ScaffoldArgs, adopt: bool) -> Result<(String, bool), kitchen::Error> {
    let target = canonical_target(args.target)?;
    if adopt && !target.is_dir() {
        return Err(HouseError::InvalidInput.into());
    }
    let registry = HouseRegistry::new(canonical_target(args.registry)?)?;
    if !adopt
        && target.is_dir()
        && std::fs::read_dir(&target)
            .map_err(HouseError::from)?
            .next()
            .is_some()
    {
        return Err(ScaffoldError::TargetNotEmpty { path: target }.into());
    }
    let mut variables = BTreeMap::new();
    for value in args.variables {
        let (name, value) = value
            .split_once('=')
            .ok_or(ScaffoldError::MalformedAssignment)?;
        let name: VariableName = name.parse()?;
        if variables.contains_key(&name) {
            return Err(ScaffoldError::DuplicateVariable { name }.into());
        }
        variables.insert(name, value.to_owned());
    }
    let plan = plan_repository(
        &registry,
        &target,
        args.house,
        args.repository,
        &args.template,
        &variables,
    )?;
    // Flush the entire preview before even asking for consent or writing files.
    {
        let mut output = io::stdout().lock();
        writeln!(output, "{plan}").map_err(HouseError::from)?;
        output.flush().map_err(HouseError::from)?;
    }
    let apply = args.yes || (args.confirm && confirm()?);
    if !apply {
        return Ok((
            "Preview only; no files changed.\nNext: review conflicts, then repeat with --confirm or --yes to add missing files.".into(),
            plan.conflicts().next().is_none(),
        ));
    }
    match plan.apply() {
        Ok(_) => {}
        Err(kitchen::Error::House(HouseError::Conflicts(report))) => {
            let paths = report
                .files
                .iter()
                .filter(|file| file.status == FileStatus::Conflict)
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Ok((
                format!(
                    "Apply blocked: destinations changed since preview: {paths}. No files added.\nNext: rerun the preview and resolve conflicts before confirming."
                ),
                false,
            ));
        }
        Err(error) => return installation_error(error),
    }
    let healthy = plan.conflicts().next().is_none();
    Ok((
        format!(
            "{}\nNext: inspect the generated files{}; run kitchen house doctor --registry '{}' --repository-path '{}'.\nKitchen runs no template scripts; follow the generated instructions to load house guidance.",
            if healthy {
                "Repository files applied."
            } else {
                "Existing files preserved; resolve the reported conflicts manually."
            },
            if healthy {
                ""
            } else {
                " and reconcile template revisions"
            },
            registry.root().display(),
            target.display(),
        ),
        healthy,
    ))
}

fn confirm() -> Result<bool, HouseError> {
    let mut output = io::stderr().lock();
    write!(output, "Add the previewed missing files? Type yes: ")?;
    output.flush()?;
    let mut input = io::stdin().lock();
    let mut answer = Vec::new();
    for _ in 0..16 {
        let Some(byte) = input.fill_buf()?.first().copied() else {
            return Ok(false);
        };
        input.consume(1);
        if byte == b'\n' {
            return Ok(answer == b"yes" || answer == b"yes\r");
        }
        answer.push(byte);
    }
    Ok(false)
}

/// Refuse a redirected final component; resolve ancestors such as /var once.
/// Missing suffixes are retained; the installer checks all paths below this root.
fn canonical_target(path: PathBuf) -> Result<PathBuf, HouseError> {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(HouseError::InvalidInput);
    }
    let absolute: PathBuf = absolute.components().collect();
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if !metadata.is_dir() => return Err(HouseError::RedirectedPath),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut root) => {
                for name in missing.into_iter().rev() {
                    root.push(name);
                }
                return Ok(root);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A dangling link is not a missing directory.
                if std::fs::symlink_metadata(ancestor).is_ok() {
                    return Err(HouseError::RedirectedPath);
                }
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or(HouseError::InvalidInput)?
                        .to_owned(),
                );
                ancestor = ancestor.parent().ok_or(HouseError::InvalidInput)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn installation_error(error: kitchen::Error) -> Result<(String, bool), kitchen::Error> {
    if let kitchen::Error::House(HouseError::PartialInstallation { remaining }) = error {
        let paths = remaining
            .iter()
            .map(|path| format!("  {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok((
            format!(
                "Partial installation remains; inspect these paths before retrying:\n{paths}\nExisting files were not overwritten. Preserve any local changes while recovering."
            ),
            false,
        ));
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_installation_reports_every_remaining_path() -> Result<(), kitchen::Error> {
        let error = HouseError::PartialInstallation {
            remaining: vec![
                PathBuf::from("/consumer/one"),
                PathBuf::from("/consumer/two"),
            ],
        };
        let (message, healthy) = installation_error(error.into())?;
        assert!(!healthy);
        assert!(message.contains("/consumer/one"));
        assert!(message.contains("/consumer/two"));
        assert!(message.contains("inspect"));
        Ok(())
    }
}
