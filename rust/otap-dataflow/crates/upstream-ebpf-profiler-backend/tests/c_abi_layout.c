// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

// Compile against the external, pinned upstream headers; never copy them here.
#include <stddef.h>
#include <stdio.h>
#include "types.h"

int main(void) {
    printf("%zu %zu %zu %zu %zu %zu %zu %zu %zu %zu %zu %zu %zu %zu %zu\n",
           sizeof(Trace),
           offsetof(Trace, frame_data),
           offsetof(Trace, frame_data_len),
           offsetof(Trace, num_frames),
           offsetof(Trace, num_kernel_frames),
           offsetof(Trace, origin),
           offsetof(Trace, value),
           offsetof(Trace, cpu_id),
           sizeof(StackDelta),
           sizeof(StackDeltaPageKey),
           sizeof(StackDeltaPageInfo),
           sizeof(PIDPage),
           sizeof(PIDPageMappingInfo),
           sizeof(SystemAnalysis),
           offsetof(SystemAnalysis, code));
    return 0;
}
