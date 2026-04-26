;; Minimal identity-transform plugin component (otap-plugin world).
;;
;; Canonical ABI for canon-lift export with indirect return: the core
;; function allocates a return area (via cabi_realloc), writes the
;; flattened result there, and returns the pointer.
(component
  (core module $m
    (memory (export "memory") 1)

    ;; Bump allocator (canonical-ABI realloc).
    (global $bump (mut i32) (i32.const 4096))
    (func $cabi_realloc (export "cabi_realloc")
      (param $oldptr i32) (param $oldsize i32) (param $align i32) (param $newsize i32)
      (result i32)
      (local $cur i32)
      (local.set $cur (global.get $bump))
      (local.set $cur
        (i32.and
          (i32.add (local.get $cur) (i32.sub (local.get $align) (i32.const 1)))
          (i32.xor (i32.sub (local.get $align) (i32.const 1)) (i32.const -1))))
      (global.set $bump (i32.add (local.get $cur) (local.get $newsize)))
      (local.get $cur)
    )

    ;; descriptor JSON, baked in at offset 0.
    (data (i32.const 0)
      "{\"name\":\"identity\",\"version\":\"0.0.1\",\"plugin_api_version\":{\"major\":0,\"minor\":1},\"components\":[{\"urn\":\"urn:otel:test:identity\",\"kind\":\"processor\",\"supported_payloads\":[\"otlp-proto-bytes\"],\"output_arity\":\"single\"}]}"
    )
    (global $desc_len i32 (i32.const 213))

    ;; descriptor: allocate 8-byte return area, write (ptr, len), return area ptr.
    (func $descriptor (export "descriptor") (result i32)
      (local $ret i32)
      (local.set $ret
        (call $cabi_realloc (i32.const 0) (i32.const 0) (i32.const 4) (i32.const 8)))
      (i32.store         (local.get $ret) (i32.const 0))
      (i32.store offset=4 (local.get $ret) (global.get $desc_len))
      (local.get $ret))

    ;; validate-config: 12-byte return area (disc + ptr + len). disc=0 means Ok.
    (func $validate_config (export "validate-config")
      (param $cfg_ptr i32) (param $cfg_len i32)
      (result i32)
      (local $ret i32)
      (local.set $ret
        (call $cabi_realloc (i32.const 0) (i32.const 0) (i32.const 4) (i32.const 12)))
      (i32.store (local.get $ret) (i32.const 0))
      (local.get $ret))

    ;; process: identity. 16-byte return area: disc + signal + payload_ptr + payload_len.
    (func $process (export "process")
      (param $signal i32) (param $payload_kind i32)
      (param $payload_ptr i32) (param $payload_len i32)
      (param $cfg_ptr i32) (param $cfg_len i32)
      (result i32)
      (local $ret i32)
      (local.set $ret
        (call $cabi_realloc (i32.const 0) (i32.const 0) (i32.const 4) (i32.const 16)))
      (i32.store          (local.get $ret) (i32.const 0))
      (i32.store offset=4  (local.get $ret) (local.get $signal))
      (i32.store offset=8  (local.get $ret) (local.get $payload_ptr))
      (i32.store offset=12 (local.get $ret) (local.get $payload_len))
      (local.get $ret))
  )
  (core instance $i (instantiate $m))

  (func (export "descriptor") (result string)
    (canon lift (core func $i "descriptor")
      (memory $i "memory")
      (realloc (func $i "cabi_realloc"))))

  (func (export "validate-config") (param "config" string) (result (result (error string)))
    (canon lift (core func $i "validate-config")
      (memory $i "memory")
      (realloc (func $i "cabi_realloc"))))

  (func (export "process")
        (param "signal" u32)
        (param "payload-kind" u32)
        (param "payload" (list u8))
        (param "config" string)
        (result (result (tuple u32 (list u8)) (error string)))
    (canon lift (core func $i "process")
      (memory $i "memory")
      (realloc (func $i "cabi_realloc"))))
)
