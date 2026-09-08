// SPDX-License-Identifier: GPL-2.0-only
#include "../src/abi.h"
#include <stdio.h>

// Scenario: The C ABI producer writes distinguishable fixed-width fields.
// Guarantees: Rust's independent decoder sees the C offsets and array padding.
int main(void) {
    const struct profiler_event event = {
        .version = ABI_VERSION,
        .header_size = ABI_HEADER_SIZE,
        .event_kind = EVENT_KIND_SAMPLE,
        .flags = FLAG_KERNEL_STACK_ERROR,
        .pid = 0x12345678,
        .tid = 0x23456789,
        .cpu = 37,
        .timestamp_ns = 0x0102030405060708ULL,
        .user_depth = 2,
        .kernel_stack_error = -14,
        .frames = {0x1122334455667788ULL, 0x8877665544332211ULL},
    };
    return fwrite(&event, sizeof(event), 1, stdout) != 1;
}
