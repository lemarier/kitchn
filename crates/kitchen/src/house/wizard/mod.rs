//! Guided `house init`: ask only for what Kitchen cannot infer, then register
//! the same [`HouseConfig`] that `house init --config` would for those values.
//!
//! [`plan_house_init`] runs over an injectable [`Prompter`]. Every question has a flag
//! ([`InitQuestion::flag`]). Without a prompter, because standard input is not a
//! terminal, answers come only from flags and inferred defaults, and anything
//! still missing fails with [`HouseInitError::MissingAnswers`] instead of
//! blocking. [`register_house`] writes only to the registry, never to a working
//! tree. Grants and policy limits stay empty: the wizard never grants
//! authority.

mod error;
mod guidance;

pub use error::HouseInitError;
pub use guidance::default_guidance;

use std::{collections::BTreeSet, fmt, path::PathBuf};

use crate::{
    HouseId,
    adoption::{HouseRegistry, InstructionBundle, ResolvedInstructions, encode, role_cards_digest},
    contracts::{CommitId, Repository, Role},
    house::{HouseConfig, HouseError},
    scheduling::AgentFamily,
    selection::{AgentPolicy, AgentSelection, RuleMatch, SelectionRule},
};

/// Prompts per question before the wizard gives up on invalid answers.
const MAX_ATTEMPTS: usize = 3;
/// Registry directory under the home directory when none is given.
const DEFAULT_REGISTRY: &str = ".kitchn";

/// A station the wizard assigns an agent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Station {
    /// Plans, splits, and supervises work.
    SousChef,
    /// Implements one task.
    StationCook,
    /// Reviews at the pass.
    Expediter,
}

impl Station {
    /// Every station, in the order the wizard asks.
    pub const ALL: [Self; 3] = [Self::SousChef, Self::StationCook, Self::Expediter];

    /// The role this station's agent works as.
    #[must_use]
    pub const fn role(self) -> Role {
        match self {
            Self::SousChef => Role::SousChef,
            Self::StationCook => Role::StationCook,
            Self::Expediter => Role::Expediter,
        }
    }

    /// Claude Code at the pass, Codex at the stations.
    #[must_use]
    pub const fn default_agent(self) -> AgentFamily {
        match self {
            Self::SousChef | Self::Expediter => AgentFamily::Claude,
            Self::StationCook => AgentFamily::Codex,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::SousChef => "Sous-chef",
            Self::StationCook => "Station cook",
            Self::Expediter => "Expediter",
        }
    }

    const fn duty(self) -> &'static str {
        match self {
            Self::SousChef => "plans and splits work",
            Self::StationCook => "writes the code",
            Self::Expediter => "reviews at the pass",
        }
    }
}

/// One wizard answer, named by the flag that supplies it without a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitQuestion {
    /// The external registry directory.
    Registry,
    /// The house name.
    House,
    /// Repositories the house may serve.
    Repositories,
    /// Where explicitly authorized tasks may post.
    PostingDestinations,
    /// The agent that works one station.
    Agent(Station),
    /// Checks every pull request must pass.
    RequiredChecks,
    /// Reviewers every pull request needs.
    RequiredReviewers,
    /// The Kitchen revision to pin.
    Kitchen,
    /// Approval to register the printed configuration.
    Confirm,
}

impl InitQuestion {
    /// The flag that answers this question without a prompt.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Registry => "--registry",
            Self::House => "--house",
            Self::Repositories => "--repositories",
            Self::PostingDestinations => "--posting-destinations",
            Self::Agent(Station::SousChef) => "--sous-chef",
            Self::Agent(Station::StationCook) => "--station-cook",
            Self::Agent(Station::Expediter) => "--expediter",
            Self::RequiredChecks => "--required-checks",
            Self::RequiredReviewers => "--required-reviewers",
            Self::Kitchen => "--kitchen",
            Self::Confirm => "--yes",
        }
    }

    /// What a valid answer looks like.
    #[must_use]
    pub const fn hint(self) -> &'static str {
        match self {
            Self::Registry => "an absolute directory outside every repository, or ~/path",
            Self::House => "a lowercase house name such as acme",
            Self::Repositories => "comma-separated owner/name repositories",
            Self::PostingDestinations => {
                "comma-separated owner/name repositories from the house's repositories, or none"
            }
            Self::Agent(_) => "claude or codex",
            Self::RequiredChecks | Self::RequiredReviewers => "comma-separated names, or none",
            Self::Kitchen => "a full 40- or 64-character lowercase hex commit",
            Self::Confirm => "yes or no",
        }
    }
}

