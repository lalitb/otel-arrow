// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded classification and retry timing for source-environment failures.

use std::fmt;
use std::io;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const MAX_EXPONENT: u32 = 7;
const MAX_TRACKED_FAILURES: u8 = 8;

/// Stable operation classes for bounded health events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnvironmentalOperation {
    Open,
    Read,
    Inspect,
    Traverse,
    Probe,
}

impl EnvironmentalOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Read => "read",
            Self::Inspect => "inspect",
            Self::Traverse => "traverse",
            Self::Probe => "probe",
        }
    }
}

impl fmt::Display for EnvironmentalOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable source-environment error classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnvironmentalErrorClass {
    DescriptorPressure,
    Permission,
    WouldBlock,
    NoSpace,
    Other,
}

impl EnvironmentalErrorClass {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::DescriptorPressure => "descriptor_pressure",
            Self::Permission => "permission",
            Self::WouldBlock => "would_block",
            Self::NoSpace => "no_space",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for EnvironmentalErrorClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One bounded exponential-backoff state entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EnvironmentalBackoff {
    failures: u8,
    retry_at: Instant,
}

/// One receiver-global descriptor-pressure state shared by discovery and
/// source-reader threads. The mutex is touched only on bounded descriptor
/// admission and retry-scheduling paths, never by an ordinary read from an
/// already-open source.
#[derive(Debug, Default)]
pub(crate) struct DescriptorPressure {
    state: Mutex<Option<EnvironmentalBackoff>>,
}

/// Shared descriptor-pressure state failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DescriptorPressureError {
    #[error("filelog descriptor-pressure state is poisoned")]
    Poisoned,
    #[error("filelog descriptor-pressure retry deadline overflowed")]
    DeadlineOverflow,
}

impl DescriptorPressure {
    /// Records one receiver-global descriptor failure and returns its checked
    /// retry deadline.
    pub(crate) fn record_failure(&self, now: Instant) -> Result<Instant, DescriptorPressureError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DescriptorPressureError::Poisoned)?;
        let next = EnvironmentalBackoff::after_failure(*state, now)
            .ok_or(DescriptorPressureError::DeadlineOverflow)?;
        *state = Some(next);
        Ok(next.retry_at())
    }

    /// Returns the future global retry deadline, if descriptor opens remain
    /// paused.
    pub(crate) fn retry_at(
        &self,
        now: Instant,
    ) -> Result<Option<Instant>, DescriptorPressureError> {
        let state = self
            .state
            .lock()
            .map_err(|_| DescriptorPressureError::Poisoned)?;
        Ok((*state)
            .filter(|state| state.retry_at() > now)
            .map(EnvironmentalBackoff::retry_at))
    }

    /// Current state regardless of whether its deadline is past.
    pub(crate) fn current(&self) -> Result<Option<EnvironmentalBackoff>, DescriptorPressureError> {
        self.state
            .lock()
            .map(|state| *state)
            .map_err(|_| DescriptorPressureError::Poisoned)
    }

    /// Clears an expired global failure state after one descriptor open
    /// succeeds. A still-active state belongs to a concurrent, later failure
    /// and must not be erased by this success.
    pub(crate) fn clear_after_success(
        &self,
        now: Instant,
    ) -> Result<bool, DescriptorPressureError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DescriptorPressureError::Poisoned)?;
        if state.is_some_and(|state| state.retry_at() <= now) {
            Ok(state.take().is_some())
        } else {
            Ok(false)
        }
    }

    /// Clears receiver-global state during terminal shutdown.
    pub(crate) fn reset(&self) -> Result<bool, DescriptorPressureError> {
        self.state
            .lock()
            .map(|mut state| state.take().is_some())
            .map_err(|_| DescriptorPressureError::Poisoned)
    }

    #[cfg(test)]
    pub(crate) fn state_for_test(
        &self,
    ) -> Result<Option<EnvironmentalBackoff>, DescriptorPressureError> {
        self.current()
    }
}

