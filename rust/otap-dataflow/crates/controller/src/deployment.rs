// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Deployment registry and generation state machine for the controller.
//!
//! The controller tracks each deployed pipeline group as a *generation*: a
//! numbered snapshot of the group's runtime state.  When a group is
//! reconfigured, a new generation is prepared alongside (or after) the current
//! one and the controller drives the transition through the phases below.
//!
//! # Generation lifecycle
//!
//! ```text
//!   Preparing ──► Starting ──► AwaitingReady ──► Live ──► Draining ──► Stopped
//!                                    │
//!                                    └──► RolledBack
//! ```
//!
//! `AwaitingReady` is the only commit gate.  A generation that does not
//! report ready within [`RolloutDeadline`] is rolled back automatically.
//! Rollback keeps (or restores) the previous generation as authoritative.
//!
//! # Relationship to the controller
//!
//! The controller owns one [`DeploymentRegistry`] for the lifetime of the
//! process.  On startup every group is registered with `generation = 0`.
//! On replacement a new entry is inserted at `generation = current + 1` and
//! driven to `Live` before the old entry is retired.

use crate::error::Error;
use otap_df_config::PipelineGroupId;
use otap_df_engine::control::RuntimeCtrlMsgSender;
use std::collections::HashMap;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Generation identifier
// ---------------------------------------------------------------------------

/// Monotonically increasing generation counter scoped to one pipeline group.
///
/// Generation 0 is always the initial deployment.  Subsequent replacements
/// increment by 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationId(pub u64);

impl GenerationId {
    /// Returns the next generation id.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for GenerationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gen{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Rollout phase (the state machine)
// ---------------------------------------------------------------------------

/// The controller-side lifecycle phase for one group generation.
///
/// This is distinct from pipeline health: it represents the *deployment*
/// state as seen by the controller, not the runtime state of the pipelines
/// within the group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutPhase {
    /// Generation is being built and validated.  No threads have been spawned.
    Preparing,
    /// Threads have been spawned.  Pipelines are initializing.
    Starting,
    /// Pipelines are running.  Waiting for the readiness signal or timeout.
    AwaitingReady,
    /// Generation is ready and authoritative.
    Live,
    /// Ingress has been stopped.  In-flight work is completing.
    Draining,
    /// All threads have stopped.  Resources have been released.
    Stopped,
    /// Readiness timed out or an error occurred.  Previous generation was
    /// kept (or restored) as authoritative.
    RolledBack {
        /// Human-readable reason for the rollback.
        reason: RollbackReason,
    },
}

/// The reason a generation was rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackReason {
    /// The generation did not report ready within the rollout deadline.
    ReadinessTimeout,
    /// Configuration admission failed.
    AdmissionFailed,
    /// A required listener could not be bound.
    ListenerBindFailed,
    /// Topic subscription or publisher setup failed.
    TopicSetupFailed,
    /// The operator explicitly aborted the rollout.
    Aborted,
}

impl std::fmt::Display for RollbackReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadinessTimeout => write!(f, "readiness timeout"),
            Self::AdmissionFailed => write!(f, "admission failed"),
            Self::ListenerBindFailed => write!(f, "listener bind failed"),
            Self::TopicSetupFailed => write!(f, "topic setup failed"),
            Self::Aborted => write!(f, "aborted by operator"),
        }
    }
}

// ---------------------------------------------------------------------------
// Rollout deadline
// ---------------------------------------------------------------------------

/// The absolute deadline by which a generation must reach `Live`.
#[derive(Debug, Clone, Copy)]
pub struct RolloutDeadline {
    /// When the rollout was started.
    pub started_at: Instant,
    /// How long the generation has to reach `Live` before automatic rollback.
    pub timeout: Duration,
}

impl RolloutDeadline {
    /// Creates a new deadline starting now with the given timeout.
    #[must_use]
    pub fn from_now(timeout: Duration) -> Self {
        Self {
            started_at: Instant::now(),
            timeout,
        }
    }

    /// Returns `true` if the deadline has passed.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.started_at.elapsed() > self.timeout
    }

    /// Returns the remaining time, or `Duration::ZERO` if expired.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.timeout
            .checked_sub(self.started_at.elapsed())
            .unwrap_or(Duration::ZERO)
    }
}

// ---------------------------------------------------------------------------
// Per-generation runtime handles
// ---------------------------------------------------------------------------

/// Control handle for a single pipeline thread within a group.
pub struct PipelineThreadHandle<PData: 'static + Clone + Send + Sync + std::fmt::Debug> {
    /// Sends control messages (e.g. shutdown, drain) to the pipeline.
    pub ctrl_sender: RuntimeCtrlMsgSender<PData>,
    /// OS thread handle; joined during stop.
    pub join_handle: JoinHandle<()>,
    /// The CPU core this thread is pinned to.
    pub core_id: usize,
}

impl<PData: 'static + Clone + Send + Sync + std::fmt::Debug> std::fmt::Debug
    for PipelineThreadHandle<PData>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineThreadHandle")
            .field("core_id", &self.core_id)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Group generation entry
// ---------------------------------------------------------------------------

