//! Verification environments: where a worker can run the software it changed.
//!
//! A backend declares the environments it offers by target: the host operating
//! system, a VM of another operating system, or a class of device. House or
//! repository policy names the targets a work type must be verified on;
//! [`VerificationPolicy::check_activation`] fails, naming every gap, when the
//! backend lacks one. Verification evidence records the [`VerificationAccess`]
//! that produced it through [`EvidenceKind::AuthorizedVerification`]. Only
//! [`crate::state::run_verification`] records that kind: it runs the target
//! through the backend's [`VerificationExecutor`] and stores the backend's
//! verdict in the task's evidence. The house store refuses the kind from any
//! other producer, and [`VerificationReport::evaluate`] reads only
//! [`RecordedEvidence`] the store returns. Evidence counts only when its
//! access matches one the evaluating task holds and only for the exact
//! subject it was observed on. Check, worker-report, and unbound
//! [`EvidenceKind::Verification`] evidence never satisfies it.
//!
//! A declared environment grants nothing. Using a VM or device needs
//! [`Permission::UseVerificationEnvironment`] from the task's authority, and a
//! device also needs [`Permission::OperateEquipment`], each from a grant that
//! names the target; see [`authorize_access`].

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::{
    BackendId, CredentialId, ErrorClass, HouseId,
    contracts::{
        BackendUnavailable, ContractError, EffectExecutor, Evidence, EvidenceKind, EvidenceSubject,
        EvidenceVerdict, ExternalRef, GrantScope, HouseGrants, Permission, Repository, Support,
        TaskAuthority, Text, ValueKind,
    },
};

/// Most environments one backend may declare.
pub const MAX_VERIFICATION_ENVIRONMENTS: usize = 64;
/// Most work types one policy level may name.
pub const MAX_POLICY_WORK_TYPES: usize = 64;
/// Most repositories a policy may name.
pub const MAX_POLICY_REPOSITORIES: usize = 256;
/// Most targets one work type may require at one policy level.
pub const MAX_TARGETS_PER_WORK_TYPE: usize = 16;
/// Longest device class name, in bytes.
pub const MAX_DEVICE_CLASS_BYTES: usize = 64;

closed_names! {
    /// An operating system a host or VM runs.
    #[non_exhaustive]
    pub enum OperatingSystem(ValueKind::VerificationTarget) {
        /// Linux.
        Linux = "linux",
        /// macOS.
        MacOs = "macos",
        /// Windows.
        Windows = "windows",
        /// FreeBSD.
        FreeBsd = "freebsd",
        /// Android.
        Android = "android",
        /// iOS.
        Ios = "ios",
    }
}

/// A house-defined device class, such as `km43-controller` or `phone`:
/// 1–64 bytes of lowercase ASCII letters, digits, and inner hyphens,
/// starting with a letter.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceClass(String);

impl DeviceClass {
    /// Validate and own a device class.
    ///
    /// # Errors
    /// Returns [`ContractError::InvalidValue`] without echoing the input.
    pub fn new(value: &str) -> Result<Self, ContractError> {
        let bytes = value.as_bytes();
        let valid = value.len() <= MAX_DEVICE_CLASS_BYTES
            && bytes.first().is_some_and(u8::is_ascii_lowercase)
            && bytes.last().is_some_and(|byte| *byte != b'-')
            && !value.contains("--")
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-');
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(ContractError::InvalidValue {
                kind: ValueKind::VerificationTarget,
            })
        }
    }

    /// Borrow the validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("DeviceClass").field(&self.0).finish()
    }
}

impl fmt::Display for DeviceClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where software can be verified. Written `host:<os>`, `vm:<os>`, or
/// `device:<class>`, such as `vm:windows`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VerificationTarget {
    /// The worker's own host, running this operating system.
    Host(OperatingSystem),
    /// A virtual machine running this operating system.
    Vm(OperatingSystem),
    /// A physical device of this class. Always under equipment authority.
    Device(DeviceClass),
}

