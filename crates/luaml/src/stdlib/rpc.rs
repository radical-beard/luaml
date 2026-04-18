// NEW DEP (shared with http module): reqwest
// ASSUMED DEP (may already be added by the http module in L8+):
//   reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
//
// If the dep has not yet landed, adding this module to the build without the
// dep will fail compilation. Cargo.toml edits are intentionally out of scope
// for this file (see task rule #6).

//! `rpc` stdlib module: JSON-RPC 2.0 client helpers.
//!
//! # Scope (initial cut): CLIENT ONLY
//!
//! This module ships the *client* side of JSON-RPC 2.0 only — `call`,
//! `notify`, `batch`, and their async twins. The **server** side
//! (accepting connections, dispatching methods, multiplexing transports) is
//! explicitly deferred: JSON-RPC server helpers require transport
//! multiplexing (HTTP vs WebSocket vs stdio), request correlation, and
//! per-method dispatch, all of which is materially more complex than the
//! straight request/response shape of the client. A follow-up module or a
//! later iteration of this one will add `rpc.server(...)`.
//!
//! ## Surface
//!
//! - `rpc.client(url) -> Client` — returns a userdata wrapping a
//!   [`reqwest::Client`] bound to the given base URL.
//!
//! Methods on the `Client` userdata:
//! - `c:call(method, params) -> result` — blocking call. Serialises params
//!   to JSON, issues the request, and returns the JSON-RPC `result` field.
//!   Remote errors raise `mlua::Error::runtime("rpc: server error <code>: <msg>")`.
//! - `c:call_async(method, params) -> Promise<result>` — same, non-blocking.
//! - `c:notify(method, params)` — JSON-RPC notification (no `id` in the
//!   request). Still awaits the HTTP round-trip so transport errors can be
//!   surfaced, but the response body is discarded.
//! - `c:notify_async(method, params) -> Promise<nil>`.
//! - `c:batch({{method, params}, ...}) -> {entry, entry, ...}` — one HTTP
//!   round-trip carrying an array of requests. See "Batch result shape".
//! - `c:batch_async(...) -> Promise<...>`.
//!
//! ## Batch result shape
//!
//! Batch entries return a uniform `{ok=..., value=..., error=...}` table per
//! element, **not** thrown errors. Rationale: a batch exists specifically so
//! callers can issue N requests and inspect each outcome independently; if a
//! single errored element raised, the caller would lose visibility into the
//! successful siblings. Each entry is:
//!
//! - `{ok=true,  value=<result>}` — the remote returned a `result`.
//! - `{ok=false, error={code=<number>, message=<string>, data=<value|nil>}}`
//!   — the remote returned an `error` object.
//!
//! Transport-level failures (bad URL, non-2xx HTTP, malformed JSON) still
//! raise — those are whole-batch failures and correlation is meaningless
//! there. Only JSON-RPC-level element errors are returned as `ok=false`.
//!
//! Results are returned in **request order**, not in the order the server
//! reports them. JSON-RPC 2.0 allows a server to reorder batch responses, so
//! we correlate by `id` and re-sort. A response with an `id` not present in
//! the request is silently dropped.
//!
//! ## Request shape
//!
//! Every request body matches the JSON-RPC 2.0 spec:
//!   `{"jsonrpc": "2.0", "method": "<m>", "params": <p>, "id": <n>}`
//! Notifications omit `id`. The id is an `AtomicU64` on the client —
//! monotonic, thread-safe, starts at 1. Params may be any JSON-serialisable
//! Lua value; `null` params are serialised as JSON `null` (spec-compliant).
//!
//! Content-Type is `application/json`. Accept header is left to reqwest's
//! default (it will accept anything) — most JSON-RPC servers return JSON
//! regardless.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use serde_json::{Number as JsonNumber, Value as JsonValue, json};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;
use crate::stdlib::promise::{Promise, PromiseResult};
use crate::types::FieldValue;

/// Stateless marker type implementing [`LuamlStdlibModule`]. All rpc state
/// lives on the `LuaRpcClient` userdata returned from `rpc.client(url)`.
pub struct RpcModule;

