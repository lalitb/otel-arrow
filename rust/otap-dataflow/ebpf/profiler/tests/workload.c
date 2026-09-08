// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

#include <stdint.h>

static volatile uint64_t sink;

__attribute__((noinline)) static void leaf(void) {
    volatile uint64_t value = 1;
    for (;;) {
        for (uint64_t index = 1; index < 10000; index++)
            value = (value * index) ^ (value >> 7);
        sink = value;
    }
}

__attribute__((noinline)) static void middle(void) {
    leaf();
    sink++;
}

__attribute__((noinline)) static void outer(void) {
    middle();
    sink++;
}

int main(void) {
    outer();
    return 0;
}