impl VerificationTarget {
    /// The permissions a task must hold to use this target, beyond launching
    /// its worker. The host needs none; a VM needs
    /// [`Permission::UseVerificationEnvironment`]; a device also needs
    /// [`Permission::OperateEquipment`].
    #[must_use]
    pub const fn required_permissions(&self) -> &'static [Permission] {
        match self {
            Self::Host(_) => &[],
            Self::Vm(_) => &[Permission::UseVerificationEnvironment],
            Self::Device(_) => &[
                Permission::UseVerificationEnvironment,
                Permission::OperateEquipment,
            ],
        }
    }
}

impl fmt::Display for VerificationTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host(os) => write!(formatter, "host:{os}"),
            Self::Vm(os) => write!(formatter, "vm:{os}"),
            Self::Device(class) => write!(formatter, "device:{class}"),
        }
    }
}

impl FromStr for VerificationTarget {
    type Err = ContractError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || ContractError::InvalidValue {
            kind: ValueKind::VerificationTarget,
        };
        let (kind, name) = value.split_once(':').ok_or_else(invalid)?;
        match kind {
            "host" => Ok(Self::Host(name.parse().map_err(|_| invalid())?)),
            "vm" => Ok(Self::Vm(name.parse().map_err(|_| invalid())?)),
            "device" => Ok(Self::Device(DeviceClass::new(name)?)),
            _ => Err(invalid()),
        }
    }
}

impl Serialize for VerificationTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for VerificationTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// The verification environments a backend declares. Undeclared means
/// unavailable; partial support does not satisfy a requirement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct VerificationEnvironments(BTreeMap<VerificationTarget, Support>);

impl VerificationEnvironments {
    /// No environments.
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Declare `target` with `support`, replacing an earlier declaration.
    ///
    /// # Errors
    /// Returns [`VerificationError::TooMany`] when the declaration would exceed
    /// [`MAX_VERIFICATION_ENVIRONMENTS`].
    pub fn with(mut self, target: VerificationTarget, support: Support) -> Result<Self> {
        self.0.insert(target, support);
        if self.0.len() > MAX_VERIFICATION_ENVIRONMENTS {
            return Err(VerificationError::TooMany);
        }
        Ok(self)
    }

    /// The declared support for `target`, or `None` when undeclared.
    #[must_use]
    pub fn support(&self, target: &VerificationTarget) -> Option<Support> {
        self.0.get(target).copied()
    }

    /// Check that every required target is fully supported.
    ///
    /// # Errors
    /// Returns [`VerificationError::UnsupportedTargets`] naming every undeclared
    /// and every partial target.
    pub fn require<'a>(
        &self,
        required: impl IntoIterator<Item = &'a VerificationTarget>,
    ) -> Result<()> {
        let mut missing = BTreeSet::new();
        let mut partial = BTreeSet::new();
        for target in required {
            match self.support(target) {
                Some(Support::Supported) => {}
                Some(Support::Partial) => {
                    partial.insert(target.clone());
                }
                None => {
                    missing.insert(target.clone());
                }
            }
        }
        if missing.is_empty() && partial.is_empty() {
            Ok(())
        } else {
            Err(VerificationError::UnsupportedTargets {
                missing: missing.into_iter().collect(),
                partial: partial.into_iter().collect(),
            })
        }
    }
}

impl<'de> Deserialize<'de> for VerificationEnvironments {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let declared = BTreeMap::<VerificationTarget, Support>::deserialize(deserializer)?;
        if declared.len() > MAX_VERIFICATION_ENVIRONMENTS {
            return Err(serde::de::Error::custom(VerificationError::TooMany));
        }
        Ok(Self(declared))
    }
}

type Requirements = BTreeMap<Text, BTreeSet<VerificationTarget>>;

/// House and repository policy naming the targets each work type must be
/// verified on. A repository entry adds to the house entry for the same work
/// type; it can never remove a house requirement. The policy lives in the
/// house's private configuration, never in a repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "RawPolicy")]
pub struct VerificationPolicy {
    /// Requirements for every repository in the house, by work type.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    house: Requirements,
    /// Additional requirements for one repository, by work type.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    repositories: BTreeMap<Repository, Requirements>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    house: Requirements,
    #[serde(default)]
    repositories: BTreeMap<Repository, Requirements>,
}