impl LuamlStdlibModule for RpcModule {
    fn namespace(&self) -> &'static str {
        "rpc"
    }

    fn install(&self, lua: &Lua, rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // rpc.client(url) -> Client
        //
        // Construct a JSON-RPC 2.0 client bound to `url`. The underlying
        // [`reqwest::Client`] is built eagerly so that TLS backend setup and
        // connection-pool wiring happen at construction time — a subsequent
        // `:call` observes transport errors only if the request itself fails,
        // not because the client was half-initialised.
        {
            let rt = rt.clone();
            table.set(
                "client",
                lua.create_function(move |_, url: String| -> mlua::Result<LuaRpcClient> {
                    let http = reqwest::Client::builder()
                        .build()
                        .map_err(|e| mlua::Error::runtime(format!("rpc: {e}")))?;
                    Ok(LuaRpcClient {
                        http,
                        url,
                        next_id: Arc::new(AtomicU64::new(1)),
                        rt: rt.clone(),
                    })
                })?,
            )?;
        }

        Ok(table)
    }
}

/// UserData wrapping a JSON-RPC 2.0 client.
///
/// Owns the base URL, an `AtomicU64` request-id counter, and a cloneable
/// handle to the tokio runtime for spawning async work. The [`reqwest::Client`]
/// internally holds an `Arc`'d connection pool, so cloning it to ship into
/// spawned tasks is cheap.
pub struct LuaRpcClient {
    http: reqwest::Client,
    url: String,
    next_id: Arc<AtomicU64>,
    rt: Handle,
}

impl LuaRpcClient {
    /// Next id from the atomic counter. `fetch_add` with `Relaxed` is fine
    /// here — we only need uniqueness within the client, not a happens-before
    /// relationship with anything else.
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Build the JSON body for a single JSON-RPC 2.0 request.
///
/// Public within the crate so the smoke tests can assert on the exact shape
/// without needing a live network. Passing `Some(id)` produces a call;
/// `None` produces a notification (no `id` field at all, per spec).
pub(crate) fn build_request(id: Option<u64>, method: &str, params: JsonValue) -> JsonValue {
    match id {
        Some(id) => json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        }),
        None => json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }),
    }
}

impl UserData for LuaRpcClient {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // c:call(method, params) -> result
        //
        // Blocking JSON-RPC call. Errors from the server's `error` field
        // become `mlua::Error::runtime("rpc: server error <code>: <msg>")`.
        // Transport / parse errors are prefixed simply `rpc: <cause>`.
        methods.add_method("call", |lua, this, (method, params): (String, Value)| {
            let params_json = lua_to_json(&params)?;
            let id = this.next_id();
            let body = build_request(Some(id), &method, params_json);
            let http = this.http.clone();
            let url = this.url.clone();
            let resp_json: JsonValue = this
                .rt
                .block_on(async move { post_json(&http, &url, &body).await })
                .map_err(|e| mlua::Error::runtime(format!("rpc: {e}")))?;
            let result_json = extract_single_result(&resp_json, id)
                .map_err(mlua::Error::runtime)?;
            json_to_lua(lua, &result_json)
        });

        // c:call_async(method, params) -> Promise<result>
        //
        // Spawn the call onto the runtime and hand back a Promise. The task
        // resolves with a FieldValue (see module docs on why the cross-thread
        // payload is FieldValue, not mlua::Value).
        methods.add_method("call_async", |_, this, (method, params): (String, Value)| {
            let params_json = lua_to_json(&params)?;
            let id = this.next_id();
            let body = build_request(Some(id), &method, params_json);
            let http = this.http.clone();
            let url = this.url.clone();
            let rt = this.rt.clone();
            let join = rt.spawn(async move {
                let resp = post_json(&http, &url, &body)
                    .await
                    .map_err(|e| format!("rpc: {e}"))?;
                let result_json = extract_single_result(&resp, id)?;
                let out: PromiseResult = Ok(json_to_field(&result_json));
                out
            });
            Ok(Promise::new(join, this.rt.clone()))
        });

