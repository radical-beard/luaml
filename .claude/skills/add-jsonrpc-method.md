---
name: add-jsonrpc-method
description: Guide for adding a new JSON-RPC method to the luaml-service crate. Use when extending the service protocol with new operations.
---

# Adding a JSON-RPC Method

A new JSON-RPC method requires changes in four places: param types, handler function, request routing, and tests.

## Step 1: Define types in `protocol.rs`

**File**: `crates/luaml-service/src/protocol.rs`

Add a params struct for your method:

```rust
#[derive(Debug, Deserialize)]
pub struct MyMethodParams {
    pub field_one: String,
    pub field_two: Option<i64>,
}
```

Follow existing patterns: `RegisterParams`, `DispatchParams`, etc. Use `#[derive(Debug, Deserialize)]` for params. If the method returns structured data beyond a simple `{"ok": true}`, define a result struct with `#[derive(Debug, Serialize)]`.

Add serde roundtrip tests in the `#[cfg(test)]` block.

## Step 2: Add handler in `connection.rs`

**File**: `crates/luaml-service/src/connection.rs`

Write the handler function. Follow the naming convention `handle_<method_name>`:

```rust
fn handle_my_method(engine: &LuamlEngine, request: &Request) -> Response {
    let params: MyMethodParams = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(request.id, INVALID_PARAMS, e.to_string()),
    };

    // Use the engine...
    // Return result
    Response::ok(request.id, serde_json::json!({"ok": true}))
}
```

Signature patterns:
- Read-only methods: `engine: &LuamlEngine`
- Mutating methods: `engine: &mut LuamlEngine`
- Methods needing API handler (like `register_api`): add `handler: &Arc<RemoteApiHandler>`

Use existing conversion functions:
- `json_map_to_field_map()` — converts JSON objects to `FieldMap`
- `field_value_to_pattern()` — converts `FieldValue` to `Pattern` (for literal patterns only)

Error codes:
- `INVALID_PARAMS` (-32602) for bad input
- `LUAML_ERROR` (-32000) for engine errors

## Step 3: Add routing in `process_request()`

**File**: `crates/luaml-service/src/connection.rs`, function `process_request()` (around line 89)

Add a match arm:

```rust
"my_method" => handle_my_method(engine, request),
```

If the method mutates the engine, it already takes `&mut LuamlEngine`. If it needs the API handler, it's already available as `handler`.

## Step 4: Add tests

**Unit tests** in `connection.rs` `#[cfg(test)]`:
- Valid request with correct params returns expected response
- Missing/invalid params return `INVALID_PARAMS` error
- Engine error conditions return `LUAML_ERROR`

**Integration test** in `crates/luaml-service/tests/protocol_integration.rs`:
- Full JSON-RPC roundtrip using `run_session()` and `request()` helpers
- Test the method in combination with other methods (e.g., register then your method)

## Step 5: Update documentation

- `README.md` — add method to the Service Mode section with request/response examples
- Include the JSON-RPC request format and a sample response

## Checklist

```sh
cargo fmt --all
cargo clippy --workspace
cargo test
```

## Files touched (summary)

| File | What to change |
|---|---|
| `crates/luaml-service/src/protocol.rs` | Params/result structs + serde tests |
| `crates/luaml-service/src/connection.rs` | Handler function + process_request() routing + tests |
| `crates/luaml-service/tests/protocol_integration.rs` | Roundtrip integration test |
| `README.md` | Service Mode method documentation |
