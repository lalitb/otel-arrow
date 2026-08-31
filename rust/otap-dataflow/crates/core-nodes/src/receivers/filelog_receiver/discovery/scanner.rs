// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Path-pattern planning and incremental filesystem scanning.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use globset::GlobMatcher;
use walkdir::WalkDir;

use super::admission::AdmissionController;
use super::{
    DiscoveredCandidate, DiscoveryError, DiscoveryIssue, ReconciliationBatch, RevocationReason,
};
use crate::receivers::filelog_receiver::config::{
    RuntimeConfig, glob_literal_prefix, reconciliation_delay_bounds_ns,
};
use crate::receivers::filelog_receiver::environment::{
    DescriptorPressure, EnvironmentalBackoff, EnvironmentalErrorClass, EnvironmentalOperation,
    classify_io_error,
};
use crate::receivers::filelog_receiver::identity::IdentityError;
use crate::receivers::filelog_receiver::identity::platform::{
    encode_advisory_path, open_candidate_at_cancellable,
    open_locator_for_stability_check_cancellable,
};

/// Requested `walkdir` enumeration-handle cap. `walkdir` may buffer remaining
/// entries when closing an iterator, and on Windows with link following it
/// retains ancestor handles outside this cap. Those aggregate-resource and
/// non-Linux runtime properties remain explicit qualification gates.
pub(super) const MAX_OPEN_TRAVERSAL_DIRECTORIES: usize = 1;

#[derive(Debug, Clone)]
struct IncludeRoot {
    lexical_root: PathBuf,
    matcher: GlobMatcher,
}

#[derive(Debug, Clone)]
struct ExcludePattern {
    lexical_root: PathBuf,
    matcher: GlobMatcher,
}

#[derive(Debug)]
struct ResolvedExclude {
    pattern_index: usize,
    lexical_root: PathBuf,
    resolved_root: Option<PathBuf>,
    matcher: GlobMatcher,
}

enum StableCandidateObservation {
    Eligible(DiscoveredCandidate),
    Revoked(crate::receivers::filelog_receiver::checkpoint::Locator),
}

#[derive(Clone, Copy, Debug)]
struct DiscoveryBackoff {
    state: EnvironmentalBackoff,
    operation: EnvironmentalOperation,
    error: EnvironmentalErrorClass,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReconciliationSchedule {
    pub(super) minimum_delay_ns: u64,
    pub(super) maximum_delay_ns: u64,
}

impl ReconciliationSchedule {
    fn from_runtime(config: &RuntimeConfig) -> Self {
        let (minimum_delay_ns, maximum_delay_ns) = reconciliation_delay_bounds_ns(
            config.discovery.reconcile_interval,
            config.discovery.reconcile_jitter_percent,
        )
        .expect("validated reconciliation delay bounds remain representable");
        Self {
            minimum_delay_ns,
            maximum_delay_ns,
        }
    }

    pub(crate) fn delay_for_sample(self, sample: u64) -> Result<Duration, DiscoveryError> {
        let width = self
            .maximum_delay_ns
            .checked_sub(self.minimum_delay_ns)
            .and_then(|spread| spread.checked_add(1))
            .ok_or(DiscoveryError::ScheduleOverflow {
                field: "discovery reconciliation jitter range",
            })?;
        let selected = self.minimum_delay_ns.checked_add(sample % width).ok_or(
            DiscoveryError::ScheduleOverflow {
                field: "discovery reconciliation jitter selection",
            },
        )?;
        Ok(Duration::from_nanos(selected))
    }

    pub(crate) fn next_delay(self) -> Result<Duration, DiscoveryError> {
        self.delay_for_sample(rand::random())
    }
}

/// Fully compiled and bounded filesystem-discovery plan.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveryPlan {
    include_roots: Vec<IncludeRoot>,
    exclude_patterns: Vec<ExcludePattern>,
    checkpoint_namespace_dir: PathBuf,
    recursive: bool,
    follow_symlinks: bool,
    max_recursion_depth: usize,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    ignore_older_than: Duration,
    reconciliation_schedule: ReconciliationSchedule,
    max_pending_candidates: usize,
    max_tracked_files: usize,
    max_candidate_events: usize,
    likely_self_ingestion: bool,
}

impl DiscoveryPlan {
    /// Builds a discovery plan from configuration that has already compiled
    /// and validated every glob.
    pub(crate) fn from_runtime(config: &RuntimeConfig) -> Result<Self, DiscoveryError> {
        let max_recursion_depth = usize::try_from(config.max_recursion_depth).map_err(|_| {
            DiscoveryError::BoundTooLarge {
                field: "max_recursion_depth",
                value: u64::from(config.max_recursion_depth),
            }
        })?;
        let max_pending_candidates = usize::try_from(config.limits.max_pending_candidates)
            .map_err(|_| DiscoveryError::BoundTooLarge {
                field: "limits.max_pending_candidates",
                value: u64::from(config.limits.max_pending_candidates),
            })?;
        let max_tracked_files = usize::try_from(config.limits.max_tracked_files).map_err(|_| {
            DiscoveryError::BoundTooLarge {
                field: "limits.max_tracked_files",
                value: u64::from(config.limits.max_tracked_files),
            }
        })?;
        let max_candidate_events = usize::try_from(config.limits.max_open_files).map_err(|_| {
            DiscoveryError::BoundTooLarge {
                field: "limits.max_open_files",
                value: u64::from(config.limits.max_open_files),
            }
        })?;
        let include_roots = config
            .include
            .iter()
            .zip(&config.compiled_include)
            .map(|(pattern, matcher)| IncludeRoot {
                lexical_root: {
                    let prefix = glob_literal_prefix(pattern);
                    if prefix.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        prefix
                    }
                },
                matcher: matcher.clone(),
            })
            .collect();
        let exclude_patterns = config
            .exclude
            .iter()
            .zip(&config.compiled_exclude)
            .map(|(pattern, matcher)| ExcludePattern {
                lexical_root: {
                    let prefix = glob_literal_prefix(pattern);
                    if prefix.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        prefix
                    }
                },
                matcher: matcher.clone(),
            })
            .collect();
        let likely_self_ingestion = config
            .compiled_include
            .iter()
            .any(|matcher| matcher.is_match(config.checkpoint_namespace_dir.join("CURRENT")));
        Ok(Self {
            include_roots,
            exclude_patterns,
            checkpoint_namespace_dir: config.checkpoint_namespace_dir.clone(),
            recursive: config.recursive,
            follow_symlinks: config.follow_symlinks,
            max_recursion_depth,
            fingerprint_bytes: u16::try_from(config.identity.fingerprint_bytes)
                .expect("validated fingerprint_bytes fits u16"),
            ignored_header_bytes: u32::try_from(config.identity.ignored_header_bytes)
                .expect("validated ignored_header_bytes fits u32"),
            ignore_older_than: config.ignore_older_than,
            reconciliation_schedule: ReconciliationSchedule::from_runtime(config),
            max_pending_candidates,
            max_tracked_files,
            max_candidate_events,
            likely_self_ingestion,
        })
    }

    pub(crate) fn reconciliation_schedule(&self) -> ReconciliationSchedule {
        self.reconciliation_schedule
    }

    pub(crate) fn max_pending_candidates(&self) -> usize {
        self.max_pending_candidates
    }

    pub(crate) fn max_tracked_files(&self) -> usize {
        self.max_tracked_files
    }

    pub(crate) fn max_candidate_events(&self) -> usize {
        self.max_candidate_events
    }

    pub(crate) fn fingerprint_bytes(&self) -> u16 {
        self.fingerprint_bytes
    }

    /// Reports the best-effort operator warning condition where an include
    /// can match an artifact under the checkpoint namespace.
    pub(crate) fn likely_self_ingestion(&self) -> bool {
        self.likely_self_ingestion
    }
}