impl fmt::Display for InitQuestion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.flag())
    }
}

/// Answers given as flags, as the text a person would type at the prompt.
/// `None` asks, or takes the inferred default when nobody can be asked.
#[derive(Debug, Clone, Default)]
pub struct InitAnswers {
    /// `--registry`, already made absolute by the caller.
    pub registry: Option<PathBuf>,
    /// `--house`.
    pub house: Option<String>,
    /// `--repositories`.
    pub repositories: Option<String>,
    /// `--posting-destinations`.
    pub posting_destinations: Option<String>,
    /// `--sous-chef`.
    pub sous_chef: Option<String>,
    /// `--station-cook`.
    pub station_cook: Option<String>,
    /// `--expediter`.
    pub expediter: Option<String>,
    /// `--required-checks`.
    pub required_checks: Option<String>,
    /// `--required-reviewers`.
    pub required_reviewers: Option<String>,
    /// `--kitchen`.
    pub kitchen: Option<String>,
    /// A verified guidance bundle to pin instead of the default guidance.
    /// Its pins supply `kitchen` and `guidance`.
    pub bundle: Option<InstructionBundle>,
    /// `--yes`: register without asking for confirmation.
    pub yes: bool,
}

impl InitAnswers {
    const fn agent(&self, station: Station) -> Option<&String> {
        match station {
            Station::SousChef => self.sous_chef.as_ref(),
            Station::StationCook => self.station_cook.as_ref(),
            Station::Expediter => self.expediter.as_ref(),
        }
    }
}

/// Which coding agents were observed installed, for the station question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstalledAgents {
    /// These agents were observed; others were looked for and not found.
    Observed {
        /// Where they were observed, such as `PATH`.
        source: &'static str,
        /// The agents found.
        found: Vec<AgentFamily>,
    },
    /// Nothing could be observed; no agent is claimed available.
    Unknown,
}

/// What Kitchen inferred from the environment before asking anything.
#[derive(Debug)]
pub struct InitFacts {
    /// The home directory, for the default registry and `~/` answers.
    pub home: Option<PathBuf>,
    /// The repository the current checkout's Git remote names, or why none.
    pub checkout: Result<Repository, HouseError>,
    /// The Kitchen revision this binary was built from, when recorded.
    pub kitchen: Option<CommitId>,
    /// The agents observed installed.
    pub agents: InstalledAgents,
}

/// Branch protection read with house-scoped GitHub access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedChecks {
    /// The target branch and the status checks it requires.
    Observed {
        /// The repository's default branch.
        branch: String,
        /// Check names branch protection requires.
        checks: BTreeSet<String>,
    },
    /// No house-scoped GitHub access was configured; nothing was read.
    NoAccess,
    /// Access was configured but could not read branch protection.
    Unavailable,
}

/// Reads a repository's required status checks for the default offer.
pub trait RequiredCheckSource {
    /// Read the checks `repository`'s default branch requires, as `house`.
    fn required_checks(&self, house: &HouseId, repository: &Repository) -> ObservedChecks;
}

/// No house-scoped GitHub access was configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoGitHubAccess;

impl RequiredCheckSource for NoGitHubAccess {
    fn required_checks(&self, _: &HouseId, _: &Repository) -> ObservedChecks {
        ObservedChecks::NoAccess
    }
}

/// Where the wizard's questions go and answers come from.
pub trait Prompter {
    /// Show a line of context.
    ///
    /// # Errors
    /// Output failures.
    fn show(&mut self, text: &str) -> Result<(), HouseInitError>;
    /// Ask one question and return the typed line, trimmed. End of input is
    /// an error, never consent or a default.
    ///
    /// # Errors
    /// Input or output failures and end of input.
    fn ask(&mut self, prompt: &str) -> Result<String, HouseInitError>;
}