        // c:notify(method, params) -> ()
        //
        // Blocking JSON-RPC notification. Per spec, notifications have no
        // `id` and the server MUST NOT reply. We still wait on the HTTP
        // round-trip so a non-2xx / network error surfaces — but the response
        // body (if any) is discarded.
        methods.add_method("notify", |_, this, (method, params): (String, Value)| {
            let params_json = lua_to_json(&params)?;
            let body = build_request(None, &method, params_json);
            let http = this.http.clone();
            let url = this.url.clone();
            this.rt
                .block_on(async move { post_notify(&http, &url, &body).await })
                .map_err(|e| mlua::Error::runtime(format!("rpc: {e}")))?;
            Ok(())
        });

        // c:notify_async(method, params) -> Promise<nil>
        methods.add_method(
            "notify_async",
            |_, this, (method, params): (String, Value)| {
                let params_json = lua_to_json(&params)?;
                let body = build_request(None, &method, params_json);
                let http = this.http.clone();
                let url = this.url.clone();
                let rt = this.rt.clone();
                let join = rt.spawn(async move {
                    post_notify(&http, &url, &body)
                        .await
                        .map_err(|e| format!("rpc: {e}"))?;
                    let out: PromiseResult = Ok(FieldValue::Null);
                    out
                });
                Ok(Promise::new(join, this.rt.clone()))
            },
        );

        // c:batch({{method, params}, ...}) -> {entry, ...}
        //
        // Blocking batch. Each element of the input must be a table shaped
        // like `{method, params}` (positional: index 1 is the method, index 2
        // is the params). Results are returned in request order, with entry
        // shape documented at the module level.
        methods.add_method("batch", |lua, this, batch: Table| {
            let (body, ids) = build_batch_body(&batch, &this.next_id)?;
            let http = this.http.clone();
            let url = this.url.clone();
            let resp: JsonValue = this
                .rt
                .block_on(async move { post_json(&http, &url, &body).await })
                .map_err(|e| mlua::Error::runtime(format!("rpc: {e}")))?;
            let entries = correlate_batch_response(&resp, &ids)
                .map_err(mlua::Error::runtime)?;
            batch_entries_to_lua(lua, &entries)
        });

        // c:batch_async(...) -> Promise<...>
        //
        // The batch result is shaped as a FieldValue::List of FieldValue::Map
        // entries; `field_value_to_lua` on the receiving side turns that into
        // the same `{ok=..., value=..., error=...}` table shape the
        // synchronous path produces.
        methods.add_method("batch_async", |_, this, batch: Table| {
            let (body, ids) = build_batch_body(&batch, &this.next_id)?;
            let http = this.http.clone();
            let url = this.url.clone();
            let rt = this.rt.clone();
            let join = rt.spawn(async move {
                let resp = post_json(&http, &url, &body)
                    .await
                    .map_err(|e| format!("rpc: {e}"))?;
                let entries = correlate_batch_response(&resp, &ids)?;
                let out: PromiseResult = Ok(batch_entries_to_field(&entries));
                out
            });
            Ok(Promise::new(join, this.rt.clone()))
        });
    }
}

// ── Transport ───────────────────────────────────────────────────────────

/// POST a JSON body and parse the response JSON. Shared by `call` and `batch`
/// (including their async twins). Errors are surfaced as a `String` so they
/// cross thread boundaries cleanly (see [`PromiseResult`]).
async fn post_json(
    http: &reqwest::Client,
    url: &str,
    body: &JsonValue,
) -> Result<JsonValue, String> {
    let resp = http
        .post(url)
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("http send error: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("http status {status}: {text}"));
    }
    resp.json::<JsonValue>()
        .await
        .map_err(|e| format!("parse response: {e}"))
}

/// POST a notification body. No response parsing — per JSON-RPC 2.0, servers
/// MUST NOT reply to notifications, but some do (and some send an empty
/// body). Either way we only care that the request completed with a 2xx.
async fn post_notify(
    http: &reqwest::Client,
    url: &str,
    body: &JsonValue,
) -> Result<(), String> {
    let resp = http
        .post(url)
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("http send error: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("http status {status}: {text}"));
    }
    Ok(())
}

// ── Response correlation ────────────────────────────────────────────────