impl EnvironmentalBackoff {
    /// Advances one failure count and computes its checked retry deadline.
    pub(crate) fn after_failure(previous: Option<Self>, now: Instant) -> Option<Self> {
        let failures = match previous {
            None => 1,
            Some(state) => state.failures.checked_add(1)?.min(MAX_TRACKED_FAILURES),
        };
        let retry_at = now.checked_add(environmental_delay(failures)?)?;
        Some(Self { failures, retry_at })
    }

    pub(crate) const fn failures(self) -> u8 {
        self.failures
    }

    pub(crate) const fn retry_at(self) -> Instant {
        self.retry_at
    }
}

/// Exact environmental retry delay:
/// `min(250ms * 2^(min(failure_count - 1, 7)), 30s)`.
pub(crate) fn environmental_delay(failure_count: u8) -> Option<Duration> {
    let exponent = u32::from(failure_count.checked_sub(1)?).min(MAX_EXPONENT);
    let scaled = INITIAL_BACKOFF.checked_mul(1u32 << exponent)?;
    Some(scaled.min(MAX_BACKOFF))
}

/// Classifies one source-side operating-system error into a bounded category.
pub(crate) fn classify_io_error(error: &io::Error) -> EnvironmentalErrorClass {
    if is_descriptor_pressure(error) {
        return EnvironmentalErrorClass::DescriptorPressure;
    }
    if is_no_space(error) {
        return EnvironmentalErrorClass::NoSpace;
    }
    if is_temporary_permission(error) {
        return EnvironmentalErrorClass::Permission;
    }
    match error.kind() {
        io::ErrorKind::PermissionDenied => EnvironmentalErrorClass::Permission,
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {
            EnvironmentalErrorClass::WouldBlock
        }
        _ => EnvironmentalErrorClass::Other,
    }
}

#[cfg(windows)]
fn is_temporary_permission(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32
                || code == windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32
                || code == windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32
    )
}

#[cfg(not(windows))]
fn is_temporary_permission(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
}

#[cfg(unix)]
fn is_descriptor_pressure(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

#[cfg(windows)]
fn is_descriptor_pressure(error: &io::Error) -> bool {
    error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_TOO_MANY_OPEN_FILES as i32)
}

#[cfg(not(any(unix, windows)))]
fn is_descriptor_pressure(_error: &io::Error) -> bool {
    false
}

#[cfg(unix)]
fn is_no_space(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOSPC)
}

#[cfg(windows)]
fn is_no_space(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == windows_sys::Win32::Foundation::ERROR_DISK_FULL as i32
                || code == windows_sys::Win32::Foundation::ERROR_HANDLE_DISK_FULL as i32
    )
}