/// What the wizard would register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HouseInitPlan {
    /// The external registry root.
    pub registry: PathBuf,
    /// The house configuration, identical to what `--config` accepts.
    pub config: HouseConfig,
    /// The guidance pinned in the same run.
    pub bundle: InstructionBundle,
}

/// The wizard's result before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitDecision {
    /// Approved by `--yes` or at the prompt.
    Confirmed(HouseInitPlan),
    /// The person declined; nothing may be written.
    Declined(HouseInitPlan),
}

/// Ask for every answer not given as a flag, print the configuration, and ask
/// for confirmation. Writes nothing.
///
/// With `prompter` set to `None`, flags and inferred defaults are the only
/// answers; every question left without one is reported together.
///
/// # Errors
/// [`HouseInitError::MissingAnswers`] without a prompter,
/// [`HouseInitError::InvalidAnswer`] for an invalid flag or repeated invalid
/// answers, [`HouseError::PinMismatch`] when `--kitchen` or the house
/// disagrees with the bundle, [`HouseInitError::BuildCommitUnknown`] or
/// [`HouseInitError::KitchenNotThisBuild`] when no bundle is given and the
/// build commit is unrecorded or differs from `--kitchen`, and configuration
/// validation failures.
pub fn plan_house_init(
    answers: &InitAnswers,
    facts: &InitFacts,
    checks: &dyn RequiredCheckSource,
    prompter: Option<&mut dyn Prompter>,
) -> Result<InitDecision, HouseInitError> {
    let mut session = Session {
        prompter,
        missing: Vec::new(),
    };
    let home = facts.home.as_deref();
    let registry = match &answers.registry {
        Some(path) if path.is_absolute() => Some(path.clone()),
        Some(_) => return Err(HouseInitError::InvalidAnswer(InitQuestion::Registry)),
        None => session.answer(
            InitQuestion::Registry,
            None,
            "Registry directory",
            home.map(|home| Offer::plain(home.join(DEFAULT_REGISTRY).display().to_string())),
            |text| registry_path(text, home),
        )?,
    };
    let house = session.answer(
        InitQuestion::House,
        answers.house.as_deref(),
        "House name",
        None,
        |text| HouseId::new(text).map_err(drop),
    )?;
    if let (None, Err(error)) = (&answers.repositories, &facts.checkout) {
        session.show(&format!(
            "No repository default from this directory: {error}."
        ))?;
    }
    let repositories = session.answer(
        InitQuestion::Repositories,
        answers.repositories.as_deref(),
        "Repositories kitchen may work in",
        facts.checkout.as_ref().ok().map(|repository| Offer {
            value: repository.to_string(),
            note: Some("from this checkout"),
        }),
        |text| {
            repositories_list(text)
                .filter(|set| !set.is_empty())
                .ok_or(())
        },
    )?;
    let posting_destinations = session.answer(
        InitQuestion::PostingDestinations,
        answers.posting_destinations.as_deref(),
        "Where kitchen may post",
        repositories.as_ref().map(|set| Offer {
            value: joined(set),
            note: Some("the repositories"),
        }),
        |text| {
            if text.eq_ignore_ascii_case("none") {
                return Ok(BTreeSet::new());
            }
            repositories_list(text)
                .filter(|set| repositories.as_ref().is_none_or(|all| set.is_subset(all)))
                .ok_or(())
        },
    )?;
    let agents = session.agents(answers, &facts.agents)?;
    let required_checks =
        session.required_checks(answers, checks, house.as_ref(), &repositories)?;
    let required_reviewers = session.answer(
        InitQuestion::RequiredReviewers,
        answers.required_reviewers.as_deref(),
        "Required reviewers",
        Some(Offer::plain(Role::Expediter.as_str().to_owned())),
        names,
    )?;
    let kitchen = session.kitchen(answers, facts.kitchen.as_ref())?;
    if !answers.yes && session.prompter.is_none() {
        session.missing.push(InitQuestion::Confirm);
    }
    let (
        Some(registry),
        Some(house),
        Some(repositories),
        Some(posting_destinations),
        Some(agents),
        Some(required_checks),
        Some(required_reviewers),
        Some(kitchen),
    ) = (
        registry,
        house,
        repositories,
        posting_destinations,
        agents,
        required_checks,
        required_reviewers,
        kitchen,
    )
    else {
        return Err(HouseInitError::MissingAnswers(session.missing));
    };
    if !session.missing.is_empty() {
        return Err(HouseInitError::MissingAnswers(session.missing));
    }
    let bundle = match &answers.bundle {
        Some(bundle) => bundle.clone(),
        None => default_guidance(&house, &kitchen)?,
    };
    let config = HouseConfig {
        schema: 1,
        house,
        kitchen,
        guidance: bundle.guidance.clone(),
        repositories,
        posting_destinations,
        required_reviewers,
        required_checks,
        policy_limits: BTreeSet::new(),
        grants: BTreeSet::new(),
        agents: Some(agents),
        stack_tool: None,
        schedules: None,
    };
    config.validate()?;
    bundle.validate(&config)?;
    let plan = HouseInitPlan {
        registry,
        config,
        bundle,
    };
    if answers.yes {
        return Ok(InitDecision::Confirmed(plan));
    }
    let Some(prompter) = session.prompter else {
        return Err(HouseInitError::MissingAnswers(vec![InitQuestion::Confirm]));
    };
    prompter.show(&plan.config_text()?)?;
    let answer = prompter.ask(&format!(
        "Register house {} in {}? [y/N]: ",
        plan.config.house,
        plan.registry.display()
    ))?;
    Ok(
        if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
            InitDecision::Confirmed(plan)
        } else {
            InitDecision::Declined(plan)
        },
    )
}