/// Pull the `result` field out of a single JSON-RPC response, or surface the
/// server's `error` field as a formatted `String`. The `expected_id` is the
/// id we sent; we verify the response echoes it (log-only mismatch handling
/// would mask server bugs, so we error on mismatch).
fn extract_single_result(resp: &JsonValue, expected_id: u64) -> Result<JsonValue, String> {
    if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
        return Err(format_server_error(err));
    }
    match resp.get("id") {
        Some(v) if v.as_u64() == Some(expected_id) => {}
        Some(v) => {
            return Err(format!(
                "response id {v} does not match request id {expected_id}"
            ));
        }
        None => return Err("response missing id field".to_string()),
    }
    resp.get("result")
        .cloned()
        .ok_or_else(|| "response missing result field".to_string())
}

/// Correlate a batch response (expected to be a JSON array) against the
/// request-ordered list of ids. Returns per-entry outcomes in **request
/// order**, filling any gap (a response with no matching entry, or vice
/// versa) with a synthesised error entry rather than silently dropping.
fn correlate_batch_response(resp: &JsonValue, ids: &[u64]) -> Result<Vec<BatchEntry>, String> {
    let arr = resp
        .as_array()
        .ok_or_else(|| "batch response is not an array".to_string())?;
    // Build an id → entry map so we can serve them back in request order
    // regardless of the server's chosen ordering.
    let mut by_id: std::collections::HashMap<u64, &JsonValue> =
        std::collections::HashMap::with_capacity(arr.len());
    for item in arr {
        if let Some(id) = item.get("id").and_then(|v| v.as_u64()) {
            by_id.insert(id, item);
        }
        // Items without an id are responses to notifications — batches should
        // not contain those per our usage (batch only holds calls), so we
        // silently drop them.
    }
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        match by_id.get(id) {
            Some(item) => {
                if let Some(err) = item.get("error").filter(|e| !e.is_null()) {
                    out.push(BatchEntry::Err(parse_server_error(err)));
                } else if let Some(result) = item.get("result") {
                    out.push(BatchEntry::Ok(result.clone()));
                } else {
                    out.push(BatchEntry::Err(ServerError {
                        code: 0,
                        message: "response missing both result and error".into(),
                        data: JsonValue::Null,
                    }));
                }
            }
            None => {
                out.push(BatchEntry::Err(ServerError {
                    code: 0,
                    message: format!("no response for request id {id}"),
                    data: JsonValue::Null,
                }));
            }
        }
    }
    Ok(out)
}

/// Per-request outcome in a batch. `Ok` carries the `result`; `Err` carries
/// the parsed server error object.
enum BatchEntry {
    Ok(JsonValue),
    Err(ServerError),
}

struct ServerError {
    code: i64,
    message: String,
    data: JsonValue,
}

/// Format a JSON-RPC error object as a human-readable string. Used when the
/// single-call path raises (no need to preserve the structured form since
/// Lua only sees the message on an `mlua::Error::runtime`).
fn format_server_error(err: &JsonValue) -> String {
    let parsed = parse_server_error(err);
    format!("server error {}: {}", parsed.code, parsed.message)
}

/// Parse a JSON-RPC error object into its typed parts. Missing fields get
/// conservative defaults: `code=0`, `message="<unknown>"`, `data=null`.
fn parse_server_error(err: &JsonValue) -> ServerError {
    let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
    let message = err
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>")
        .to_string();
    let data = err.get("data").cloned().unwrap_or(JsonValue::Null);
    ServerError {
        code,
        message,
        data,
    }
}

// ── Batch body assembly ─────────────────────────────────────────────────

/// Build the outbound JSON array for a batch, plus the parallel id vector
/// used to correlate the response. Each batch element is expected to be a
/// 2-slot table: `{method, params}`.
fn build_batch_body(
    batch: &Table,
    next_id: &Arc<AtomicU64>,
) -> mlua::Result<(JsonValue, Vec<u64>)> {
    let len = batch.len()? as usize;
    if len == 0 {
        return Err(mlua::Error::runtime("rpc: batch is empty"));
    }
    let mut arr = Vec::with_capacity(len);
    let mut ids = Vec::with_capacity(len);
    for i in 1..=len as i64 {
        let entry: Table = batch
            .get(i)
            .map_err(|e| mlua::Error::runtime(format!("rpc: batch[{i}]: {e}")))?;
        let method: String = entry
            .get(1)
            .map_err(|e| mlua::Error::runtime(format!("rpc: batch[{i}].method: {e}")))?;
        let params_val: Value = entry.get(2).unwrap_or(Value::Nil);
        let params_json = lua_to_json(&params_val)?;
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        ids.push(id);
        arr.push(build_request(Some(id), &method, params_json));
    }
    Ok((JsonValue::Array(arr), ids))
}

