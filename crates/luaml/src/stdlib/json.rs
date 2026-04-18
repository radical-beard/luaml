//! `json` stdlib module: synchronous JSON encode/decode.
//!
//! Exposes three Lua functions on the bare `json` namespace:
//!
//!   - `json.encode(value, {pretty=false}?) -> string` — serialize a Lua value.
//!   - `json.decode(s) -> value` — parse a JSON string into a Lua value.
//!   - `json.encode_canonical(value) -> string` — deterministic encoder with
//!     recursively-sorted object keys and no whitespace, suitable for
//!     signatures and content-addressed hashing.
//!
//! All three methods are synchronous (no `Promise`). The module uses
//! `serde_json::Value` as an intermediate representation — Lua → `JsonValue` →
//! string and back — so that every encode/decode path shares the same
//! conversion rules.
//!
//! ## Lua ↔ JSON mapping
//!
//! | Lua                                  | JSON    |
//! |--------------------------------------|---------|
//! | `nil`                                | `null`  |
//! | `boolean`                            | bool    |
//! | integer                              | number  |
//! | float                                | number  |
//! | string                               | string  |
//! | table (1..N consecutive int keys)    | array   |
//! | table (string keys)                  | object  |
//!
//! ### Array vs object detection
//!
//! A Lua table encodes as a JSON **array** iff every key is an integer and the
//! keys form the contiguous range `1..=N` (1-indexed, no gaps, no duplicates).
//! Otherwise the table encodes as a JSON **object** with every key coerced to a
//! string. Any non-integer, non-string key (e.g. a boolean or table key) or a
//! table that mixes integers and strings outside the contiguous-1..N shape is
//! rejected with `mlua::Error::runtime("invalid JSON object key: ...")`.
//!
//! The empty table is always encoded as `[]` (empty array). This is a
//! judgment call — JSON has two distinct empty collections and Lua has one. We
//! pick array because it round-trips identically (decoding `[]` produces an
//! empty Lua table).
//!
//! ### NaN / Infinity
//!
//! JSON has no representation for `NaN`, `+Infinity`, or `-Infinity`.
//! `serde_json::Number::from_f64` returns `None` for these values and this
//! module surfaces that as `mlua::Error::runtime("invalid JSON number: ...")`
//! rather than silently emitting `null` or a non-standard token. Scripts that
//! need to send non-finite numbers across a JSON boundary must encode them
//! explicitly (e.g. as the string `"NaN"`).

use mlua::{Lua, Table, Value};
use serde_json::Value as JsonValue;
use tokio::runtime::Handle;

use super::LuamlStdlibModule;

/// Stateless stdlib module installer for the `json` namespace.
pub struct JsonModule;

impl LuamlStdlibModule for JsonModule {
    fn namespace(&self) -> &'static str {
        "json"
    }

    fn install(&self, lua: &Lua, _rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // json.encode(value, {pretty=false}?) -> string
        let encode_fn = lua.create_function(|_, (value, opts): (Value, Option<Table>)| {
            let pretty = match opts {
                Some(t) => t.get::<Option<bool>>("pretty")?.unwrap_or(false),
                None => false,
            };
            let json = lua_to_json(&value)?;
            let out = if pretty {
                serde_json::to_string_pretty(&json)
                    .map_err(|e| mlua::Error::runtime(format!("json encode error: {e}")))?
            } else {
                serde_json::to_string(&json)
                    .map_err(|e| mlua::Error::runtime(format!("json encode error: {e}")))?
            };
            Ok(out)
        })?;
        table.set("encode", encode_fn)?;

        // json.decode(s) -> value
        let decode_fn = lua.create_function(|lua, s: String| {
            let json: JsonValue = serde_json::from_str(&s)
                .map_err(|e| mlua::Error::runtime(format!("json decode error: {e}")))?;
            json_to_lua(lua, &json)
        })?;
        table.set("decode", decode_fn)?;

        // json.encode_canonical(value) -> string
        let encode_canonical_fn = lua.create_function(|_, value: Value| {
            let json = lua_to_json(&value)?;
            let canonical = canonicalize(json);
            // Canonical form has no whitespace, so `to_string` (compact)
            // emits exactly the form we want.
            serde_json::to_string(&canonical)
                .map_err(|e| mlua::Error::runtime(format!("json encode error: {e}")))
        })?;
        table.set("encode_canonical", encode_canonical_fn)?;

        Ok(table)
    }
}