impl HouseInitPlan {
    /// The exact JSON [`register_house`] will save as the house config.
    ///
    /// # Errors
    /// [`HouseError::InvalidInput`] when the config cannot be encoded.
    pub fn config_text(&self) -> Result<String, HouseError> {
        String::from_utf8(encode(&self.config)?).map_err(|_| HouseError::InvalidInput)
    }
}

/// What [`register_house`] stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HouseInitReport {
    /// The saved configuration, for review and editing.
    pub config_path: PathBuf,
    /// The verified guidance snapshot.
    pub instructions: ResolvedInstructions,
}

/// Register a confirmed plan and pin its guidance in the same run. Create-only:
/// identical existing content is kept, so rerunning with the same answers
/// resumes a run that stopped between the two writes.
///
/// # Errors
/// Refuses a registry inside a repository or a bundle that does not match
/// the configuration before writing; a different existing house is a
/// conflict; [`HouseInitError::GuidanceNotPinned`] when only the snapshot
/// failed.
pub fn register_house(plan: &HouseInitPlan) -> Result<HouseInitReport, HouseInitError> {
    let registry = HouseRegistry::new(plan.registry.clone())?;
    plan.bundle.validate(&plan.config)?;
    if plan.bundle.role_cards_digest != role_cards_digest() {
        return Err(HouseError::PinMismatch.into());
    }
    registry.initialize(&plan.config)?;
    let instructions = registry
        .sync(&plan.config.house, &plan.bundle)
        .map_err(|source| HouseInitError::GuidanceNotPinned { source })?;
    Ok(HouseInitReport {
        config_path: registry
            .root()
            .join("houses")
            .join(format!("{}.json", plan.config.house)),
        instructions,
    })
}

/// A default answer, and why it is the default.
struct Offer {
    value: String,
    note: Option<&'static str>,
}

impl Offer {
    const fn plain(value: String) -> Self {
        Self { value, note: None }
    }
}

struct Session<'a> {
    prompter: Option<&'a mut dyn Prompter>,
    missing: Vec<InitQuestion>,
}

