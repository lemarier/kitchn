//! A worker backend over Kitchen's HTTP worker protocol.
//!
//! Any service that implements the protocol can run a house's workers, such
//! as a hosted sandbox control plane that starts one container per worker.
//! The protocol is documented in the website reference
//! (`docs/reference/http-backend`). [`HttpBackend`] implements
//! [`EffectExecutor`], [`WorkerBackend`], and [`CoordinatorMailbox`]; the
//! protocol has no schedule calls, so it does not implement
//! [`crate::contracts::ScheduleBackend`].
//!
//! Calls are synchronous and bounded: each runs one `curl` subprocess with a
//! deadline. Kitchen sends the persisted [`EffectRequest`], including its
//! idempotency key and the credential's *name*, and never a credential value;
//! the bearer token that authenticates Kitchen to the service travels only in
//! the `Authorization` header, through curl's configuration on stdin.
//!
//! Refusals Kitchen can decide itself (another house, another backend
//! namespace, an undeclared capability or agent selection, a target on
//! another backend) are returned before anything is sent. A call that may
//! have reached the service and produced no typed answer is
//! [`EffectFailure::Uncertain`], for the caller to reconcile with
//! [`EffectExecutor::lookup`].

mod transport;
mod wire;

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    BackendId, ErrorClass, HouseId,
    contracts::{
        BackendDescriptor, BackendUnavailable, Capability, ContractError, CoordinatorMailbox,
        Delivery, Effect, EffectExecutor, EffectFailure, EffectRequest, ExternalRef, Lookup,
        MAX_INVENTORY_RESOURCES, MAX_MAILBOX_WAIT, MailboxError, NotAppliedReason, Operation,
        Receipt, ResourceObservation, ResourceRef, UncertainReason, WorkerBackend, WorkerState,
    },
    house::HttpEndpoint,
    state::UsageReport,
};

use transport::{BearerToken, CallError, MAX_CALL, Method, Response, Transport};

/// Longest accepted per-call deadline. A mailbox wait adds its wait to it,
/// and the sum must stay within one bounded call.
pub const MAX_CALL_TIMEOUT: Duration = Duration::from_secs(25);

/// At most this many long-poll calls serve one [`CoordinatorMailbox::await_delivery`],
/// so a service that answers at once cannot turn a wait into a busy loop.
const MAX_AWAIT_CALLS: usize = 32;

/// How to reach one service, for one house and one supervised run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpConfig {
    /// The service's base URL.
    pub endpoint: HttpEndpoint,
    /// The backend namespace the house is bound to; the service must report
    /// exactly this one.
    pub backend: BackendId,
    /// The house; the service must report exactly this one.
    pub house: HouseId,
    /// The supervised run workers report to.
    pub run: ExternalRef,
    /// This coordinator instance, which the service fences after another
    /// instance adopts the run.
    pub coordinator: ExternalRef,
    /// Absolute path of the `curl` executable.
    pub curl: PathBuf,
    /// Deadline of one call, at most [`MAX_CALL_TIMEOUT`].
    pub call_timeout: Duration,
}

/// Why connecting to a service failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HttpError {
    /// The configuration or token is unusable: a relative `curl` path, a
    /// deadline outside its bounds, or a token that is empty or not
    /// printable ASCII. Nothing was sent.
    #[error(
        "invalid HTTP backend configuration: curl must be an absolute path, the call timeout between 1 and 25 seconds, and the token printable ASCII"
    )]
    InvalidConfig,
    /// The service refused Kitchen's token.
    #[error("the HTTP backend at {endpoint} refused the house's token")]
    Unauthorized {
        /// The service.
        endpoint: HttpEndpoint,
    },
    /// The service could not be reached or answered with an error.
    #[error("the HTTP backend at {endpoint} is unavailable: {source}")]
    Unavailable {
        /// The service.
        endpoint: HttpEndpoint,
        /// What failed.
        #[source]
        source: BackendUnavailable,
    },
    /// The service's descriptor did not parse.
    #[error("the HTTP backend at {endpoint} returned a malformed descriptor")]
    Malformed {
        /// The service.
        endpoint: HttpEndpoint,
    },
    /// The service serves another backend namespace or house than the
    /// binding names.
    #[error(
        "the HTTP backend at {endpoint} serves backend {reported_backend} for house {reported_house}, not the bound backend {backend} for house {house}"
    )]
    DescriptorMismatch {
        /// The service.
        endpoint: HttpEndpoint,
        /// The bound namespace.
        backend: BackendId,
        /// The bound house.
        house: HouseId,
        /// The namespace the service reported.
        reported_backend: BackendId,
        /// The house the service reported.
        reported_house: HouseId,
    },
}