impl TryFrom<RawPolicy> for VerificationPolicy {
    type Error = VerificationError;

    fn try_from(raw: RawPolicy) -> Result<Self> {
        if raw.repositories.len() > MAX_POLICY_REPOSITORIES {
            return Err(VerificationError::TooMany);
        }
        for requirements in std::iter::once(&raw.house).chain(raw.repositories.values()) {
            validate_requirements(requirements)?;
        }
        Ok(Self {
            house: raw.house,
            repositories: raw.repositories,
        })
    }
}

fn validate_requirements(requirements: &Requirements) -> Result<()> {
    if requirements.len() > MAX_POLICY_WORK_TYPES {
        return Err(VerificationError::TooMany);
    }
    for targets in requirements.values() {
        if targets.is_empty() {
            return Err(VerificationError::EmptyRequirement);
        }
        if targets.len() > MAX_TARGETS_PER_WORK_TYPE {
            return Err(VerificationError::TooMany);
        }
    }
    Ok(())
}

impl VerificationPolicy {
    /// A policy requiring nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            house: BTreeMap::new(),
            repositories: BTreeMap::new(),
        }
    }

    /// Require `targets` for `work_type` in every repository of the house.
    ///
    /// # Errors
    /// Returns [`VerificationError::EmptyRequirement`] for no targets and
    /// [`VerificationError::TooMany`] past a policy bound.
    pub fn require_in_house(
        mut self,
        work_type: Text,
        targets: impl IntoIterator<Item = VerificationTarget>,
    ) -> Result<Self> {
        add_requirement(&mut self.house, work_type, targets)?;
        Ok(self)
    }

    /// Require `targets` for `work_type` in `repository`, in addition to the
    /// house requirements.
    ///
    /// # Errors
    /// Returns [`VerificationError::EmptyRequirement`] for no targets and
    /// [`VerificationError::TooMany`] past a policy bound.
    pub fn require_in_repository(
        mut self,
        repository: Repository,
        work_type: Text,
        targets: impl IntoIterator<Item = VerificationTarget>,
    ) -> Result<Self> {
        if !self.repositories.contains_key(&repository)
            && self.repositories.len() >= MAX_POLICY_REPOSITORIES
        {
            return Err(VerificationError::TooMany);
        }
        add_requirement(
            self.repositories.entry(repository).or_default(),
            work_type,
            targets,
        )?;
        Ok(self)
    }

    /// Every target `work_type` must be verified on: the house requirements
    /// plus those of `repository`.
    #[must_use]
    pub fn required_targets(
        &self,
        repository: &Repository,
        work_type: &Text,
    ) -> BTreeSet<VerificationTarget> {
        let in_repository = self
            .repositories
            .get(repository)
            .and_then(|requirements| requirements.get(work_type));
        self.house
            .get(work_type)
            .into_iter()
            .chain(in_repository)
            .flatten()
            .cloned()
            .collect()
    }

    /// The house requirements for `work_type`, for work that targets no
    /// repository. Callers choose this deliberately; work in a repository uses
    /// [`Self::required_targets`].
    ///
    /// # Errors
    /// Returns [`VerificationError::RepositoryRequired`] when any repository
    /// adds requirements for `work_type`, because omitting the repository
    /// could otherwise skip them.
    pub fn required_house_targets(&self, work_type: &Text) -> Result<BTreeSet<VerificationTarget>> {
        if self
            .repositories
            .values()
            .any(|requirements| requirements.contains_key(work_type))
        {
            return Err(VerificationError::RepositoryRequired {
                work_type: work_type.clone(),
            });
        }
        Ok(self
            .house
            .get(work_type)
            .into_iter()
            .flatten()
            .cloned()
            .collect())
    }

    /// Check at activation that the backend's declared environments cover
    /// every target the policy requires for `work_type` in `repository`, and
    /// return them.
    ///
    /// # Errors
    /// Returns [`VerificationError::UnsupportedTargets`] naming every missing
    /// and partially supported target.
    pub fn check_activation(
        &self,
        repository: &Repository,
        work_type: &Text,
        environments: &VerificationEnvironments,
    ) -> Result<BTreeSet<VerificationTarget>> {
        let required = self.required_targets(repository, work_type);
        environments.require(&required)?;
        Ok(required)
    }

    /// [`Self::check_activation`] for work that targets no repository.
    ///
    /// # Errors
    /// Returns [`VerificationError::RepositoryRequired`] when a repository
    /// adds requirements for `work_type`, and
    /// [`VerificationError::UnsupportedTargets`] as for
    /// [`Self::check_activation`].
    pub fn check_house_activation(
        &self,
        work_type: &Text,
        environments: &VerificationEnvironments,
    ) -> Result<BTreeSet<VerificationTarget>> {
        let required = self.required_house_targets(work_type)?;
        environments.require(&required)?;
        Ok(required)
    }
}