/// Convert a Lua [`Value`] into a [`JsonValue`].
///
/// Rejects non-finite floats and tables with mixed/non-string keys. Functions,
/// threads, and userdata are also rejected — there is no sensible JSON
/// representation for them.
fn lua_to_json(value: &Value) -> mlua::Result<JsonValue> {
    match value {
        Value::Nil => Ok(JsonValue::Null),
        Value::Boolean(b) => Ok(JsonValue::Bool(*b)),
        Value::Integer(i) => Ok(JsonValue::Number((*i).into())),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(JsonValue::Number)
            .ok_or_else(|| mlua::Error::runtime(format!("invalid JSON number: {n}"))),
        Value::String(s) => Ok(JsonValue::String(s.to_str()?.to_string())),
        Value::Table(t) => lua_table_to_json(t),
        other => Err(mlua::Error::runtime(format!(
            "cannot encode Lua value as JSON: {}",
            other.type_name()
        ))),
    }
}

/// Convert a Lua [`Table`] into a JSON array or object, deciding based on the
/// key shape.
fn lua_table_to_json(t: &Table) -> mlua::Result<JsonValue> {
    // First pass: decide whether the table is an array (keys are exactly
    // 1..=N) or an object. We have to walk the table twice because mlua's
    // `pairs` iteration order is unspecified — we can't rely on encountering
    // keys in order.
    let mut max_int: i64 = 0;
    let mut int_count: usize = 0;
    let mut has_non_int = false;

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
                    "invalid JSON object key: integer {i} (must be >= 1 for array or string for object)"
                )));
            }
            Value::Number(n) => {
                // Accept a float that is an exact positive integer (mlua may
                // hand us `1.0` where Lua code wrote `1`). Otherwise reject —
                // JSON object keys must be strings.
                if n.is_finite() && n.fract() == 0.0 && *n >= 1.0 {
                    let i = *n as i64;
                    int_count += 1;
                    if i > max_int {
                        max_int = i;
                    }
                } else {
                    return Err(mlua::Error::runtime(format!(
                        "invalid JSON object key: non-integer number {n}"
                    )));
                }
            }
            Value::String(_) => {
                has_non_int = true;
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "invalid JSON object key: {} (must be string or positive integer)",
                    other.type_name()
                )));
            }
        }
    }

    let total: usize = t.clone().pairs::<Value, Value>().count();

    if total == 0 {
        // Empty table → empty array. Judgment call documented at module level.
        return Ok(JsonValue::Array(Vec::new()));
    }

    let is_array = !has_non_int && int_count == total && (max_int as usize) == total;

    if has_non_int && int_count > 0 {
        return Err(mlua::Error::runtime(
            "invalid JSON object key: table mixes integer and string keys",
        ));
    }

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
                        "invalid JSON object key: {} (object requires string keys)",
                        other.type_name()
                    )));
                }
            };
            obj.insert(key, lua_to_json(&v)?);
        }
        Ok(JsonValue::Object(obj))
    }
}