impl HttpError {
    /// Broad handling class for callers.
    #[must_use]
    pub const fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidConfig => ErrorClass::InvalidInput,
            Self::Unauthorized { .. } | Self::DescriptorMismatch { .. } => ErrorClass::Refused,
            Self::Unavailable { .. } | Self::Malformed { .. } => ErrorClass::Execution,
        }
    }
}

/// A house's workers on one HTTP service. See the module documentation.
#[derive(Debug)]
pub struct HttpBackend {
    descriptor: BackendDescriptor,
    transport: Transport,
    run: ExternalRef,
    coordinator: ExternalRef,
    call_timeout: Duration,
}

impl HttpBackend {
    /// Read the service's descriptor and check that it serves `config`'s
    /// backend namespace and house. The backend then declares the
    /// capabilities the service reported that the protocol covers.
    ///
    /// Commands build backends through
    /// [`crate::adapters::resolve_http_backend`], which reads `token` from
    /// the house's private registry and checks required capabilities.
    ///
    /// # Errors
    /// [`HttpError::InvalidConfig`] before anything is sent; the other
    /// [`HttpError`] variants for what the service answered.
    pub fn connect(config: HttpConfig, token: &str) -> Result<Self, HttpError> {
        let token = BearerToken::new(token).ok_or(HttpError::InvalidConfig)?;
        if !config.curl.is_absolute()
            || config.call_timeout < Duration::from_secs(1)
            || config.call_timeout > MAX_CALL_TIMEOUT
        {
            return Err(HttpError::InvalidConfig);
        }
        let transport = Transport {
            curl: config.curl,
            endpoint: config.endpoint.clone(),
            token,
        };
        let endpoint = config.endpoint;
        let unavailable = |source| HttpError::Unavailable {
            endpoint: endpoint.clone(),
            source,
        };
        let response = transport
            .call(
                Method::Get,
                "/v1/descriptor",
                None,
                None,
                config.call_timeout,
            )
            .map_err(|error| unavailable(read_failure(error)))?;
        match response.status {
            200 => {}
            401 | 403 => return Err(HttpError::Unauthorized { endpoint }),
            _ => return Err(unavailable(BackendUnavailable::Transport)),
        }
        let descriptor = serde_json::from_slice::<wire::DescriptorBody>(&response.body)
            .map_err(|_| HttpError::Malformed {
                endpoint: endpoint.clone(),
            })?
            .into_descriptor();
        if descriptor.backend != config.backend || descriptor.house != config.house {
            return Err(HttpError::DescriptorMismatch {
                endpoint,
                backend: config.backend,
                house: config.house,
                reported_backend: descriptor.backend,
                reported_house: descriptor.house,
            });
        }
        Ok(Self {
            descriptor,
            transport,
            run: config.run,
            coordinator: config.coordinator,
            call_timeout: config.call_timeout,
        })
    }

    /// The usage the service reports for `worker`, or `None` when it has
    /// none yet. Record it with
    /// [`crate::workflows::coordination::record_worker_usage`] once the
    /// worker's attempt ended.
    ///
    /// # Errors
    /// [`BackendUnavailable::Unsupported`] unless
    /// [`Capability::UsageAttribution`] is fully supported, and read failures.
    pub fn worker_usage(
        &self,
        worker: &ResourceRef,
    ) -> Result<Option<UsageReport>, BackendUnavailable> {
        self.require(Capability::UsageAttribution)?;
        if worker.backend != self.descriptor.backend {
            return Ok(None);
        }
        let response = self.read(
            Method::Post,
            "/v1/workers/usage",
            &wire::WorkerBody { worker },
        )?;
        wire::usage(&response.body).ok_or(BackendUnavailable::Transport)
    }

    fn require(&self, capability: Capability) -> Result<(), BackendUnavailable> {
        if self.descriptor.capabilities.supports(capability) {
            Ok(())
        } else {
            Err(BackendUnavailable::Unsupported(capability))
        }
    }