fn add_requirement(
    requirements: &mut Requirements,
    work_type: Text,
    targets: impl IntoIterator<Item = VerificationTarget>,
) -> Result<()> {
    let targets: BTreeSet<_> = targets.into_iter().collect();
    if targets.is_empty() {
        return Err(VerificationError::EmptyRequirement);
    }
    if !requirements.contains_key(&work_type) && requirements.len() >= MAX_POLICY_WORK_TYPES {
        return Err(VerificationError::TooMany);
    }
    let entry = requirements.entry(work_type).or_default();
    if entry.union(&targets).count() > MAX_TARGETS_PER_WORK_TYPE {
        return Err(VerificationError::TooMany);
    }
    entry.extend(targets);
    Ok(())
}

/// One task's authorized use of a verification target: the house, the
/// executor namespace, the scope, the target, and the credential selected for
/// each permission the target needs.
///
/// [`authorize_access`] builds one; deserializing reads a stored record.
/// Verification evidence records it in
/// [`EvidenceKind::AuthorizedVerification`]. A copy authorizes nothing by
/// itself: the store accepts that evidence only from
/// [`crate::state::run_verification`], and [`VerificationReport::evaluate`]
/// counts it only when it equals an access the evaluating task obtained.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[must_use]
pub struct VerificationAccess {
    house: HouseId,
    backend: BackendId,
    scope: GrantScope,
    target: VerificationTarget,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    credentials: BTreeMap<Permission, CredentialId>,
}

impl VerificationAccess {
    /// The house whose grants authorized the access.
    #[must_use]
    pub const fn house(&self) -> &HouseId {
        &self.house
    }

    /// The executor namespace that runs the verification.
    #[must_use]
    pub const fn backend(&self) -> &BackendId {
        &self.backend
    }

    /// The grant scope the access was authorized for.
    #[must_use]
    pub const fn scope(&self) -> &GrantScope {
        &self.scope
    }

    /// The authorized target.
    #[must_use]
    pub const fn target(&self) -> &VerificationTarget {
        &self.target
    }

    /// The credential for each permission the target needs; empty for the host.
    #[must_use]
    pub const fn credentials(&self) -> &BTreeMap<Permission, CredentialId> {
        &self.credentials
    }
}

/// Authorize a task to use `target` on `backend`. Call this before every use:
/// it rechecks the house's current grants, so revocation applies at once.
///
/// The backend's own [`EffectExecutor::verification_environments`] must fully
/// support the target, and the task must hold each of the target's
/// [`VerificationTarget::required_permissions`] on that backend from a grant
/// naming the target. A declared environment grants nothing by itself.
///
/// # Errors
/// Returns [`VerificationError::UnsupportedTargets`] when the backend does not
/// fully support the target, and a [`ContractError`] when the backend serves
/// another house or the task lacks a permission for the target.
pub fn authorize_access(
    authority: &TaskAuthority,
    current: &HouseGrants,
    backend: &(impl EffectExecutor + ?Sized),
    target: &VerificationTarget,
    scope: &GrantScope,
) -> Result<VerificationAccess> {
    let descriptor = backend.descriptor();
    if &descriptor.house != authority.house() {
        return Err(ContractError::CrossHouse {
            expected: authority.house().clone(),
            found: descriptor.house.clone(),
        }
        .into());
    }
    backend.verification_environments().require([target])?;
    let credentials = target
        .required_permissions()
        .iter()
        .map(|permission| {
            authority
                .authorize_target(current, *permission, scope, &descriptor.backend, target)
                .map(|credential| (*permission, credential))
        })
        .collect::<Result<_, ContractError>>()?;
    Ok(VerificationAccess {
        house: descriptor.house.clone(),
        backend: descriptor.backend.clone(),
        scope: scope.clone(),
        target: target.clone(),
        credentials,
    })
}

