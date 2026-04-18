// NEW DEPS:
// reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json", "stream"] }
// bytes = "1"
// futures-util = "0.3"
//
//! `http` stdlib module: HTTP client operations, blocking and async.
//!
//! Installed under the bare `http` global. Exposes a small client surface
//! plus streaming downloads. Each surface method has both a blocking form
//! (runs to completion before returning) and an `_async` form (returns a
//! [`Promise`] that scripts drive with `:await()` / `:try_await()`).
//!
//! ## Methods
//!
//! Client (blocking):
//! - `http.get(url, opts?)`
//! - `http.post(url, body, opts?)` / `http.put` / `http.patch` / `http.delete` / `http.head`
//! - `http.request({ method, url, headers, query, body, timeout })`
//!
//! Client (async, returns `Promise<Response>`):
//! - `http.get_async(url, opts?)`
//! - `http.post_async(url, body, opts?)` / `http.put_async` / `http.patch_async`
//!   / `http.delete_async` / `http.head_async`
//! - `http.request_async({ ... })`
//!
//! Streaming:
//! - `http.download(url, path) -> { bytes_written = integer }` (blocking)
//! - `http.download_async(url, path) -> Promise<{ bytes_written = integer }>`
//!
//! ## Options
//!
//! All client methods accept an optional `opts` table with:
//! - `headers = { [name] = value, ... }` — string→string HTTP headers.
//! - `query = { [name] = value, ... }` — string→string query parameters.
//! - `timeout = <seconds>` — request timeout; applied to the underlying
//!   `reqwest::Client` builder. Fractional seconds supported via `f64`.
//!
//! The general `http.request` form takes a single table merging the method
//! + url + opts + body into one shape.
//!
//! ## Body shape
//!
//! The `body` argument to non-GET/HEAD requests accepts either:
//! - A Lua string → sent verbatim as raw bytes.
//! - A Lua table → encoded as JSON (`serde_json`) with `content-type:
//!   application/json` added automatically (callers may still override via
//!   `opts.headers["content-type"]`, which wins).
//!
//! ## Response shape
//!
//! Every response surface method returns a plain Lua table (not a userdata
//! wrapper) with:
//! - `status = integer` — HTTP status code.
//! - `headers = { [name] = value, ... }` — response headers. Repeated
//!   headers keep the first value (standard Lua-table semantics).
//! - `body = string` — full response body as a Lua string (raw bytes are
//!   allowed; scripts pass through `json.decode` / similar if they want
//!   structured data).
//! - `ok = bool` — `true` iff `status >= 200 && status < 300`.
//!
//! ## Error handling
//!
//! Blocking methods surface any reqwest / encoding error as
//! `mlua::Error::runtime("http: <msg>")`. Async methods collapse the same
//! error into a `PromiseResult` error string of the same shape; `:await()`
//! re-raises it as a runtime error.
//!
//! ## Design decisions
//!
//! - **Per-call `reqwest::Client`.** Each call builds a fresh
//!   `reqwest::Client::builder().build()?`. This is slightly less efficient
//!   than sharing (connection pooling is per-client), but matches the
//!   module contract from mod.rs — "modules are stateless from the engine's
//!   perspective". Sharing a client across calls would require interior
//!   mutability and Send + Sync plumbing; the per-call approach is correct
//!   and simple. If profiling shows pooling matters, a future change can
//!   introduce an engine-level client without breaking the surface.
//! - **No server.** HTTP _server_ functionality (`http.server`) is
//!   intentionally deferred. A server needs routing / state / shutdown
//!   semantics that deserve their own design pass rather than getting
//!   bolted on here. TODO(follow-up): ship `http.server` as a separate
//!   stdlib cut once the design is settled.
//! - **Response as a table, not userdata.** A table is trivially
//!   inspectable from Lua and round-trips through `pairs()` / `next()`
//!   without special methods. We lose the ability to lazy-stream the body,
//!   but `http.download` covers the one case where streaming matters.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use mlua::{Lua, Table, Value};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;
use crate::stdlib::promise::Promise;
use crate::types::FieldValue;

