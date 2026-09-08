// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded reads for procfs and external artifacts, including growing files.

use std::io::Read;
use std::path::Path;

use crate::error::BackendError;

pub(crate) fn read_file(path: &Path, maximum: usize) -> Result<Vec<u8>, BackendError> {
    let file = std::fs::File::open(path).map_err(|source| BackendError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    read_limited(file, path, maximum)
}

pub(crate) fn read_limited(
    mut reader: impl Read,
    path: &Path,
    maximum: usize,
) -> Result<Vec<u8>, BackendError> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        // Probe at most one byte beyond the limit. procfs reports st_size = 0,
        // and regular files can grow after a metadata check.
        let request = chunk
            .len()
            .min(maximum.saturating_sub(bytes.len()).saturating_add(1));
        let count = match reader.read(&mut chunk[..request]) {
            Ok(count) => count,
            Err(source) => {
                return Err(BackendError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        if count > request {
            return Err(BackendError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other("reader returned more bytes than requested"),
            });
        }
        if count == 0 {
            return Ok(bytes);
        }
        if count > maximum.saturating_sub(bytes.len()) {
            return Err(BackendError::InputTooLarge {
                path: path.to_path_buf(),
                maximum,
            });
        }
        let needed = bytes.len() + count;
        if needed > bytes.capacity() {
            let capacity = needed.max(bytes.capacity().saturating_mul(2)).min(maximum);
            bytes.reserve_exact(capacity - bytes.len());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

pub(crate) fn read_prefix(reader: &mut impl Read, output: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < output.len() {
        let count = reader.read(&mut output[filled..])?;
        if count > output.len() - filled {
            return Err(std::io::Error::other(
                "reader returned more bytes than requested",
            ));
        }
        if count == 0 {
            break;
        }
        filled += count;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Scenario: A reader is repeatedly interrupted before producing any bytes.
    /// Guarantees: I/O returns the interruption to the caller's bounded retry policy after one attempt.
    #[test]
    fn interrupted_reads_are_not_retried_internally() {
        struct Interrupted(usize);
        impl Read for Interrupted {
            fn read(&mut self, _output: &mut [u8]) -> std::io::Result<usize> {
                self.0 += 1;
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
            }
        }
        let mut reader = Interrupted(0);
        assert!(matches!(
            read_limited(&mut reader, Path::new("/input"), 4096),
            Err(BackendError::Io { source, .. }) if source.kind() == std::io::ErrorKind::Interrupted,
        ));
        assert_eq!(reader.0, 1);
        assert!(read_prefix(&mut reader, &mut [0; 8]).is_err());
        assert_eq!(reader.0, 2);
    }

    /// Scenario: An input stream has more bytes than its metadata or configured limit permits.
    /// Guarantees: Reading stops after one excess byte, without storing more than the limit.
    #[test]
    fn stream_limit_does_not_depend_on_metadata() {
        let mut source = Cursor::new(vec![1; 32_768]);
        assert!(matches!(
            read_limited(&mut source, Path::new("/input"), 4096),
            Err(BackendError::InputTooLarge { maximum: 4096, .. })
        ));
        assert_eq!(source.position(), 4097);
    }

    /// Scenario: A stream ends exactly at its byte limit, including a zero-byte limit.
    /// Guarantees: Exact-size input succeeds without retaining excess allocation capacity.
    #[test]
    fn exact_size_input_fits() {
        for length in [0, 1, 8192, 9000] {
            let bytes = read_limited(Cursor::new(vec![7; length]), Path::new("/input"), length)
                .expect("exact-size input");
            assert_eq!(bytes, vec![7; length]);
            assert!(bytes.capacity() <= length);
        }
    }
}