/// What a verification environment reported for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationResult {
    /// The outcome of running the changed software on the target.
    pub verdict: EvidenceVerdict,
    /// The backend's reference for the run, where its output can be read.
    pub source: ExternalRef,
}

/// A backend that can run changed software in the verification environments
/// it declares.
///
/// Contract: `verify` runs the software at `subject` on the access's target,
/// through the access's backend namespace and credentials, and reports that
/// run's own result. It never reports a pass it did not observe, and it
/// returns [`EvidenceVerdict::Unavailable`] or an error when it cannot
/// establish one. The backend bounds every call with its own deadline and
/// reports an expired one as [`BackendUnavailable::Timeout`], never a pass.
pub trait VerificationExecutor: EffectExecutor {
    /// Run the software at `subject` under `access`.
    ///
    /// # Errors
    /// Returns [`BackendUnavailable`] when the environment cannot be reached
    /// or the run exceeds its deadline.
    fn verify(
        &self,
        access: &VerificationAccess,
        subject: &EvidenceSubject,
    ) -> Result<VerificationResult, BackendUnavailable>;
}

/// One task's evidence as the house store holds it.
///
/// Only the store builds one ([`crate::state::HouseStore::recorded_evidence`]),
/// so [`EvidenceKind::AuthorizedVerification`] items in it were recorded by
/// [`crate::state::run_verification`] from a backend's own result, never
/// supplied by a producer. It cannot be deserialized or assembled from
/// caller-built evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct RecordedEvidence(Vec<Evidence>);

impl RecordedEvidence {
    pub(crate) const fn new(items: Vec<Evidence>) -> Self {
        Self(items)
    }

    /// The recorded items.
    #[must_use]
    pub fn items(&self) -> &[Evidence] {
        &self.0
    }
}

/// The verification state of one required target at the current subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetStatus {
    /// Passing verification evidence exists for the exact subject.
    Verified,
    /// Verification on the exact subject failed.
    Failed,
    /// The environment could not establish a result for the exact subject.
    Unavailable,
    /// Verification evidence exists only for another revision.
    Stale,
    /// Verification evidence names this target, but no access the task
    /// holds produced it.
    Unauthenticated,
    /// No verification evidence names this target.
    Missing,
}

/// Per-target verification state for one subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct VerificationReport(BTreeMap<VerificationTarget, TargetStatus>);

impl VerificationReport {
    /// Evaluate a task's recorded `evidence` against the `required` targets
    /// for `subject`.
    ///
    /// `authorized` holds the task's current [`authorize_access`] results.
    /// Only [`EvidenceKind::AuthorizedVerification`] evidence whose access
    /// equals one of them counts: same house, executor namespace, scope,
    /// target, and credentials. Other verification evidence for a target is
    /// [`TargetStatus::Unauthenticated`]. Of the authenticated evidence, only
    /// the exact subject counts; a failure outweighs a pass, and a pass
    /// outweighs an unavailable result. Check, worker-report, and forge-merge
    /// evidence never verify a target.
    pub fn evaluate(
        required: &BTreeSet<VerificationTarget>,
        subject: &EvidenceSubject,
        evidence: &RecordedEvidence,
        authorized: &[VerificationAccess],
    ) -> Self {
        Self(
            required
                .iter()
                .map(|target| {
                    let mut status = TargetStatus::Missing;
                    for item in evidence.items() {
                        let (observed, authenticated) = match &item.kind {
                            EvidenceKind::AuthorizedVerification(access) => {
                                (&access.target, authorized.contains(access))
                            }
                            EvidenceKind::Verification(observed) => (observed, false),
                            EvidenceKind::Check
                            | EvidenceKind::WorkerReport(_)
                            | EvidenceKind::ForgeMerge(_) => continue,
                        };
                        if observed != target {
                            continue;
                        }
                        let next = if !authenticated {
                            TargetStatus::Unauthenticated
                        } else if &item.subject == subject {
                            match item.verdict {
                                EvidenceVerdict::Fail => TargetStatus::Failed,
                                EvidenceVerdict::Pass => TargetStatus::Verified,
                                EvidenceVerdict::Unavailable => TargetStatus::Unavailable,
                            }
                        } else {
                            TargetStatus::Stale
                        };
                        if precedence(next) > precedence(status) {
                            status = next;
                        }
                    }
                    (target.clone(), status)
                })
                .collect(),
        )
    }

