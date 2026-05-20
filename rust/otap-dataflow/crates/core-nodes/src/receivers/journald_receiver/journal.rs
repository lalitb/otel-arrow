// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Linux `sd-journal` reader abstraction.

#[cfg(target_os = "linux")]
mod imp {
    #![allow(unsafe_code)]

    use crate::receivers::journald_receiver::arrow_records_encoder::{JournalEntry, JournalField};
    use crate::receivers::journald_receiver::config::{RuntimeConfig, StartAt};

    use libc::{RTLD_NOW, c_char, c_int, c_void, size_t};
    use std::ffi::{CStr, CString};
    use std::path::Path;
    use std::ptr::NonNull;
    use std::time::Duration;

    const SD_JOURNAL_LOCAL_ONLY: c_int = 1;
    const SD_JOURNAL_OS_ROOT: c_int = 1 << 4;

    type SdJournal = c_void;
    type OpenFn = unsafe extern "C" fn(*mut *mut SdJournal, c_int) -> c_int;
    type OpenDirectoryFn = unsafe extern "C" fn(*mut *mut SdJournal, *const c_char, c_int) -> c_int;
    type CloseFn = unsafe extern "C" fn(*mut SdJournal);
    type NextFn = unsafe extern "C" fn(*mut SdJournal) -> c_int;
    type WaitFn = unsafe extern "C" fn(*mut SdJournal, u64) -> c_int;
    type SeekHeadFn = unsafe extern "C" fn(*mut SdJournal) -> c_int;
    type SeekTailFn = unsafe extern "C" fn(*mut SdJournal) -> c_int;
    type SeekCursorFn = unsafe extern "C" fn(*mut SdJournal, *const c_char) -> c_int;
    type TestCursorFn = unsafe extern "C" fn(*mut SdJournal, *const c_char) -> c_int;
    type PreviousFn = unsafe extern "C" fn(*mut SdJournal) -> c_int;
    type GetCursorFn = unsafe extern "C" fn(*mut SdJournal, *mut *mut c_char) -> c_int;
    type GetRealtimeUsecFn = unsafe extern "C" fn(*mut SdJournal, *mut u64) -> c_int;
    type RestartDataFn = unsafe extern "C" fn(*mut SdJournal);
    type EnumerateDataFn =
        unsafe extern "C" fn(*mut SdJournal, *mut *const c_void, *mut size_t) -> c_int;
    type AddMatchFn = unsafe extern "C" fn(*mut SdJournal, *const c_void, size_t) -> c_int;
    type AddDisjunctionFn = unsafe extern "C" fn(*mut SdJournal) -> c_int;
    type AddConjunctionFn = unsafe extern "C" fn(*mut SdJournal) -> c_int;

    struct LibSystemd {
        _handle: NonNull<c_void>,
        open: OpenFn,
        open_directory: OpenDirectoryFn,
        close: CloseFn,
        next: NextFn,
        wait: WaitFn,
        seek_head: SeekHeadFn,
        seek_tail: SeekTailFn,
        seek_cursor: SeekCursorFn,
        test_cursor: TestCursorFn,
        previous: PreviousFn,
        get_cursor: GetCursorFn,
        get_realtime_usec: GetRealtimeUsecFn,
        restart_data: RestartDataFn,
        enumerate_data: EnumerateDataFn,
        add_match: AddMatchFn,
        add_disjunction: AddDisjunctionFn,
        add_conjunction: AddConjunctionFn,
    }

    // Function pointers are immutable after load and libsystemd is process-global.
    unsafe impl Send for LibSystemd {}
    unsafe impl Sync for LibSystemd {}

    impl LibSystemd {
        fn load() -> Result<&'static Self, String> {
            static LIB: std::sync::OnceLock<Result<LibSystemd, String>> =
                std::sync::OnceLock::new();
            LIB.get_or_init(Self::load_inner)
                .as_ref()
                .map_err(Clone::clone)
        }

