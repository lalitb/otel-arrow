// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Actual new-key, stack interning, and repeated-key aggregation benchmarks.

mod support;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use object::{Object, ObjectSection, ObjectSymbol};
use otel_arrow_dfe_ebpf_profiler::{ObjectSymbolResolver, SymbolResolver, parse_proc_maps};

fn bench_aggregation(criterion: &mut Criterion) {
    for kernel in [false, true] {
        let name = if kernel {
            "repeated_user_kernel_stack"
        } else {
            "repeated_user_stack"
        };
        let _benchmark = criterion.bench_function(name, |bencher| {
            let mut window = support::window();
            let sample = support::sample(1, kernel);
            assert!(window.record(&sample).expect("warm-up sample is valid"));
            bencher.iter(|| {
                assert!(
                    window
                        .record(std::hint::black_box(&sample))
                        .expect("sample is valid")
                );
            });
        });
    }
    let mut group = criterion.benchmark_group("new_key_and_stack_interning");
    let _throughput = group.throughput(Throughput::Elements(512));
    let _benchmark = group.bench_function("512_distinct_stacks", |bencher| {
        // Each timed batch starts empty and remains strictly below all limits.
        // A capacity-rejection fast path cannot masquerade as insertion work.
        bencher.iter_batched_ref(
            support::window,
            |window| {
                for tid in 1..=512 {
                    assert!(
                        window
                            .record(&support::sample(tid, false))
                            .expect("sample is valid")
                    );
                }
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
    let _rejection = criterion.bench_function("full_thread_table_rejection", |bencher| {
        let mut window = support::populated(1024);
        let rejected = support::sample(1025, false);
        bencher.iter(|| {
            assert!(
                !window
                    .record(std::hint::black_box(&rejected))
                    .expect("bounded rejection")
            );
        });
    });
}

fn bench_metadata(criterion: &mut Criterion) {
    let maps = (0..512u64)
        .map(|index| {
            let start = 0x1000 + index * 0x1000;
            format!(
                "{start:x}-{:x} r-xp {:08x} 00:00 1 /fixture\n",
                start + 0x1000,
                index * 0x1000
            )
        })
        .collect::<String>();
    let mappings = parse_proc_maps(&maps, 512, 256).expect("benchmark mapping table is valid");
    let _mapping = criterion.bench_function("mapping_lookup_512_ranges", |bencher| {
        bencher.iter(|| std::hint::black_box(mappings.find(std::hint::black_box(0x181010))));
    });

    let executable = std::env::current_exe().expect("benchmark executable is available");
    let bytes = std::fs::read(&executable).expect("benchmark executable is readable");
    let object = object::File::parse(bytes.as_slice()).expect("benchmark ELF parses");
    let file_offset = object
        .symbols()
        .find_map(|symbol| {
            if symbol.kind() != object::SymbolKind::Text
                || symbol.is_undefined()
                || symbol.size() == 0
            {
                return None;
            }
            let section = object.section_by_index(symbol.section_index()?).ok()?;
            let (offset, size) = section.file_range()?;
            let relative = symbol.address().checked_sub(section.address())?;
            (relative < size)
                .then(|| offset.checked_add(relative))
                .flatten()
        })
        .expect("benchmark ELF has a defined native symbol");
    let mut resolver = ObjectSymbolResolver::with_limits(65_536, 8, 64 * 1024 * 1024, 512)
        .expect("symbol benchmark limits are valid");
    assert!(
        resolver
            .resolve(&executable, file_offset)
            .expect("symbol lookup succeeds")
            .is_some()
    );
    let _symbols = criterion.bench_function("native_symbol_cache_lookup", |bencher| {
        bencher.iter(|| {
            std::hint::black_box(
                resolver
                    .resolve(&executable, std::hint::black_box(file_offset))
                    .expect("cached symbol lookup succeeds"),
            )
        });
    });
}

criterion_group!(benches, bench_aggregation, bench_metadata);
criterion_main!(benches);
