// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Process, thread, executable, and PID-generation metadata.

use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

use crate::{MappingTable, ProfilerError, Result, mappings::truncate_utf8, parse_proc_maps};

/// PID plus procfs start time, distinguishing PID reuse.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProcessIdentity {
    /// Operating-system process identifier.
    pub pid: u32,
    /// Start time in clock ticks since boot from `/proc/<pid>/stat`.
    pub start_time_ticks: u64,
}

/// Fixed diagnostics for optional metadata omitted from an otherwise usable row.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MetadataIssues {
    /// The executable link could not be read or represented as UTF-8.
    pub executable_unavailable: bool,
    /// Executable mappings were inaccessible or malformed.
    pub mappings_unavailable: bool,
    /// A byte or mapping-count limit omitted some mappings.
    pub mappings_truncated: bool,
    /// A display string exceeded its configured byte bound.
    pub strings_truncated: bool,
    /// An optional thread name was inaccessible or malformed.
    pub name_unavailable: bool,
}

/// Metadata for one process generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessMetadata {
    /// Reuse-safe identity.
    pub identity: ProcessIdentity,
    /// Process command name.
    pub name: String,
    /// Executable path when accessible.
    pub executable: Option<String>,
    /// Sorted executable mappings.
    pub mappings: MappingTable,
    /// Fixed diagnostics for omitted optional metadata.
    pub issues: MetadataIssues,
}

/// Metadata for one thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadMetadata {
    /// Owning process generation.
    pub process: ProcessIdentity,
    /// Operating-system thread identifier.
    pub tid: u32,
    /// Thread start time in clock ticks, distinguishing TID reuse.
    pub start_time_ticks: u64,
    /// Thread name when accessible.
    pub name: Option<String>,
    /// Fixed diagnostics for omitted optional metadata.
    pub issues: MetadataIssues,
}

/// Injectable procfs or test metadata boundary.
pub trait ProcessProvider: Send + Sync {
    /// Looks up one process generation.
    fn process(&self, pid: u32) -> Result<ProcessMetadata>;
    /// Reads the current identity without requiring optional process metadata.
    fn identity(&self, pid: u32) -> Result<ProcessIdentity> {
        self.process(pid).map(|metadata| metadata.identity)
    }
    /// Checks whether a BOOTTIME sample can belong to a process generation.
    /// Injected providers may use another timestamp domain and default to true.
    fn sample_matches(&self, _identity: ProcessIdentity, _timestamp_ns: u64) -> Result<bool> {
        Ok(true)
    }
    /// Looks up one thread within a known process generation.
    fn thread(&self, process: ProcessIdentity, tid: u32) -> Result<ThreadMetadata>;
    /// Translates a mapping pathname into the process mount namespace.
    fn symbol_path(&self, _pid: u32, path: &Path) -> PathBuf {
        path.to_path_buf()
    }
}

/// Bounded Linux procfs metadata provider.
#[derive(Clone, Debug)]
pub struct ProcfsProcessProvider {
    root: PathBuf,
    max_mappings: usize,
    max_file_bytes: usize,
    collect_thread_names: bool,
    collect_mappings: bool,
    max_string_bytes: usize,
}

#[derive(Debug, Eq, PartialEq)]
struct ExecutableSnapshot {
    bytes: Vec<u8>,
    file_identity: Option<(u64, u64)>,
}

impl ProcfsProcessProvider {
    /// Creates a provider rooted at a configurable procfs mount.
    #[must_use]
    pub fn new(
        root: PathBuf,
        max_mappings: usize,
        max_file_bytes: usize,
        collect_thread_names: bool,
        collect_mappings: bool,
        max_string_bytes: usize,
    ) -> Self {
        Self {
            root,
            max_mappings,
            max_file_bytes,
            collect_thread_names,
            collect_mappings,
            max_string_bytes,
        }
    }

    fn process_path(&self, pid: u32, name: &str) -> PathBuf {
        self.root.join(pid.to_string()).join(name)
    }

    /// Validates bounds without opening any procfs files.
    pub fn validate(&self) -> Result<()> {
        validate_read_limit(self.max_file_bytes)?;
        if self.max_string_bytes == 0 {
            return Err(ProfilerError::invalid(
                "max_string_bytes",
                "must be positive",
            ));
        }
        if self.collect_mappings && self.max_mappings == 0 {
            return Err(ProfilerError::invalid("max_mappings", "must be positive"));
        }
        Ok(())
    }