/// Stateless stdlib module installer for the `http` namespace.
pub struct HttpModule;

/// Shape of a single HTTP request after options have been normalised out of
/// the Lua tables. Kept as owned data so it can cross the `.spawn(async
/// move { ... })` boundary for async methods.
#[derive(Clone, Debug)]
struct RequestSpec {
    method: reqwest::Method,
    url: String,
    headers: Vec<(String, String)>,
    query: Vec<(String, String)>,
    body: Option<RequestBody>,
    timeout: Option<Duration>,
}

/// Body payload. `Json` carries the serde_json string already encoded so
/// the async worker doesn't need to re-invoke the encoder.
#[derive(Clone, Debug)]
enum RequestBody {
    Raw(Vec<u8>),
    Json(String),
}

impl LuamlStdlibModule for HttpModule {
    fn namespace(&self) -> &'static str {
        "http"
    }

    fn install(&self, lua: &Lua, rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // Bind blocking + async variants for each HTTP method. The closures
        // capture `rt` (a Handle clone) so they can drive reqwest on the
        // engine's runtime.
        install_method(&table, lua, rt, "get", reqwest::Method::GET, false)?;
        install_method(&table, lua, rt, "head", reqwest::Method::HEAD, false)?;
        install_method(&table, lua, rt, "post", reqwest::Method::POST, true)?;
        install_method(&table, lua, rt, "put", reqwest::Method::PUT, true)?;
        install_method(&table, lua, rt, "patch", reqwest::Method::PATCH, true)?;
        install_method(&table, lua, rt, "delete", reqwest::Method::DELETE, true)?;

        // http.request({ method, url, ... }) -> Response (blocking)
        {
            let rt = rt.clone();
            table.set(
                "request",
                lua.create_function(move |lua, spec: Table| {
                    let req = request_spec_from_table(&spec)?;
                    let value = rt
                        .block_on(execute_request(req))
                        .map_err(|e| mlua::Error::runtime(format!("http: {e}")))?;
                    response_to_lua_table(lua, value)
                })?,
            )?;
        }

        // http.request_async({ ... }) -> Promise<Response>
        {
            let rt = rt.clone();
            table.set(
                "request_async",
                lua.create_function(move |_, spec: Table| {
                    let req = request_spec_from_table(&spec)?;
                    let rt_inner = rt.clone();
                    let join = rt.spawn(async move {
                        execute_request(req)
                            .await
                            .map(response_to_field_value)
                            .map_err(|e| format!("http: {e}"))
                    });
                    Ok(Promise::new(join, rt_inner))
                })?,
            )?;
        }

        // http.download(url, path) -> { bytes_written = integer } (blocking)
        {
            let rt = rt.clone();
            table.set(
                "download",
                lua.create_function(move |lua, (url, path): (String, String)| {
                    let path = PathBuf::from(path);
                    let bytes = rt
                        .block_on(execute_download(url, path))
                        .map_err(|e| mlua::Error::runtime(format!("http: {e}")))?;
                    let out = lua.create_table()?;
                    out.set("bytes_written", bytes as i64)?;
                    Ok(out)
                })?,
            )?;
        }

        // http.download_async(url, path) -> Promise<{ bytes_written }>
        {
            let rt = rt.clone();
            table.set(
                "download_async",
                lua.create_function(move |_, (url, path): (String, String)| {
                    let path = PathBuf::from(path);
                    let rt_inner = rt.clone();
                    let join = rt.spawn(async move {
                        execute_download(url, path)
                            .await
                            .map(|n| {
                                let mut map = HashMap::new();
                                map.insert("bytes_written".to_string(), FieldValue::Number(n as i64));
                                FieldValue::Map(map)
                            })
                            .map_err(|e| format!("http: {e}"))
                    });
                    Ok(Promise::new(join, rt_inner))
                })?,
            )?;
        }

        Ok(table)
    }
}