        fn load_inner() -> Result<Self, String> {
            let name = CString::new("libsystemd.so.0").expect("static string");
            let handle = unsafe { libc::dlopen(name.as_ptr(), RTLD_NOW) };
            let handle =
                NonNull::new(handle).ok_or_else(|| "failed to load libsystemd.so.0".to_owned())?;

            macro_rules! sym {
                ($name:literal, $ty:ty) => {{
                    let cname = CString::new($name).expect("static string");
                    let ptr = unsafe { libc::dlsym(handle.as_ptr(), cname.as_ptr()) };
                    if ptr.is_null() {
                        return Err(format!("missing libsystemd symbol {}", $name));
                    }
                    unsafe { std::mem::transmute::<*mut c_void, $ty>(ptr) }
                }};
            }

            Ok(Self {
                _handle: handle,
                open: sym!("sd_journal_open", OpenFn),
                open_directory: sym!("sd_journal_open_directory", OpenDirectoryFn),
                close: sym!("sd_journal_close", CloseFn),
                next: sym!("sd_journal_next", NextFn),
                wait: sym!("sd_journal_wait", WaitFn),
                seek_head: sym!("sd_journal_seek_head", SeekHeadFn),
                seek_tail: sym!("sd_journal_seek_tail", SeekTailFn),
                seek_cursor: sym!("sd_journal_seek_cursor", SeekCursorFn),
                test_cursor: sym!("sd_journal_test_cursor", TestCursorFn),
                previous: sym!("sd_journal_previous", PreviousFn),
                get_cursor: sym!("sd_journal_get_cursor", GetCursorFn),
                get_realtime_usec: sym!("sd_journal_get_realtime_usec", GetRealtimeUsecFn),
                restart_data: sym!("sd_journal_restart_data", RestartDataFn),
                enumerate_data: sym!("sd_journal_enumerate_data", EnumerateDataFn),
                add_match: sym!("sd_journal_add_match", AddMatchFn),
                add_disjunction: sym!("sd_journal_add_disjunction", AddDisjunctionFn),
                add_conjunction: sym!("sd_journal_add_conjunction", AddConjunctionFn),
            })
        }
    }

    pub(crate) struct SdJournalReader {
        lib: &'static LibSystemd,
        journal: NonNull<SdJournal>,
        wait_timeout: Duration,
        max_entry_bytes: usize,
        max_field_bytes: usize,
        max_fields_per_entry: usize,
    }

    pub(crate) trait JournalSource {
        fn next_entry(&mut self) -> Result<Option<JournalEntry>, String>;
    }

    impl SdJournalReader {
        pub(crate) fn open(
            config: &RuntimeConfig,
            checkpoint: Option<&str>,
        ) -> Result<Self, String> {
            let lib = LibSystemd::load()?;
            let mut raw = std::ptr::null_mut();
            if config.journal.root_path == Path::new("/") {
                check(
                    unsafe { (lib.open)(&mut raw, SD_JOURNAL_LOCAL_ONLY) },
                    "sd_journal_open",
                )?;
            } else {
                let root = path_to_cstring(&config.journal.root_path)?;
                check(
                    unsafe {
                        (lib.open_directory)(
                            &mut raw,
                            root.as_ptr(),
                            SD_JOURNAL_LOCAL_ONLY | SD_JOURNAL_OS_ROOT,
                        )
                    },
                    "sd_journal_open_directory",
                )?;
            }
            let journal =
                NonNull::new(raw).ok_or_else(|| "sd_journal_open returned null".to_owned())?;
            let mut reader = Self {
                lib,
                journal,
                wait_timeout: config.wait_timeout,
                max_entry_bytes: config.extraction.max_entry_bytes,
                max_field_bytes: config.extraction.max_field_bytes,
                max_fields_per_entry: config.extraction.max_fields_per_entry,
            };
            if let Err(err) = reader.configure(config, checkpoint) {
                unsafe { (reader.lib.close)(reader.journal.as_ptr()) };
                return Err(err);
            }
            Ok(reader)
        }

        fn configure(
            &mut self,
            config: &RuntimeConfig,
            checkpoint: Option<&str>,
        ) -> Result<(), String> {
            let mut has_match_group = false;
            has_match_group |=
                self.add_match_group("_SYSTEMD_UNIT", config.units.iter().map(String::as_str))?;
            if has_match_group && !config.identifiers.is_empty() {
                self.add_conjunction()?;
            }
            has_match_group |= self.add_match_group(
                "SYSLOG_IDENTIFIER",
                config.identifiers.iter().map(String::as_str),
            )?;
            if has_match_group {
                self.add_conjunction()?;
            }
            let _ = self.add_match_group(
                "PRIORITY",
                config
                    .priorities
                    .iter()
                    .map(|priority| priority.to_string()),
            )?;

            if let Some(cursor) = checkpoint {
                self.seek_after_committed_cursor(cursor)?;
                return Ok(());
            }

            match config.start_at {
                StartAt::Beginning => check(
                    unsafe { (self.lib.seek_head)(self.journal.as_ptr()) },
                    "sd_journal_seek_head",
                ),
                StartAt::End => {
                    check(
                        unsafe { (self.lib.seek_tail)(self.journal.as_ptr()) },
                        "sd_journal_seek_tail",
                    )?;
                    let previous = unsafe { (self.lib.previous)(self.journal.as_ptr()) };
                    if previous < 0 {
                        Err(format!("sd_journal_previous failed with {previous}"))
                    } else {
                        Ok(())
                    }
                }
            }
        }

        fn seek_after_committed_cursor(&mut self, cursor: &str) -> Result<(), String> {
            let c =
                CString::new(cursor).map_err(|_| "checkpoint cursor contains NUL".to_owned())?;
            check(
                unsafe { (self.lib.seek_cursor)(self.journal.as_ptr(), c.as_ptr()) },
                "sd_journal_seek_cursor",
            )?;
            let next = unsafe { (self.lib.next)(self.journal.as_ptr()) };
            if next < 0 {
                return Err(format!("sd_journal_next failed with {next}"));
            }
            if next == 0 {
                return Err("checkpoint cursor is invalid or stale".to_owned());
            }
            let matches = unsafe { (self.lib.test_cursor)(self.journal.as_ptr(), c.as_ptr()) };
            if matches < 0 {
                return Err(format!("sd_journal_test_cursor failed with {matches}"));
            }
            if matches == 0 {
                return Err("checkpoint cursor is invalid or stale".to_owned());
            }
            Ok(())
        }

        fn add_match_group<I, V>(&mut self, field: &str, values: I) -> Result<bool, String>
        where
            I: IntoIterator<Item = V>,
            V: AsRef<str>,
        {
            let mut added = false;
            for value in values {
                if added {
                    check(
                        unsafe { (self.lib.add_disjunction)(self.journal.as_ptr()) },
                        "sd_journal_add_disjunction",
                    )?;
                }
                self.add_match(field, value.as_ref())?;
                added = true;
            }
            Ok(added)
        }

        fn add_conjunction(&mut self) -> Result<(), String> {
            check(
                unsafe { (self.lib.add_conjunction)(self.journal.as_ptr()) },
                "sd_journal_add_conjunction",
            )
        }

        fn add_match(&mut self, field: &str, value: &str) -> Result<(), String> {
            let matcher = format!("{field}={value}");
            check(
                unsafe {
                    (self.lib.add_match)(
                        self.journal.as_ptr(),
                        matcher.as_ptr().cast(),
                        matcher.len(),
                    )
                },
                "sd_journal_add_match",
            )
        }

        fn read_next_entry(&mut self) -> Result<Option<JournalEntry>, String> {
            loop {
                let next = unsafe { (self.lib.next)(self.journal.as_ptr()) };
                if next < 0 {
                    return Err(format!("sd_journal_next failed with {next}"));
                }
                if next > 0 {
                    return self.current_entry().map(Some);
                }
                let timeout = self.wait_timeout.as_micros().min(u64::MAX as u128) as u64;
                let waited = unsafe { (self.lib.wait)(self.journal.as_ptr(), timeout) };
                if waited < 0 {
                    return Err(format!("sd_journal_wait failed with {waited}"));
                }
                if waited == 0 {
                    return Ok(None);
                }
            }
        }

        fn current_entry(&mut self) -> Result<JournalEntry, String> {
            let mut cursor_ptr: *mut c_char = std::ptr::null_mut();
            check(
                unsafe { (self.lib.get_cursor)(self.journal.as_ptr(), &mut cursor_ptr) },
                "sd_journal_get_cursor",
            )?;
            let cursor = unsafe { CStr::from_ptr(cursor_ptr) }
                .to_string_lossy()
                .into_owned();
            unsafe { libc::free(cursor_ptr.cast()) };

            let mut realtime_usec = 0u64;
            check(
                unsafe { (self.lib.get_realtime_usec)(self.journal.as_ptr(), &mut realtime_usec) },
                "sd_journal_get_realtime_usec",
            )?;

            unsafe { (self.lib.restart_data)(self.journal.as_ptr()) };
            let mut fields = Vec::new();
            let mut entry_bytes = 0usize;
            let mut dropped_fields = 0usize;
            let mut dropped_large_fields = 0usize;
            loop {
                let mut data: *const c_void = std::ptr::null();
                let mut len: size_t = 0;
                let rc = unsafe {
                    (self.lib.enumerate_data)(self.journal.as_ptr(), &mut data, &mut len)
                };
                if rc < 0 {
                    return Err(format!("sd_journal_enumerate_data failed with {rc}"));
                }
                if rc == 0 {
                    break;
                }
                if fields.len() >= self.max_fields_per_entry {
                    dropped_fields = dropped_fields.saturating_add(1);
                    continue;
                }
                let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
                if let Some(eq) = bytes.iter().position(|b| *b == b'=') {
                    let value_len = bytes.len().saturating_sub(eq + 1);
                    if value_len > self.max_field_bytes
                        || entry_bytes.saturating_add(value_len) > self.max_entry_bytes
                    {
                        dropped_fields = dropped_fields.saturating_add(1);
                        dropped_large_fields = dropped_large_fields.saturating_add(1);
                        continue;
                    }
                    let name = String::from_utf8_lossy(&bytes[..eq]).into_owned();
                    let value = bytes[eq + 1..].to_vec();
                    entry_bytes = entry_bytes.saturating_add(value.len());
                    fields.push(JournalField { name, value });
                }
            }

            Ok(JournalEntry {
                cursor,
                realtime_unix_nano: realtime_usec.saturating_mul(1000),
                fields,
                dropped_fields,
                dropped_large_fields,
            })
        }
    }

    impl JournalSource for SdJournalReader {
        fn next_entry(&mut self) -> Result<Option<JournalEntry>, String> {
            self.read_next_entry()
        }
    }

    impl Drop for SdJournalReader {
        fn drop(&mut self) {
            unsafe { (self.lib.close)(self.journal.as_ptr()) };
        }
    }

    fn check(rc: c_int, name: &str) -> Result<(), String> {
        if rc < 0 {
            Err(format!("{name} failed with {rc}"))
        } else {
            Ok(())
        }
    }

    fn path_to_cstring(path: &Path) -> Result<CString, String> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            CString::new(path.as_os_str().as_bytes())
                .map_err(|_| format!("journal.root_path contains NUL: {}", path.display()))
        }
        #[cfg(not(unix))]
        {
            CString::new(path.to_string_lossy().as_bytes())
                .map_err(|_| format!("journal.root_path contains NUL: {}", path.display()))
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use imp::{JournalSource, SdJournalReader};