    fn stat(&self, path: &Path, expected_pid: u32) -> Result<(String, u64)> {
        let stat = read_bounded(path, self.max_file_bytes)?;
        let recorded_pid = stat
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u32>().ok());
        if recorded_pid != Some(expected_pid) {
            return Err(metadata_error(
                expected_pid,
                "stat PID does not match requested identity",
            ));
        }
        parse_stat(&stat).map_err(|reason| ProfilerError::ProcessMetadata {
            pid: expected_pid,
            reason,
        })
    }

    fn executable(
        &self,
        pid: u32,
        issues: &mut MetadataIssues,
    ) -> Result<Option<ExecutableSnapshot>> {
        let path = self.process_path(pid, "exe");
        let bounded = match read_link_prefix(&path, self.max_file_bytes.min(self.max_string_bytes))
        {
            Ok(bounded) => bounded,
            Err(ProfilerError::Io { .. } | ProfilerError::ProcessMetadata { .. }) => {
                issues.executable_unavailable = true;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        issues.strings_truncated |= bounded.truncated;
        if std::str::from_utf8(&bounded.bytes).is_err() {
            issues.executable_unavailable = true;
        }
        #[cfg(unix)]
        let file_identity = {
            use std::os::unix::fs::MetadataExt;
            match std::fs::metadata(path) {
                Ok(metadata) => Some((metadata.dev(), metadata.ino())),
                Err(_) => {
                    issues.executable_unavailable = true;
                    None
                }
            }
        };
        #[cfg(not(unix))]
        let file_identity = None;
        Ok(Some(ExecutableSnapshot {
            bytes: bounded.bytes,
            file_identity,
        }))
    }

    fn mappings(&self, pid: u32, issues: &mut MetadataIssues) -> Result<MappingTable> {
        if !self.collect_mappings {
            return Ok(MappingTable::default());
        }
        let result = (|| {
            let mut bounded =
                read_bounded_file(&self.process_path(pid, "maps"), self.max_file_bytes)?;
            if bounded.truncated {
                issues.mappings_truncated = true;
                // Never parse the last partial line as a complete pathname.
                let complete = bounded
                    .bytes
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |index| index + 1);
                bounded.bytes.truncate(complete);
            }
            let maps = std::str::from_utf8(&bounded.bytes)
                .map_err(|_| metadata_error(pid, "maps contains non-UTF-8 bytes"))?;
            let mut table = parse_proc_maps(maps, self.max_mappings, self.max_string_bytes)?;
            if bounded.truncated {
                table.mark_input_truncated();
            }
            issues.mappings_truncated |= table.is_truncated();
            issues.strings_truncated |= table.strings_truncated();
            Ok(table)
        })();
        match result {
            Ok(table) => Ok(table),
            Err(
                error @ (ProfilerError::Allocation(_) | ProfilerError::InvalidConfiguration { .. }),
            ) => Err(error),
            Err(ProfilerError::Io { .. } | ProfilerError::ProcessMetadata { .. }) => {
                issues.mappings_unavailable = true;
                Ok(MappingTable::default())
            }
            Err(error) => Err(error),
        }
    }

    fn process_with(&self, pid: u32, after_metadata: impl FnOnce()) -> Result<ProcessMetadata> {
        self.validate()?;
        let stat_path = self.process_path(pid, "stat");
        let before = self.stat(&stat_path, pid)?;
        let mut issues = MetadataIssues::default();
        let executable_before = self.executable(pid, &mut issues)?;
        let mut mappings = self.mappings(pid, &mut issues)?;
        after_metadata();
        let executable_after = self.executable(pid, &mut issues)?;
        let after = self.stat(&stat_path, pid)?;
        ensure_generation(pid, before.1, after.1)?;
        if before.0 != after.0
            || matches!((&executable_before, &executable_after), (Some(before), Some(after)) if before != after)
        {
            return Err(metadata_error(
                pid,
                "process changed during metadata lookup",
            ));
        }
        if executable_before.is_some() != executable_after.is_some() && self.collect_mappings {
            issues.mappings_unavailable = true;
            mappings = MappingTable::default();
        }
        let (name, start_time_ticks) = after;
        issues.strings_truncated |= name.len() > self.max_string_bytes;
        let name = truncate_utf8(&name, self.max_string_bytes);
        let executable = executable_after
            .as_ref()
            .filter(|_| !issues.executable_unavailable)
            .and_then(|executable| std::str::from_utf8(&executable.bytes).ok())
            .map(|path| {
                issues.strings_truncated |= path.len() > self.max_string_bytes;
                truncate_utf8(path, self.max_string_bytes)
            });
        Ok(ProcessMetadata {
            identity: ProcessIdentity {
                pid,
                start_time_ticks,
            },
            name,
            executable,
            mappings,
            issues,
        })
    }
}