/// Incremental scanner that never materializes the complete filesystem match
/// set.
#[derive(Debug)]
pub(crate) struct FilesystemScanner {
    plan: DiscoveryPlan,
    shutdown_requested: Arc<AtomicBool>,
    descriptor_pressure: Arc<DescriptorPressure>,
    include_backoffs: Vec<Option<DiscoveryBackoff>>,
    exclude_resolution_backoffs: Vec<Option<DiscoveryBackoff>>,
    exclude_scan_backoffs: Vec<Option<DiscoveryBackoff>>,
    #[cfg(test)]
    next_candidate_sample_gate: Mutex<Option<CandidateSamplingGate>>,
    #[cfg(test)]
    next_candidate_open_gate: Mutex<Option<CandidateSamplingGate>>,
    #[cfg(test)]
    next_candidate_open_error: Mutex<Option<io::Error>>,
    #[cfg(test)]
    next_candidate_resolution_gate: Mutex<Option<CandidateSamplingGate>>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct CandidateSamplingGateState {
    entered: bool,
    released: bool,
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct CandidateSamplingGate {
    state: Arc<(Mutex<CandidateSamplingGateState>, Condvar)>,
}

#[cfg(test)]
impl CandidateSamplingGate {
    fn block(&self) {
        let (state, condition) = &*self.state;
        let mut state = state.lock().expect("candidate sampling gate lock poisoned");
        state.entered = true;
        condition.notify_all();
        while !state.released {
            state = condition
                .wait(state)
                .expect("candidate sampling gate lock poisoned while blocked");
        }
    }

    pub(crate) fn wait_until_entered(&self, timeout: Duration) -> bool {
        let (state, condition) = &*self.state;
        let state = state.lock().expect("candidate sampling gate lock poisoned");
        let (state, _) = condition
            .wait_timeout_while(state, timeout, |state| !state.entered)
            .expect("candidate sampling gate lock poisoned while waiting");
        state.entered
    }

    pub(crate) fn release(&self) {
        let (state, condition) = &*self.state;
        let mut state = state.lock().expect("candidate sampling gate lock poisoned");
        state.released = true;
        condition.notify_all();
    }
}

impl FilesystemScanner {
    pub(crate) fn new(plan: DiscoveryPlan) -> Self {
        Self::with_shutdown_signal_and_pressure(
            plan,
            Arc::new(AtomicBool::new(false)),
            Arc::new(DescriptorPressure::default()),
        )
    }

    pub(crate) fn with_shutdown_signal(
        plan: DiscoveryPlan,
        shutdown_requested: Arc<AtomicBool>,
    ) -> Self {
        Self::with_shutdown_signal_and_pressure(
            plan,
            shutdown_requested,
            Arc::new(DescriptorPressure::default()),
        )
    }

    pub(crate) fn with_shutdown_signal_and_pressure(
        plan: DiscoveryPlan,
        shutdown_requested: Arc<AtomicBool>,
        descriptor_pressure: Arc<DescriptorPressure>,
    ) -> Self {
        let include_backoffs = vec![None; plan.include_roots.len()];
        let exclude_resolution_backoffs = vec![None; plan.exclude_patterns.len()];
        let exclude_scan_backoffs = vec![None; plan.exclude_patterns.len()];
        Self {
            plan,
            shutdown_requested,
            descriptor_pressure,
            include_backoffs,
            exclude_resolution_backoffs,
            exclude_scan_backoffs,
            #[cfg(test)]
            next_candidate_sample_gate: Mutex::new(None),
            #[cfg(test)]
            next_candidate_open_gate: Mutex::new(None),
            #[cfg(test)]
            next_candidate_open_error: Mutex::new(None),
            #[cfg(test)]
            next_candidate_resolution_gate: Mutex::new(None),
        }
    }

    pub(crate) fn plan(&self) -> &DiscoveryPlan {
        &self.plan
    }

    #[cfg(test)]
    pub(crate) fn gate_next_candidate_after_first_sample_for_test(
        &mut self,
    ) -> CandidateSamplingGate {
        let gate = CandidateSamplingGate::default();
        *self
            .next_candidate_sample_gate
            .lock()
            .expect("candidate sampling gate lock poisoned") = Some(gate.clone());
        gate
    }

    #[cfg(test)]
    pub(crate) fn gate_next_candidate_before_first_open_for_test(
        &mut self,
    ) -> CandidateSamplingGate {
        let gate = CandidateSamplingGate::default();
        *self
            .next_candidate_open_gate
            .lock()
            .expect("candidate open gate lock poisoned") = Some(gate.clone());
        gate
    }

    #[cfg(test)]
    pub(crate) fn fail_next_candidate_open_for_test(&self, error: io::Error) {
        *self
            .next_candidate_open_error
            .lock()
            .expect("candidate open error lock poisoned") = Some(error);
    }

    #[cfg(test)]
    pub(crate) fn descriptor_pressure_retry_at_for_test(&self) -> Option<Instant> {
        self.descriptor_pressure
            .current()
            .ok()
            .flatten()
            .map(EnvironmentalBackoff::retry_at)
    }

    #[cfg(test)]
    pub(crate) fn include_retry_state_for_test(
        &self,
        pattern_index: usize,
    ) -> Option<(u8, Instant)> {
        self.include_backoffs
            .get(pattern_index)
            .copied()
            .flatten()
            .map(|backoff| (backoff.state.failures(), backoff.state.retry_at()))
    }

    #[cfg(test)]
    pub(crate) fn gate_next_candidate_before_resolution_for_test(
        &mut self,
    ) -> CandidateSamplingGate {
        let gate = CandidateSamplingGate::default();
        *self
            .next_candidate_resolution_gate
            .lock()
            .expect("candidate resolution gate lock poisoned") = Some(gate.clone());
        gate
    }

    /// Runs one reconciliation pass into the bounded admission controller.
    pub(crate) fn reconcile(
        &mut self,
        admission: &mut AdmissionController,
        now: SystemTime,
    ) -> Result<ReconciliationBatch, DiscoveryError> {
        self.ensure_running()?;
        let generation = admission.begin_scan(now)?;
        if self.descriptor_pressure.retry_at(Instant::now())?.is_some() {
            admission.record_issue(DiscoveryIssue::EnvironmentalBackoff {
                operation: EnvironmentalOperation::Probe,
                error: EnvironmentalErrorClass::DescriptorPressure,
            })?;
            return admission.finish_scan();
        }
        let Some(resolved_excludes) = self.resolve_excludes(admission)? else {
            return admission.finish_scan();
        };
        let checkpoint_namespace = canonicalize_optional(&self.plan.checkpoint_namespace_dir);
        self.ensure_running()?;
        let checkpoint_namespace = match checkpoint_namespace {
            Ok(path) => path,
            Err(source) => {
                admission.record_denial_issue(DiscoveryIssue::Io {
                    operation: "resolve checkpoint namespace",
                    path: self.plan.checkpoint_namespace_dir.clone(),
                    source,
                })?;
                return admission.finish_scan();
            }
        };
        let mut scanned_exclude_roots = HashSet::with_capacity(resolved_excludes.len());
        for exclude in &resolved_excludes {
            let Some(resolved_root) = exclude.resolved_root.as_deref() else {
                continue;
            };
            if scanned_exclude_roots.insert(resolved_root) {
                self.ensure_running()?;
                self.scan_exclude_root(
                    exclude.pattern_index,
                    resolved_root,
                    generation,
                    &resolved_excludes,
                    checkpoint_namespace.as_deref(),
                    admission,
                )?;
            }
        }
        for index in 0..self.plan.include_roots.len() {
            self.ensure_running()?;
            let include = self.plan.include_roots[index].clone();
            self.scan_include(
                index,
                &include,
                generation,
                &resolved_excludes,
                checkpoint_namespace.as_deref(),
                admission,
            )?;
        }
        admission.finish_scan()
    }

    fn scan_exclude_root(
        &mut self,
        pattern_index: usize,
        resolved_root: &Path,
        generation: u64,
        resolved_excludes: &[ResolvedExclude],
        checkpoint_namespace: Option<&Path>,
        admission: &mut AdmissionController,
    ) -> Result<(), DiscoveryError> {
        self.ensure_running()?;
        if self.descriptor_pressure_active()? {
            admission.record_denial_issue(DiscoveryIssue::EnvironmentalBackoff {
                operation: EnvironmentalOperation::Traverse,
                error: EnvironmentalErrorClass::DescriptorPressure,
            })?;
            return Ok(());
        }
        if let Some(backoff) = self
            .exclude_scan_backoffs
            .get(pattern_index)
            .copied()
            .flatten()
            .filter(|backoff| backoff.state.retry_at() > Instant::now())
        {
            admission.record_denial_issue(DiscoveryIssue::EnvironmentalBackoff {
                operation: backoff.operation,
                error: backoff.error,
            })?;
            return Ok(());
        }
        let maximum_depth = if self.plan.recursive {
            self.plan.max_recursion_depth
        } else {
            1
        };
        let mut entries = WalkDir::new(resolved_root)
            .follow_links(self.plan.follow_symlinks)
            .max_depth(maximum_depth)
            .max_open(MAX_OPEN_TRAVERSAL_DIRECTORIES)
            .into_iter();
        loop {
            self.ensure_running()?;
            let entry = entries.next();
            self.ensure_running()?;
            let Some(entry) = entry else {
                break;
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    let path = source
                        .path()
                        .map_or_else(|| resolved_root.to_path_buf(), Path::to_path_buf);
                    let issue = DiscoveryIssue::Walk { path, source };
                    let retry =
                        self.note_exclude_scan_environmental_failure(pattern_index, &issue)?;
                    admission.record_denial_issue(issue)?;
                    if retry {
                        return Ok(());
                    }
                    continue;
                }
            };
            let matched_path = entry.path();
            if entry.depth() == 0 && entry.file_type().is_dir() {
                if self.path_is_checkpoint_excluded(
                    matched_path,
                    matched_path,
                    checkpoint_namespace,
                ) {
                    entries.skip_current_dir();
                }
                continue;
            }
            if entry.file_type().is_dir() {
                let resolved_directory = std::fs::canonicalize(entry.path());
                self.ensure_running()?;
                let resolved_directory = match resolved_directory {
                    Ok(path) => path,
                    Err(source) => {
                        let issue = DiscoveryIssue::Io {
                            operation: "resolve excluded directory",
                            path: matched_path.to_path_buf(),
                            source,
                        };
                        let retry =
                            self.note_exclude_scan_environmental_failure(pattern_index, &issue)?;
                        admission.record_denial_issue(issue)?;
                        if retry {
                            return Ok(());
                        }
                        entries.skip_current_dir();
                        continue;
                    }
                };
                if let Err(issue) = validate_walk_entry_path_stability(
                    matched_path,
                    entry.path(),
                    &resolved_directory,
                    resolved_root,
                    self.plan.follow_symlinks,
                ) {
                    admission.record_denial_issue(issue)?;
                    entries.skip_current_dir();
                    continue;
                }
                if self.path_is_checkpoint_excluded(
                    matched_path,
                    &resolved_directory,
                    checkpoint_namespace,
                ) {
                    entries.skip_current_dir();
                }
                continue;
            }
            if !self.path_matches_user_exclude(matched_path, matched_path, resolved_excludes) {
                continue;
            }
            if !self.plan.follow_symlinks && entry.path_is_symlink() {
                continue;
            }
            let resolved_path = match std::fs::canonicalize(entry.path()) {
                Ok(path) => path,
                Err(source) => {
                    let issue = DiscoveryIssue::Io {
                        operation: "resolve excluded candidate",
                        path: matched_path.to_path_buf(),
                        source,
                    };
                    let retry =
                        self.note_exclude_scan_environmental_failure(pattern_index, &issue)?;
                    admission.record_denial_issue(issue)?;
                    if retry {
                        return Ok(());
                    }
                    continue;
                }
            };
            self.ensure_running()?;
            if let Err(issue) = validate_walk_entry_path_stability(
                matched_path,
                entry.path(),
                &resolved_path,
                resolved_root,
                self.plan.follow_symlinks,
            ) {
                admission.record_denial_issue(issue)?;
                continue;
            }
            if self.path_is_checkpoint_excluded(matched_path, &resolved_path, checkpoint_namespace)
            {
                continue;
            }
            if self.descriptor_pressure_active()? {
                admission.record_denial_issue(DiscoveryIssue::EnvironmentalBackoff {
                    operation: EnvironmentalOperation::Probe,
                    error: EnvironmentalErrorClass::DescriptorPressure,
                })?;
                return Ok(());
            }
            let locator = self.collect_stable_revoked_locator(
                matched_path,
                &resolved_path,
                resolved_root,
                resolved_excludes,
                checkpoint_namespace,
            );
            self.ensure_running()?;
            match locator {
                Ok(Some(locator)) => {
                    if self
                        .descriptor_pressure
                        .clear_after_success(Instant::now())?
                    {
                        admission.record_environmental_recovery()?;
                    }
                    admission.observe_revoked(
                        generation,
                        locator,
                        RevocationReason::ExcludedByPolicy,
                    )?;
                }
                Ok(None) => {}
                Err(issue) => {
                    let retry =
                        self.note_exclude_scan_environmental_failure(pattern_index, &issue)?;
                    admission.record_denial_issue(issue)?;
                    if retry {
                        return Ok(());
                    }
                }
            }
        }
        self.clear_exclude_scan_environmental_backoff(pattern_index, admission)?;
        Ok(())
    }

    fn scan_include(
        &mut self,
        pattern_index: usize,
        include: &IncludeRoot,
        generation: u64,
        resolved_excludes: &[ResolvedExclude],
        checkpoint_namespace: Option<&Path>,
        admission: &mut AdmissionController,
    ) -> Result<(), DiscoveryError> {
        self.ensure_running()?;
        if self.descriptor_pressure_active()? {
            admission.record_issue(DiscoveryIssue::EnvironmentalBackoff {
                operation: EnvironmentalOperation::Traverse,
                error: EnvironmentalErrorClass::DescriptorPressure,
            })?;
            return Ok(());
        }
        if let Some(backoff) = self
            .include_backoffs
            .get(pattern_index)
            .copied()
            .flatten()
            .filter(|backoff| backoff.state.retry_at() > Instant::now())
        {
            admission.record_issue(DiscoveryIssue::EnvironmentalBackoff {
                operation: backoff.operation,
                error: backoff.error,
            })?;
            return Ok(());
        }
        let mut lexical_root_is_symlink = false;
        if !self.plan.follow_symlinks {
            let metadata = std::fs::symlink_metadata(&include.lexical_root);
            self.ensure_running()?;
            match metadata {
                Ok(metadata) => lexical_root_is_symlink = metadata.file_type().is_symlink(),
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    self.clear_include_environmental_backoff(pattern_index, admission)?;
                    return Ok(());
                }
                Err(source) => {
                    let issue = DiscoveryIssue::Io {
                        operation: "inspect include root without following links",
                        path: include.lexical_root.clone(),
                        source,
                    };
                    let _ = self.note_include_environmental_failure(pattern_index, &issue)?;
                    admission.record_issue(issue)?;
                    return Ok(());
                }
            }
        }
        let resolved_root = std::fs::canonicalize(&include.lexical_root);
        self.ensure_running()?;
        let resolved_root = match resolved_root {
            Ok(path) => path,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                self.clear_include_environmental_backoff(pattern_index, admission)?;
                return Ok(());
            }
            Err(source) => {
                let issue = DiscoveryIssue::Io {
                    operation: "resolve include root",
                    path: include.lexical_root.clone(),
                    source,
                };
                let _ = self.note_include_environmental_failure(pattern_index, &issue)?;
                admission.record_issue(issue)?;
                return Ok(());
            }
        };
        if lexical_root_is_symlink {
            let metadata = std::fs::metadata(&resolved_root);
            self.ensure_running()?;
            match metadata {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    self.clear_include_environmental_backoff(pattern_index, admission)?;
                    return Ok(());
                }
                Err(source) => {
                    let issue = DiscoveryIssue::Io {
                        operation: "inspect resolved include root",
                        path: include.lexical_root.clone(),
                        source,
                    };
                    let _ = self.note_include_environmental_failure(pattern_index, &issue)?;
                    admission.record_issue(issue)?;
                    return Ok(());
                }
            }
        }
        let maximum_depth = if self.plan.recursive {
            self.plan.max_recursion_depth
        } else {
            1
        };
        let mut entries = WalkDir::new(&resolved_root)
            .follow_links(self.plan.follow_symlinks)
            .max_depth(maximum_depth)
            .max_open(MAX_OPEN_TRAVERSAL_DIRECTORIES)
            .into_iter();
        loop {
            self.ensure_running()?;
            let entry = entries.next();
            self.ensure_running()?;
            let Some(entry) = entry else {
                break;
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    let path = source
                        .path()
                        .map_or_else(|| resolved_root.clone(), Path::to_path_buf);
                    let issue = DiscoveryIssue::Walk { path, source };
                    let retry = self.note_include_environmental_failure(pattern_index, &issue)?;
                    admission.record_issue(issue)?;
                    if retry {
                        return Ok(());
                    }
                    continue;
                }
            };
            if entry.depth() == 0 && entry.file_type().is_dir() {
                continue;
            }
            let matched_path = match entry.path().strip_prefix(&resolved_root) {
                Ok(relative) => join_root(&include.lexical_root, relative),
                Err(_) => {
                    admission.record_issue(DiscoveryIssue::Io {
                        operation: "map resolved entry to include root",
                        path: entry.path().to_path_buf(),
                        source: io::Error::new(
                            io::ErrorKind::InvalidData,
                            "walk entry escaped its resolved root",
                        ),
                    })?;
                    continue;
                }
            };

            if entry.file_type().is_dir() {
                let resolved_directory = std::fs::canonicalize(entry.path());
                self.ensure_running()?;
                let resolved_directory = match resolved_directory {
                    Ok(path) => path,
                    Err(source) => {
                        let issue = DiscoveryIssue::Io {
                            operation: "resolve candidate directory",
                            path: matched_path,
                            source,
                        };
                        let retry =
                            self.note_include_environmental_failure(pattern_index, &issue)?;
                        admission.record_issue(issue)?;
                        if retry {
                            return Ok(());
                        }
                        entries.skip_current_dir();
                        continue;
                    }
                };
                if let Err(issue) = validate_walk_entry_path_stability(
                    &matched_path,
                    entry.path(),
                    &resolved_directory,
                    &resolved_root,
                    self.plan.follow_symlinks,
                ) {
                    admission.record_issue(issue)?;
                    entries.skip_current_dir();
                    continue;
                }
                let checkpoint_excluded = self.path_is_checkpoint_excluded(
                    &matched_path,
                    &resolved_directory,
                    checkpoint_namespace,
                );
                if checkpoint_excluded {
                    entries.skip_current_dir();
                }
                continue;
            }
            let include_matches = include.matcher.is_match(&matched_path);
            let lexically_excluded =
                self.path_matches_user_exclude(&matched_path, entry.path(), resolved_excludes);
            if !include_matches && !lexically_excluded {
                continue;
            }
            if include_matches {
                admission.increment_matched_paths()?;
            }
            if !self.plan.follow_symlinks && entry.path_is_symlink() {
                continue;
            }

            #[cfg(test)]
            let gate = self
                .next_candidate_resolution_gate
                .lock()
                .expect("candidate resolution gate lock poisoned")
                .take();
            #[cfg(test)]
            if let Some(gate) = gate {
                gate.block();
            }
            self.ensure_running()?;
            let resolved_path = match std::fs::canonicalize(entry.path()) {
                Ok(path) => path,
                Err(source) => {
                    let issue = DiscoveryIssue::Io {
                        operation: if lexically_excluded {
                            "resolve excluded candidate"
                        } else {
                            "resolve matched candidate"
                        },
                        path: matched_path,
                        source,
                    };
                    let retry = self.note_include_environmental_failure(pattern_index, &issue)?;
                    if lexically_excluded {
                        admission.record_denial_issue(issue)?;
                    } else {
                        admission.record_issue(issue)?;
                    }
                    if retry {
                        return Ok(());
                    }
                    continue;
                }
            };
            self.ensure_running()?;
            if let Err(issue) = validate_walk_entry_path_stability(
                &matched_path,
                entry.path(),
                &resolved_path,
                &resolved_root,
                self.plan.follow_symlinks,
            ) {
                if lexically_excluded {
                    admission.record_denial_issue(issue)?;
                } else {
                    admission.record_issue(issue)?;
                }
                continue;
            }
            if self.path_is_checkpoint_excluded(&matched_path, &resolved_path, checkpoint_namespace)
            {
                continue;
            }
            if self.path_matches_user_exclude(&matched_path, &resolved_path, resolved_excludes) {
                if self.descriptor_pressure_active()? {
                    admission.record_denial_issue(DiscoveryIssue::EnvironmentalBackoff {
                        operation: EnvironmentalOperation::Probe,
                        error: EnvironmentalErrorClass::DescriptorPressure,
                    })?;
                    return Ok(());
                }
                let locator = self.collect_stable_revoked_locator(
                    &matched_path,
                    &resolved_path,
                    &resolved_root,
                    resolved_excludes,
                    checkpoint_namespace,
                );
                self.ensure_running()?;
                match locator {
                    Ok(Some(locator)) => {
                        if self
                            .descriptor_pressure
                            .clear_after_success(Instant::now())?
                        {
                            admission.record_environmental_recovery()?;
                        }
                        admission.observe_revoked(
                            generation,
                            locator,
                            RevocationReason::ExcludedByPolicy,
                        )?;
                    }
                    Ok(None) => {}
                    Err(issue) => {
                        let retry =
                            self.note_include_environmental_failure(pattern_index, &issue)?;
                        admission.record_denial_issue(issue)?;
                        if retry {
                            return Ok(());
                        }
                    }
                }
                continue;
            }
            if lexically_excluded {
                admission.record_denial_issue(DiscoveryIssue::Identity(
                    IdentityError::CandidateChangedDuringIdentity {
                        path: matched_path.clone(),
                    },
                ))?;
                continue;
            }
            if !include_matches {
                continue;
            }
            if let Err(source) = encode_advisory_path(&resolved_path) {
                admission.record_issue(DiscoveryIssue::Identity(source))?;
                continue;
            }

            if self.descriptor_pressure_active()? {
                admission.record_issue(DiscoveryIssue::EnvironmentalBackoff {
                    operation: EnvironmentalOperation::Probe,
                    error: EnvironmentalErrorClass::DescriptorPressure,
                })?;
                return Ok(());
            }
            let observation = self.collect_stable_candidate(
                &matched_path,
                &resolved_path,
                &resolved_root,
                resolved_excludes,
                checkpoint_namespace,
            );
            self.ensure_running()?;
            let observation = match observation {
                Ok(Some(observation)) => {
                    if self
                        .descriptor_pressure
                        .clear_after_success(Instant::now())?
                    {
                        admission.record_environmental_recovery()?;
                    }
                    observation
                }
                Ok(None) => continue,
                Err(issue) => {
                    let retry = self.note_include_environmental_failure(pattern_index, &issue)?;
                    admission.record_issue(issue)?;
                    if retry {
                        return Ok(());
                    }
                    continue;
                }
            };
            match observation {
                StableCandidateObservation::Eligible(candidate) => {
                    admission.observe(generation, candidate, self.plan.ignore_older_than)?;
                }
                StableCandidateObservation::Revoked(locator) => admission.observe_revoked(
                    generation,
                    locator,
                    RevocationReason::ExcludedByPolicy,
                )?,
            }
        }
        self.clear_include_environmental_backoff(pattern_index, admission)?;
        Ok(())
    }

    fn collect_stable_revoked_locator(
        &self,
        matched_path: &Path,
        resolved_path: &Path,
        resolved_root: &Path,
        resolved_excludes: &[ResolvedExclude],
        checkpoint_namespace: Option<&Path>,
    ) -> Result<Option<crate::receivers::filelog_receiver::checkpoint::Locator>, DiscoveryIssue>
    {
        if self.cancellation_requested() {
            return Ok(None);
        }
        let first = open_locator_for_stability_check_cancellable(resolved_path, false, || {
            self.cancellation_requested()
        });
        if self.cancellation_requested() {
            return Ok(None);
        }
        let Some(first) = first? else {
            return Ok(None);
        };
        let resolved_again =
            std::fs::canonicalize(matched_path).map_err(|source| DiscoveryIssue::Io {
                operation: "revalidate excluded candidate",
                path: matched_path.to_path_buf(),
                source,
            });
        if self.cancellation_requested() {
            return Ok(None);
        }
        let resolved_again = resolved_again?;
        validate_candidate_path_stability(
            matched_path,
            resolved_path,
            &resolved_again,
            resolved_root,
            self.plan.follow_symlinks,
        )?;
        if self.path_is_checkpoint_excluded(matched_path, &resolved_again, checkpoint_namespace)
            || !self.path_matches_user_exclude(matched_path, &resolved_again, resolved_excludes)
        {
            return Err(DiscoveryIssue::Identity(
                IdentityError::CandidateChangedDuringIdentity {
                    path: matched_path.to_path_buf(),
                },
            ));
        }
        let second = open_locator_for_stability_check_cancellable(&resolved_again, false, || {
            self.cancellation_requested()
        });
        if self.cancellation_requested() {
            return Ok(None);
        }
        let Some(second) = second? else {
            return Ok(None);
        };
        if first != second {
            return Err(DiscoveryIssue::Identity(
                IdentityError::CandidateChangedDuringIdentity {
                    path: matched_path.to_path_buf(),
                },
            ));
        }
        Ok(Some(second))
    }

    fn collect_stable_candidate(
        &self,
        matched_path: &Path,
        resolved_path: &Path,
        resolved_root: &Path,
        resolved_excludes: &[ResolvedExclude],
        checkpoint_namespace: Option<&Path>,
    ) -> Result<Option<StableCandidateObservation>, DiscoveryIssue> {
        if self.cancellation_requested() {
            return Ok(None);
        }
        #[cfg(test)]
        if let Some(source) = self
            .next_candidate_open_error
            .lock()
            .expect("candidate open error lock poisoned")
            .take()
        {
            return Err(DiscoveryIssue::Identity(IdentityError::Io {
                operation: "open injected candidate",
                path: matched_path.to_path_buf(),
                source,
            }));
        }
        #[cfg(test)]
        let gate = self
            .next_candidate_open_gate
            .lock()
            .expect("candidate open gate lock poisoned")
            .take();
        #[cfg(test)]
        if let Some(gate) = gate {
            gate.block();
        }
        if self.cancellation_requested() {
            return Ok(None);
        }
        let first = open_candidate_at_cancellable(
            resolved_path,
            matched_path,
            false,
            self.plan.fingerprint_bytes,
            self.plan.ignored_header_bytes,
            || self.cancellation_requested(),
        );
        if self.cancellation_requested() {
            return Ok(None);
        }
        let Some(first) = first? else {
            return Ok(None);
        };
        let first_evidence = first.evidence;
        drop(first.file);
        #[cfg(test)]
        let gate = self
            .next_candidate_sample_gate
            .lock()
            .expect("candidate sampling gate lock poisoned")
            .take();
        #[cfg(test)]
        if let Some(gate) = gate {
            gate.block();
        }
        if self.cancellation_requested() {
            return Ok(None);
        }
        let resolved_again =
            std::fs::canonicalize(matched_path).map_err(|source| DiscoveryIssue::Io {
                operation: "revalidate resolved candidate",
                path: matched_path.to_path_buf(),
                source,
            });
        if self.cancellation_requested() {
            return Ok(None);
        }
        let resolved_again = resolved_again?;
        validate_candidate_path_stability(
            matched_path,
            resolved_path,
            &resolved_again,
            resolved_root,
            self.plan.follow_symlinks,
        )?;
        if self.path_is_checkpoint_excluded(matched_path, &resolved_again, checkpoint_namespace) {
            return Err(DiscoveryIssue::Identity(
                IdentityError::CandidateChangedDuringIdentity {
                    path: matched_path.to_path_buf(),
                },
            ));
        }
        if self.path_matches_user_exclude(matched_path, &resolved_again, resolved_excludes) {
            let second =
                open_locator_for_stability_check_cancellable(&resolved_again, false, || {
                    self.cancellation_requested()
                });
            if self.cancellation_requested() {
                return Ok(None);
            }
            let Some(second) = second? else {
                return Ok(None);
            };
            if first_evidence.locator != second {
                return Err(DiscoveryIssue::Identity(
                    IdentityError::CandidateChangedDuringIdentity {
                        path: matched_path.to_path_buf(),
                    },
                ));
            }
            return Ok(Some(StableCandidateObservation::Revoked(second)));
        }
        if self.cancellation_requested() {
            return Ok(None);
        }
        let second = open_candidate_at_cancellable(
            &resolved_again,
            matched_path,
            false,
            self.plan.fingerprint_bytes,
            self.plan.ignored_header_bytes,
            || self.cancellation_requested(),
        );
        if self.cancellation_requested() {
            return Ok(None);
        }
        let Some(second) = second? else {
            return Ok(None);
        };
        if first_evidence.locator != second.evidence.locator
            || second.evidence.size < first_evidence.size
            || !second
                .evidence
                .fingerprint
                .starts_with(&first_evidence.fingerprint)
        {
            return Err(DiscoveryIssue::Identity(
                IdentityError::CandidateChangedDuringIdentity {
                    path: matched_path.to_path_buf(),
                },
            ));
        }
        let modified = if self.plan.ignore_older_than.is_zero() {
            None
        } else {
            if self.cancellation_requested() {
                return Ok(None);
            }
            let metadata = second.file.metadata().map_err(|source| DiscoveryIssue::Io {
                operation: "read candidate metadata",
                path: matched_path.to_path_buf(),
                source,
            });
            if self.cancellation_requested() {
                return Ok(None);
            }
            let metadata = metadata?;
            let modified = metadata.modified().map_err(|source| DiscoveryIssue::Io {
                operation: "read candidate modification time",
                path: matched_path.to_path_buf(),
                source,
            });
            if self.cancellation_requested() {
                return Ok(None);
            }
            Some(modified?)
        };
        Ok(Some(StableCandidateObservation::Eligible(
            DiscoveredCandidate {
                matched_path: matched_path.to_path_buf(),
                resolved_path: resolved_again,
                evidence: second.evidence,
                modified,
            },
        )))
    }

    fn path_is_excluded(
        &self,
        matched_path: &Path,
        resolved_path: &Path,
        resolved_excludes: &[ResolvedExclude],
        checkpoint_namespace: Option<&Path>,
    ) -> bool {
        self.path_matches_user_exclude(matched_path, resolved_path, resolved_excludes)
            || self.path_is_checkpoint_excluded(matched_path, resolved_path, checkpoint_namespace)
    }

    fn path_matches_user_exclude(
        &self,
        matched_path: &Path,
        resolved_path: &Path,
        resolved_excludes: &[ResolvedExclude],
    ) -> bool {
        resolved_excludes.iter().any(|exclude| {
            matcher_matches_path_or_ancestor(&exclude.matcher, matched_path)
                || matcher_matches_path_or_ancestor(&exclude.matcher, resolved_path)
                || exclude.resolved_root.as_ref().is_some_and(|root| {
                    resolved_path.strip_prefix(root).is_ok_and(|relative| {
                        matcher_matches_path_or_ancestor(
                            &exclude.matcher,
                            &join_root(&exclude.lexical_root, relative),
                        )
                    })
                })
        })
    }

    fn path_is_checkpoint_excluded(
        &self,
        matched_path: &Path,
        resolved_path: &Path,
        checkpoint_namespace: Option<&Path>,
    ) -> bool {
        checkpoint_namespace.is_some_and(|namespace| resolved_path.starts_with(namespace))
            || matched_path.starts_with(&self.plan.checkpoint_namespace_dir)
    }

    fn note_descriptor_pressure(&mut self, issue: &DiscoveryIssue) -> Result<bool, DiscoveryError> {
        let Some(source) = discovery_issue_io_error(issue) else {
            return Ok(false);
        };
        if classify_io_error(source) != EnvironmentalErrorClass::DescriptorPressure {
            return Ok(false);
        }
        let _ = self.descriptor_pressure.record_failure(Instant::now())?;
        Ok(true)
    }

    fn note_include_environmental_failure(
        &mut self,
        pattern_index: usize,
        issue: &DiscoveryIssue,
    ) -> Result<bool, DiscoveryError> {
        let Some((operation, error)) = classify_environmental_issue(issue) else {
            return Ok(false);
        };
        if error == EnvironmentalErrorClass::DescriptorPressure {
            return self.note_descriptor_pressure(issue);
        }
        let previous = self
            .include_backoffs
            .get(pattern_index)
            .copied()
            .flatten()
            .map(|backoff| backoff.state);
        let state = EnvironmentalBackoff::after_failure(previous, Instant::now()).ok_or(
            DiscoveryError::ScheduleOverflow {
                field: "discovery include-root environmental retry deadline",
            },
        )?;
        self.include_backoffs[pattern_index] = Some(DiscoveryBackoff {
            state,
            operation,
            error,
        });
        Ok(true)
    }

    fn note_exclude_scan_environmental_failure(
        &mut self,
        pattern_index: usize,
        issue: &DiscoveryIssue,
    ) -> Result<bool, DiscoveryError> {
        let Some((operation, error)) = classify_environmental_issue(issue) else {
            return Ok(false);
        };
        if error == EnvironmentalErrorClass::DescriptorPressure {
            return self.note_descriptor_pressure(issue);
        }
        let previous = self
            .exclude_scan_backoffs
            .get(pattern_index)
            .copied()
            .flatten()
            .map(|backoff| backoff.state);
        let state = EnvironmentalBackoff::after_failure(previous, Instant::now()).ok_or(
            DiscoveryError::ScheduleOverflow {
                field: "discovery exclude-root environmental retry deadline",
            },
        )?;
        self.exclude_scan_backoffs[pattern_index] = Some(DiscoveryBackoff {
            state,
            operation,
            error,
        });
        Ok(true)
    }

    fn note_exclude_resolution_environmental_failure(
        &mut self,
        pattern_index: usize,
        issue: &DiscoveryIssue,
    ) -> Result<(), DiscoveryError> {
        let Some((operation, error)) = classify_environmental_issue(issue) else {
            return Ok(());
        };
        if error == EnvironmentalErrorClass::DescriptorPressure {
            let _ = self.note_descriptor_pressure(issue)?;
            return Ok(());
        }
        let previous = self
            .exclude_resolution_backoffs
            .get(pattern_index)
            .copied()
            .flatten()
            .map(|backoff| backoff.state);
        let state = EnvironmentalBackoff::after_failure(previous, Instant::now()).ok_or(
            DiscoveryError::ScheduleOverflow {
                field: "discovery exclude-root resolution retry deadline",
            },
        )?;
        self.exclude_resolution_backoffs[pattern_index] = Some(DiscoveryBackoff {
            state,
            operation,
            error,
        });
        Ok(())
    }

    fn descriptor_pressure_active(&self) -> Result<bool, DiscoveryError> {
        Ok(self.descriptor_pressure.retry_at(Instant::now())?.is_some())
    }

    fn clear_include_environmental_backoff(
        &mut self,
        pattern_index: usize,
        admission: &mut AdmissionController,
    ) -> Result<(), DiscoveryError> {
        if self.include_backoffs[pattern_index].take().is_some() {
            admission.record_environmental_recovery()?;
        }
        Ok(())
    }

    fn clear_exclude_scan_environmental_backoff(
        &mut self,
        pattern_index: usize,
        admission: &mut AdmissionController,
    ) -> Result<(), DiscoveryError> {
        if self.exclude_scan_backoffs[pattern_index].take().is_some() {
            admission.record_environmental_recovery()?;
        }
        Ok(())
    }

    fn clear_exclude_resolution_environmental_backoff(
        &mut self,
        pattern_index: usize,
        admission: &mut AdmissionController,
    ) -> Result<(), DiscoveryError> {
        if self.exclude_resolution_backoffs[pattern_index]
            .take()
            .is_some()
        {
            admission.record_environmental_recovery()?;
        }
        Ok(())
    }

    fn resolve_excludes(
        &mut self,
        admission: &mut AdmissionController,
    ) -> Result<Option<Vec<ResolvedExclude>>, DiscoveryError> {
        let mut resolved = Vec::with_capacity(self.plan.exclude_patterns.len());
        for pattern_index in 0..self.plan.exclude_patterns.len() {
            self.ensure_running()?;
            if let Some(backoff) = self
                .exclude_resolution_backoffs
                .get(pattern_index)
                .copied()
                .flatten()
                .filter(|backoff| backoff.state.retry_at() > Instant::now())
            {
                admission.record_denial_issue(DiscoveryIssue::EnvironmentalBackoff {
                    operation: backoff.operation,
                    error: backoff.error,
                })?;
                return Ok(None);
            }
            let exclude = self.plan.exclude_patterns[pattern_index].clone();
            let resolved_root = canonicalize_optional(&exclude.lexical_root);
            self.ensure_running()?;
            let resolved_root = match resolved_root {
                Ok(path) => {
                    self.clear_exclude_resolution_environmental_backoff(pattern_index, admission)?;
                    path
                }
                Err(source) => {
                    let issue = DiscoveryIssue::Io {
                        operation: "resolve exclude root",
                        path: exclude.lexical_root.clone(),
                        source,
                    };
                    self.note_exclude_resolution_environmental_failure(pattern_index, &issue)?;
                    admission.record_denial_issue(issue)?;
                    return Ok(None);
                }
            };
            resolved.push(ResolvedExclude {
                pattern_index,
                lexical_root: exclude.lexical_root.clone(),
                resolved_root,
                matcher: exclude.matcher.clone(),
            });
        }
        Ok(Some(resolved))
    }

    fn ensure_running(&self) -> Result<(), DiscoveryError> {
        if self.cancellation_requested() {
            Err(DiscoveryError::ShutdownRequested)
        } else {
            Ok(())
        }
    }

    fn cancellation_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }
}