/// One deployed generation of a pipeline group.
#[derive(Debug)]
pub struct GroupGeneration<PData: 'static + Clone + Send + Sync + std::fmt::Debug> {
    /// The group this generation belongs to.
    pub group_id: PipelineGroupId,
    /// The generation number.
    pub generation: GenerationId,
    /// Current lifecycle phase.
    pub phase: RolloutPhase,
    /// Runtime handles for the pipelines in this generation.
    /// Empty until the generation reaches `Starting`.
    pub pipeline_threads: Vec<PipelineThreadHandle<PData>>,
    /// Rollout deadline, set when the generation enters `Starting`.
    pub deadline: Option<RolloutDeadline>,
}

impl<PData: 'static + Clone + Send + Sync + std::fmt::Debug> GroupGeneration<PData> {
    /// Creates a new generation in the `Preparing` phase.
    #[must_use]
    pub fn new(group_id: PipelineGroupId, generation: GenerationId) -> Self {
        Self {
            group_id,
            generation,
            phase: RolloutPhase::Preparing,
            pipeline_threads: Vec::new(),
            deadline: None,
        }
    }

    /// Transitions the phase, enforcing the allowed state machine edges.
    ///
    /// Returns an error if the transition is illegal.
    pub fn transition(&mut self, next: RolloutPhase) -> Result<(), Error> {
        let allowed = match self.phase {
            RolloutPhase::Preparing => matches!(next, RolloutPhase::Starting),
            RolloutPhase::Starting => matches!(next, RolloutPhase::AwaitingReady),
            RolloutPhase::AwaitingReady => {
                matches!(next, RolloutPhase::Live | RolloutPhase::RolledBack { .. })
            }
            RolloutPhase::Live => matches!(next, RolloutPhase::Draining),
            RolloutPhase::Draining => matches!(next, RolloutPhase::Stopped),
            RolloutPhase::Stopped | RolloutPhase::RolledBack { .. } => false,
        };
        if allowed {
            self.phase = next;
            Ok(())
        } else {
            Err(Error::IllegalGenerationTransition {
                group_id: self.group_id.clone(),
                generation: self.generation,
                from: format!("{:?}", self.phase),
                to: format!("{next:?}"),
            })
        }
    }

    /// Returns `true` if this generation is the authoritative live generation.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.phase == RolloutPhase::Live
    }

    /// Returns `true` if the readiness deadline has passed while
    /// `AwaitingReady`.
    #[must_use]
    pub fn is_readiness_expired(&self) -> bool {
        matches!(self.phase, RolloutPhase::AwaitingReady)
            && self.deadline.as_ref().is_some_and(|d| d.is_expired())
    }
}

// ---------------------------------------------------------------------------
// Deployment registry
// ---------------------------------------------------------------------------

/// Tracks all deployed group generations for the lifetime of the controller.
///
/// The registry holds at most two generations per group at any time: the
/// current live generation and the incoming generation during a rollout.
/// Once a rollout completes (commit or rollback), the retired generation is
/// removed.
pub struct DeploymentRegistry<PData: 'static + Clone + Send + Sync + std::fmt::Debug> {
    /// Active generations, keyed by `(group_id, generation_id)`.
    generations: HashMap<(PipelineGroupId, GenerationId), GroupGeneration<PData>>,
    /// The current authoritative generation per group.
    live: HashMap<PipelineGroupId, GenerationId>,
}

impl<PData: 'static + Clone + Send + Sync + std::fmt::Debug> DeploymentRegistry<PData> {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            generations: HashMap::new(),
            live: HashMap::new(),
        }
    }

    /// Inserts a new generation into the registry.
    pub fn insert(&mut self, entry: GroupGeneration<PData>) {
        let key = (entry.group_id.clone(), entry.generation);
        _ = self.generations.insert(key, entry);
    }

    /// Returns the current live generation id for a group, if any.
    #[must_use]
    pub fn live_generation(&self, group_id: &PipelineGroupId) -> Option<GenerationId> {
        self.live.get(group_id).copied()
    }

    /// Returns the next generation id for a group.
    ///
    /// If the group has no live generation, returns `GenerationId(0)`.
    #[must_use]
    pub fn next_generation_id(&self, group_id: &PipelineGroupId) -> GenerationId {
        self.live
            .get(group_id)
            .map_or(GenerationId(0), |g| g.next())
    }

    /// Returns a mutable reference to a generation entry.
    pub fn get_mut(
        &mut self,
        group_id: &PipelineGroupId,
        generation: GenerationId,
    ) -> Option<&mut GroupGeneration<PData>> {
        self.generations.get_mut(&(group_id.clone(), generation))
    }

    /// Promotes a generation to live, replacing the previous live entry.
    ///
    /// The previous live generation is left in the registry (in `Draining` or
    /// `Stopped` state) until the caller explicitly removes it via
    /// [`retire`](Self::retire).
    pub fn commit(&mut self, group_id: &PipelineGroupId, generation: GenerationId) {
        _ = self.live.insert(group_id.clone(), generation);
    }

    /// Removes a generation from the registry after it has been stopped.
    pub fn retire(&mut self, group_id: &PipelineGroupId, generation: GenerationId) {
        _ = self.generations.remove(&(group_id.clone(), generation));
    }

    /// Returns all group ids currently tracked.
    pub fn group_ids(&self) -> impl Iterator<Item = &PipelineGroupId> {
        self.live.keys()
    }
}

impl<PData: 'static + Clone + Send + Sync + std::fmt::Debug> Default
    for DeploymentRegistry<PData>
{
    fn default() -> Self {
        Self::new()
    }
}