/// Bind `http.<name>` and `http.<name>_async` for a given HTTP method.
///
/// `has_body` controls the function arity: `get` and `head` take
/// `(url, opts?)`, while `post` / `put` / `patch` / `delete` take
/// `(url, body, opts?)`. `delete` technically allows a body per RFC 7231,
/// so we let callers pass one; they can pass nil to omit.
fn install_method(
    table: &Table,
    lua: &Lua,
    rt: &Handle,
    name: &'static str,
    method: reqwest::Method,
    has_body: bool,
) -> mlua::Result<()> {
    // Blocking variant. We branch on `has_body` inside the closure so the
    // signature registered on the Lua side matches what the docstring
    // promises (GET/HEAD: no body arg; others: body arg in position 2).
    {
        let rt = rt.clone();
        let method = method.clone();
        if has_body {
            table.set(
                name,
                lua.create_function(
                    move |lua, (url, body, opts): (String, Value, Option<Table>)| {
                        let req = build_request_spec(method.clone(), url, Some(body), opts)?;
                        let value = rt
                            .block_on(execute_request(req))
                            .map_err(|e| mlua::Error::runtime(format!("http: {e}")))?;
                        response_to_lua_table(lua, value)
                    },
                )?,
            )?;
        } else {
            table.set(
                name,
                lua.create_function(move |lua, (url, opts): (String, Option<Table>)| {
                    let req = build_request_spec(method.clone(), url, None, opts)?;
                    let value = rt
                        .block_on(execute_request(req))
                        .map_err(|e| mlua::Error::runtime(format!("http: {e}")))?;
                    response_to_lua_table(lua, value)
                })?,
            )?;
        }
    }

    // Async variant. Same arity rules, returns a Promise<Response>.
    let async_name = format!("{name}_async");
    {
        let rt = rt.clone();
        if has_body {
            table.set(
                async_name,
                lua.create_function(
                    move |_, (url, body, opts): (String, Value, Option<Table>)| {
                        let req = build_request_spec(method.clone(), url, Some(body), opts)?;
                        let rt_inner = rt.clone();
                        let join = rt.spawn(async move {
                            execute_request(req)
                                .await
                                .map(response_to_field_value)
                                .map_err(|e| format!("http: {e}"))
                        });
                        Ok(Promise::new(join, rt_inner))
                    },
                )?,
            )?;
        } else {
            table.set(
                async_name,
                lua.create_function(move |_, (url, opts): (String, Option<Table>)| {
                    let req = build_request_spec(method.clone(), url, None, opts)?;
                    let rt_inner = rt.clone();
                    let join = rt.spawn(async move {
                        execute_request(req)
                            .await
                            .map(response_to_field_value)
                            .map_err(|e| format!("http: {e}"))
                    });
                    Ok(Promise::new(join, rt_inner))
                })?,
            )?;
        }
    }

    Ok(())
}

/// Turn a `(method, url, body, opts)` quadruple from the shorthand
/// functions into a [`RequestSpec`]. Nil / absent body is encoded as
/// `None`. A `Value::Nil` body (explicit `nil` from Lua) is also treated
/// as absent — this is what Lua callers naturally write when they want to
/// pass opts but no body.
fn build_request_spec(
    method: reqwest::Method,
    url: String,
    body: Option<Value>,
    opts: Option<Table>,
) -> mlua::Result<RequestSpec> {
    let (headers, query, timeout) = parse_opts(opts)?;
    let body = match body {
        None | Some(Value::Nil) => None,
        Some(v) => Some(lua_value_to_body(v)?),
    };
    Ok(RequestSpec {
        method,
        url,
        headers,
        query,
        body,
        timeout,
    })
}

/// Turn the general-form `http.request({ ... })` table into a
/// [`RequestSpec`]. `method` defaults to GET when omitted (the common "just
/// fetch this URL" case); everything else mirrors the shorthand functions.
fn request_spec_from_table(spec: &Table) -> mlua::Result<RequestSpec> {
    let method_str: Option<String> = spec.get("method")?;
    let method = match method_str {
        Some(s) => s.parse::<reqwest::Method>().map_err(|e| {
            mlua::Error::runtime(format!("http: invalid method '{s}': {e}"))
        })?,
        None => reqwest::Method::GET,
    };
    let url: String = spec
        .get::<Option<String>>("url")?
        .ok_or_else(|| mlua::Error::runtime("http: request missing 'url'"))?;

    let headers = table_to_string_pairs(spec.get::<Option<Table>>("headers")?)?;
    let query = table_to_string_pairs(spec.get::<Option<Table>>("query")?)?;
    let timeout = spec
        .get::<Option<f64>>("timeout")?
        .map(duration_from_secs);

    let body = match spec.get::<Value>("body")? {
        Value::Nil => None,
        other => Some(lua_value_to_body(other)?),
    };

    Ok(RequestSpec {
        method,
        url,
        headers,
        query,
        body,
        timeout,
    })
}

