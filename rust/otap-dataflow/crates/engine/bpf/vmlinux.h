/* SPDX-License-Identifier: Apache-2.0 */
/*
 * Minimal BPF target header for the reuseport selector.
 *
 * The selector does not dereference struct sk_reuseport_md; it only passes the
 * context pointer to bpf_sk_select_reuseport(). Keeping this checked-in header
 * lets normal Linux --all-features builds compile without requiring bpftool or
 * a host-generated vmlinux.h. Operators can still override it with
 * OTAP_DF_REUSEPORT_EBPF_VMLINUX_H when they want a generated kernel header.
 */

#ifndef OTAP_DF_MINIMAL_VMLINUX_H
#define OTAP_DF_MINIMAL_VMLINUX_H

typedef unsigned char __u8;
typedef unsigned short __u16;
typedef unsigned int __u32;
typedef unsigned long long __u64;
typedef signed char __s8;
typedef signed short __s16;
typedef signed int __s32;
typedef signed long long __s64;
typedef __u16 __be16;
typedef __u32 __be32;
typedef __u32 __wsum;
typedef _Bool bool;

#ifndef true
#define true 1
#endif

#ifndef false
#define false 0
#endif

struct sk_reuseport_md;

enum bpf_map_type {
	BPF_MAP_TYPE_ARRAY = 2,
	BPF_MAP_TYPE_REUSEPORT_SOCKARRAY = 20,
};

enum sk_action {
	SK_DROP = 0,
	SK_PASS = 1,
};

#endif /* OTAP_DF_MINIMAL_VMLINUX_H */
