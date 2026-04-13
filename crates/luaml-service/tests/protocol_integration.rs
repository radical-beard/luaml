//! Full connection lifecycle tests using handle_stream() with pipe-based I/O.
//!
//! Each test creates an in-memory reader/writer, feeds JSON-RPC requests to
//! handle_stream(), and verifies the responses.

use std::io::{Cursor, Read, Write};
use std::sync::{Arc, Mutex};

/// A Write impl that writes to a shared Vec<u8>.
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Run handle_stream with the given input lines and return all response lines.
fn run_session(input_lines: &[&str]) -> Vec<serde_json::Value> {
    let input = input_lines.join("\n") + "\n";
    let reader: Box<dyn Read + Send> = Box::new(Cursor::new(input.into_bytes()));
    let write_buf = Arc::new(Mutex::new(Vec::new()));
    let writer: Box<dyn Write + Send> = Box::new(SharedWriter(write_buf.clone()));

    luaml_service::connection::handle_stream(reader, writer);

    let written = write_buf.lock().unwrap();
    let written_str = String::from_utf8(written.clone()).unwrap();
    written_str
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("response should be valid JSON"))
        .collect()
}

fn request(method: &str, params: serde_json::Value, id: u64) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": id
    })
    .to_string()
}

fn register_request(source_path: &str, text: &str, id: u64) -> String {
    request(
        "register",
        serde_json::json!({"source_path": source_path, "text": text}),
        id,
    )
}

fn dispatch_request(event: serde_json::Value, id: u64) -> String {
    request("dispatch", serde_json::json!({"event": event}), id)
}

fn query_request(event: serde_json::Value, id: u64) -> String {
    request("query", serde_json::json!({"event": event}), id)
}

#[test]
fn register_valid_script() {
    let responses = run_session(&[&register_request(
        "test.luaml",
        "---\ntype: :input:\n---\nprint('hi')\n",
        1,
    )]);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["id"], 1);
    assert!(responses[0]["result"]["ok"].as_bool().unwrap());
    assert!(responses[0].get("error").is_none());
}

#[test]
fn register_invalid_script() {
    let responses = run_session(&[&register_request("bad.luaml", "not valid", 1)]);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32000); // LUAML_ERROR
}

#[test]
fn dispatch_after_register() {
    let reg = register_request(
        "test.luaml",
        "---\ntype: :input:\nkey: \"q\"\n---\nresult = \"quit\"\n",
        1,
    );
    let disp = dispatch_request(
        serde_json::json!({"type": {"Enum": "input"}, "key": {"String": "q"}}),
        2,
    );
    let responses = run_session(&[&reg, &disp]);
    assert_eq!(responses.len(), 2);

    // Dispatch result should have 1 match
    let matches = &responses[1]["result"]["matches"];
    assert_eq!(matches.as_array().unwrap().len(), 1);
    assert_eq!(matches[0]["script_path"], "test.luaml");
}

#[test]
fn dispatch_no_match() {
    let reg = register_request("test.luaml", "---\ntype: :input:\n---\nprint('hi')\n", 1);
    let disp = dispatch_request(serde_json::json!({"type": {"Enum": "lifecycle"}}), 2);
    let responses = run_session(&[&reg, &disp]);
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses[1]["result"]["matches"].as_array().unwrap().len(),
        0
    );
}

#[test]
fn query_after_register() {
    let reg = register_request(
        "test.luaml",
        "---\ntype: :input:\nkey: $k\n---\nresult = k\n",
        1,
    );
    let q = query_request(
        serde_json::json!({"type": {"Enum": "input"}, "key": {"String": "z"}}),
        2,
    );
    let responses = run_session(&[&reg, &q]);
    assert_eq!(responses.len(), 2);
    let matches = &responses[1]["result"]["matches"];
    assert_eq!(matches.as_array().unwrap().len(), 1);

    // Bindings should include k=String("z")
    let bindings = &matches[0]["bindings"];
    assert_eq!(bindings["k"]["String"], "z");
}

#[test]
fn unknown_method() {
    let req = request("foo", serde_json::json!(null), 1);
    let responses = run_session(&[&req]);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32601); // METHOD_NOT_FOUND
    assert!(
        responses[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("foo")
    );
}

#[test]
fn malformed_json() {
    let responses = run_session(&["not json at all"]);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32700); // PARSE_ERROR
}

