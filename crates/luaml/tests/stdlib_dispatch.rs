//! Stdlib integration tests exercising each bare-namespaced luaml stdlib
//! module from inside a **dispatched luaml clause** — not directly on a raw
//! Lua VM.
//!
//! Each test:
//!   1. Constructs a `LuamlEngine` (which auto-installs the full stdlib).
//!   2. Registers a tiny `.luaml` script whose body calls one representative
//!      method from the module under test.
//!   3. Dispatches a matching event.
//!   4. Asserts the side effect via a Lua global on the engine's VM.
//!
//! These complement the per-module unit tests under `src/stdlib/<name>.rs`:
//! the unit tests drive the modules against a raw `Lua::new()`, while these
//! prove the stdlib is reachable through the production execution path.

use luaml::LuamlEngine;
use luaml::types::{FieldMap, FieldValue};

/// Build a `FieldMap` from a slice of `(name, value)` pairs. Mirrors the
/// helper used in `integration.rs`.
fn event(pairs: &[(&str, FieldValue)]) -> FieldMap {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// Register a single-clause script on a fresh engine, dispatch an
/// `:input:` event, and assert that exactly one clause matched. Centralises
/// the scaffolding so each module test stays focused on the assertion.
fn run_script(body: &str) -> LuamlEngine {
    let mut engine = LuamlEngine::new().unwrap();
    let script = format!("---\ntype: :input:\n---\n{body}\n");
    engine.register("t.luaml", &script).unwrap();
    let results = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
    assert_eq!(results.len(), 1, "expected exactly one clause to match");
    // Surface any body error for easier debugging on test regressions.
    if let Err(err) = &results[0].result {
        panic!("clause body errored: {}", err.message);
    }
    engine
}

#[test]
fn json_encode_decode_round_trip() {
    let engine = run_script(
        r#"
local s = json.encode({a = 1, b = "x"})
local t = json.decode(s)
got_a = t.a
got_b = t.b
"#,
    );
    let a: i64 = engine.lua().globals().get("got_a").unwrap();
    let b: String = engine.lua().globals().get("got_b").unwrap();
    assert_eq!(a, 1);
    assert_eq!(b, "x");
}

#[test]
fn crypto_hash_sha256_matches_known_digest() {
    // SHA-256("abc") → ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
    let engine = run_script(
        r#"
local digest = crypto.hash("sha256", "abc")
-- Expose hex-encoded digest via codec.
hex_digest = codec.hex_encode(digest)
"#,
    );
    let hex: String = engine.lua().globals().get("hex_digest").unwrap();
    assert_eq!(
        hex,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn math_mean_of_three_values() {
    let engine = run_script(
        r#"
mean_value = math.mean({1, 2, 3})
"#,
    );
    let v: f64 = engine.lua().globals().get("mean_value").unwrap();
    assert!((v - 2.0).abs() < 1e-12);
}

#[test]
fn vec_add_dimensions() {
    let engine = run_script(
        r#"
local r = vec.add({1, 2}, {3, 4})
sum_x = r[1]
sum_y = r[2]
"#,
    );
    let x: f64 = engine.lua().globals().get("sum_x").unwrap();
    let y: f64 = engine.lua().globals().get("sum_y").unwrap();
    assert_eq!(x, 4.0);
    assert_eq!(y, 6.0);
}

#[test]
fn url_encode_percent_escapes_space() {
    let engine = run_script(
        r#"
encoded = url.encode("hello world")
"#,
    );
    let v: String = engine.lua().globals().get("encoded").unwrap();
    assert_eq!(v, "hello%20world");
}

#[test]
fn codec_base64_round_trip() {
    let engine = run_script(
        r#"
local enc = codec.base64_encode("hi")
-- base64("hi") == "aGk="
encoded = enc
decoded = codec.base64_decode(enc)
"#,
    );
    let enc: String = engine.lua().globals().get("encoded").unwrap();
    let dec: String = engine.lua().globals().get("decoded").unwrap();
    assert_eq!(enc, "aGk=");
    assert_eq!(dec, "hi");
}

#[test]
fn regex_compile_is_match() {
    let engine = run_script(
        r#"
local r = regex.compile("wor.d")
matched = r:is_match("hello world")
"#,
    );
    let v: bool = engine.lua().globals().get("matched").unwrap();
    assert!(v);
}

#[test]
fn path_join_two_segments() {
    let engine = run_script(
        r#"
joined = path.join("a", "b")
"#,
    );
    let v: String = engine.lua().globals().get("joined").unwrap();
    assert_eq!(v, "a/b");
}

#[test]
fn time_now_is_positive() {
    let engine = run_script(
        r#"
now_value = time.now()
"#,
    );
    let v: f64 = engine.lua().globals().get("now_value").unwrap();
    assert!(v > 0.0, "time.now() must be a positive unix timestamp");
}

#[test]
fn env_get_path_is_non_nil() {
    // PATH is effectively guaranteed to be present in any sane test env.
    // Skip the assertion on the extremely unusual case where it's unset.
    if std::env::var("PATH").is_err() {
        // SKIP: requires PATH environment variable to be set
        return;
    }
    let engine = run_script(
        r#"
path_var = env.get("PATH")
"#,
    );
    let v: Option<String> = engine.lua().globals().get("path_var").unwrap();
    let val = v.expect("env.get(\"PATH\") should return a string");
    assert!(!val.is_empty(), "PATH must be non-empty");
}

#[test]
fn console_info_does_not_error() {
    // console.info writes to stderr and returns nil — the test just proves
    // the namespace is reachable and the call doesn't raise.
    let engine = run_script(
        r#"
console.info("stdlib_dispatch test line")
console_called = true
"#,
    );
    let v: bool = engine.lua().globals().get("console_called").unwrap();
    assert!(v);
}

#[test]
fn fs_write_then_read_round_trip() {
    // Use a process-unique path under the OS temp dir so tests don't
    // collide on parallel runs.
    let path = std::env::temp_dir()
        .join(format!(
            "luaml_stdlib_dispatch_fs_{}.txt",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);

    let mut engine = LuamlEngine::new().unwrap();
    engine
        .register(
            "fs.luaml",
            "---\ntype: :input:\npath: $p\n---\nfs.write(p, \"payload\")\ncontent = fs.read(p)\n",
        )
        .unwrap();

    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("path", FieldValue::String(path.clone())),
    ]));
    assert_eq!(results.len(), 1);
    let content: String = engine.lua().globals().get("content").unwrap();
    assert_eq!(content, "payload");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn http_namespace_installed() {
    // SKIP: a real HTTP round-trip requires an external server. Prove the
    // namespace is reachable from a dispatched clause and the blocking
    // `http.get` function is a Lua function — a full request is covered
    // by the http module's own unit tests.
    let engine = run_script(
        r#"
http_is_table = type(http) == "table"
http_get_is_fn = type(http.get) == "function"
http_post_is_fn = type(http.post) == "function"
"#,
    );
    assert!(
        engine
            .lua()
            .globals()
            .get::<bool>("http_is_table")
            .unwrap()
    );
    assert!(
        engine
            .lua()
            .globals()
            .get::<bool>("http_get_is_fn")
            .unwrap()
    );
    assert!(
        engine
            .lua()
            .globals()
            .get::<bool>("http_post_is_fn")
            .unwrap()
    );
}

#[test]
fn tcp_listen_ephemeral_port_on_localhost() {
    let engine = run_script(
        r#"
local l = tcp.listen("127.0.0.1:0")
addr = l:local_addr()
l:close()
"#,
    );
    let addr: String = engine.lua().globals().get("addr").unwrap();
    assert!(
        addr.starts_with("127.0.0.1:"),
        "expected loopback address, got {addr}"
    );
}

#[test]
fn udp_bind_reports_local_addr() {
    let engine = run_script(
        r#"
local s = udp.bind("127.0.0.1:0")
addr = s:local_addr()
s:close()
"#,
    );
    let addr: String = engine.lua().globals().get("addr").unwrap();
    assert!(
        addr.starts_with("127.0.0.1:"),
        "expected loopback address, got {addr}"
    );
}

#[test]
fn rpc_namespace_installed() {
    // SKIP: a real JSON-RPC call requires an external server. Prove that
    // `rpc.client(url)` returns a userdata handle from inside a dispatched
    // clause — the transport behaviour is covered by the rpc module's
    // unit tests.
    let engine = run_script(
        r#"
local c = rpc.client("http://127.0.0.1:1/")
client_kind = type(c)
"#,
    );
    let kind: String = engine.lua().globals().get("client_kind").unwrap();
    assert_eq!(kind, "userdata");
}

#[test]
fn thread_sleep_returns_without_error() {
    // thread.sleep blocks on the engine's tokio runtime; zero ms is a
    // cheap way to prove the namespace is reachable from a dispatched
    // clause without slowing the suite.
    let engine = run_script(
        r#"
thread.sleep(0)
thread_called = true
"#,
    );
    let v: bool = engine.lua().globals().get("thread_called").unwrap();
    assert!(v);
}

#[test]
fn process_pid_is_positive() {
    let engine = run_script(
        r#"
pid_value = process.pid()
"#,
    );
    let v: i64 = engine.lua().globals().get("pid_value").unwrap();
    assert!(v > 0, "process.pid() must be positive, got {v}");
}

#[test]
fn promise_await_resolves_async_value() {
    // `fs.read_async` produces a Promise; `:await()` on it resolves to the
    // synchronous equivalent's result. Use a temp file so we don't depend
    // on any shipped asset.
    let path = std::env::temp_dir()
        .join(format!(
            "luaml_stdlib_dispatch_promise_{}.txt",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned();
    std::fs::write(&path, "promise-payload").unwrap();

    let mut engine = LuamlEngine::new().unwrap();
    engine
        .register(
            "p.luaml",
            "---\ntype: :input:\npath: $p\n---\nlocal promise = fs.read_async(p)\nresolved = promise:await()\n",
        )
        .unwrap();

    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("path", FieldValue::String(path.clone())),
    ]));
    assert_eq!(results.len(), 1);
    if let Err(err) = &results[0].result {
        panic!("clause body errored: {}", err.message);
    }
    let resolved: String = engine.lua().globals().get("resolved").unwrap();
    assert_eq!(resolved, "promise-payload");

    let _ = std::fs::remove_file(&path);
}
