//! `console` stdlib module: structured logging to stderr.
//!
//! Methods installed under the `console` global:
//! - `console.log(...)` — alias of `info`, the most common call site.
//! - `console.debug(...)`
//! - `console.info(...)`
//! - `console.warn(...)`
//! - `console.error(...)`
//!
//! Each method takes zero or more arguments and writes a single line to
//! stderr, prefixed with the uppercase level name (e.g. `INFO connected 42
//! true`). Everything here is synchronous — the tokio [`Handle`] passed to
//! [`LuamlStdlibModule::install`] is ignored.
//!
//! ## Formatting depth
//!
//! Scalar values (strings, numbers, booleans, nil) render as themselves.
//! Tables are rendered one level deep as space-separated `key=value` pairs —
//! nested tables appear as the opaque token `<table>` rather than being
//! recursively expanded. This keeps log lines one-line-per-call and avoids
//! accidentally dumping huge structures; a script that wants deeper rendering
//! should serialize with `json.encode` (a future stdlib module) before
//! logging. Functions/userdata/threads render as `<function>` / `<userdata>`
//! / `<thread>` — they have no meaningful string form and we refuse to call
//! `tostring` on them (which could invoke `__tostring` metamethods with
//! unpredictable side effects from a logging call).
//!
//! No structured sink, no timestamp, no colour. The idea is to be the minimum
//! viable `printf`-to-stderr that a script can reach for. A richer logging
//! module (JSON, fields, filters) belongs in a downstream consumer; this one
//! stays dependency-free and synchronous so it's always safe to call.

use std::io::{self, Write};

use mlua::{Lua, Table, Value, Variadic};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;

/// Zero-sized module handle. Registration happens in `mod.rs` under the
/// `stdlib-console` feature flag; this type exists only to implement the
/// trait.
pub struct ConsoleModule;

impl LuamlStdlibModule for ConsoleModule {
    fn namespace(&self) -> &'static str {
        "console"
    }

    fn install(&self, lua: &Lua, _rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // console.log(...) — info-level alias. Named `log` to match the
        // conventional JavaScript/devtools API so scripts don't have to
        // memorise a new default level. Writes `INFO ...` to stderr.
        table.set(
            "log",
            lua.create_function(|_, args: Variadic<Value>| {
                emit("INFO", args);
                Ok(())
            })?,
        )?;

        // console.debug(...) — lowest level. Prefixed `DEBUG`.
        table.set(
            "debug",
            lua.create_function(|_, args: Variadic<Value>| {
                emit("DEBUG", args);
                Ok(())
            })?,
        )?;

        // console.info(...) — informational, same severity as `log`.
        table.set(
            "info",
            lua.create_function(|_, args: Variadic<Value>| {
                emit("INFO", args);
                Ok(())
            })?,
        )?;

        // console.warn(...) — warning level. Prefixed `WARN`.
        table.set(
            "warn",
            lua.create_function(|_, args: Variadic<Value>| {
                emit("WARN", args);
                Ok(())
            })?,
        )?;

        // console.error(...) — highest level. Prefixed `ERROR`. Still just a
        // stderr write — it does not raise or abort the script.
        table.set(
            "error",
            lua.create_function(|_, args: Variadic<Value>| {
                emit("ERROR", args);
                Ok(())
            })?,
        )?;

        Ok(table)
    }
}

/// Build the wire-format string for a log line. Factored out of the handlers
/// so tests can assert on output without touching stderr.
///
/// Returns `"<LEVEL>"` when called with zero args (the level prefix alone is
/// still useful as a "trace I reached here" marker) and `"<LEVEL> a b c"`
/// with the values joined by a single space.
pub(crate) fn render_message(level: &str, args: Variadic<Value>) -> String {
    let body = format_varargs(args);
    if body.is_empty() {
        level.to_string()
    } else {
        format!("{level} {body}")
    }
}