#[cfg(not(any(unix, windows)))]
fn is_no_space(_error: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: failure counts advance from one through and beyond the
    /// exponent cap.
    /// Guarantees: delays follow 250ms exponential growth, clamp at 30s, and
    /// never wrap.
    #[test]
    fn environmental_delay_matches_normative_sequence() {
        let expected = [
            Duration::from_millis(250),
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ];
        for (index, expected) in expected.into_iter().enumerate() {
            assert_eq!(environmental_delay(index as u8 + 1), Some(expected));
        }
        assert_eq!(environmental_delay(u8::MAX), Some(MAX_BACKOFF));
        assert_eq!(environmental_delay(0), None);
    }

    /// Scenario: one environmental state repeatedly fails at a representable
    /// clock value.
    /// Guarantees: failure count and retry deadline advance monotonically
    /// using the normative delay.
    #[test]
    fn environmental_state_advances_monotonically() {
        let now = Instant::now();
        let first = EnvironmentalBackoff::after_failure(None, now).unwrap();
        assert_eq!(first.failures(), 1);
        assert_eq!(
            first.retry_at().duration_since(now),
            Duration::from_millis(250)
        );
        let second = EnvironmentalBackoff::after_failure(Some(first), now).unwrap();
        assert_eq!(second.failures(), 2);
        assert_eq!(
            second.retry_at().duration_since(now),
            Duration::from_millis(500)
        );
        let mut state = second;
        for _ in 0..20 {
            state = EnvironmentalBackoff::after_failure(Some(state), now).unwrap();
        }
        assert_eq!(state.failures(), MAX_TRACKED_FAILURES);
        assert_eq!(state.retry_at().duration_since(now), MAX_BACKOFF);
    }

    /// Scenario: discovery and reader callers record failures through one
    /// shared descriptor-pressure object, then opens succeed before and after
    /// the retry deadline.
    /// Guarantees: both failures advance the same counter/deadline, an
    /// in-flight success cannot erase active pressure, and a post-deadline
    /// success removes the sole receiver-global state.
    #[test]
    fn descriptor_pressure_state_is_shared_and_clearable() {
        let pressure = DescriptorPressure::default();
        let now = Instant::now();
        let first = pressure.record_failure(now).unwrap();
        assert_eq!(first.duration_since(now), Duration::from_millis(250));
        let second = pressure.record_failure(now).unwrap();
        assert_eq!(second.duration_since(now), Duration::from_millis(500));
        assert_eq!(
            pressure
                .state_for_test()
                .unwrap()
                .map(|state| state.failures()),
            Some(2)
        );
        assert!(!pressure.clear_after_success(now).unwrap());
        assert!(pressure.state_for_test().unwrap().is_some());
        assert!(pressure.clear_after_success(second).unwrap());
        assert_eq!(pressure.state_for_test().unwrap(), None);
        assert!(!pressure.clear_after_success(second).unwrap());
        assert!(!pressure.reset().unwrap());
    }

    /// Scenario: portable permission, would-block, interrupted, and generic
    /// I/O errors are classified without inspecting message text.
    /// Guarantees: retry policy uses stable bounded classes for every common
    /// source-environment category.
    #[test]
    fn portable_environmental_error_classes_are_stable() {
        assert_eq!(
            classify_io_error(&io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permission"
            )),
            EnvironmentalErrorClass::Permission
        );
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted] {
            assert_eq!(
                classify_io_error(&io::Error::new(kind, "retry")),
                EnvironmentalErrorClass::WouldBlock
            );
        }
        assert_eq!(
            classify_io_error(&io::Error::other("other")),
            EnvironmentalErrorClass::Other
        );
    }

    #[cfg(unix)]
    /// Scenario: Unix descriptor exhaustion and source-side capacity errors
    /// are classified from their raw OS values.
    /// Guarantees: `EMFILE`/`ENFILE` select receiver descriptor pressure and
    /// `ENOSPC` selects source no-space without string matching.
    #[test]
    fn unix_environmental_error_classes_are_stable() {
        for code in [libc::EMFILE, libc::ENFILE] {
            assert_eq!(
                classify_io_error(&io::Error::from_raw_os_error(code)),
                EnvironmentalErrorClass::DescriptorPressure
            );
        }
        assert_eq!(
            classify_io_error(&io::Error::from_raw_os_error(libc::ENOSPC)),
            EnvironmentalErrorClass::NoSpace
        );
    }

    #[cfg(windows)]
    /// Scenario: Windows descriptor exhaustion, temporary sharing failures,
    /// and source-side capacity errors are classified from Win32 codes.
    /// Guarantees: the portable retry policy selects the fixed descriptor,
    /// permission, and no-space classes without message-text matching.
    #[test]
    fn windows_environmental_error_classes_are_stable() {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL, ERROR_LOCK_VIOLATION,
            ERROR_SHARING_VIOLATION, ERROR_TOO_MANY_OPEN_FILES,
        };

        assert_eq!(
            classify_io_error(&io::Error::from_raw_os_error(
                ERROR_TOO_MANY_OPEN_FILES as i32
            )),
            EnvironmentalErrorClass::DescriptorPressure
        );
        for code in [
            ERROR_ACCESS_DENIED,
            ERROR_SHARING_VIOLATION,
            ERROR_LOCK_VIOLATION,
        ] {
            assert_eq!(
                classify_io_error(&io::Error::from_raw_os_error(code as i32)),
                EnvironmentalErrorClass::Permission
            );
        }
        for code in [ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL] {
            assert_eq!(
                classify_io_error(&io::Error::from_raw_os_error(code as i32)),
                EnvironmentalErrorClass::NoSpace
            );
        }
    }
}