impl ProcessProvider for ProcfsProcessProvider {
    fn process(&self, pid: u32) -> Result<ProcessMetadata> {
        self.process_with(pid, || {})
    }

    fn identity(&self, pid: u32) -> Result<ProcessIdentity> {
        self.validate()?;
        let (_, start_time_ticks) = self.stat(&self.process_path(pid, "stat"), pid)?;
        Ok(ProcessIdentity {
            pid,
            start_time_ticks,
        })
    }

    fn sample_matches(&self, identity: ProcessIdentity, timestamp_ns: u64) -> Result<bool> {
        #[cfg(target_os = "linux")]
        {
            use nix::unistd::{SysconfVar, sysconf};
            let ticks = sysconf(SysconfVar::CLK_TCK)
                .map_err(|_| metadata_error(identity.pid, "clock tick query failed"))?
                .filter(|ticks| *ticks > 0)
                .ok_or_else(|| metadata_error(identity.pid, "clock tick rate unavailable"))?;
            let ticks = u64::try_from(ticks)
                .map_err(|_| metadata_error(identity.pid, "invalid clock tick rate"))?;
            Ok(sample_at_or_after_start(
                identity.start_time_ticks,
                timestamp_ns,
                ticks,
            ))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _unused = (identity, timestamp_ns);
            Err(ProfilerError::UnsupportedOperatingSystem(
                std::env::consts::OS.to_owned(),
            ))
        }
    }