// ── Result → Lua / FieldValue conversion ────────────────────────────────

/// Convert a slice of batch entries into the Lua-side table returned by the
/// synchronous `batch` method. Each element is a `{ok, value|error}` table.
fn batch_entries_to_lua(lua: &Lua, entries: &[BatchEntry]) -> mlua::Result<Value> {
    let out = lua.create_table()?;
    for (i, entry) in entries.iter().enumerate() {
        let t = lua.create_table()?;
        match entry {
            BatchEntry::Ok(v) => {
                t.set("ok", true)?;
                t.set("value", json_to_lua(lua, v)?)?;
            }
            BatchEntry::Err(e) => {
                t.set("ok", false)?;
                let err_tbl = lua.create_table()?;
                err_tbl.set("code", e.code)?;
                err_tbl.set("message", e.message.as_str())?;
                err_tbl.set("data", json_to_lua(lua, &e.data)?)?;
                t.set("error", err_tbl)?;
            }
        }
        out.set(i as i64 + 1, t)?;
    }
    Ok(Value::Table(out))
}

/// Same as [`batch_entries_to_lua`] but emitting a [`FieldValue`] — used by
/// the async path where the spawned task's output must be Send.
/// `field_value_to_lua` on the receive side produces the identical Lua shape.
fn batch_entries_to_field(entries: &[BatchEntry]) -> FieldValue {
    let mut list = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut map = std::collections::HashMap::new();
        match entry {
            BatchEntry::Ok(v) => {
                map.insert("ok".to_string(), FieldValue::Bool(true));
                map.insert("value".to_string(), json_to_field(v));
            }
            BatchEntry::Err(e) => {
                map.insert("ok".to_string(), FieldValue::Bool(false));
                let mut err_map = std::collections::HashMap::new();
                err_map.insert("code".to_string(), FieldValue::Number(e.code));
                err_map.insert("message".to_string(), FieldValue::String(e.message.clone()));
                err_map.insert("data".to_string(), json_to_field(&e.data));
                map.insert("error".to_string(), FieldValue::Map(err_map));
            }
        }
        list.push(FieldValue::Map(map));
    }
    FieldValue::List(list)
}

// ── Lua ↔ JSON conversion (small inline version) ────────────────────────
//
// Deliberately inlined rather than imported from `super::json` to keep this
// module standalone. The rules are a trimmed subset of json.rs:
//
//   - nil            → null
//   - boolean        → bool
//   - integer/number → number (non-finite floats error)
//   - string         → string
//   - table with 1..N contiguous int keys → array
//   - table with string keys               → object
//   - anything else errors
//
// Mixed integer/string keys error. Non-string/non-positive-integer keys
// error. Functions / threads / userdata error. Same judgments as json.rs;
// if the rules diverge between the two, whichever you're reading today is
// authoritative for its own module.

fn lua_to_json(value: &Value) -> mlua::Result<JsonValue> {
    match value {
        Value::Nil => Ok(JsonValue::Null),
        Value::Boolean(b) => Ok(JsonValue::Bool(*b)),
        Value::Integer(i) => Ok(JsonValue::Number((*i).into())),
        Value::Number(n) => JsonNumber::from_f64(*n)
            .map(JsonValue::Number)
            .ok_or_else(|| mlua::Error::runtime(format!("rpc: invalid JSON number: {n}"))),
        Value::String(s) => Ok(JsonValue::String(s.to_str()?.to_string())),
        Value::Table(t) => lua_table_to_json(t),
        other => Err(mlua::Error::runtime(format!(
            "rpc: cannot encode Lua value as JSON: {}",
            other.type_name()
        ))),
    }
}