fn discovery_issue_io_error(issue: &DiscoveryIssue) -> Option<&io::Error> {
    match issue {
        DiscoveryIssue::Io { source, .. } => Some(source),
        DiscoveryIssue::Walk { source, .. } => source.io_error(),
        DiscoveryIssue::Identity(IdentityError::Io { source, .. }) => Some(source),
        DiscoveryIssue::EnvironmentalBackoff { .. }
        | DiscoveryIssue::ConflictingPathRebind { .. }
        | DiscoveryIssue::Identity(_) => None,
    }
}

pub(crate) fn classify_environmental_issue(
    issue: &DiscoveryIssue,
) -> Option<(EnvironmentalOperation, EnvironmentalErrorClass)> {
    let source = discovery_issue_io_error(issue)?;
    if source.kind() == io::ErrorKind::NotFound {
        return None;
    }
    let operation = match issue {
        DiscoveryIssue::Walk { .. } => EnvironmentalOperation::Traverse,
        DiscoveryIssue::Identity(IdentityError::Io { .. }) => EnvironmentalOperation::Probe,
        DiscoveryIssue::Io { operation, .. }
            if matches!(
                *operation,
                "inspect include root without following links"
                    | "resolve include root"
                    | "inspect resolved include root"
                    | "resolve candidate directory"
                    | "resolve excluded directory"
                    | "map resolved entry to include root"
                    | "resolve exclude root"
            ) =>
        {
            EnvironmentalOperation::Traverse
        }
        DiscoveryIssue::Io { .. } => EnvironmentalOperation::Probe,
        DiscoveryIssue::EnvironmentalBackoff { .. }
        | DiscoveryIssue::ConflictingPathRebind { .. }
        | DiscoveryIssue::Identity(_) => return None,
    };
    Some((operation, classify_io_error(source)))
}