    /// The status of `target`, or `None` when it was not required.
    #[must_use]
    pub fn status(&self, target: &VerificationTarget) -> Option<TargetStatus> {
        self.0.get(target).copied()
    }

    /// Whether every required target is verified on the subject.
    #[must_use]
    pub fn is_satisfied(&self) -> bool {
        self.0
            .values()
            .all(|status| *status == TargetStatus::Verified)
    }

    /// The required targets that are not verified, with their status.
    pub fn unsatisfied(&self) -> impl Iterator<Item = (&VerificationTarget, TargetStatus)> {
        self.0
            .iter()
            .filter(|(_, status)| **status != TargetStatus::Verified)
            .map(|(target, status)| (target, *status))
    }
}

const fn precedence(status: TargetStatus) -> u8 {
    match status {
        TargetStatus::Missing => 0,
        TargetStatus::Unauthenticated => 1,
        TargetStatus::Stale => 2,
        TargetStatus::Unavailable => 3,
        TargetStatus::Verified => 4,
        TargetStatus::Failed => 5,
    }
}

/// A verification environment declaration, policy, or access check failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VerificationError {
    /// The core contract refused the request, such as a missing permission.
    #[error(transparent)]
    Contract(#[from] ContractError),
    /// The backend does not fully support targets the policy requires.
    #[error(
        "backend lacks required verification environments (missing: {}; partial: {})",
        TargetList(missing),
        TargetList(partial)
    )]
    UnsupportedTargets {
        /// Required but undeclared.
        missing: Vec<VerificationTarget>,
        /// Required but only partially supported.
        partial: Vec<VerificationTarget>,
    },
    /// Work with no repository was checked, but a repository adds requirements
    /// for its type.
    #[error("work type {} has repository requirements; name the repository", work_type.as_str())]
    RepositoryRequired {
        /// The work type.
        work_type: Text,
    },
    /// A policy entry names a work type with no targets.
    #[error("a verification requirement names no targets")]
    EmptyRequirement,
    /// A declaration or policy exceeds its bound.
    #[error("verification declaration or policy exceeds its bound")]
    TooMany,
    /// Authorized verification evidence was offered by a producer instead of
    /// a verification run.
    #[error("authorized verification evidence is recorded only by a verification run")]
    NotRun,
    /// The verification environment could not run the software.
    #[error("verification environment unavailable: {0}")]
    Unavailable(BackendUnavailable),
}

impl VerificationError {
    /// The broad handling class.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::Contract(error) => error.class(),
            Self::UnsupportedTargets { .. } | Self::RepositoryRequired { .. } | Self::NotRun => {
                ErrorClass::Refused
            }
            Self::EmptyRequirement | Self::TooMany => ErrorClass::InvalidInput,
            Self::Unavailable(_) => ErrorClass::Execution,
        }
    }
}

type Result<T, E = VerificationError> = std::result::Result<T, E>;

struct TargetList<'a>(&'a [VerificationTarget]);

impl fmt::Display for TargetList<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut targets = self.0.iter();
        let Some(first) = targets.next() else {
            return formatter.write_str("none");
        };
        write!(formatter, "{first}")?;
        for target in targets {
            write!(formatter, ", {target}")?;
        }
        Ok(())
    }
}