fn lua_table_to_json(t: &Table) -> mlua::Result<JsonValue> {
    let mut max_int: i64 = 0;
    let mut int_count: usize = 0;
    let mut has_string = false;

    for pair in t.clone().pairs::<Value, Value>() {
        let (k, _) = pair?;
        match &k {
            Value::Integer(i) if *i >= 1 => {
                int_count += 1;
                if *i > max_int {
                    max_int = *i;
                }
            }
            Value::Integer(i) => {
                return Err(mlua::Error::runtime(format!(
                    "rpc: invalid JSON object key: integer {i}"
                )));
            }
            Value::String(_) => {
                has_string = true;
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "rpc: invalid JSON object key: {}",
                    other.type_name()
                )));
            }
        }
    }

    let total: usize = t.clone().pairs::<Value, Value>().count();
    if total == 0 {
        return Ok(JsonValue::Array(Vec::new()));
    }
    if has_string && int_count > 0 {
        return Err(mlua::Error::runtime(
            "rpc: invalid JSON object key: table mixes integer and string keys",
        ));
    }

    let is_array = !has_string && int_count == total && (max_int as usize) == total;
    if is_array {
        let mut out = Vec::with_capacity(total);
        for i in 1..=(total as i64) {
            let v: Value = t.get(i)?;
            out.push(lua_to_json(&v)?);
        }
        Ok(JsonValue::Array(out))
    } else {
        let mut obj = serde_json::Map::with_capacity(total);
        for pair in t.clone().pairs::<Value, Value>() {
            let (k, v) = pair?;
            let key = match k {
                Value::String(s) => s.to_str()?.to_string(),
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "rpc: invalid JSON object key: {}",
                        other.type_name()
                    )));
                }
            };
            obj.insert(key, lua_to_json(&v)?);
        }
        Ok(JsonValue::Object(obj))
    }
}

fn json_to_lua(lua: &Lua, json: &JsonValue) -> mlua::Result<Value> {
    match json {
        JsonValue::Null => Ok(Value::Nil),
        JsonValue::Bool(b) => Ok(Value::Boolean(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Number(f))
            } else {
                Err(mlua::Error::runtime(format!("rpc: number out of range: {n}")))
            }
        }
        JsonValue::String(s) => Ok(Value::String(lua.create_string(s)?)),
        JsonValue::Array(arr) => {
            let t = lua.create_table()?;
            for (i, item) in arr.iter().enumerate() {
                t.set(i as i64 + 1, json_to_lua(lua, item)?)?;
            }
            Ok(Value::Table(t))
        }
        JsonValue::Object(obj) => {
            let t = lua.create_table()?;
            for (k, v) in obj {
                t.set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            Ok(Value::Table(t))
        }
    }
}