    /// Declared at any support level, as the mailbox contract states.
    fn declared(&self, capability: Capability) -> Result<(), MailboxError> {
        match self.descriptor.capabilities.support(capability) {
            Some(_) => Ok(()),
            None => Err(BackendUnavailable::Unsupported(capability).into()),
        }
    }

    /// One read-only call; only a 200 response is an answer.
    fn read(
        &self,
        method: Method,
        path: &str,
        body: &impl serde::Serialize,
    ) -> Result<Response, BackendUnavailable> {
        self.read_within(method, path, body, self.call_timeout)
    }

    fn read_within(
        &self,
        method: Method,
        path: &str,
        body: &impl serde::Serialize,
        timeout: Duration,
    ) -> Result<Response, BackendUnavailable> {
        let body = match method {
            Method::Get => None,
            Method::Post => {
                Some(serde_json::to_vec(body).map_err(|_| BackendUnavailable::LocalConfiguration)?)
            }
        };
        let response = self
            .transport
            .call(method, path, body.as_deref(), None, timeout)
            .map_err(read_failure)?;
        if response.status == 200 {
            Ok(response)
        } else {
            Err(BackendUnavailable::Transport)
        }
    }

    fn mailbox_call(
        &self,
        path: &str,
        delivery: Option<&ExternalRef>,
        wait: Option<Duration>,
    ) -> Result<Option<Delivery>, MailboxError> {
        self.declared(Capability::WorkerDeliveries)?;
        let body = wire::MailboxBody {
            run: &self.run,
            coordinator: &self.coordinator,
            delivery,
            wait_ms: wait.map(|wait| u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)),
        };
        let timeout = wait.map_or(self.call_timeout, |wait| {
            wait.saturating_add(self.call_timeout)
        });
        let response = self.read_within(Method::Post, path, &body, timeout)?;
        match wire::mailbox(&response.body, &self.descriptor.backend) {
            Some(wire::Mailbox::Batch(batch)) => Ok(batch),
            Some(wire::Mailbox::Fenced) => Err(MailboxError::Fenced),
            None => Err(BackendUnavailable::Transport.into()),
        }
    }
}

/// A read failure: nothing may be inferred from it.
const fn read_failure(error: CallError) -> BackendUnavailable {
    match error {
        CallError::NotSent => BackendUnavailable::LocalConfiguration,
        CallError::Timeout => BackendUnavailable::Timeout,
        CallError::Transport => BackendUnavailable::Transport,
    }
}

impl EffectExecutor for HttpBackend {
    fn descriptor(&self) -> &BackendDescriptor {
        &self.descriptor
    }

    fn execute(&self, request: &EffectRequest) -> Result<Receipt, EffectFailure> {
        let refused = |reason| Err(EffectFailure::NotApplied(reason));
        let descriptor = &self.descriptor;
        if request.house() != &descriptor.house {
            return refused(NotAppliedReason::CrossHouse);
        }
        if request.backend() != &descriptor.backend {
            return refused(NotAppliedReason::ForeignBackend);
        }
        // Only worker operations are covered, so no other effect's
        // capability is ever declared.
        let capability = request.effect().required_capability();
        let Effect::Worker(operation) = request.effect() else {
            return refused(NotAppliedReason::Unsupported(capability));
        };
        if !descriptor.capabilities.supports(capability) {
            return refused(NotAppliedReason::Unsupported(capability));
        }
        if let Operation::LaunchWorker {
            agent: Some(agent), ..
        } = operation
            && let Err(error) = descriptor.check_worker_selection(agent)
        {
            // Never replaced by the service's default agent.
            return refused(match error {
                ContractError::UnsupportedCapabilities { missing, .. } => missing
                    .first()
                    .map_or(NotAppliedReason::Rejected, |missing| {
                        NotAppliedReason::Unsupported(*missing)
                    }),
                _ => NotAppliedReason::Rejected,
            });
        }
        if operation
            .target()
            .is_some_and(|target| target.backend != descriptor.backend)
        {
            return refused(NotAppliedReason::Rejected);
        }
        let Ok(body) = serde_json::to_vec(&wire::EffectBody {
            run: &self.run,
            request,
        }) else {
            return refused(NotAppliedReason::Rejected);
        };
        match self.transport.call(
            Method::Post,
            "/v1/effects",
            Some(&body),
            Some(request.key()),
            self.call_timeout,
        ) {
            Ok(response) => wire::execute_outcome(
                response.status,
                &response.body,
                request,
                &descriptor.backend,
            ),
            Err(CallError::NotSent) => refused(NotAppliedReason::Rejected),
            Err(CallError::Timeout) => Err(EffectFailure::Uncertain(UncertainReason::Timeout)),
            Err(CallError::Transport) => Err(EffectFailure::Uncertain(UncertainReason::Transport)),
        }
    }