impl Session<'_> {
    fn show(&mut self, text: &str) -> Result<(), HouseInitError> {
        match self.prompter.as_deref_mut() {
            Some(prompter) => prompter.show(text),
            None => Ok(()),
        }
    }

    /// A flag answer, else a prompt, else the default. `None` means nobody
    /// could be asked and there is no default; the question is recorded.
    fn answer<T>(
        &mut self,
        question: InitQuestion,
        flag: Option<&str>,
        label: &str,
        offer: Option<Offer>,
        parse: impl Fn(&str) -> Result<T, ()>,
    ) -> Result<Option<T>, HouseInitError> {
        let invalid = |_| HouseInitError::InvalidAnswer(question);
        if let Some(text) = flag {
            return parse(text.trim()).map(Some).map_err(invalid);
        }
        let Some(prompter) = self.prompter.as_deref_mut() else {
            return match offer {
                Some(offer) => parse(&offer.value).map(Some).map_err(invalid),
                None => {
                    self.missing.push(question);
                    Ok(None)
                }
            };
        };
        let prompt = match &offer {
            Some(Offer {
                value,
                note: Some(note),
            }) => format!("{label} [{value}, {note}]: "),
            Some(Offer { value, note: None }) => format!("{label} [{value}]: "),
            None => format!("{label}: "),
        };
        for _ in 0..MAX_ATTEMPTS {
            let typed = prompter.ask(&prompt)?;
            let text = match (&offer, typed.is_empty()) {
                (Some(offer), true) => offer.value.as_str(),
                (None, true) => {
                    prompter.show(&format!("An answer is required ({}).", question.hint()))?;
                    continue;
                }
                (_, false) => typed.as_str(),
            };
            match parse(text) {
                Ok(value) => return Ok(Some(value)),
                Err(()) => prompter.show(&format!("Expected {}.", question.hint()))?,
            }
        }
        Err(HouseInitError::InvalidAnswer(question))
    }

    fn agents(
        &mut self,
        answers: &InitAnswers,
        installed: &InstalledAgents,
    ) -> Result<Option<AgentPolicy>, HouseInitError> {
        match installed {
            InstalledAgents::Observed { source, found } if found.is_empty() => {
                self.show(&format!(
                    "Found neither Claude Code nor Codex on {source}. Who works each station?"
                ))?
            }
            InstalledAgents::Observed { source, found } => self.show(&format!(
                "Found {} on {source}. Who works each station?",
                found
                    .iter()
                    .map(|agent| agent_name(*agent))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ))?,
            InstalledAgents::Unknown => self
                .show("Kitchen cannot tell which agents are installed. Who works each station?")?,
        }
        let mut chosen = Vec::with_capacity(Station::ALL.len());
        for station in Station::ALL {
            let agent = self.answer(
                InitQuestion::Agent(station),
                answers.agent(station).map(String::as_str),
                &format!("  {:<13} {:<22}", station.label(), station.duty()),
                Some(Offer::plain(agent_name(station.default_agent()).to_owned())),
                parse_agent,
            )?;
            if let (Some(agent), InstalledAgents::Observed { source, found }) = (agent, installed)
                && !found.contains(&agent)
            {
                self.show(&format!(
                    "  {} was not found on {source}; install it before this station starts work.",
                    agent_name(agent)
                ))?;
            }
            chosen.push((station, agent));
        }
        let mut rules = Vec::new();
        let mut default = None;
        for (station, agent) in chosen {
            let Some(agent) = agent else {
                return Ok(None);
            };
            let selection = AgentSelection::agent_default(agent);
            // The station cook is the default for every other role too
            // (commis, inspector, ...); the pass roles are explicit rules.
            if station == Station::StationCook {
                default = Some(selection);
            } else {
                rules.push(SelectionRule {
                    when: RuleMatch {
                        role: Some(station.role()),
                        ..RuleMatch::default()
                    },
                    selection,
                });
            }
        }
        Ok(default.map(|default| AgentPolicy { default, rules }))
    }

    fn required_checks(
        &mut self,
        answers: &InitAnswers,
        source: &dyn RequiredCheckSource,
        house: Option<&HouseId>,
        repositories: &Option<BTreeSet<Repository>>,
    ) -> Result<Option<BTreeSet<String>>, HouseInitError> {
        let mut offer = None;
        if answers.required_checks.is_none()
            && let (Some(house), Some(repositories)) = (house, repositories)
        {
            // House checks apply to every repository: offer only the checks
            // all of them require, and only when every one could be read.
            let mut common: Option<BTreeSet<String>> = None;
            for repository in repositories {
                match source.required_checks(house, repository) {
                    ObservedChecks::Observed { branch, checks } => {
                        self.show(&format!(
                            "Required checks on {repository} {branch}: {}",
                            if checks.is_empty() {
                                "none".to_owned()
                            } else {
                                joined(&checks)
                            }
                        ))?;
                        common = Some(match common {
                            Some(common) => common.intersection(&checks).cloned().collect(),
                            None => checks,
                        });
                    }
                    ObservedChecks::NoAccess => {
                        self.show(
                            "No house-scoped GitHub access to read required checks (see --github-requester).",
                        )?;
                        common = None;
                        break;
                    }
                    ObservedChecks::Unavailable => {
                        self.show(&format!(
                            "Could not read the required checks of {repository} with house-scoped GitHub access."
                        ))?;
                        common = None;
                        break;
                    }
                }
            }
            offer = common.map(|checks| Offer {
                value: if checks.is_empty() {
                    "none".to_owned()
                } else {
                    joined(&checks)
                },
                note: Some("from branch protection"),
            });
        }
        self.answer(
            InitQuestion::RequiredChecks,
            answers.required_checks.as_deref(),
            "Required checks",
            offer,
            names,
        )
    }

    fn kitchen(
        &mut self,
        answers: &InitAnswers,
        built_from: Option<&CommitId>,
    ) -> Result<Option<CommitId>, HouseInitError> {
        let parse = |text: &str| CommitId::new(text).map_err(drop);
        if let Some(bundle) = &answers.bundle {
            if let Some(flag) = &answers.kitchen
                && parse(flag.trim())
                    .map_err(|()| HouseInitError::InvalidAnswer(InitQuestion::Kitchen))?
                    != bundle.kitchen
            {
                return Err(HouseError::PinMismatch.into());
            }
            return Ok(Some(bundle.kitchen.clone()));
        }
        // The embedded guidance came from this build's revision only.
        let built_from = built_from.ok_or(HouseInitError::BuildCommitUnknown)?;
        if let Some(flag) = &answers.kitchen
            && parse(flag.trim())
                .map_err(|()| HouseInitError::InvalidAnswer(InitQuestion::Kitchen))?
                != *built_from
        {
            return Err(HouseInitError::KitchenNotThisBuild);
        }
        Ok(Some(built_from.clone()))
    }
}