    fn thread(&self, process: ProcessIdentity, tid: u32) -> Result<ThreadMetadata> {
        self.validate()?;
        ensure_generation(
            process.pid,
            process.start_time_ticks,
            self.identity(process.pid)?.start_time_ticks,
        )?;
        let task = self
            .root
            .join(process.pid.to_string())
            .join("task")
            .join(tid.to_string());
        let before = self.stat(&task.join("stat"), tid)?;
        let mut issues = MetadataIssues::default();
        let name = if self.collect_thread_names {
            match read_bounded_file(&task.join("comm"), self.max_file_bytes) {
                Ok(bounded) => {
                    issues.strings_truncated |= bounded.truncated;
                    match std::str::from_utf8(&bounded.bytes) {
                        Ok(name) => {
                            let name = name.strip_suffix('\n').unwrap_or(name);
                            issues.strings_truncated |=
                                bounded.truncated || name.len() > self.max_string_bytes;
                            Some(truncate_utf8(name, self.max_string_bytes))
                        }
                        Err(_) => {
                            issues.name_unavailable = true;
                            None
                        }
                    }
                }
                Err(ProfilerError::Io { .. } | ProfilerError::ProcessMetadata { .. }) => {
                    issues.name_unavailable = true;
                    None
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let after = self.stat(&task.join("stat"), tid)?;
        ensure_generation(process.pid, before.1, after.1)?;
        ensure_generation(
            process.pid,
            process.start_time_ticks,
            self.identity(process.pid)?.start_time_ticks,
        )?;
        Ok(ThreadMetadata {
            process,
            tid,
            start_time_ticks: after.1,
            name,
            issues,
        })
    }

    fn symbol_path(&self, pid: u32, path: &Path) -> PathBuf {
        if !path.is_absolute() {
            return PathBuf::new();
        }
        let mut translated = self.process_path(pid, "root");
        // Normal components only prevent callers from escaping the namespace
        // prefix with an absolute path or a parent-directory component.
        for component in path.components() {
            match component {
                std::path::Component::Normal(value) => translated.push(value),
                std::path::Component::RootDir => {}
                _ => return PathBuf::new(),
            }
        }
        translated
    }
}

fn ensure_generation(pid: u32, before: u64, after: u64) -> Result<()> {
    if before != after {
        return Err(ProfilerError::StaleProcess(pid));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn sample_at_or_after_start(start_ticks: u64, timestamp_ns: u64, ticks_per_second: u64) -> bool {
    // Procfs floors start time to a tick. Compare in u128 without rounding the
    // sample into an earlier generation, even near the u64 time horizon.
    u128::from(timestamp_ns) * u128::from(ticks_per_second)
        >= u128::from(start_ticks) * 1_000_000_000
}

fn metadata_error(pid: u32, reason: &'static str) -> ProfilerError {
    ProfilerError::ProcessMetadata {
        pid,
        reason: reason.to_owned(),
    }
}

pub(crate) fn validate_read_limit(max_bytes: usize) -> Result<()> {
    if max_bytes == 0 {
        return Err(ProfilerError::invalid("max_file_bytes", "must be positive"));
    }
    let _limit = max_bytes
        .checked_add(1)
        .and_then(|limit| u64::try_from(limit).ok())
        .ok_or_else(|| ProfilerError::invalid("max_file_bytes", "read bound overflows"))?;
    Ok(())
}

pub(crate) fn open_regular_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    let _options = options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _options = options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOCTTY);
    }
    #[cfg(not(target_os = "linux"))]
    if !std::fs::metadata(path)
        .map_err(|source| ProfilerError::Io {
            operation: "inspect metadata file",
            path: path.to_path_buf(),
            source,
        })?
        .is_file()
    {
        return Err(metadata_error(0, "metadata source is not a regular file"));
    }
    let file = options.open(path).map_err(|source| ProfilerError::Io {
        operation: "open",
        path: path.to_path_buf(),
        source,
    })?;
    if !file
        .metadata()
        .map_err(|source| ProfilerError::Io {
            operation: "inspect metadata file",
            path: path.to_path_buf(),
            source,
        })?
        .is_file()
    {
        return Err(metadata_error(0, "metadata source is not a regular file"));
    }
    Ok(file)
}

pub(crate) struct BoundedFile {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn read_link_prefix(path: &Path, max_bytes: usize) -> Result<BoundedFile> {
    use nix::NixPath;

    validate_read_limit(max_bytes)?;
    let mut buffer = [0_u8; 4096];
    let requested = max_bytes.min(buffer.len() - 1) + 1;
    let count = path
        .with_nix_path(|path| {
            // SAFETY: NixPath provides a NUL-terminated pathname; the output
            // slice is writable for requested bytes and lives across the call.
            unsafe { nix::libc::readlink(path.as_ptr(), buffer.as_mut_ptr().cast(), requested) }
        })
        .map_err(|_| metadata_error(0, "invalid executable link pathname"))?;
    if count < 0 {
        return Err(ProfilerError::Io {
            operation: "read executable link",
            path: path.to_path_buf(),
            source: std::io::Error::last_os_error(),
        });
    }
    let count =
        usize::try_from(count).map_err(|_| metadata_error(0, "invalid executable link length"))?;
    let retained = count.min(max_bytes);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(retained)
        .map_err(|_| ProfilerError::Allocation("executable link"))?;
    bytes.extend_from_slice(&buffer[..retained]);
    Ok(BoundedFile {
        bytes,
        truncated: count > max_bytes || count == buffer.len(),
    })
}

#[cfg(not(target_os = "linux"))]
fn read_link_prefix(path: &Path, max_bytes: usize) -> Result<BoundedFile> {
    validate_read_limit(max_bytes)?;
    let link = std::fs::read_link(path).map_err(|source| ProfilerError::Io {
        operation: "read executable link",
        path: path.to_path_buf(),
        source,
    })?;
    let bytes = link.as_os_str().as_encoded_bytes();
    Ok(BoundedFile {
        bytes: bytes[..bytes.len().min(max_bytes)].to_vec(),
        truncated: bytes.len() > max_bytes,
    })
}

pub(crate) fn read_bounded_file(path: &Path, max_bytes: usize) -> Result<BoundedFile> {
    validate_read_limit(max_bytes)?;
    read_file_prefix(&mut open_regular_file(path)?, path, max_bytes)
}

pub(crate) fn read_file_prefix(
    file: &mut File,
    path: &Path,
    max_bytes: usize,
) -> Result<BoundedFile> {
    validate_read_limit(max_bytes)?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    while bytes.len() < max_bytes {
        let requested = buffer.len().min(max_bytes - bytes.len());
        let count = match file.read(&mut buffer[..requested]) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(ProfilerError::Io {
                    operation: "read",
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        if count == 0 {
            return Ok(BoundedFile {
                bytes,
                truncated: false,
            });
        }
        bytes
            .try_reserve_exact(count)
            .map_err(|_| ProfilerError::Allocation("bounded metadata read"))?;
        bytes.extend_from_slice(&buffer[..count]);
    }
    let truncated = loop {
        match file.read(&mut buffer[..1]) {
            Ok(count) => break count != 0,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(ProfilerError::Io {
                    operation: "read",
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    };
    Ok(BoundedFile { bytes, truncated })
}

fn read_bounded(path: &Path, max_bytes: usize) -> Result<String> {
    let bounded = read_bounded_file(path, max_bytes)?;
    if bounded.truncated {
        return Err(metadata_error(0, "required metadata exceeds read limit"));
    }
    String::from_utf8(bounded.bytes)
        .map_err(|_| metadata_error(0, "required metadata is not UTF-8"))
}

fn parse_stat(input: &str) -> std::result::Result<(String, u64), String> {
    let open = input
        .find('(')
        .ok_or_else(|| "stat comm start delimiter is missing".to_owned())?;
    let close = input
        .rfind(')')
        .ok_or_else(|| "stat comm end delimiter is missing".to_owned())?;
    if close <= open {
        return Err("stat comm delimiters are malformed".to_owned());
    }
    let name = input[open + 1..close].to_owned();
    let after = input
        .get(close + 1..)
        .filter(|after| after.starts_with(char::is_whitespace))
        .ok_or_else(|| "stat fields are missing".to_owned())?;
    let start_time = after
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| "stat start-time field is missing".to_owned())?
        .parse::<u64>()
        .map_err(|_| "stat start-time field is invalid".to_owned())?;
    Ok((name, start_time))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn directory() -> TempDir {
        tempfile::Builder::new()
            .prefix("procfs-")
            .tempdir_in(".")
            .expect("project fixture directory")
    }

    fn stat(pid: u32, name: &str, start: u64) -> String {
        format!("{pid} ({name}) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 {start} 20")
    }

    fn provider(root: &Path) -> ProcfsProcessProvider {
        ProcfsProcessProvider::new(root.to_path_buf(), 8, 4096, true, true, 128)
    }

    fn create_process(root: &Path) {
        fs::create_dir_all(root.join("42/task/43")).expect("process and task directories");
        fs::write(root.join("42/stat"), stat(42, "process", 700)).expect("process stat");
        fs::write(root.join("42/task/43/stat"), stat(43, "thread", 710)).expect("thread stat");
    }

    /// Scenario: A procfs stat command name contains spaces and parentheses.
    /// Guarantees: PID-generation parsing uses the final comm delimiter and the
    /// correct start-time field.
    #[test]
    fn parses_complex_proc_stat_name() {
        let stat = "42 (worker (one)) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 777 20";
        let (name, start) = parse_stat(stat).expect("stat should parse");
        assert_eq!(name, "worker (one)");
        assert_eq!(start, 777);
    }

    /// Scenario: A process disappears while procfs metadata is being read.
    /// Guarantees: The race surfaces as a typed error rather than a panic.
    #[test]
    fn missing_process_returns_error() {
        let directory = directory();
        let provider =
            ProcfsProcessProvider::new(directory.path().to_path_buf(), 8, 1024, true, true, 128);
        assert!(provider.process(42).is_err());
    }

    /// Scenario: A procfs file exceeds the configured bounded read size.
    /// Guarantees: Host metadata cannot force an unbounded file allocation.
    #[test]
    fn oversized_procfs_file_is_rejected() {
        let directory = directory();
        let process = directory.path().join("42");
        fs::create_dir_all(&process).expect("process directory should be created");
        fs::write(process.join("stat"), "x".repeat(128)).expect("stat should be written");
        let provider =
            ProcfsProcessProvider::new(directory.path().to_path_buf(), 8, 16, true, true, 128);
        assert!(provider.process(42).is_err());
    }

    /// Scenario: A live process has no accessible executable link or maps file.
    /// Guarantees: Required identity survives and optional omissions use fixed flags.
    #[test]
    fn optional_metadata_failures_keep_process_rows() {
        let directory = directory();
        create_process(directory.path());
        let metadata = provider(directory.path())
            .process(42)
            .expect("usable process");
        assert_eq!(metadata.identity.start_time_ticks, 700);
        assert_eq!(metadata.name, "process");
        assert!(metadata.executable.is_none());
        assert!(metadata.mappings.as_slice().is_empty());
        assert!(metadata.issues.executable_unavailable);
        assert!(metadata.issues.mappings_unavailable);
        assert!(!metadata.issues.mappings_truncated);
        assert!(!metadata.issues.strings_truncated);
    }

    /// Scenario: Maps exceed byte or cardinality limits with complete executable lines available.
    /// Guarantees: All admitted complete mappings survive and omitted input is explicitly flagged.
    #[test]
    fn bounded_maps_keep_complete_useful_subset() {
        let directory = directory();
        create_process(directory.path());
        let line = "1000-2000 r-xp 0 08:01 1 /bin/first\n";
        let input = format!("{line}2000-3000 r-xp 0 08:01 2 /bin/{}\n", "x".repeat(512));
        fs::write(directory.path().join("42/maps"), input).expect("maps");
        let mut provider = provider(directory.path());
        provider.max_file_bytes = 128;
        let metadata = provider.process(42).expect("partial mappings");
        assert_eq!(metadata.mappings.as_slice().len(), 1);
        assert_eq!(
            metadata.mappings.as_slice()[0].path.as_deref(),
            Some("/bin/first")
        );
        assert!(metadata.issues.mappings_truncated);
        assert!(metadata.mappings.is_truncated());
        assert!(!metadata.issues.mappings_unavailable);
        provider.max_file_bytes = 4096;
        provider.max_mappings = 1;
        let metadata = provider.process(42).expect("mapping-count subset");
        assert_eq!(metadata.mappings.as_slice().len(), 1);
        assert!(metadata.issues.mappings_truncated);
    }

    /// Scenario: A readable maps file contains malformed fields or non-UTF-8 path bytes.
    /// Guarantees: Missing mappings are diagnosed without dropping known process identity.
    #[test]
    fn malformed_and_non_utf8_maps_are_explicit_omissions() {
        let directory = directory();
        create_process(directory.path());
        for bytes in [
            b"1000-2000 r-xp\n".as_slice(),
            b"1000-2000 r-xp 0 08:01 1 /\xff\n",
        ] {
            fs::write(directory.path().join("42/maps"), bytes).expect("maps");
            let metadata = provider(directory.path())
                .process(42)
                .expect("identity survives");
            assert!(metadata.issues.mappings_unavailable);
            assert!(metadata.mappings.as_slice().is_empty());
        }
    }

    /// Scenario: Task identity is valid but the optional comm file disappears.
    /// Guarantees: Missing names do not drop samples, and TID generations remain available.
    #[test]
    fn missing_thread_names_preserve_generation() {
        let directory = directory();
        create_process(directory.path());
        let provider = provider(directory.path());
        let identity = provider.identity(42).expect("identity");
        let thread = provider.thread(identity, 43).expect("thread generation");
        assert_eq!(thread.start_time_ticks, 710);
        assert!(thread.name.is_none());
        assert!(thread.issues.name_unavailable);
        fs::write(directory.path().join("42/task/43/comm"), "worker\n").expect("comm");
        let thread = provider.thread(identity, 43).expect("named thread");
        assert_eq!(thread.name.as_deref(), Some("worker"));
        assert!(!thread.issues.name_unavailable);
    }

    /// Scenario: Process and thread display names exceed a small UTF-8 byte budget.
    /// Guarantees: Display strings are bounded and every shortening is observable.
    #[test]
    fn metadata_strings_are_bounded_and_reported() {
        let directory = directory();
        create_process(directory.path());
        fs::write(
            directory.path().join("42/task/43/comm"),
            "worker \u{00e9}\n",
        )
        .expect("comm");
        let mut provider = provider(directory.path());
        provider.max_string_bytes = 8;
        let identity = provider.identity(42).expect("identity");
        let thread = provider.thread(identity, 43).expect("thread");
        assert_eq!(thread.name.as_deref(), Some("worker "));
        assert!(thread.issues.strings_truncated);
        provider.max_string_bytes = 4;
        let metadata = provider.process(42).expect("process");
        assert_eq!(metadata.name, "proc");
        assert!(metadata.issues.strings_truncated);
    }

    /// Scenario: PID reuse occurs after maps are read but before the final stat check.
    /// Guarantees: A full metadata lookup never returns fields from two generations.
    #[test]
    fn full_lookup_rejects_generation_race() {
        let directory = directory();
        create_process(directory.path());
        let provider = provider(directory.path());
        let result = provider.process_with(42, || {
            fs::write(
                directory.path().join("42/stat"),
                stat(42, "replacement", 900),
            )
            .expect("replace process generation");
        });
        assert!(matches!(result, Err(ProfilerError::StaleProcess(42))));
    }

    /// Scenario: A cached process identity is used after PID reuse or a mismatched stat PID.
    /// Guarantees: Identity-only refresh and task lookup reject stale or corrupt identities.
    #[test]
    fn identity_refresh_and_thread_parent_validation() {
        let directory = directory();
        create_process(directory.path());
        let provider = provider(directory.path());
        let old = provider.identity(42).expect("old generation");
        fs::write(
            directory.path().join("42/stat"),
            stat(42, "replacement", 900),
        )
        .expect("reuse");
        assert_eq!(
            provider
                .identity(42)
                .expect("fresh generation")
                .start_time_ticks,
            900
        );
        assert!(matches!(
            provider.thread(old, 43),
            Err(ProfilerError::StaleProcess(42))
        ));
        fs::write(directory.path().join("42/stat"), stat(99, "wrong PID", 900))
            .expect("corrupt stat");
        assert!(matches!(
            provider.identity(42),
            Err(ProfilerError::ProcessMetadata { .. })
        ));
    }

    /// Scenario: A process changes executable path while its PID and start ticks stay unchanged.
    /// Guarantees: Exec races fail rather than combining old mappings with new executable names.
    #[cfg(unix)]
    #[test]
    fn executable_change_during_lookup_is_rejected() {
        use std::os::unix::fs::symlink;
        let directory = directory();
        create_process(directory.path());
        let exe = directory.path().join("42/exe");
        symlink("/bin/before", &exe).expect("exe");
        let result = provider(directory.path()).process_with(42, || {
            fs::remove_file(&exe).expect("remove previous link");
            symlink("/bin/after", &exe).expect("new executable");
        });
        assert!(matches!(result, Err(ProfilerError::ProcessMetadata { .. })));
    }

    /// Scenario: Optional executable access disappears while required identity remains stable.
    /// Guarantees: The process row survives with explicit omissions and no mismatched maps.
    #[cfg(unix)]
    #[test]
    fn executable_disappearance_preserves_required_identity() {
        use std::os::unix::fs::symlink;
        let directory = directory();
        create_process(directory.path());
        let exe = directory.path().join("42/exe");
        symlink("/bin/before", &exe).expect("exe");
        fs::write(
            directory.path().join("42/maps"),
            "1000-2000 r-xp 0 08:01 1 /before\n",
        )
        .expect("maps");
        let metadata = provider(directory.path())
            .process_with(42, || {
                fs::remove_file(&exe).expect("remove optional executable link");
            })
            .expect("required identity survives");
        assert_eq!(metadata.identity.start_time_ticks, 700);
        assert!(metadata.issues.executable_unavailable);
        assert!(metadata.issues.mappings_unavailable);
        assert!(metadata.executable.is_none());
        assert!(metadata.mappings.as_slice().is_empty());
    }

    /// Scenario: An executable link exceeds the configured byte and display limits.
    /// Guarantees: The link reader retains only a bounded prefix and reports the truncation.
    #[cfg(unix)]
    #[test]
    fn executable_links_have_bounded_reads() {
        use std::os::unix::fs::symlink;
        let directory = directory();
        let path = directory.path().join("exe");
        symlink("/long/executable/path", &path).expect("link fixture");
        let bounded = read_link_prefix(&path, 5).expect("bounded executable read");
        assert_eq!(bounded.bytes, b"/long");
        assert!(bounded.truncated);
        assert!(bounded.bytes.capacity() <= 5);
    }

    /// Scenario: An executable file is replaced without changing its pathname or process ticks.
    /// Guarantees: Device and inode checks reject mixed executable and mapping snapshots.
    #[cfg(unix)]
    #[test]
    fn executable_replacement_at_same_path_is_rejected() {
        use std::os::unix::fs::symlink;
        let directory = directory();
        create_process(directory.path());
        let target = directory.path().join("binary");
        let replacement = directory.path().join("replacement");
        fs::write(&target, b"original").expect("original");
        fs::write(&replacement, b"replacement").expect("replacement");
        let target_absolute = fs::canonicalize(&target).expect("absolute target");
        symlink(&target_absolute, directory.path().join("42/exe")).expect("exe");
        let result = provider(directory.path()).process_with(42, || {
            fs::rename(&replacement, &target).expect("replace executable");
        });
        assert!(matches!(result, Err(ProfilerError::ProcessMetadata { .. })));
    }

    /// Scenario: BOOTTIME samples straddle a process start and clock rates differ from 100 Hz.
    /// Guarantees: Generation checks use the actual tick rate without arithmetic overflow.
    #[test]
    fn sample_generation_comparison_uses_clock_rate() {
        assert!(!sample_at_or_after_start(250, 999_999_999, 250));
        assert!(sample_at_or_after_start(250, 1_000_000_000, 250));
        assert!(sample_at_or_after_start(100, u64::MAX, 1000));
        assert!(!sample_at_or_after_start(u64::MAX, u64::MAX, 1000));
        assert!(sample_at_or_after_start(u64::MAX, u64::MAX, 1_000_000_000));
    }

    /// Scenario: Procfs BOOTTIME validation is used with the host's clock tick configuration.
    /// Guarantees: Samples immediately before and at one second obey the sysconf tick rate.
    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_sample_validation_queries_actual_clock_ticks() {
        use nix::unistd::{SysconfVar, sysconf};
        let ticks = sysconf(SysconfVar::CLK_TCK)
            .expect("query CLK_TCK")
            .expect("CLK_TCK");
        let identity = ProcessIdentity {
            pid: 42,
            start_time_ticks: ticks as u64,
        };
        let provider = provider(Path::new("/proc"));
        assert!(
            !provider
                .sample_matches(identity, 999_999_999)
                .expect("sample comparison")
        );
        assert!(
            provider
                .sample_matches(identity, 1_000_000_000)
                .expect("sample comparison")
        );
    }

    /// Scenario: A configurable procfs root and an absolute mapping path are used for symbolization.
    /// Guarantees: Objects are opened through the process mount namespace without prefix escape.
    #[test]
    fn symbol_paths_use_process_root() {
        let provider = provider(Path::new("/fixture/proc"));
        assert_eq!(
            provider.symbol_path(42, Path::new("/usr/lib/work.so")),
            Path::new("/fixture/proc/42/root/usr/lib/work.so")
        );
        assert!(
            provider
                .symbol_path(42, Path::new("/../other"))
                .as_os_str()
                .is_empty()
        );
        assert!(
            provider
                .symbol_path(42, Path::new("relative"))
                .as_os_str()
                .is_empty()
        );
    }

    /// Scenario: Read bounds are exact, exhausted, overflowing, or applied to a directory.
    /// Guarantees: Reads store at most their byte limit and reject non-regular sources.
    #[test]
    fn bounded_reader_enforces_exact_limits_and_file_type() {
        let directory = directory();
        let path = directory.path().join("bytes");
        fs::write(&path, b"12345678").expect("fixture");
        let exact = read_bounded_file(&path, 8).expect("exact read");
        assert_eq!(exact.bytes, b"12345678");
        assert!(!exact.truncated);
        let partial = read_bounded_file(&path, 4).expect("partial read");
        assert_eq!(partial.bytes, b"1234");
        assert!(partial.truncated);
        assert!(partial.bytes.capacity() <= 4);
        assert!(read_bounded_file(&path, usize::MAX).is_err());
        assert!(read_bounded_file(&path, 0).is_err());
        assert!(read_bounded_file(directory.path(), 8).is_err());
    }

    /// Scenario: A fake procfs stat source is a FIFO without a writer.
    /// Guarantees: Linux opens nonblocking and rejects it instead of hanging a metadata worker.
    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_metadata_is_rejected_without_waiting_for_writer() {
        let directory = directory();
        let path = directory.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        assert!(read_bounded_file(&path, 128).is_err());
    }

    /// Scenario: Provider bounds are zero or overflow the extra-byte read check.
    /// Guarantees: Invalid configuration fails before procfs access.
    #[test]
    fn provider_validates_bounds_on_access() {
        let mut provider = provider(Path::new("/not/a/procfs/root"));
        provider.max_file_bytes = usize::MAX;
        assert!(matches!(
            provider.identity(42),
            Err(ProfilerError::InvalidConfiguration { .. })
        ));
        provider.max_file_bytes = 128;
        provider.max_string_bytes = 0;
        assert!(provider.validate().is_err());
        provider.max_string_bytes = 128;
        provider.max_mappings = 0;
        assert!(provider.validate().is_err());
    }
}
