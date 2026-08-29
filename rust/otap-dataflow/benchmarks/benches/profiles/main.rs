// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

#![allow(missing_docs)]

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use otel_arrow_dfe_pdata::encode::encode_profiles_otap_batch;
use otel_arrow_dfe_pdata::otap::OtapArrowRecords;
use otel_arrow_dfe_pdata::otlp::OtlpProtoBytes;
use otel_arrow_dfe_pdata::proto::opentelemetry::profiles::v1development::ProfilesData;
use otel_arrow_dfe_pdata::testing::profiles::{ProfilesDatasetKind, profiles_dataset};
use otel_arrow_dfe_pdata::{OtapPayload, TryIntoWithOptions};
use prost::Message;

#[cfg(not(windows))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[derive(Clone, Copy)]
struct ProfilesCase {
    kind: ProfilesDatasetKind,
    profile_count: usize,
    samples_per_profile: usize,
    stack_depth: usize,
}

impl ProfilesCase {
    fn total_samples(self) -> u64 {
        u64::try_from(self.profile_count * self.samples_per_profile)
            .expect("benchmark sizes fit u64")
    }

    fn label(self, otlp_bytes: usize, otap_bytes: usize, retained_bytes: usize) -> String {
        format!(
            "{}-profiles{}-samples{}-depth{}-otlp{}-otap{}-retained{}",
            self.kind.as_str(),
            self.profile_count,
            self.samples_per_profile,
            self.stack_depth,
            otlp_bytes,
            otap_bytes,
            retained_bytes,
        )
    }
}

fn cases() -> [ProfilesCase; 6] {
    [
        ProfilesCase {
            kind: ProfilesDatasetKind::Cpu,
            profile_count: 1,
            samples_per_profile: 128,
            stack_depth: 32,
        },
        ProfilesCase {
            kind: ProfilesDatasetKind::Allocation,
            profile_count: 4,
            samples_per_profile: 256,
            stack_depth: 16,
        },
        ProfilesCase {
            kind: ProfilesDatasetKind::OffCpu,
            profile_count: 4,
            samples_per_profile: 256,
            stack_depth: 16,
        },
        ProfilesCase {
            kind: ProfilesDatasetKind::TimestampOnly,
            profile_count: 4,
            samples_per_profile: 256,
            stack_depth: 16,
        },
        ProfilesCase {
            kind: ProfilesDatasetKind::HighCardinalityAttributes,
            profile_count: 2,
            samples_per_profile: 128,
            stack_depth: 16,
        },
        ProfilesCase {
            kind: ProfilesDatasetKind::OriginalPayload,
            profile_count: 2,
            samples_per_profile: 64,
            stack_depth: 16,
        },
    ]
}

fn bench_profiles(c: &mut Criterion) {
    for case in cases() {
        let data = profiles_dataset(
            case.kind,
            case.profile_count,
            case.samples_per_profile,
            case.stack_depth,
        );
        let encoded = data.encode_to_vec();
        let records = encode_profiles_otap_batch(&data).expect("valid benchmark Profiles");
        let otap_bytes = records.logical_arrow_bytes().expect("measurable OTAP");
        let retained_bytes = records.retained_memory_bytes();
        let label = case.label(encoded.len(), otap_bytes, retained_bytes);

        let mut decode_group = c.benchmark_group("profiles_otlp_to_otap");
        _ = decode_group.throughput(Throughput::Elements(case.total_samples()));
        _ = decode_group.bench_with_input(
            BenchmarkId::new("prost_object", &label),
            &data,
            |b, input| {
                b.iter(|| {
                    black_box(
                        encode_profiles_otap_batch(black_box(input))
                            .expect("Profiles encode should succeed"),
                    )
                })
            },
        );
        _ = decode_group.bench_with_input(
            BenchmarkId::new("serialized_request", &label),
            &encoded,
            |b, input| {
                b.iter_batched(
                    || {
                        OtlpProtoBytes::ExportProfilesRequest(
                            black_box(input.as_slice()).to_vec().into(),
                        )
                    },
                    |payload| {
                        let records: OtapArrowRecords = payload
                            .try_into_with_default()
                            .expect("Profiles conversion should succeed");
                        black_box(records)
                    },
                    BatchSize::SmallInput,
                )
            },
        );
        decode_group.finish();

        let mut encode_group = c.benchmark_group("profiles_otap_to_otlp");
        _ = encode_group.throughput(Throughput::Elements(case.total_samples()));
        _ = encode_group.bench_with_input(
            BenchmarkId::new("canonical_request", &label),
            &records,
            |b, input| {
                b.iter_batched(
                    || input.clone(),
                    |records| {
                        let bytes: OtlpProtoBytes = OtapPayload::from_otap(records)
                            .try_into_with_default()
                            .expect("Profiles reconstruction should succeed");
                        black_box(bytes)
                    },
                    BatchSize::SmallInput,
                )
            },
        );
        encode_group.finish();

        let mut round_trip_group = c.benchmark_group("profiles_round_trip");
        _ = round_trip_group.throughput(Throughput::Elements(case.total_samples()));
        _ = round_trip_group.bench_with_input(
            BenchmarkId::new("otlp_otap_otlp", &label),
            &data,
            |b, input| {
                b.iter_batched(
                    || input.clone(),
                    |data: ProfilesData| {
                        let records =
                            encode_profiles_otap_batch(&data).expect("Profiles encode succeeds");
                        let bytes: OtlpProtoBytes = OtapPayload::from_otap(records)
                            .try_into_with_default()
                            .expect("Profiles reconstruction succeeds");
                        black_box(bytes)
                    },
                    BatchSize::SmallInput,
                )
            },
        );
        round_trip_group.finish();
    }
}

criterion_group!(profiles, bench_profiles);
criterion_main!(profiles);