fn registry_path(text: &str, home: Option<&std::path::Path>) -> Result<PathBuf, ()> {
    let path = match (text.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        (Some(_), None) => return Err(()),
        (None, _) => PathBuf::from(text),
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(())
    }
}

fn repositories_list(text: &str) -> Option<BTreeSet<Repository>> {
    text.split(',')
        .map(|item| Repository::new(item.trim()).ok())
        .collect()
}

/// Comma-separated names, or `none` for an empty set. Bounds are checked by
/// [`HouseConfig::validate`].
fn names(text: &str) -> Result<BTreeSet<String>, ()> {
    if text.eq_ignore_ascii_case("none") {
        return Ok(BTreeSet::new());
    }
    text.split(',')
        .map(|item| {
            let item = item.trim();
            if item.is_empty() || item.len() > 128 || item.chars().any(char::is_control) {
                Err(())
            } else {
                Ok(item.to_owned())
            }
        })
        .collect()
}

fn parse_agent(text: &str) -> Result<AgentFamily, ()> {
    match text.to_ascii_lowercase().as_str() {
        "claude" | "claude code" | "claude-code" => Ok(AgentFamily::Claude),
        "codex" => Ok(AgentFamily::Codex),
        _ => Err(()),
    }
}

const fn agent_name(agent: AgentFamily) -> &'static str {
    match agent {
        AgentFamily::Claude => "Claude Code",
        AgentFamily::Codex => "Codex",
    }
}

fn joined(values: &BTreeSet<impl fmt::Display>) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
