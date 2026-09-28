//! House repository templates: loading, rendering, and file plans.
//!
//! A house owns its templates and versions them with its guidance. Kitchen
//! supplies the mechanism and a generic example in the repository's root
//! `templates/` directory; it ships no house policy as a default.
//!
//! [`Template::load`] validates a template directory, [`Template::render`]
//! renders it in memory for a selected house, and [`FilePlan::new`] classifies
//! each file against a target as add, unchanged, or conflict using the
//! create-only installer's preview. The plan's [`Display`](std::fmt::Display)
//! output is the preview shown before confirmation; [`FilePlan::apply`] then
//! creates only the additions through [`crate::adoption::install_new_files`],
//! which never overwrites or deletes. Scaffolding creates no remotes, pushes
//! nothing, enables no workflows or schedules, and handles no credentials.
//!
//! ```
//! use std::collections::BTreeMap;
//! use kitchen::{
//!     HouseId, adoption::RelativePath, contracts::CommitId,
//!     scaffold::{FilePlan, Manifest, PlanAction, Template},
//! };
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let manifest = Manifest::parse(r#"
//!     schema = 1
//!     name = "minimal"
//!     house = "home"
//!     revision = 1
//!     description = "A README"
//!     [variables.project]
//!     description = "Project name"
//!     [[files]]
//!     source = "README.md.tera"
//! "#)?;
//! let sources = BTreeMap::from([(
//!     RelativePath::new("README.md.tera")?,
//!     "# {{ vars.project }}\n".to_owned(),
//! )]);
//! let template = Template::from_parts(manifest, sources)?;
//! let variables = BTreeMap::from([("project".parse()?, "demo".to_owned())]);
//! let guidance = CommitId::new(&"a".repeat(40))?;
//! let rendered = template.render(&HouseId::new("home")?, &guidance, &variables)?;
//! let workspace = tempfile::tempdir()?;
//! let target = workspace.path().canonicalize()?.join("absent");
//! let plan = FilePlan::new(rendered, &target)?;
//! assert_eq!(plan.files()[0].action, PlanAction::Add);
//! assert_eq!(plan.files()[0].file.contents, "# demo\n");
//! # Ok(())
//! # }
//! ```

mod activation;
mod error;
mod manifest;
mod plan;
mod provenance;
mod template;

pub use activation::{Activation, MAX_WORKFLOW_YAML_DEPTH};
pub use error::{
    MissingVariable, ScaffoldError, ScaffoldLimit, ScaffoldOperation, TemplateProblem,
};
pub use manifest::{
    FileEntry, MAX_VARIABLES, Manifest, MarkerStyle, TEMPLATE_SCHEMA, TemplateName,
    TemplateRevision, VariableKind, VariableName, VariableSpec,
};
pub use plan::{Conflict, FilePlan, PlanAction, PlanKind, PlannedFile};
pub use provenance::{
    ContentDigest, ManagedMarker, ManagedState, TemplateProvenance, inspect_managed,
};
pub use template::{
    MAX_BINDING_BYTES, MAX_MANIFEST_BYTES, MAX_OUTPUT_PATH_BYTES, MAX_RENDERED_BYTES,
    MAX_SOURCE_BYTES, MAX_TEMPLATE_DEPTH, MAX_TEMPLATE_FILES, MAX_TEMPLATE_OUTPUT_BYTES,
    MAX_VARIABLE_BYTES, RenderedFile, RenderedTemplate, Template,
};

mod repository;
pub use repository::{RepositoryPlan, plan_repository};
