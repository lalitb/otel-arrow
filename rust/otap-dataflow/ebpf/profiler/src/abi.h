// SPDX-License-Identifier: GPL-2.0-only
#ifndef OTEL_PROFILER_ABI_H
#define OTEL_PROFILER_ABI_H

typedef unsigned char __u8;
typedef unsigned short __u16;
typedef unsigned int __u32;
typedef unsigned long long __u64;
typedef int __s32;

#define ABI_VERSION 1
#define ABI_HEADER_SIZE 48
#define EVENT_KIND_SAMPLE 1
#define MAX_STACK_DEPTH 64
#define FLAG_USER_TRUNCATED (1U << 0)
#define FLAG_KERNEL_TRUNCATED (1U << 1)
#define FLAG_USER_STACK_ERROR (1U << 2)
#define FLAG_KERNEL_STACK_ERROR (1U << 3)

struct profiler_event {
    __u16 version;
    __u16 header_size;
    __u16 event_kind;
    __u16 flags;
    __u32 pid;
    __u32 tid;
    __u32 cpu;
    __u32 reserved0;
    __u64 timestamp_ns;
    __u16 user_depth;
    __u16 kernel_depth;
    __s32 user_stack_error;
    __s32 kernel_stack_error;
    __u32 reserved1;
    __u64 frames[MAX_STACK_DEPTH * 2];
};

_Static_assert(__BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__,
               "profiler ABI requires a little-endian target");
_Static_assert(__builtin_offsetof(struct profiler_event, frames) == ABI_HEADER_SIZE,
               "profiler event header layout changed");
_Static_assert(__builtin_offsetof(struct profiler_event, timestamp_ns) == 24,
               "profiler timestamp offset changed");
_Static_assert(sizeof(struct profiler_event) == 1072,
               "profiler event size changed");
_Static_assert(_Alignof(struct profiler_event) == 8,
               "profiler event alignment changed");
#endif