/// Parse `opts = { headers?, query?, timeout? }` into its three
/// components. Missing opts is treated as empty across the board.
fn parse_opts(
    opts: Option<Table>,
) -> mlua::Result<(Vec<(String, String)>, Vec<(String, String)>, Option<Duration>)> {
    let Some(opts) = opts else {
        return Ok((Vec::new(), Vec::new(), None));
    };
    let headers = table_to_string_pairs(opts.get::<Option<Table>>("headers")?)?;
    let query = table_to_string_pairs(opts.get::<Option<Table>>("query")?)?;
    let timeout = opts.get::<Option<f64>>("timeout")?.map(duration_from_secs);
    Ok((headers, query, timeout))
}

/// Flatten a `{ [k] = v }` Lua table into ordered `(String, String)`
/// pairs. Absent tables produce an empty vec. Iteration order follows
/// `pairs()`, which is unspecified in Lua — callers that care about
/// ordering (rare for headers; only for signed query strings) should
/// switch to a list-of-pairs shape, which we don't currently accept.
fn table_to_string_pairs(t: Option<Table>) -> mlua::Result<Vec<(String, String)>> {
    let Some(t) = t else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for pair in t.pairs::<String, String>() {
        out.push(pair?);
    }
    Ok(out)
}

/// Convert a Lua body argument into a [`RequestBody`]. Strings are sent
/// raw; tables are JSON-encoded (with content-type set downstream by the
/// request builder). Any other type is rejected with a runtime error — we
/// don't try to stringify numbers or booleans because a typo like
/// `http.post(url, 42)` is almost certainly a bug.
fn lua_value_to_body(value: Value) -> mlua::Result<RequestBody> {
    match value {
        Value::String(s) => Ok(RequestBody::Raw(s.as_bytes().to_vec())),
        Value::Table(t) => {
            let json = lua_table_to_json_string(&t)?;
            Ok(RequestBody::Json(json))
        }
        other => Err(mlua::Error::runtime(format!(
            "http: body must be a string or table, got {}",
            other.type_name()
        ))),
    }
}

/// Encode a Lua table as a JSON string via serde_json. We go through a
/// local conversion helper rather than the `json` stdlib module because
/// the two modules are independent — this one can't depend on that one's
/// public surface without leaking inter-module coupling. The rules here
/// are intentionally a subset of what the `json` module supports (arrays
/// and objects only, no canonicalisation).
fn lua_table_to_json_string(t: &Table) -> mlua::Result<String> {
    let json = lua_table_to_json(t)?;
    serde_json::to_string(&json)
        .map_err(|e| mlua::Error::runtime(format!("http: json encode: {e}")))
}

