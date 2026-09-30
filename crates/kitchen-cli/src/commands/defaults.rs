//! Checkout defaults for CLI arguments. Scheduled trigger definitions still
//! write explicit registry and house flags so they work outside a checkout.

use std::{ffi::OsString, path::PathBuf};

use kitchen::{
    adoption::{HouseRegistry, origin_repository},
    house::HouseError,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    None,
    Registry,
    House,
    Both,
}

/// Fill omitted CLI scope flags before Clap checks required arguments. A
/// stored repository binding is the only source for an inferred house.
pub fn arguments(mut args: Vec<OsString>) -> Result<Vec<OsString>, kitchen::Error> {
    let insertion = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let option_args = &args[..insertion];
    if args.len() < 2
        || option_args
            .iter()
            .any(|arg| arg == "--help" || arg == "-h" || arg == "--version" || arg == "-V")
    {
        return Ok(args);
    }
    let scope = scope(option_args);
    if scope == Scope::None {
        return Ok(args);
    }
    let registry_present = has_flag(option_args, "--registry");
    let house_present = has_flag(option_args, "--house");
    let needs_registry = matches!(scope, Scope::Registry | Scope::Both);
    let needs_house = matches!(scope, Scope::House | Scope::Both);
    let registry = if registry_present {
        flag_value(option_args, "--registry").map(PathBuf::from)
    } else if needs_registry || (needs_house && !house_present) {
        Some(default_registry()?)
    } else {
        None
    };
    let house = if needs_house && !house_present {
        let root = registry
            .as_ref()
            .ok_or(HouseError::MissingFlag { flag: "--registry" })?;
        let cwd = std::env::current_dir().map_err(HouseError::from)?;
        Some(bound_origin_house(root, &cwd)?)
    } else {
        None
    };
    let mut defaults = Vec::new();
    if needs_registry && !registry_present {
        defaults.push(OsString::from("--registry"));
        defaults.push(
            registry
                .ok_or(HouseError::MissingFlag { flag: "--registry" })?
                .into_os_string(),
        );
    }
    if let Some(house) = house {
        defaults.push(OsString::from("--house"));
        defaults.push(OsString::from(house));
    }
    args.splice(insertion..insertion, defaults);
    Ok(args)
}

fn bound_origin_house(
    registry_root: &std::path::Path,
    checkout: &std::path::Path,
) -> Result<String, HouseError> {
    let repository = origin_repository(checkout).map_err(|error| match error {
        HouseError::RepositoryUnidentified => HouseError::MissingFlag { flag: "--house" },
        other => other,
    })?;
    let root = super::house::canonical_root(registry_root.to_path_buf())?;
    let registry = HouseRegistry::new(root)?;
    let binding = registry
        .binding(&repository)
        .map_err(unresolved_house)?
        .ok_or(HouseError::MissingFlag { flag: "--house" })?;
    let house = registry.load(&binding.house).map_err(unresolved_house)?;
    binding.validate(&house).map_err(unresolved_house)?;
    Ok(binding.house.to_string())
}

fn unresolved_house(error: HouseError) -> HouseError {
    match error {
        HouseError::InvalidInput
        | HouseError::HouseSelection
        | HouseError::Io(std::io::ErrorKind::NotFound) => {
            HouseError::MissingFlag { flag: "--house" }
        }
        other => other,
    }
}

fn scope(args: &[OsString]) -> Scope {
    let command = args.get(1).and_then(|arg| arg.to_str()).unwrap_or_default();
    let subcommand = args.get(2).and_then(|arg| arg.to_str()).unwrap_or_default();
    match (command, subcommand) {
        ("house", "init" | "setup" | "import" | "doctor") => Scope::Registry,
        ("house", "sync" | "update") => Scope::Both,
        ("init" | "adopt" | "work" | "pr" | "issue" | "hand-back", _) => Scope::Registry,
        ("decompose", "apply") => Scope::Both,
        ("decompose", "acknowledge") if has_flag(args, "--store") => Scope::House,
        ("decompose", "acknowledge") => Scope::Both,
        ("decompose", _) => Scope::None,
        ("gardener", "precheck") | ("trust", _) => Scope::House,
        ("tick", "trigger") => Scope::None,
        ("tick", _)
        | ("run", _)
        | ("gate", _)
        | ("forge", _)
        | ("cleanup", _)
        | ("budget", _)
        | ("store", _)
        | ("mailbox", _)
        | ("audit", _)
        | ("gardener", "report-stale") => Scope::Both,
        _ => Scope::None,
    }
}

fn has_flag(args: &[OsString], flag: &str) -> bool {
    args.iter().any(|arg| {
        arg == flag
            || arg
                .to_str()
                .is_some_and(|value| value.starts_with(&format!("{flag}=")))
    })
}

fn flag_value<'a>(args: &'a [OsString], flag: &str) -> Option<&'a std::ffi::OsStr> {
    args.iter().enumerate().find_map(|(index, arg)| {
        if arg == flag {
            args.get(index + 1).map(OsString::as_os_str)
        } else {
            arg.to_str()
                .and_then(|value| value.strip_prefix(&format!("{flag}=")))
                .map(std::ffi::OsStr::new)
        }
    })
}

fn default_registry() -> Result<PathBuf, HouseError> {
    if let Some(root) = std::env::var_os("KITCHN_HOME") {
        if root.is_empty() {
            return Err(HouseError::MissingFlag { flag: "--registry" });
        }
        return super::house::canonical_root(PathBuf::from(root));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".kitchn"))
        .filter(|root| root.is_absolute())
        .ok_or(HouseError::MissingFlag { flag: "--registry" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_house_preserves_io_failures() {
        assert!(matches!(
            unresolved_house(HouseError::Io(std::io::ErrorKind::PermissionDenied)),
            HouseError::Io(std::io::ErrorKind::PermissionDenied)
        ));
    }
}
