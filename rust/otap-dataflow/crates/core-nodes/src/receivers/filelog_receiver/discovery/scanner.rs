// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Path-pattern planning and incremental filesystem scanning.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use globset::GlobMatcher;
use walkdir::WalkDir;

use super::admission::AdmissionController;
use super::{DiscoveredCandidate, DiscoveryError, DiscoveryIssue, ReconciliationBatch};
use crate::receivers::filelog_receiver::config::{RuntimeConfig, glob_literal_prefix};
use crate::receivers::filelog_receiver::identity::IdentityError;
use crate::receivers::filelog_receiver::identity::platform::{
    encode_advisory_path, open_candidate_at,
};

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
    lexical_root: PathBuf,
    resolved_root: Option<PathBuf>,
    matcher: GlobMatcher,
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
    poll_interval: Duration,
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
            poll_interval: config.discovery.poll_interval,
            max_pending_candidates,
            max_tracked_files,
            max_candidate_events,
            likely_self_ingestion,
        })
    }

    pub(crate) fn poll_interval(&self) -> Duration {
        self.poll_interval
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
}

impl FilesystemScanner {
    pub(crate) fn new(plan: DiscoveryPlan) -> Self {
        Self {
            plan,
            shutdown_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn with_shutdown_signal(
        plan: DiscoveryPlan,
        shutdown_requested: Arc<AtomicBool>,
    ) -> Self {
        Self {
            plan,
            shutdown_requested,
        }
    }

    pub(crate) fn plan(&self) -> &DiscoveryPlan {
        &self.plan
    }

    /// Runs one reconciliation pass into the bounded admission controller.
    pub(crate) fn reconcile(
        &mut self,
        admission: &mut AdmissionController,
        now: SystemTime,
    ) -> Result<ReconciliationBatch, DiscoveryError> {
        self.ensure_running()?;
        let generation = admission.begin_scan(now)?;
        let Some(resolved_excludes) = self.resolve_excludes(admission)? else {
            return admission.finish_scan();
        };
        let checkpoint_namespace = match canonicalize_optional(&self.plan.checkpoint_namespace_dir)
        {
            Ok(path) => path,
            Err(source) => {
                admission.record_issue(DiscoveryIssue::Io {
                    operation: "resolve checkpoint namespace",
                    path: self.plan.checkpoint_namespace_dir.clone(),
                    source,
                })?;
                return admission.finish_scan();
            }
        };
        for include in &self.plan.include_roots {
            self.ensure_running()?;
            self.scan_include(
                include,
                generation,
                &resolved_excludes,
                checkpoint_namespace.as_deref(),
                admission,
            )?;
        }
        admission.finish_scan()
    }

    fn scan_include(
        &self,
        include: &IncludeRoot,
        generation: u64,
        resolved_excludes: &[ResolvedExclude],
        checkpoint_namespace: Option<&Path>,
        admission: &mut AdmissionController,
    ) -> Result<(), DiscoveryError> {
        let mut lexical_root_is_symlink = false;
        if !self.plan.follow_symlinks {
            match std::fs::symlink_metadata(&include.lexical_root) {
                Ok(metadata) => lexical_root_is_symlink = metadata.file_type().is_symlink(),
                Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(source) => {
                    admission.record_issue(DiscoveryIssue::Io {
                        operation: "inspect include root without following links",
                        path: include.lexical_root.clone(),
                        source,
                    })?;
                    return Ok(());
                }
            }
        }
        let resolved_root = match std::fs::canonicalize(&include.lexical_root) {
            Ok(path) => path,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                admission.record_issue(DiscoveryIssue::Io {
                    operation: "resolve include root",
                    path: include.lexical_root.clone(),
                    source,
                })?;
                return Ok(());
            }
        };
        if lexical_root_is_symlink {
            match std::fs::metadata(&resolved_root) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return Ok(()),
                Err(source) => {
                    admission.record_issue(DiscoveryIssue::Io {
                        operation: "inspect resolved include root",
                        path: include.lexical_root.clone(),
                        source,
                    })?;
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
            .into_iter();
        loop {
            self.ensure_running()?;
            let Some(entry) = entries.next() else {
                break;
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    let path = source
                        .path()
                        .map_or_else(|| resolved_root.clone(), Path::to_path_buf);
                    admission.record_issue(DiscoveryIssue::Walk { path, source })?;
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
                let resolved_directory = match std::fs::canonicalize(entry.path()) {
                    Ok(path) => path,
                    Err(source) => {
                        admission.record_issue(DiscoveryIssue::Io {
                            operation: "resolve candidate directory",
                            path: matched_path,
                            source,
                        })?;
                        entries.skip_current_dir();
                        continue;
                    }
                };
                if let Err(issue) = validate_candidate_path_stability(
                    &matched_path,
                    &resolved_directory,
                    &resolved_directory,
                    &resolved_root,
                    self.plan.follow_symlinks,
                ) {
                    admission.record_issue(issue)?;
                    entries.skip_current_dir();
                    continue;
                }
                if self.path_is_excluded(
                    &matched_path,
                    &resolved_directory,
                    resolved_excludes,
                    checkpoint_namespace,
                ) {
                    entries.skip_current_dir();
                }
                continue;
            }
            if !include.matcher.is_match(&matched_path) {
                continue;
            }
            admission.increment_matched_paths()?;
            if !self.plan.follow_symlinks && entry.path_is_symlink() {
                continue;
            }

            let resolved_path = match std::fs::canonicalize(entry.path()) {
                Ok(path) => path,
                Err(source) => {
                    admission.record_issue(DiscoveryIssue::Io {
                        operation: "resolve matched candidate",
                        path: matched_path,
                        source,
                    })?;
                    continue;
                }
            };
            self.ensure_running()?;
            if let Err(issue) = validate_candidate_path_stability(
                &matched_path,
                &resolved_path,
                &resolved_path,
                &resolved_root,
                self.plan.follow_symlinks,
            ) {
                admission.record_issue(issue)?;
                continue;
            }
            if self.path_is_excluded(
                &matched_path,
                &resolved_path,
                resolved_excludes,
                checkpoint_namespace,
            ) {
                continue;
            }
            if let Err(source) = encode_advisory_path(&resolved_path) {
                admission.record_issue(DiscoveryIssue::Identity(source))?;
                continue;
            }

            let candidate = match self.collect_stable_candidate(
                &matched_path,
                &resolved_path,
                &resolved_root,
                resolved_excludes,
                checkpoint_namespace,
            ) {
                Ok(Some(candidate)) => candidate,
                Ok(None) => continue,
                Err(issue) => {
                    admission.record_issue(issue)?;
                    continue;
                }
            };
            admission.observe(generation, candidate, self.plan.ignore_older_than)?;
        }
        Ok(())
    }

    fn collect_stable_candidate(
        &self,
        matched_path: &Path,
        resolved_path: &Path,
        resolved_root: &Path,
        resolved_excludes: &[ResolvedExclude],
        checkpoint_namespace: Option<&Path>,
    ) -> Result<Option<DiscoveredCandidate>, DiscoveryIssue> {
        let first = open_candidate_at(
            resolved_path,
            matched_path,
            false,
            self.plan.fingerprint_bytes,
            self.plan.ignored_header_bytes,
        )?;
        let first_evidence = first.evidence;
        drop(first.file);
        let resolved_again =
            std::fs::canonicalize(matched_path).map_err(|source| DiscoveryIssue::Io {
                operation: "revalidate resolved candidate",
                path: matched_path.to_path_buf(),
                source,
            })?;
        validate_candidate_path_stability(
            matched_path,
            resolved_path,
            &resolved_again,
            resolved_root,
            self.plan.follow_symlinks,
        )?;
        if self.path_is_excluded(
            matched_path,
            &resolved_again,
            resolved_excludes,
            checkpoint_namespace,
        ) {
            return Ok(None);
        }
        let second = open_candidate_at(
            &resolved_again,
            matched_path,
            false,
            self.plan.fingerprint_bytes,
            self.plan.ignored_header_bytes,
        )?;
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
            let metadata = second
                .file
                .metadata()
                .map_err(|source| DiscoveryIssue::Io {
                    operation: "read candidate metadata",
                    path: matched_path.to_path_buf(),
                    source,
                })?;
            Some(metadata.modified().map_err(|source| DiscoveryIssue::Io {
                operation: "read candidate modification time",
                path: matched_path.to_path_buf(),
                source,
            })?)
        };
        Ok(Some(DiscoveredCandidate {
            matched_path: matched_path.to_path_buf(),
            resolved_path: resolved_again,
            evidence: second.evidence,
            modified,
        }))
    }

    fn path_is_excluded(
        &self,
        matched_path: &Path,
        resolved_path: &Path,
        resolved_excludes: &[ResolvedExclude],
        checkpoint_namespace: Option<&Path>,
    ) -> bool {
        resolved_excludes.iter().any(|exclude| {
            exclude.matcher.is_match(matched_path)
                || exclude.matcher.is_match(resolved_path)
                || exclude.resolved_root.as_ref().is_some_and(|root| {
                    resolved_path.strip_prefix(root).is_ok_and(|relative| {
                        exclude
                            .matcher
                            .is_match(join_root(&exclude.lexical_root, relative))
                    })
                })
        }) || checkpoint_namespace.is_some_and(|namespace| resolved_path.starts_with(namespace))
            || matched_path.starts_with(&self.plan.checkpoint_namespace_dir)
    }

    fn resolve_excludes(
        &self,
        admission: &mut AdmissionController,
    ) -> Result<Option<Vec<ResolvedExclude>>, DiscoveryError> {
        let mut resolved = Vec::with_capacity(self.plan.exclude_patterns.len());
        for exclude in &self.plan.exclude_patterns {
            self.ensure_running()?;
            let resolved_root = match canonicalize_optional(&exclude.lexical_root) {
                Ok(path) => path,
                Err(source) => {
                    admission.record_issue(DiscoveryIssue::Io {
                        operation: "resolve exclude root",
                        path: exclude.lexical_root.clone(),
                        source,
                    })?;
                    return Ok(None);
                }
            };
            resolved.push(ResolvedExclude {
                lexical_root: exclude.lexical_root.clone(),
                resolved_root,
                matcher: exclude.matcher.clone(),
            });
        }
        Ok(Some(resolved))
    }

    fn ensure_running(&self) -> Result<(), DiscoveryError> {
        if self.shutdown_requested.load(Ordering::Relaxed) {
            Err(DiscoveryError::ShutdownRequested)
        } else {
            Ok(())
        }
    }
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