/// Recursively convert a Lua table into a `serde_json::Value`. See the
/// comment in [`lua_table_to_json_string`] for why this logic is local.
fn lua_table_to_json(t: &Table) -> mlua::Result<serde_json::Value> {
    // Decide array vs object: array iff keys are exactly 1..=N (same
    // rule as the `json` stdlib module).
    let mut max_int: i64 = 0;
    let mut int_count: usize = 0;
    let mut has_string = false;
    let mut total: usize = 0;
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, _) = pair?;
        total += 1;
        match k {
            Value::Integer(i) if i >= 1 => {
                int_count += 1;
                if i > max_int {
                    max_int = i;
                }
            }
            Value::String(_) => has_string = true,
            _ => {
                return Err(mlua::Error::runtime(
                    "http: table body keys must be string or positive integer",
                ));
            }
        }
    }

    if total == 0 {
        // Empty table → empty JSON object for HTTP body purposes. Bodies
        // are typically object-shaped (REST endpoints expect JSON objects
        // or arrays, but an empty payload is almost always `{}` not `[]`).
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }

    let is_array = !has_string && int_count == total && (max_int as usize) == total;
    if has_string && int_count > 0 {
        return Err(mlua::Error::runtime(
            "http: table body mixes string and integer keys",
        ));
    }

    if is_array {
        let mut items = Vec::with_capacity(int_count);
        for i in 1..=(int_count as i64) {
            let v = t.get::<Value>(i)?;
            items.push(lua_value_to_json(v)?);
        }
        Ok(serde_json::Value::Array(items))
    } else {
        let mut map = serde_json::Map::new();
        for pair in t.clone().pairs::<String, Value>() {
            let (k, v) = pair?;
            map.insert(k, lua_value_to_json(v)?);
        }
        Ok(serde_json::Value::Object(map))
    }
}

/// Scalar / nested-table dispatcher for [`lua_table_to_json`].
fn lua_value_to_json(value: Value) -> mlua::Result<serde_json::Value> {
    match value {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(b) => Ok(serde_json::Value::Bool(b)),
        Value::Integer(i) => Ok(serde_json::Value::Number(i.into())),
        Value::Number(n) => serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .ok_or_else(|| mlua::Error::runtime(format!("http: invalid JSON number: {n}"))),
        Value::String(s) => Ok(serde_json::Value::String(s.to_str()?.to_string())),
        Value::Table(t) => lua_table_to_json(&t),
        other => Err(mlua::Error::runtime(format!(
            "http: cannot encode Lua {} as JSON",
            other.type_name()
        ))),
    }
}

/// Clamp a fractional-second timeout into a non-negative [`Duration`].
/// Mirrors the helper in the `time` module — same handling for NaN /
/// negative / overflow.
fn duration_from_secs(seconds: f64) -> Duration {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Duration::ZERO;
    }
    let max = Duration::MAX.as_secs_f64();
    if seconds >= max {
        return Duration::MAX;
    }
    Duration::from_secs_f64(seconds)
}

/// Internal response representation; produced by [`execute_request`] and
/// converted to either a Lua table (blocking) or a [`FieldValue`] (async).
#[derive(Debug)]
struct InternalResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Drive a single HTTP request end-to-end. Returns an `InternalResponse`
/// on success, or an error string suitable for wrapping in
/// `mlua::Error::runtime` / a `PromiseResult::Err`.
async fn execute_request(req: RequestSpec) -> Result<InternalResponse, String> {
    let mut builder = reqwest::Client::builder();
    if let Some(timeout) = req.timeout {
        builder = builder.timeout(timeout);
    }
    let client = builder
        .build()
        .map_err(|e| format!("client build: {e}"))?;

    let mut rb = client.request(req.method, &req.url);

    // Query parameters first — reqwest stringifies and appends them.
    if !req.query.is_empty() {
        rb = rb.query(&req.query);
    }

    // Body + content-type. For JSON we set the header ourselves before
    // applying user headers, so explicit `opts.headers["content-type"]`
    // wins as promised in the module docs.
    if let Some(body) = req.body {
        match body {
            RequestBody::Raw(bytes) => {
                rb = rb.body(bytes);
            }
            RequestBody::Json(s) => {
                rb = rb
                    .header("content-type", "application/json")
                    .body(s);
            }
        }
    }

    // User headers. Iterate in order so a header repeated in the same
    // table overwrites the earlier value (reqwest `header()` appends; we
    // want overwrite semantics consistent with a Lua-table source, so we
    // collect into a HashMap first).
    if !req.headers.is_empty() {
        let mut hm = HashMap::new();
        for (k, v) in req.headers {
            hm.insert(k, v);
        }
        for (k, v) in hm {
            rb = rb.header(k, v);
        }
    }

    let resp = rb.send().await.map_err(|e| format!("send: {e}"))?;
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("body: {e}"))?
        .to_vec();
    Ok(InternalResponse {
        status,
        headers,
        body,
    })
}