#[test]
fn register_missing_params() {
    let req = request("register", serde_json::json!({}), 1);
    let responses = run_session(&[&req]);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32602); // INVALID_PARAMS
}

#[test]
fn dispatch_missing_params() {
    let req = request("dispatch", serde_json::json!({}), 1);
    let responses = run_session(&[&req]);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32602); // INVALID_PARAMS
}

#[test]
fn register_api_empty_pattern() {
    let reg = register_request("test.luaml", "---\ntype: :input:\n---\nprint('hi')\n", 1);
    let reg_api = request(
        "register_api",
        serde_json::json!({"namespace": "svc", "pattern": {}}),
        2,
    );
    let responses = run_session(&[&reg, &reg_api]);
    assert_eq!(responses.len(), 2);
    assert!(responses[1]["result"]["ok"].as_bool().unwrap());
}

#[test]
fn register_api_non_literal_rejected() {
    let reg_api = request(
        "register_api",
        serde_json::json!({"namespace": "svc", "pattern": {"x": {"List": [{"Number": 1}]}}}),
        1,
    );
    let responses = run_session(&[&reg_api]);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32602); // INVALID_PARAMS
    assert!(
        responses[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("literal")
    );
}

#[test]
fn multiple_sequential_requests() {
    let r1 = register_request("a.luaml", "---\ntype: :a:\n---\na()\n", 1);
    let r2 = register_request("b.luaml", "---\ntype: :b:\n---\nb()\n", 2);
    let r3 = register_request("c.luaml", "---\ntype: :c:\n---\nc()\n", 3);
    let responses = run_session(&[&r1, &r2, &r3]);
    assert_eq!(responses.len(), 3);
    for (i, resp) in responses.iter().enumerate() {
        assert_eq!(resp["id"], (i + 1) as u64);
        assert!(resp["result"]["ok"].as_bool().unwrap());
    }
}

#[test]
fn empty_line_ignored() {
    let reg = register_request("a.luaml", "---\ntype: :a:\n---\na()\n", 1);
    // Insert empty lines between requests
    let responses = run_session(&[
        &reg,
        "",
        "",
        &register_request("b.luaml", "---\ntype: :b:\n---\nb()\n", 2),
    ]);
    assert_eq!(responses.len(), 2);
}

#[test]
fn dispatch_captures_bindings() {
    let reg = register_request(
        "test.luaml",
        "---\ntype: :input:\nkey: $k\nmode: $m\n---\nresult = k\n",
        1,
    );
    let disp = dispatch_request(
        serde_json::json!({
            "type": {"Enum": "input"},
            "key": {"String": "q"},
            "mode": {"Enum": "normal"}
        }),
        2,
    );
    let responses = run_session(&[&reg, &disp]);
    let bindings = &responses[1]["result"]["matches"][0]["bindings"];
    assert_eq!(bindings["k"]["String"], "q");
    assert_eq!(bindings["m"]["Enum"], "normal");
}

#[test]
fn dispatch_with_guard_pass() {
    let reg = register_request(
        "test.luaml",
        "---\ntype: :lifecycle:\ndepth: $d\n? d > 0\n---\nresult = d\n",
        1,
    );
    let disp = dispatch_request(
        serde_json::json!({"type": {"Enum": "lifecycle"}, "depth": {"Number": 5}}),
        2,
    );
    let responses = run_session(&[&reg, &disp]);
    assert_eq!(
        responses[1]["result"]["matches"].as_array().unwrap().len(),
        1
    );
}

#[test]
fn dispatch_with_guard_fail() {
    let reg = register_request(
        "test.luaml",
        "---\ntype: :lifecycle:\ndepth: $d\n? d > 0\n---\nresult = d\n",
        1,
    );
    let disp = dispatch_request(
        serde_json::json!({"type": {"Enum": "lifecycle"}, "depth": {"Number": 0}}),
        2,
    );
    let responses = run_session(&[&reg, &disp]);
    assert_eq!(
        responses[1]["result"]["matches"].as_array().unwrap().len(),
        0
    );
}

// Note: register_api_then_dispatch_with_callback and connection_eof_ends_loop
// are implicitly tested by the existing tests (run_session always hits EOF,
// and API callback requires a bidirectional pipe which is tested in remote_api.rs).