/// Write a fully-rendered log line to stderr. Errors from the write
/// (broken pipe, EBADF on a detached terminal) are intentionally swallowed —
/// a logging call must never fail a script. We use `writeln!` onto a locked
/// stderr handle rather than `eprintln!` so we can drop the error explicitly
/// instead of relying on the std macro's panic-on-broken-pipe behaviour.
fn emit(level: &str, args: Variadic<Value>) {
    let line = render_message(level, args);
    let stderr = io::stderr();
    let mut lock = stderr.lock();
    let _ = writeln!(lock, "{line}");
}

/// Render a varargs list as a single space-separated string. See module docs
/// for the rendering rules per value kind.
fn format_varargs(args: Variadic<Value>) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(args.len());
    for v in args.into_iter() {
        parts.push(render_value(&v));
    }
    parts.join(" ")
}

/// Render a single Lua value as a string using the rules documented at the
/// module level. Scalars stringify naturally; tables render one level deep;
/// non-representable kinds render to a `<kind>` token.
fn render_value(v: &Value) -> String {
    match v {
        Value::Nil => "nil".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        // LightUserData has no meaningful textual form; surface the same
        // opaque token we use for full userdata so scripts don't get a
        // pointer address leaking into logs.
        Value::LightUserData(_) => "<userdata>".to_string(),
        Value::String(s) => s.to_str().map(|c| c.to_string()).unwrap_or_else(|_| {
            // Non-utf8 Lua strings are rare but legal. Fall back to a debug
            // repr rather than panicking or silently dropping bytes.
            format!("{:?}", s.as_bytes())
        }),
        Value::Table(t) => render_table_one_level(t),
        Value::Function(_) => "<function>".to_string(),
        Value::Thread(_) => "<thread>".to_string(),
        Value::UserData(_) => "<userdata>".to_string(),
        Value::Error(e) => format!("<error: {e}>"),
        // mlua's Value enum may grow new variants in future minor versions;
        // a catchall here keeps us forward-compatible without a build break.
        _ => "<unknown>".to_string(),
    }
}

/// Walk a table's pairs once and render each entry as `key=value`, joined by
/// spaces. Nested tables inside this table render as `<table>` — we do not
/// recurse. Iteration order is whatever `pairs()` yields, which is
/// unspecified by Lua for mixed integer/string keys; that's acceptable for a
/// logging call.
fn render_table_one_level(t: &Table) -> String {
    let mut parts: Vec<String> = Vec::new();
    // `for_each` iterates every pair and swallows no errors — if an entry
    // can't be fetched (e.g. a poisoned metamethod), we log the rendering
    // failure inline rather than aborting the whole log call.
    let result = t.clone().pairs::<Value, Value>().try_for_each(|pair| {
        let (k, val) = pair?;
        let key_s = render_scalar_key(&k);
        let val_s = render_nested_value(&val);
        parts.push(format!("{key_s}={val_s}"));
        Ok::<(), mlua::Error>(())
    });
    if let Err(e) = result {
        parts.push(format!("<render-error: {e}>"));
    }
    format!("{{{}}}", parts.join(" "))
}

/// Like `render_value` but collapses any nested table/userdata/function to
/// an opaque token. Used when rendering the *values* of a one-level table so
/// we never recurse.
fn render_nested_value(v: &Value) -> String {
    match v {
        Value::Table(_) => "<table>".to_string(),
        other => render_value(other),
    }
}