/// JsonValue → FieldValue. Used by the spawned-task paths where the output
/// must be Send. Mirrors [`json_to_lua`]: integers stay integers, fractional
/// numbers become floats, arrays become lists, objects become maps.
fn json_to_field(json: &JsonValue) -> FieldValue {
    match json {
        JsonValue::Null => FieldValue::Null,
        JsonValue::Bool(b) => FieldValue::Bool(*b),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                FieldValue::Number(i)
            } else if let Some(f) = n.as_f64() {
                FieldValue::Float(f)
            } else {
                // Out-of-range u64 fallback — serialise as string so nothing
                // is silently lost. A caller that can tell the difference can
                // inspect the string form.
                FieldValue::String(n.to_string())
            }
        }
        JsonValue::String(s) => FieldValue::String(s.clone()),
        JsonValue::Array(arr) => FieldValue::List(arr.iter().map(json_to_field).collect()),
        JsonValue::Object(obj) => {
            let mut map = std::collections::HashMap::new();
            for (k, v) in obj {
                map.insert(k.clone(), json_to_field(v));
            }
            FieldValue::Map(map)
        }
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests. We deliberately avoid live network — the client's
    //! behaviour with a running JSON-RPC server is an integration concern.
    //! What's worth pinning here is the **request shaping** (via the pure
    //! [`build_request`] helper) and the **error-path wiring** (an invalid
    //! URL surfaces a runtime error rather than panicking).
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    fn install() -> (tokio::runtime::Runtime, Lua) {
        let rt = Builder::new_multi_thread().enable_all().build().unwrap();
        let lua = Lua::new();
        let table = RpcModule
            .install(&lua, &rt.handle().clone())
            .expect("install rpc module");
        lua.globals().set("rpc", table).expect("set rpc global");
        (rt, lua)
    }

    #[test]
    fn build_request_call_shape_has_jsonrpc_method_params_and_id() {
        // A call's request body must have exactly the four spec-mandated
        // top-level fields in the 2.0 shape. We assert on each field rather
        // than a string-equals so the test doesn't break if serde_json
        // changes key ordering internally.
        let req = build_request(Some(42), "echo", json!({"msg": "hi"}));
        assert_eq!(req["jsonrpc"], JsonValue::from("2.0"));
        assert_eq!(req["method"], JsonValue::from("echo"));
        assert_eq!(req["params"]["msg"], JsonValue::from("hi"));
        assert_eq!(req["id"], JsonValue::from(42));
    }

    #[test]
    fn build_request_notification_shape_omits_id_entirely() {
        // A notification MUST NOT include `id` (even as null, per JSON-RPC
        // 2.0 §4.1). The object must not even contain the key.
        let req = build_request(None, "ping", JsonValue::Null);
        assert_eq!(req["jsonrpc"], JsonValue::from("2.0"));
        assert_eq!(req["method"], JsonValue::from("ping"));
        assert_eq!(req["params"], JsonValue::Null);
        let obj = req.as_object().expect("request is object");
        assert!(
            !obj.contains_key("id"),
            "notification must not carry id: {req}"
        );
    }

    #[test]
    fn call_with_invalid_url_surfaces_runtime_error() {
        // Error-path wiring: a client constructed with a URL that cannot be
        // parsed / resolved must surface a runtime error prefixed `rpc:`.
        // "not-a-url" has no scheme, so reqwest refuses to send before any
        // network I/O happens — making this a fast, hermetic test.
        let (_rt, lua) = install();
        let err = lua
            .load(r#"return rpc.client("not-a-url"):call("m", nil)"#)
            .eval::<mlua::Value>()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("rpc:"),
            "error should be prefixed `rpc:`, got: {err}"
        );
    }

    #[test]
    fn extract_single_result_mismatched_id_errors() {
        // Sanity on the correlation guard: if the server echoes an id other
        // than the one we sent, we refuse the response rather than returning
        // data we can't prove belongs to this call.
        let resp = json!({"jsonrpc": "2.0", "id": 99, "result": 7});
        let err = extract_single_result(&resp, 1).unwrap_err();
        assert!(err.contains("does not match"), "got: {err}");
    }

    #[test]
    fn extract_single_result_server_error_formats_code_and_message() {
        // Error field → "server error <code>: <msg>" string. The format is
        // load-bearing because it composes into the `mlua::Error::runtime`
        // message the Lua caller sees.
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32601, "message": "method not found"},
        });
        let err = extract_single_result(&resp, 1).unwrap_err();
        assert!(err.contains("-32601"), "got: {err}");
        assert!(err.contains("method not found"), "got: {err}");
    }

    #[test]
    fn correlate_batch_response_reorders_by_id() {
        // Server replies out of request order (legal per JSON-RPC 2.0). We
        // must return entries in *request* order, correlated by id.
        let ids = vec![10u64, 11, 12];
        let resp = json!([
            {"jsonrpc": "2.0", "id": 12, "result": "third"},
            {"jsonrpc": "2.0", "id": 10, "result": "first"},
            {"jsonrpc": "2.0", "id": 11, "error": {"code": -1, "message": "oops"}},
        ]);
        let entries = correlate_batch_response(&resp, &ids).expect("correlate");
        assert_eq!(entries.len(), 3);
        match &entries[0] {
            BatchEntry::Ok(v) => assert_eq!(v, &JsonValue::from("first")),
            _ => panic!("entry 0 should be Ok"),
        }
        match &entries[1] {
            BatchEntry::Err(e) => {
                assert_eq!(e.code, -1);
                assert_eq!(e.message, "oops");
            }
            _ => panic!("entry 1 should be Err"),
        }
        match &entries[2] {
            BatchEntry::Ok(v) => assert_eq!(v, &JsonValue::from("third")),
            _ => panic!("entry 2 should be Ok"),
        }
    }
}
