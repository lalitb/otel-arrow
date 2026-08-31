// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Profiles-specific validation helpers.

use otel_arrow_dfe_pdata::proto::OtlpProtoMessage;

pub(crate) fn validate_profile_counts(
    messages: &[OtlpProtoMessage],
    min_profiles: Option<usize>,
    max_profiles: Option<usize>,
    min_samples: Option<usize>,
    max_samples: Option<usize>,
) -> bool {
    if messages.is_empty() {
        return false;
    }

    let mut profile_count = 0_usize;
    let mut sample_count = 0_usize;
    for message in messages {
        let OtlpProtoMessage::Profiles(data) = message else {
            return false;
        };
        for resource in &data.resource_profiles {
            for scope in &resource.scope_profiles {
                let Some(next_profiles) = profile_count.checked_add(scope.profiles.len()) else {
                    return false;
                };
                profile_count = next_profiles;
                for profile in &scope.profiles {
                    let Some(next_samples) = sample_count.checked_add(profile.samples.len()) else {
                        return false;
                    };
                    sample_count = next_samples;
                }
            }
        }
    }

    within_bounds(profile_count, min_profiles, max_profiles)
        && within_bounds(sample_count, min_samples, max_samples)
}

fn within_bounds(value: usize, minimum: Option<usize>, maximum: Option<usize>) -> bool {
    minimum.is_none_or(|minimum| value >= minimum) && maximum.is_none_or(|maximum| value <= maximum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otel_arrow_dfe_pdata::testing::profiles::{ProfilesDatasetKind, profiles_dataset};

    /// Scenario: Profiles count validation receives several roots with multiple samples.
    /// Guarantees: Root and sample totals are checked independently and exactly.
    #[test]
    fn validates_profile_and_sample_totals() {
        let messages = [OtlpProtoMessage::Profiles(profiles_dataset(
            ProfilesDatasetKind::Cpu,
            3,
            4,
            2,
        ))];

        assert!(validate_profile_counts(
            &messages,
            Some(3),
            Some(3),
            Some(12),
            Some(12)
        ));
        assert!(!validate_profile_counts(
            &messages,
            Some(4),
            None,
            None,
            None
        ));
        assert!(!validate_profile_counts(
            &messages,
            None,
            None,
            None,
            Some(11)
        ));
    }
}
