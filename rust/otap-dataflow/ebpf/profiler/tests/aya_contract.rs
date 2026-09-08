// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Native harness linked to the exact aya-obj artifact selected by Cargo.
//! No kernel syscall or additional Cargo dependency is needed.

/// Scenario: Aya is upgraded or built for another supported architecture.
/// Guarantees: The BTF allocation model's enum/member slot assumptions remain
/// valid for the exact dependency artifact selected by Cargo.
#[test]
fn aya_btf_layout_fits_the_initialization_envelope() {
    use aya_obj::btf::{BtfType, BtfEnum, BtfEnum64, BtfParam, DataSecEntry};
    assert!(size_of::<BtfType>() <= 128, "BTF enum size: {}", size_of::<BtfType>());
    assert!(size_of::<BtfEnum>() <= 32);
    assert!(size_of::<BtfEnum64>() <= 32);
    assert!(size_of::<BtfParam>() <= 32);
    assert!(size_of::<DataSecEntry>() <= 32);
}

/// Scenario: Both independently compiled target objects reach Aya's own parser.
/// Guarantees: Aya recognizes the original C programs, legacy maps and BTF
/// without relying solely on the profiler's separate ELF preflight.
#[test]
fn aya_parses_both_generated_objects() {
    for variable in [
        "OTEL_EBPF_PROFILER_CONTRACT_OBJECT",
        "OTEL_EBPF_PROFILER_CROSS_OBJECT",
    ] {
        let path = std::env::var_os(variable).expect("generated object path");
        let bytes = std::fs::read(path).expect("generated object bytes");
        let object = aya_obj::Object::parse(&bytes).expect("Aya must parse the original C object");
        assert_eq!(object.programs.len(), 2);
        for name in ["profile_cpu", "profile_cpu_kernel"] {
            let program = &object.programs[name];
            assert!(matches!(
                program.section,
                aya_obj::ProgramSection::PerfEvent
            ));
        }
        assert_eq!(object.maps.len(), 3);
        for (name, kind, value_size, max_entries) in [
            ("EVENTS", 4, 4, 4096),
            ("SCRATCH", 6, 1072, 1),
            ("COUNTERS", 6, 8, 3),
        ] {
            let map = &object.maps[name];
            assert_eq!(map.map_type(), kind);
            assert_eq!(map.key_size(), 4);
            assert_eq!(map.value_size(), value_size);
            assert_eq!(map.max_entries(), max_entries);
        }
        assert!(object.btf.is_some());
    }
}