/// Render a value used as a table key. Keys that aren't a natural scalar
/// render to a short placeholder — a table-as-key in a log line is pretty
/// much always a bug, but we shouldn't panic on it.
fn render_scalar_key(v: &Value) -> String {
    match v {
        Value::Table(_) => "<table-key>".to_string(),
        Value::Function(_) => "<function-key>".to_string(),
        Value::Thread(_) => "<thread-key>".to_string(),
        Value::UserData(_) | Value::LightUserData(_) => "<userdata-key>".to_string(),
        other => render_value(other),
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests asserting on `render_message` output. We deliberately do
    //! not try to capture stderr — redirecting `io::stderr` is flaky across
    //! platforms and irrelevant to the module's responsibilities (the
    //! formatter is the interesting part; the `writeln!` is trivial).
    use super::*;
    use mlua::Lua;

    /// Build a Variadic by evaluating a Lua expression that returns multiple
    /// values. Makes the test bodies read closer to the call sites they're
    /// simulating.
    fn varargs_from_lua(lua: &Lua, expr: &str) -> Variadic<Value> {
        lua.load(format!("return {expr}"))
            .eval::<Variadic<Value>>()
            .expect("eval varargs")
    }

    #[test]
    fn render_info_with_scalars() {
        let lua = Lua::new();
        let args = varargs_from_lua(&lua, r#""connected", 42, true"#);
        assert_eq!(render_message("INFO", args), "INFO connected 42 true");
    }

    #[test]
    fn render_includes_nil_literally() {
        let lua = Lua::new();
        let args = varargs_from_lua(&lua, r#""a", nil, "b""#);
        // Lua's Variadic collapses trailing nils but preserves middle ones,
        // which matches what a human reading the log line would want.
        assert_eq!(render_message("DEBUG", args), "DEBUG a nil b");
    }

    #[test]
    fn render_empty_varargs_is_level_only() {
        let lua = Lua::new();
        // A bare `console.log()` call — zero args. We still want the prefix
        // so a "trace reached here" style marker is visible.
        let args = varargs_from_lua(&lua, "");
        assert_eq!(render_message("WARN", args), "WARN");
    }

    #[test]
    fn render_table_one_level_with_scalars() {
        let lua = Lua::new();
        // Use string keys and fully deterministic values so we can assert on
        // the rendered string after sorting. Lua pair iteration order for
        // string keys is implementation-defined; we normalise here.
        let args = varargs_from_lua(&lua, r#"{ a = 1, b = "two", c = true }"#);
        let rendered = render_message("INFO", args);
        assert!(
            rendered.starts_with("INFO {"),
            "expected braces wrapper, got: {rendered}"
        );
        assert!(rendered.contains("a=1"), "missing a=1: {rendered}");
        assert!(rendered.contains("b=two"), "missing b=two: {rendered}");
        assert!(rendered.contains("c=true"), "missing c=true: {rendered}");
    }

    #[test]
    fn render_nested_table_collapses_to_token() {
        let lua = Lua::new();
        // The outer table has a nested table value — the nested table must
        // render as `<table>`, not be recursively expanded.
        let args = varargs_from_lua(&lua, r#"{ outer = { inner = 1 } }"#);
        let rendered = render_message("ERROR", args);
        assert!(
            rendered.contains("outer=<table>"),
            "nested table should collapse: {rendered}"
        );
        assert!(
            !rendered.contains("inner"),
            "should not recurse into nested table: {rendered}"
        );
    }

    #[test]
    fn render_function_and_userdata_tokens() {
        let lua = Lua::new();
        let args = varargs_from_lua(&lua, r#""label", function() end"#);
        let rendered = render_message("DEBUG", args);
        assert_eq!(rendered, "DEBUG label <function>");
    }

    #[test]
    fn render_negative_and_float_numbers() {
        let lua = Lua::new();
        let args = varargs_from_lua(&lua, "-3, 1.5");
        let rendered = render_message("INFO", args);
        // We don't pin the exact float formatting (Lua may hand us 1.5 as a
        // number or an integer path depending on the VM build) — just check
        // both tokens are present.
        assert!(rendered.starts_with("INFO -3 "), "got: {rendered}");
        assert!(rendered.contains("1.5"), "got: {rendered}");
    }

    #[test]
    fn install_attaches_all_methods() {
        // End-to-end sanity: the module installs under "console" and every
        // documented method is a callable. We don't check their output here
        // (that would require redirecting stderr); `render_message` tests
        // cover the formatting contract. What we assert is that calling each
        // method doesn't raise — a crash-free path is the minimum bar.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let lua = Lua::new();
        let table = ConsoleModule
            .install(&lua, rt.handle())
            .expect("install console module");
        lua.globals().set("console", table).unwrap();
        lua.load(
            r#"
            console.log("log-message")
            console.debug("debug-message", 1)
            console.info("info-message")
            console.warn("warn-message")
            console.error("error-message", { k = "v" })
            "#,
        )
        .exec()
        .expect("all console methods should execute without error");
    }
}
