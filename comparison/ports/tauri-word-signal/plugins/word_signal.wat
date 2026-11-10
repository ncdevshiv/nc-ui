(module
  ;; The two imports the contract allows, and nothing else: the host defines exactly these, so
  ;; any other import in this module would fail to link rather than reach something in the host.
  (import "ncui" "capability_check" (func $check (param i32 i32) (result i32)))
  (import "ncui" "host_call" (func $host (param i32 i32 i32 i32 i32 i32) (result i32)))

  ;; One page of memory, four allowed. The host caps growth far below the module's own maximum,
  ;; so growing past the cap is the host's decision and not this module's.
  (memory (export "memory") 1 4)

  (data (i32.const 64) "{\"abi_version\":\"1.0\",\"requested\":[],\"imports\":[],\"exports\":[{\"name\":\"return_string\",\"kind\":\"function\"}]}")
  (data (i32.const 512) "return_string")
  (data (i32.const 544) "{\"error\":\"refused\"}")

  ;; Packs a pointer and a length into the single i64 the contract returns.
  (func $pack (param $p i32) (param $n i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $n))))

  ;; Byte-for-byte comparison. The caller always compares lengths first, and in an `if` rather
  ;; than `i32.and`, because wasm `and` does not short-circuit: a guarded comparison that
  ;; still read the bytes would trap on a bad pointer instead of falling through.
  (func $streq (param $a i32) (param $b i32) (param $n i32) (result i32)
    (local $i i32)
    (local $bad i32)
    (loop $cont
      (local.set $bad (i32.or (local.get $bad)
        (i32.ne (i32.load8_u (i32.add (local.get $a) (local.get $i)))
                (i32.load8_u (i32.add (local.get $b) (local.get $i))))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $cont (i32.lt_u (local.get $i) (local.get $n))))
    (i32.eqz (local.get $bad)))

  (func $is (param $p i32) (param $n i32) (param $lit i32) (param $lit_len i32) (result i32)
    (if (result i32)
      (i32.ne (local.get $n) (local.get $lit_len))
      (then (i32.const 0))
      (else (call $streq (local.get $p) (local.get $lit) (local.get $lit_len)))))

  ;; The calling-convention revision this artifact was built against.
  (func (export "ncui_abi_version") (result i32) i32.const 1)

  (func (export "ncui_manifest") (result i64)
    (call $pack (i32.const 64) (i32.const 104)))

  ;; Where the host may write the method name and the request document.
  (func (export "ncui_scratch") (result i64)
    (call $pack (i32.const 1024) (i32.const 2048)))

  (func (export "ncui_invoke") (param $np i32) (param $nl i32) (param $ip i32) (param $il i32)
        (result i64)
    ;; `return_string` answers with the caller's own bytes, which is the whole method: the
    ;; original Tauri command echoes the word it was given, and this is that command.
    (if (call $is (local.get $np) (local.get $nl) (i32.const 512)
                  (i32.const 13))
      (then (return (call $pack (local.get $ip) (local.get $il)))))
    ;; Anything else is a method this component never declared.
    (return (call $pack (i32.const 544) (i32.const 19))))
)