/// Convert a [`JsonValue`] into a Lua [`Value`]. Arrays become 1-indexed
/// tables; objects become string-keyed tables; numbers become integers when
/// they round-trip losslessly, otherwise floats.
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
                Err(mlua::Error::runtime(format!(
                    "JSON number out of range: {n}"
                )))
            }
        }
        JsonValue::String(s) => Ok(Value::String(lua.create_string(s)?)),
        JsonValue::Array(arr) => {
            let table = lua.create_table()?;
            for (i, item) in arr.iter().enumerate() {
                table.set(i as i64 + 1, json_to_lua(lua, item)?)?;
            }
            Ok(Value::Table(table))
        }
        JsonValue::Object(obj) => {
            let table = lua.create_table()?;
            for (k, v) in obj {
                table.set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

/// Recursively sort object keys. Arrays keep element order; numbers, strings,
/// bools, and null are returned unchanged. `serde_json::Map` ships with the
/// `preserve_order` feature disabled by default, so inserting in sorted order
/// produces a map that also serializes in sorted order.
fn canonicalize(value: JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(obj) => {
            let mut entries: Vec<(String, JsonValue)> = obj.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                sorted.insert(k, canonicalize(v));
            }
            JsonValue::Object(sorted)
        }
        JsonValue::Array(arr) => JsonValue::Array(arr.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    fn install() -> (tokio::runtime::Runtime, Lua) {
        let rt = Builder::new_multi_thread().enable_all().build().unwrap();
        let lua = Lua::new();
        let table = JsonModule
            .install(&lua, &rt.handle().clone())
            .expect("install json module");
        lua.globals().set("json", table).expect("set json global");
        (rt, lua)
    }

    #[test]
    fn encode_nested_table() {
        let (_rt, lua) = install();
        let out: String = lua
            .load(
                r#"
                return json.encode({ name = "ada", scores = {1, 2, 3}, meta = { active = true } })
                "#,
            )
            .eval()
            .expect("encode");
        // Can't assert exact output because serde_json::Map key order is
        // insertion order and Lua `pairs` order is unspecified. Assert that
        // the result is valid JSON with the expected shape instead.
        let parsed: JsonValue = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed["name"], JsonValue::from("ada"));
        assert_eq!(parsed["scores"], serde_json::json!([1, 2, 3]));
        assert_eq!(parsed["meta"]["active"], JsonValue::Bool(true));
    }

    #[test]
    fn decode_json_object() {
        let (_rt, lua) = install();
        let (name, age, active): (String, i64, bool) = lua
            .load(
                r#"
                local t = json.decode('{"name":"grace","age":85,"active":true}')
                return t.name, t.age, t.active
                "#,
            )
            .eval()
            .expect("decode object");
        assert_eq!(name, "grace");
        assert_eq!(age, 85);
        assert!(active);
    }

    #[test]
    fn decode_json_array() {
        let (_rt, lua) = install();
        let (len, first, third): (i64, String, String) = lua
            .load(
                r#"
                local t = json.decode('["a","b","c"]')
                return #t, t[1], t[3]
                "#,
            )
            .eval()
            .expect("decode array");
        assert_eq!(len, 3);
        assert_eq!(first, "a");
        assert_eq!(third, "c");
    }

    #[test]
    fn round_trip_mixed_values() {
        let (_rt, lua) = install();
        // Numbers, strings, bools, null, nested — everything round-trips via
        // decode(encode(x)).
        let ok: bool = lua
            .load(
                r#"
                local original = {
                    n = 42,
                    f = 3.5,
                    s = "hello",
                    t = true,
                    f2 = false,
                    nada = nil,
                    nested = { deep = { list = {10, 20, 30} } },
                }
                local s = json.encode(original)
                local decoded = json.decode(s)
                return decoded.n == 42
                   and decoded.f == 3.5
                   and decoded.s == "hello"
                   and decoded.t == true
                   and decoded.f2 == false
                   and decoded.nada == nil
                   and decoded.nested.deep.list[1] == 10
                   and decoded.nested.deep.list[2] == 20
                   and decoded.nested.deep.list[3] == 30
                "#,
            )
            .eval()
            .expect("round trip");
        assert!(ok);
    }

    #[test]
    fn encode_canonical_sorts_keys_recursively() {
        let (_rt, lua) = install();
        // Build a table where the insertion order is definitely not sorted
        // and check that canonical output is sorted at every level.
        let out: String = lua
            .load(
                r#"
                local t = { zebra = 1, apple = 2, middle = { zulu = "z", alpha = "a" } }
                return json.encode_canonical(t)
                "#,
            )
            .eval()
            .expect("canonical");
        assert_eq!(
            out,
            r#"{"apple":2,"middle":{"alpha":"a","zulu":"z"},"zebra":1}"#
        );
    }

    #[test]
    fn encode_pretty_option_produces_whitespace() {
        let (_rt, lua) = install();
        let pretty: String = lua
            .load(r#"return json.encode({a = 1}, { pretty = true })"#)
            .eval()
            .expect("pretty");
        let compact: String = lua
            .load(r#"return json.encode({a = 1})"#)
            .eval()
            .expect("compact");
        assert!(
            pretty.contains('\n'),
            "pretty form should contain newlines, got {pretty:?}"
        );
        assert!(
            !compact.contains('\n'),
            "compact form should not contain newlines, got {compact:?}"
        );
        // Both must decode to the same value.
        let p: JsonValue = serde_json::from_str(&pretty).unwrap();
        let c: JsonValue = serde_json::from_str(&compact).unwrap();
        assert_eq!(p, c);
    }

    #[test]
    fn encode_array_shape() {
        // Contiguous 1..N integer keys → JSON array, not an object.
        let (_rt, lua) = install();
        let out: String = lua
            .load(r#"return json.encode({10, 20, 30})"#)
            .eval()
            .expect("array");
        assert_eq!(out, "[10,20,30]");
    }

    #[test]
    fn encode_rejects_non_finite_number() {
        // NaN / Infinity have no JSON representation; we surface a runtime
        // error rather than silently emitting `null` or a non-standard token.
        let (_rt, lua) = install();
        let err = lua
            .load(r#"return json.encode(0/0)"#)
            .eval::<String>()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("invalid JSON number"),
            "error should mention invalid number, got {err}"
        );
    }

    #[test]
    fn encode_rejects_mixed_key_table() {
        let (_rt, lua) = install();
        let err = lua
            .load(r#"return json.encode({ [1] = "a", name = "b" })"#)
            .eval::<String>()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("invalid JSON object key"),
            "error should flag mixed keys, got {err}"
        );
    }

    #[test]
    fn decode_null_becomes_nil() {
        let (_rt, lua) = install();
        let is_nil: bool = lua
            .load(r#"return json.decode('null') == nil"#)
            .eval()
            .expect("decode null");
        assert!(is_nil);
    }
}
