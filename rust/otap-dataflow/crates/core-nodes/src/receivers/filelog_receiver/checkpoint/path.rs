// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Receiver-side lookup and ordering for durable advisory paths.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use hashbrown::Equivalent;
use otel_arrow_dfe_filelog_checkpoint::AdvisoryPath;

/// Owned structural key for one durable advisory path.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct AdvisoryPathKey {
    kind: u8,
    truncated: bool,
    full_path_len: u64,
    stored_path_bytes: Vec<u8>,
    full_path_digest: [u8; 32],
}

impl From<&AdvisoryPath> for AdvisoryPathKey {
    fn from(path: &AdvisoryPath) -> Self {
        Self {
            kind: path.kind().to_wire(),
            truncated: path.is_truncated(),
            full_path_len: path.full_path_len(),
            stored_path_bytes: path.stored_path_bytes().to_vec(),
            full_path_digest: *path.full_path_digest(),
        }
    }
}

/// Borrowed path lookup that avoids cloning stored path bytes.
pub(crate) struct AdvisoryPathRef<'a>(&'a AdvisoryPath);

impl<'a> AdvisoryPathRef<'a> {
    pub(crate) const fn new(path: &'a AdvisoryPath) -> Self {
        Self(path)
    }
}

impl Hash for AdvisoryPathRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.kind().to_wire().hash(state);
        self.0.is_truncated().hash(state);
        self.0.full_path_len().hash(state);
        self.0.stored_path_bytes().hash(state);
        self.0.full_path_digest().hash(state);
    }
}

impl Equivalent<AdvisoryPathKey> for AdvisoryPathRef<'_> {
    fn equivalent(&self, key: &AdvisoryPathKey) -> bool {
        self.0.kind().to_wire() == key.kind
            && self.0.is_truncated() == key.truncated
            && self.0.full_path_len() == key.full_path_len
            && self.0.stored_path_bytes() == key.stored_path_bytes
            && self.0.full_path_digest() == &key.full_path_digest
    }
}

/// Total order used to select one distinguished binding among path aliases.
pub(crate) fn distinguished_binding_order(left: &AdvisoryPath, right: &AdvisoryPath) -> Ordering {
    left.kind()
        .to_wire()
        .cmp(&right.kind().to_wire())
        .then_with(|| {
            if !left.is_truncated() && !right.is_truncated() {
                left.stored_path_bytes().cmp(right.stored_path_bytes())
            } else {
                left.full_path_len()
                    .cmp(&right.full_path_len())
                    .then_with(|| left.full_path_digest().cmp(right.full_path_digest()))
                    .then_with(|| left.stored_path_bytes().cmp(right.stored_path_bytes()))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashbrown::HashMap;

    /// Scenario: complete and truncated standalone advisory paths are inserted
    /// as owned receiver keys and queried through borrowed path values.
    /// Guarantees: borrowed hashing and equality match the owned key exactly
    /// without cloning stored path bytes during lookup.
    #[test]
    fn borrowed_advisory_paths_match_owned_keys() {
        let complete = AdvisoryPath::from_unix_bytes(b"/var/log/app.log").unwrap();
        let truncated =
            AdvisoryPath::from_unix_bytes(&vec![b'x'; 5_000]).expect("long path is representable");
        let mut paths = HashMap::new();
        let _ = paths.insert(AdvisoryPathKey::from(&complete), 1);
        let _ = paths.insert(AdvisoryPathKey::from(&truncated), 2);

        assert_eq!(paths.get(&AdvisoryPathRef::new(&complete)), Some(&1));
        assert_eq!(paths.get(&AdvisoryPathRef::new(&truncated)), Some(&2));
        assert_eq!(
            paths.get(&AdvisoryPathRef::new(
                &AdvisoryPath::from_unix_bytes(b"/var/log/other.log").unwrap()
            )),
            None
        );
    }

    /// Scenario: complete aliases and truncated aliases compete for one
    /// distinguished binding.
    /// Guarantees: complete paths use native-byte order, while any truncated
    /// comparison uses full length before digest and retained suffix.
    #[test]
    fn distinguished_binding_order_preserves_receiver_semantics() {
        let complete_a = AdvisoryPath::from_unix_bytes(b"/a.log").unwrap();
        let complete_b = AdvisoryPath::from_unix_bytes(b"/b.log").unwrap();
        assert_eq!(
            distinguished_binding_order(&complete_a, &complete_b),
            Ordering::Less
        );

        let shorter =
            AdvisoryPath::from_unix_bytes(&vec![b'a'; 4_097]).expect("long path is representable");
        let longer =
            AdvisoryPath::from_unix_bytes(&vec![b'a'; 5_000]).expect("long path is representable");
        assert_eq!(
            distinguished_binding_order(&shorter, &longer),
            Ordering::Less
        );
        assert_eq!(
            distinguished_binding_order(&longer, &shorter),
            Ordering::Greater
        );
    }
}