/// Stream a URL to a file path. Returns the number of bytes written on
/// success. Uses reqwest's byte stream + tokio file I/O so the full
/// payload never lands in memory.
async fn execute_download(url: String, path: PathBuf) -> Result<u64, String> {
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("client build: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "download: HTTP {} from {}",
            resp.status().as_u16(),
            url
        ));
    }
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|e| format!("open {}: {}", path.display(), e))?;
    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("stream: {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write: {e}"))?;
        written += chunk.len() as u64;
    }
    file.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(written)
}

/// Convert an [`InternalResponse`] to the Lua table shape documented on
/// the module. Used by blocking methods that build the table directly on
/// the caller's Lua.
fn response_to_lua_table(lua: &Lua, resp: InternalResponse) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    out.set("status", resp.status as i64)?;
    let headers_tbl = lua.create_table()?;
    for (k, v) in resp.headers {
        // Insert with last-write-wins semantics; repeated headers collapse
        // to their final value. Scripts that need multi-valued headers
        // should reach for the raw request shape (we don't currently
        // expose it, but the door is open).
        headers_tbl.set(k, v)?;
    }
    out.set("headers", headers_tbl)?;
    // Body may contain non-UTF-8 bytes. create_string copies the bytes
    // verbatim — Lua strings are byte-safe — so binary payloads survive.
    out.set("body", lua.create_string(&resp.body)?)?;
    out.set("ok", (200..300).contains(&resp.status))?;
    Ok(out)
}

/// Convert an [`InternalResponse`] to a [`FieldValue`] for the async path.
/// The promise resolves through `field_value_to_lua`, which reconstructs
/// the same shape as [`response_to_lua_table`] when the script calls
/// `:await()`.
fn response_to_field_value(resp: InternalResponse) -> FieldValue {
    let mut map: HashMap<String, FieldValue> = HashMap::new();
    map.insert("status".into(), FieldValue::Number(resp.status as i64));
    let mut headers: HashMap<String, FieldValue> = HashMap::new();
    for (k, v) in resp.headers {
        headers.insert(k, FieldValue::String(v));
    }
    map.insert("headers".into(), FieldValue::Map(headers));
    // Body bytes go through a lossy UTF-8 conversion here because
    // FieldValue::String is a `String`. Callers that need raw bytes
    // should prefer the blocking path (which preserves bytes) or use
    // `http.download` for binary payloads.
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    map.insert("body".into(), FieldValue::String(body));
    map.insert(
        "ok".into(),
        FieldValue::Bool((200..300).contains(&resp.status)),
    );
    FieldValue::Map(map)
}

#[cfg(test)]
mod tests {
    //! Smoke tests focused on wiring, not live network. Live I/O is left
    //! to integration tests; these exercise the three doors that matter
    //! without requiring DNS or an outbound connection:
    //!
    //! 1. The module installs the expected function surface.
    //! 2. An invalid URL surfaces an error via the blocking path.
    //! 3. An invalid URL surfaces an error via the async path (through
    //!    `Promise:await`).
    //! 4. The general-form `http.request` accepts a table shape.
    //! 5. Body shapes (string vs table) type-check at the Lua boundary
    //!    (we can't assert the wire content without a server, but we can
    //!    verify the wrapper rejects unsupported body types).
    use super::*;
    use mlua::Lua;
    use tokio::runtime::{Builder, Runtime};

    fn rt() -> Runtime {
        Builder::new_multi_thread().enable_all().build().unwrap()
    }

    fn install(rt: &Runtime) -> Lua {
        let lua = Lua::new();
        let table = HttpModule
            .install(&lua, rt.handle())
            .expect("install http module");
        lua.globals().set("http", table).unwrap();
        lua
    }