fn validate_walk_entry_path_stability(
    matched_path: &Path,
    walked_path: &Path,
    resolved_path: &Path,
    resolved_root: &Path,
    follow_symlinks: bool,
) -> Result<(), DiscoveryIssue> {
    let expected_resolved_path = if follow_symlinks {
        resolved_path
    } else {
        walked_path
    };
    validate_candidate_path_stability(
        matched_path,
        expected_resolved_path,
        resolved_path,
        resolved_root,
        follow_symlinks,
    )
}

pub(super) fn validate_candidate_path_stability(
    matched_path: &Path,
    resolved_path: &Path,
    resolved_again: &Path,
    resolved_root: &Path,
    follow_symlinks: bool,
) -> Result<(), DiscoveryIssue> {
    if resolved_again != resolved_path
        || (!follow_symlinks && !resolved_again.starts_with(resolved_root))
    {
        Err(DiscoveryIssue::Identity(
            IdentityError::CandidateChangedDuringIdentity {
                path: matched_path.to_path_buf(),
            },
        ))
    } else {
        Ok(())
    }
}

fn matcher_matches_path_or_ancestor(matcher: &GlobMatcher, path: &Path) -> bool {
    path.ancestors().any(|ancestor| matcher.is_match(ancestor))
}

fn canonicalize_optional(path: &Path) -> io::Result<Option<PathBuf>> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(Some(path)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(source),
    }
}

fn join_root(root: &Path, relative: &Path) -> PathBuf {
    if root == Path::new(".") {
        relative.to_path_buf()
    } else if relative.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    }
}