    fn lookup(&self, request: &EffectRequest) -> Result<Lookup, BackendUnavailable> {
        let descriptor = &self.descriptor;
        if !descriptor.supports_lookup(request.effect()) {
            return Err(BackendUnavailable::Unsupported(
                request.effect().kind().lookup_capability(),
            ));
        }
        if request.house() != &descriptor.house || request.backend() != &descriptor.backend {
            // Never sent from here; whether another executor applied it is
            // not this service's to say.
            return Ok(Lookup::Unknown);
        }
        if !matches!(request.effect(), Effect::Worker(_)) {
            // The protocol cannot carry it, so no service applied it.
            return Ok(Lookup::Absent);
        }
        let response = self.read(
            Method::Post,
            "/v1/effects/lookup",
            &wire::EffectBody {
                run: &self.run,
                request,
            },
        )?;
        wire::lookup(&response.body, &descriptor.backend).ok_or(BackendUnavailable::Transport)
    }
}

impl WorkerBackend for HttpBackend {
    fn observe_worker(&self, worker: &ResourceRef) -> Result<WorkerState, BackendUnavailable> {
        self.require(Capability::WorkerStatusAndOutcome)?;
        if worker.backend != self.descriptor.backend {
            return Ok(WorkerState::Missing);
        }
        let response = self.read(
            Method::Post,
            "/v1/workers/observe",
            &wire::WorkerBody { worker },
        )?;
        wire::worker_state(&response.body).ok_or(BackendUnavailable::Transport)
    }

    fn inventory(&self) -> Result<Vec<ResourceObservation>, BackendUnavailable> {
        self.require(Capability::ResourceInventory)?;
        let response = self.read(Method::Get, "/v1/inventory", &())?;
        let listed = wire::inventory(&response.body, &self.descriptor.backend)
            .ok_or(BackendUnavailable::Transport)?;
        if listed.len() > MAX_INVENTORY_RESOURCES {
            return Err(BackendUnavailable::LimitExceeded);
        }
        Ok(listed)
    }
}

impl CoordinatorMailbox for HttpBackend {
    fn adopt_run(&self) -> Result<(), MailboxError> {
        self.declared(Capability::RunTransfer)?;
        self.read(
            Method::Post,
            "/v1/runs/adopt",
            &wire::MailboxBody {
                run: &self.run,
                coordinator: &self.coordinator,
                delivery: None,
                wait_ms: None,
            },
        )?;
        Ok(())
    }

    fn next_delivery(&self) -> Result<Option<Delivery>, MailboxError> {
        self.mailbox_call("/v1/deliveries/next", None, None)
    }

    fn acknowledge(&self, delivery: &ExternalRef) -> Result<Option<Delivery>, MailboxError> {
        self.mailbox_call("/v1/deliveries/acknowledge", Some(delivery), None)
    }

    /// Long-polls in slices that each fit one bounded call, until a batch
    /// arrives or `wait` (at most [`MAX_MAILBOX_WAIT`]) ends.
    fn await_delivery(&self, wait: Duration) -> Result<Option<Delivery>, MailboxError> {
        self.declared(Capability::WorkerDeliveries)?;
        let started = Instant::now();
        let wait = wait.min(MAX_MAILBOX_WAIT);
        let slice = MAX_CALL.saturating_sub(self.call_timeout);
        for _ in 0..MAX_AWAIT_CALLS {
            let remaining = wait.saturating_sub(started.elapsed());
            let batch =
                self.mailbox_call("/v1/deliveries/await", None, Some(remaining.min(slice)))?;
            if batch.is_some() || remaining <= slice {
                return Ok(batch);
            }
        }
        Ok(None)
    }
}