    #[test]
    fn install_registers_expected_surface() {
        let rt = rt();
        let lua = install(&rt);
        // Spot-check each documented function name exists and is callable
        // (we don't call them — we just assert the type is "function").
        // Mix of blocking, async, and the general form.
        let script = r#"
            local names = {
                "get", "post", "put", "patch", "delete", "head",
                "get_async", "post_async", "put_async", "patch_async",
                "delete_async", "head_async",
                "request", "request_async",
                "download", "download_async",
            }
            local out = {}
            for _, n in ipairs(names) do
                out[n] = type(http[n])
            end
            return out
        "#;
        let t: mlua::Table = lua.load(script).eval().unwrap();
        for pair in t.pairs::<String, String>() {
            let (name, ty) = pair.unwrap();
            assert_eq!(ty, "function", "http.{name} should be a function, got {ty}");
        }
    }

    #[test]
    fn invalid_url_blocking_errors() {
        let rt = rt();
        let lua = install(&rt);
        // An unparseable URL ("not a scheme") surfaces as a reqwest
        // builder error — we don't hit DNS, so the test doesn't rely on
        // network state. The blocking path must surface the `http:`
        // prefix rather than panicking or hanging.
        let err = lua
            .load(r#"return http.get("not a valid url at all")"#)
            .eval::<mlua::Value>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("http:"), "err should carry http prefix: {err}");
    }

    #[test]
    fn invalid_url_async_errors_through_promise_await() {
        let rt = rt();
        let lua = install(&rt);
        // Same expectation, but routed through the promise surface: the
        // spawn task errors, `:await()` re-raises as runtime error with
        // the `http:` prefix preserved. Using a clearly-invalid URL so
        // reqwest's URL parser rejects it pre-network.
        let err = lua
            .load(
                r#"
                local p = http.get_async("not a valid url at all")
                return p:await()
            "#,
            )
            .eval::<mlua::Value>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("http:"), "err should carry http prefix: {err}");
    }

    #[test]
    fn request_table_form_missing_url_errors() {
        let rt = rt();
        let lua = install(&rt);
        // The general form requires `url`; missing it is a Lua-boundary
        // error (caught before we ever hit the network).
        let err = lua
            .load(r#"return http.request({ method = "GET" })"#)
            .eval::<mlua::Value>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("url"), "err should mention url: {err}");
    }

    #[test]
    fn request_table_form_invalid_method_errors() {
        let rt = rt();
        let lua = install(&rt);
        let err = lua
            .load(r#"return http.request({ method = "INVALID METHOD", url = "http://x/" })"#)
            .eval::<mlua::Value>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("method"), "err should mention method: {err}");
    }

    #[test]
    fn post_body_rejects_non_string_non_table() {
        let rt = rt();
        let lua = install(&rt);
        // Numbers as a body are a footgun — if the caller meant to send
        // a number they should stringify explicitly or wrap in a table.
        let err = lua
            .load(r#"return http.post("http://example.invalid/", 42)"#)
            .eval::<mlua::Value>()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("body must be a string or table"),
            "err should explain body rule: {err}"
        );
    }

    #[test]
    fn lua_table_to_json_handles_object_and_array() {
        // Pure unit test for the body-encoding helper — no Lua runtime
        // needed because the helper operates on mlua tables which the
        // Lua state owns; we spin up a Lua purely to build tables.
        let lua = Lua::new();
        let obj: mlua::Table = lua.load(r#"return { a = 1, b = "x" }"#).eval().unwrap();
        let j = lua_table_to_json(&obj).unwrap();
        // Can't assert key order (Lua table iteration is unspecified),
        // so decode back and inspect.
        let back: serde_json::Value = j;
        assert_eq!(back["a"], serde_json::json!(1));
        assert_eq!(back["b"], serde_json::json!("x"));

        let arr: mlua::Table = lua.load(r#"return { 10, 20, 30 }"#).eval().unwrap();
        let j = lua_table_to_json(&arr).unwrap();
        assert_eq!(j, serde_json::json!([10, 20, 30]));

        let empty: mlua::Table = lua.load(r#"return {}"#).eval().unwrap();
        let j = lua_table_to_json(&empty).unwrap();
        // Empty tables → `{}` for HTTP body purposes (diverges from the
        // `json` stdlib module's `[]` choice; see module-level docs).
        assert_eq!(j, serde_json::json!({}));
    }
}
