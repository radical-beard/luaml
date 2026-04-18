use std::sync::{Arc, Mutex};

use mlua::{Lua, Result as LuaResult, Value as LuaValue};

use crate::api::{ApiBindingEntry, ApiHandler};
use crate::clause::Clause;
use crate::error::LuamlError;
use crate::pattern_match::match_fields;
use crate::types::{FieldBindings, FieldMap, FieldValue};

/// Execute a clause with bindings in a Lua environment.
///
/// - Injects pattern-bound variables as locals on the clause's env table.
/// - Creates API proxy tables for matching `ApiBindingSpec`s.
/// - Injects the always-on `luaml` namespace (currently: `luaml.dispatch` and
///   `luaml.enum`). These do not participate in matching.
/// - Runs the Lua body wrapped in an IIFE (so `return` exits the inner
///   function instead of the chunk).
///
/// Returns the ordered list of `FieldMap`s the script enqueued via
/// `luaml.dispatch(...)`. Dispatches are Rust-side queued — the Lua call only
/// appends. The engine drains this list after the clause returns cleanly and
/// feeds each entry into its cascade loop.
pub(crate) fn execute_clause(
    lua: &Lua,
    clause: &Clause,
    bindings: &FieldBindings,
    api_bindings: &[ApiBindingEntry],
    local_api_bindings: &[crate::api::LocalApiBindingEntry],
) -> Result<Vec<FieldMap>, LuamlError> {
    let env = lua.create_table()?;
    let mt = lua.create_table()?;
    let globals = lua.globals();
    mt.set("__index", globals.clone())?;
    mt.set("__newindex", globals)?;
    env.set_metatable(Some(mt))?;

    for (name, value) in bindings {
        env.set(name.as_str(), field_value_to_lua(lua, value)?)?;
    }

    let policy_as_fieldmap = clause_policy_to_fieldmap(clause);
    for entry in api_bindings {
        let spec = &entry.spec;
        if spec.pattern.is_empty() || match_fields(&spec.pattern, &policy_as_fieldmap).is_some() {
            inject_api_namespace(lua, &env, &spec.namespace, spec.handler.clone())?;
        }
    }
    for entry in local_api_bindings {
        let spec = &entry.spec;
        if spec.pattern.is_empty() || match_fields(&spec.pattern, &policy_as_fieldmap).is_some() {
            let table = (spec.builder)(lua)?;
            install_namespace_table(lua, &env, &spec.namespace, table)?;
        }
    }

    // Always-on `luaml` namespace. `luaml.dispatch(t)` appends `t` to a
    // Rust-owned Mutex<Vec<FieldMap>>; the engine drains it after the body
    // returns and feeds each entry into the cascade loop.
    let emissions: Arc<Mutex<Vec<FieldMap>>> = Arc::new(Mutex::new(Vec::new()));
    let luaml_ns = lua.create_table()?;
    let emissions_ref = emissions.clone();
    let dispatch_fn = lua.create_function(move |_, t: mlua::Table| -> LuaResult<()> {
        let fm = lua_table_to_fieldmap(&t)?;
        emissions_ref.lock().unwrap().push(fm);
        Ok(())
    })?;
    luaml_ns.set("dispatch", dispatch_fn)?;
    // `luaml.enum(name)` wraps a string as a `FieldValue::Enum` so it can be
    // written into a dispatch table with enum identity preserved (Lua strings
    // otherwise become `FieldValue::String`, which would fail to match
    // `Pattern::Enum(_)` in downstream clauses).
    let enum_fn = lua.create_function(|_, s: String| Ok(LuaEnum(s)))?;
    luaml_ns.set("enum", enum_fn)?;
    env.set("luaml", luaml_ns)?;

    let wrapped = format!("(function()\n{}\nend)()", clause.behavior.lua_source);

    lua.load(&wrapped).set_environment(env).exec()?;

    let out = emissions.lock().unwrap().clone();
    Ok(out)
}

/// A Lua userdata that carries an enum value — produced by `luaml.enum(name)`
/// and recognized by [`lua_value_to_field_value`] so it round-trips as
/// `FieldValue::Enum` instead of `FieldValue::String`.
#[derive(Clone, Debug)]
struct LuaEnum(String);
impl mlua::UserData for LuaEnum {}

/// Convert a Lua table to a FieldMap, recognizing `LuaEnum` userdata values.
fn lua_table_to_fieldmap(t: &mlua::Table) -> LuaResult<FieldMap> {
    let mut map = FieldMap::new();
    for pair in t.clone().pairs::<String, LuaValue>() {
        let (k, v) = pair?;
        map.insert(k, lua_value_to_field_value(&v));
    }
    Ok(map)
}

/// Convert clause execution policy patterns to a FieldMap for API pattern matching.
/// Only literal patterns (Enum, StringLiteral, NumberLiteral, BoolLiteral) are converted;
/// variables, wildcards, etc. are skipped since they don't have a fixed value.
fn clause_policy_to_fieldmap(clause: &Clause) -> FieldMap {
    let mut map = FieldMap::new();
    for (key, pattern) in &clause.policy.fields {
        match pattern {
            crate::pattern::Pattern::Enum(s) => {
                map.insert(key.clone(), FieldValue::Enum(s.clone()));
            }
            crate::pattern::Pattern::StringLiteral(s) => {
                map.insert(key.clone(), FieldValue::String(s.clone()));
            }
            crate::pattern::Pattern::NumberLiteral(n) => {
                map.insert(key.clone(), FieldValue::Number(*n));
            }
            crate::pattern::Pattern::BoolLiteral(b) => {
                map.insert(key.clone(), FieldValue::Bool(*b));
            }
            // Variables, wildcards, pins, lists, maps don't have fixed values
            _ => {}
        }
    }
    map
}

/// Install a pre-built namespace table at the dotted path, creating any
/// intermediate tables as needed. Used by local-mode builders whose closure
/// returns a fully-populated `mlua::Table` directly.
fn install_namespace_table(
    lua: &Lua,
    env: &mlua::Table,
    namespace: &str,
    table: mlua::Table,
) -> LuaResult<()> {
    let parts: Vec<&str> = namespace.split('.').collect();
    let mut current_table = env.clone();
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            current_table.set(*part, table)?;
            return Ok(());
        }
        let next: mlua::Table = match current_table.get::<mlua::Value>(*part)? {
            mlua::Value::Table(t) => t,
            _ => {
                let t = lua.create_table()?;
                current_table.set(*part, t.clone())?;
                t
            }
        };
        current_table = next;
    }
    Ok(())
}

/// Inject an API namespace into the Lua environment.
/// For namespace "foo.bar", creates nested tables: env.foo.bar = proxy_table.
/// The proxy table uses __index to intercept method calls and route them through ApiHandler.
fn inject_api_namespace(
    lua: &Lua,
    env: &mlua::Table,
    namespace: &str,
    handler: Arc<dyn ApiHandler>,
) -> LuaResult<()> {
    let parts: Vec<&str> = namespace.split('.').collect();

    // Navigate/create nested tables for the namespace path.
    let mut current_table = env.clone();
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // Last part: create the proxy table with __index metatable.
            let proxy = create_api_proxy(lua, namespace, handler.clone())?;
            current_table.set(*part, proxy)?;
        } else {
            // Intermediate part: create or get existing table.
            let next: mlua::Table = match current_table.get::<mlua::Value>(*part)? {
                mlua::Value::Table(t) => t,
                _ => {
                    let t = lua.create_table()?;
                    current_table.set(*part, t.clone())?;
                    t
                }
            };
            current_table = next;
        }
    }

    Ok(())
}

/// Create a proxy table that intercepts method calls and routes them through ApiHandler.
fn create_api_proxy(
    lua: &Lua,
    namespace: &str,
    handler: Arc<dyn ApiHandler>,
) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    let mt = lua.create_table()?;

    let ns = namespace.to_string();
    let handler_clone = handler.clone();

    // __index: when accessing table.method, return a function that calls the handler.
    mt.set(
        "__index",
        lua.create_function(move |lua, (_, method_name): (mlua::Value, String)| {
            let ns = ns.clone();
            let handler = handler_clone.clone();
            lua.create_function(move |lua, args: mlua::MultiValue| {
                let field_args: Vec<FieldValue> = args
                    .into_iter()
                    .map(|v| lua_value_to_field_value(&v))
                    .collect();

                match handler.call(&ns, &method_name, field_args) {
                    Ok(result) => field_value_to_lua(lua, &result),
                    Err(e) => Err(mlua::Error::RuntimeError(format!(
                        "{}.{}: {}",
                        ns, method_name, e.message
                    ))),
                }
            })
        })?,
    )?;

    table.set_metatable(Some(mt))?;
    Ok(table)
}

/// Convert a FieldValue to a Lua value.
pub fn field_value_to_lua(lua: &Lua, value: &FieldValue) -> LuaResult<LuaValue> {
    match value {
        FieldValue::Enum(s) => Ok(LuaValue::String(lua.create_string(s)?)),
        FieldValue::String(s) => Ok(LuaValue::String(lua.create_string(s)?)),
        FieldValue::Number(n) => Ok(LuaValue::Integer(*n)),
        FieldValue::Float(f) => Ok(LuaValue::Number(*f)),
        FieldValue::Bool(b) => Ok(LuaValue::Boolean(*b)),
        FieldValue::Null => Ok(LuaValue::Nil),
        FieldValue::List(items) => {
            let table = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                table.set(i + 1, field_value_to_lua(lua, item)?)?;
            }
            Ok(LuaValue::Table(table))
        }
        FieldValue::Map(map) => {
            let table = lua.create_table()?;
            for (key, val) in map {
                table.set(key.as_str(), field_value_to_lua(lua, val)?)?;
            }
            Ok(LuaValue::Table(table))
        }
    }
}

/// Convert a Lua value to a FieldValue.
pub fn lua_value_to_field_value(value: &LuaValue) -> FieldValue {
    match value {
        LuaValue::Nil => FieldValue::Null,
        LuaValue::Boolean(b) => FieldValue::Bool(*b),
        LuaValue::Integer(n) => FieldValue::Number(*n),
        LuaValue::Number(f) => FieldValue::Float(*f),
        LuaValue::UserData(ud) => {
            if let Ok(e) = ud.borrow::<LuaEnum>() {
                return FieldValue::Enum(e.0.clone());
            }
            FieldValue::Null
        }
        LuaValue::String(s) => {
            FieldValue::String(s.to_str().map(|s| s.to_string()).unwrap_or_default())
        }
        LuaValue::Table(t) => {
            // Check if it's an array (sequential integer keys starting at 1).
            let len = t.raw_len();
            if len > 0 {
                let mut items = Vec::new();
                for i in 1..=len {
                    if let Ok(v) = t.get::<LuaValue>(i) {
                        items.push(lua_value_to_field_value(&v));
                    }
                }
                FieldValue::List(items)
            } else {
                let mut map = FieldMap::new();
                if let Ok(pairs) = t.pairs::<String, LuaValue>().collect::<Result<Vec<_>, _>>() {
                    for (k, v) in pairs {
                        map.insert(k, lua_value_to_field_value(&v));
                    }
                }
                FieldValue::Map(map)
            }
        }
        _ => FieldValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiBindingEntry, ApiBindingSpec, ApiError, ApiHandler};
    use crate::clause::{Behavior, ExecutionPolicy};
    use crate::pattern::Pattern;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    fn test_clause(
        fields: Vec<(String, Pattern)>,
        guard: Option<String>,
        lua_source: &str,
    ) -> Clause {
        Clause {
            policy: ExecutionPolicy { fields },
            guard,
            behavior: Behavior {
                lua_source: lua_source.into(),
            },
            annotations: Vec::new(),
            field_annotations: BTreeMap::new(),
        }
    }

    /// Wrap owned specs in binding entries with synthetic ids for tests.
    fn entries<I>(specs: I) -> Vec<ApiBindingEntry>
    where
        I: IntoIterator<Item = ApiBindingSpec>,
    {
        specs
            .into_iter()
            .enumerate()
            .map(|(i, spec)| ApiBindingEntry {
                id: crate::api::ApiBindingId(i as u64),
                spec,
            })
            .collect()
    }

    struct MockHandler {
        calls: Mutex<Vec<(String, String, Vec<FieldValue>)>>,
        return_value: FieldValue,
    }

    impl MockHandler {
        fn new(return_value: FieldValue) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                return_value,
            }
        }

        fn call_log(&self) -> Vec<(String, String, Vec<FieldValue>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ApiHandler for MockHandler {
        fn call(
            &self,
            namespace: &str,
            method: &str,
            args: Vec<FieldValue>,
        ) -> Result<FieldValue, ApiError> {
            self.calls
                .lock()
                .unwrap()
                .push((namespace.into(), method.into(), args));
            Ok(self.return_value.clone())
        }
    }

    #[test]
    fn execute_simple_clause() {
        let lua = Lua::new();
        let clause = test_clause(vec![], None, "result = 1 + 2");

        execute_clause(&lua, &clause, &FieldBindings::new(), &[], &[]).unwrap();

        let result: i64 = lua.globals().get("result").unwrap();
        assert_eq!(result, 3);
    }

    #[test]
    fn bindings_injected_as_globals() {
        let lua = Lua::new();
        let clause = test_clause(vec![], None, "result = name .. '_' .. tostring(count)");

        let mut bindings = FieldBindings::new();
        bindings.insert("name".into(), FieldValue::String("agent".into()));
        bindings.insert("count".into(), FieldValue::Number(3));

        execute_clause(&lua, &clause, &bindings, &[], &[]).unwrap();

        let result: String = lua.globals().get("result").unwrap();
        assert_eq!(result, "agent_3");
    }

    #[test]
    fn api_handler_called_from_lua() {
        let lua = Lua::new();
        let handler = Arc::new(MockHandler::new(FieldValue::String("ok".into())));

        let clause = test_clause(
            vec![("surface".into(), Pattern::Enum("tui".into()))],
            None,
            "result = client.quit()",
        );

        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![("surface".into(), Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        }]);

        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();

        let calls = handler.call_log();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "client");
        assert_eq!(calls[0].1, "quit");

        let result: String = lua.globals().get("result").unwrap();
        assert_eq!(result, "ok");
    }

    #[test]
    fn api_handler_receives_args() {
        let lua = Lua::new();
        let handler = Arc::new(MockHandler::new(FieldValue::Null));

        let clause = test_clause(
            vec![("surface".into(), Pattern::Enum("tui".into()))],
            None,
            "client.move(1, \"up\")",
        );

        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![],
            handler: handler.clone(),
        }]);

        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();

        let calls = handler.call_log();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "move");
        assert_eq!(
            calls[0].2,
            vec![FieldValue::Number(1), FieldValue::String("up".into())]
        );
    }

    #[test]
    fn api_not_injected_when_pattern_doesnt_match() {
        let lua = Lua::new();
        let handler = Arc::new(MockHandler::new(FieldValue::Null));

        let clause = test_clause(
            vec![("surface".into(), Pattern::Enum("runner".into()))],
            None,
            "result = client == nil",
        );

        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![("surface".into(), Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        }]);

        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();

        // client should not be injected (pattern doesn't match)
        let result: bool = lua.globals().get("result").unwrap();
        assert!(result);
        assert_eq!(handler.call_log().len(), 0);
    }

    #[test]
    fn nested_namespace_injection() {
        let lua = Lua::new();
        let handler = Arc::new(MockHandler::new(FieldValue::String("done".into())));

        let clause = test_clause(vec![], None, "result = crucible.client.save()");

        let api_bindings = entries([ApiBindingSpec {
            namespace: "crucible.client".into(),
            pattern: vec![],
            handler: handler.clone(),
        }]);

        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();

        let result: String = lua.globals().get("result").unwrap();
        assert_eq!(result, "done");
    }

    #[test]
    fn api_error_propagates_to_lua() {
        struct FailHandler;
        impl ApiHandler for FailHandler {
            fn call(&self, _: &str, _: &str, _: Vec<FieldValue>) -> Result<FieldValue, ApiError> {
                Err(ApiError::new("something broke"))
            }
        }

        let lua = Lua::new();
        let clause = test_clause(vec![], None, "client.explode()");

        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![],
            handler: Arc::new(FailHandler),
        }]);

        let err = execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("something broke"));
    }

    #[test]
    fn field_value_roundtrip_through_lua() {
        let lua = Lua::new();

        let values = vec![
            FieldValue::String("hello".into()),
            FieldValue::Number(42),
            FieldValue::Float(1.5),
            FieldValue::Bool(true),
            FieldValue::Null,
        ];

        for value in values {
            let lua_val = field_value_to_lua(&lua, &value).unwrap();
            let back = lua_value_to_field_value(&lua_val);
            assert_eq!(value, back, "roundtrip failed for {value:?}");
        }
    }

    #[test]
    fn list_roundtrip_through_lua() {
        let lua = Lua::new();
        let value = FieldValue::List(vec![
            FieldValue::Number(1),
            FieldValue::String("two".into()),
            FieldValue::Bool(true),
        ]);

        let lua_val = field_value_to_lua(&lua, &value).unwrap();
        let back = lua_value_to_field_value(&lua_val);
        assert_eq!(value, back);
    }

    // ── Value conversion edge cases ────────────────────────────────

    #[test]
    fn enum_roundtrip_loses_distinction() {
        // Enum("x") → Lua string → String("x") — enum distinction lost through Lua
        let lua = Lua::new();
        let lua_val = field_value_to_lua(&lua, &FieldValue::Enum("x".into())).unwrap();
        let back = lua_value_to_field_value(&lua_val);
        assert_eq!(back, FieldValue::String("x".into())); // Not Enum!
    }

    #[test]
    fn map_roundtrip_through_lua() {
        let lua = Lua::new();
        let mut map = std::collections::HashMap::new();
        map.insert("a".into(), FieldValue::Number(1));
        map.insert("b".into(), FieldValue::String("hello".into()));
        let value = FieldValue::Map(map.clone());
        let lua_val = field_value_to_lua(&lua, &value).unwrap();
        let back = lua_value_to_field_value(&lua_val);
        assert_eq!(value, back);
    }

    #[test]
    fn nested_list_of_maps_through_lua() {
        let lua = Lua::new();
        let mut m = std::collections::HashMap::new();
        m.insert("key".into(), FieldValue::Number(42));
        let value = FieldValue::List(vec![FieldValue::Map(m)]);
        let lua_val = field_value_to_lua(&lua, &value).unwrap();
        let back = lua_value_to_field_value(&lua_val);
        assert_eq!(value, back);
    }

    #[test]
    fn empty_list_through_lua() {
        // Empty list → table with raw_len=0 → becomes Map({}) on roundtrip
        let lua = Lua::new();
        let lua_val = field_value_to_lua(&lua, &FieldValue::List(vec![])).unwrap();
        let back = lua_value_to_field_value(&lua_val);
        // Known behavior: empty list becomes empty map
        assert_eq!(back, FieldValue::Map(std::collections::HashMap::new()));
    }

    #[test]
    fn empty_map_through_lua() {
        let lua = Lua::new();
        let lua_val =
            field_value_to_lua(&lua, &FieldValue::Map(std::collections::HashMap::new())).unwrap();
        let back = lua_value_to_field_value(&lua_val);
        assert_eq!(back, FieldValue::Map(std::collections::HashMap::new()));
    }

    #[test]
    fn float_precision_roundtrip() {
        let lua = Lua::new();
        let value = FieldValue::Float(1.0 / 3.0);
        let lua_val = field_value_to_lua(&lua, &value).unwrap();
        let back = lua_value_to_field_value(&lua_val);
        assert_eq!(value, back);
    }

    #[test]
    fn null_through_lua() {
        let lua = Lua::new();
        let lua_val = field_value_to_lua(&lua, &FieldValue::Null).unwrap();
        let back = lua_value_to_field_value(&lua_val);
        assert_eq!(back, FieldValue::Null);
    }

    // ── Lua sandbox ────────────────────────────────────────────────

    #[test]
    fn lua_access_standard_globals() {
        let lua = Lua::new();
        let clause = test_clause(
            vec![],
            None,
            "\
result_type = type(42)
result_str = tostring(123)
result_math = math.floor(3.7)
result_sub = string.sub('hello', 1, 3)
",
        );
        execute_clause(&lua, &clause, &FieldBindings::new(), &[], &[]).unwrap();
        let rt: String = lua.globals().get("result_type").unwrap();
        assert_eq!(rt, "number");
        let rs: String = lua.globals().get("result_str").unwrap();
        assert_eq!(rs, "123");
        let rm: i64 = lua.globals().get("result_math").unwrap();
        assert_eq!(rm, 3);
        let rsub: String = lua.globals().get("result_sub").unwrap();
        assert_eq!(rsub, "hel");
    }

    #[test]
    fn lua_pcall_in_script() {
        let lua = Lua::new();
        let clause = test_clause(
            vec![],
            None,
            "ok, err = pcall(function() error('boom') end)\nresult = not ok",
        );
        execute_clause(&lua, &clause, &FieldBindings::new(), &[], &[]).unwrap();
        let result: bool = lua.globals().get("result").unwrap();
        assert!(result);
    }

    #[test]
    fn lua_sets_global_visible_after() {
        let lua = Lua::new();
        let clause = test_clause(vec![], None, "my_global = 'persisted'");
        execute_clause(&lua, &clause, &FieldBindings::new(), &[], &[]).unwrap();
        let val: String = lua.globals().get("my_global").unwrap();
        assert_eq!(val, "persisted");
    }

    #[test]
    fn lua_empty_body_succeeds() {
        let lua = Lua::new();
        let clause = test_clause(vec![], None, "");
        execute_clause(&lua, &clause, &FieldBindings::new(), &[], &[]).unwrap();
    }

    // ── Return sandboxing ─────────────────────────────────────────

    #[test]
    fn return_in_conditional_exits_iife_not_chunk() {
        let lua = Lua::new();
        // return inside an if block exits the IIFE, globals set before are preserved
        let clause = test_clause(
            vec![],
            None,
            "before = true\nif true then return end\n-- never reached but syntactically valid",
        );
        execute_clause(&lua, &clause, &FieldBindings::new(), &[], &[]).unwrap();
        let before: bool = lua.globals().get("before").unwrap();
        assert!(before);
    }

    #[test]
    fn bare_return_completes_cleanly() {
        let lua = Lua::new();
        // A script that's just `return` should work fine
        let clause = test_clause(vec![], None, "return");
        execute_clause(&lua, &clause, &FieldBindings::new(), &[], &[]).unwrap();
    }

    #[test]
    fn return_with_value_completes_without_error() {
        let lua = Lua::new();
        let clause = test_clause(vec![], None, "result = 'ok'\nreturn 42");
        execute_clause(&lua, &clause, &FieldBindings::new(), &[], &[]).unwrap();
        let val: String = lua.globals().get("result").unwrap();
        assert_eq!(val, "ok");
    }

    #[test]
    fn script_without_return_works_as_before() {
        let lua = Lua::new();
        let clause = test_clause(vec![], None, "a = 1\nb = 2\nc = a + b");
        execute_clause(&lua, &clause, &FieldBindings::new(), &[], &[]).unwrap();
        let c: i64 = lua.globals().get("c").unwrap();
        assert_eq!(c, 3);
    }

    // ── API injection edge cases ───────────────────────────────────

    #[test]
    fn api_multiple_namespaces_simultaneously() {
        let lua = Lua::new();
        let handler1 = Arc::new(MockHandler::new(FieldValue::String("from_a".into())));
        let handler2 = Arc::new(MockHandler::new(FieldValue::String("from_b".into())));

        let clause = test_clause(vec![], None, "r1 = ns_a.method()\nr2 = ns_b.method()");

        let api_bindings = entries([
            ApiBindingSpec {
                namespace: "ns_a".into(),
                pattern: vec![],
                handler: handler1.clone(),
            },
            ApiBindingSpec {
                namespace: "ns_b".into(),
                pattern: vec![],
                handler: handler2.clone(),
            },
        ]);

        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();
        let r1: String = lua.globals().get("r1").unwrap();
        let r2: String = lua.globals().get("r2").unwrap();
        assert_eq!(r1, "from_a");
        assert_eq!(r2, "from_b");
    }

    #[test]
    fn api_three_level_namespace() {
        let lua = Lua::new();
        let handler = Arc::new(MockHandler::new(FieldValue::String("deep".into())));

        let clause = test_clause(vec![], None, "result = a.b.c.method()");

        let api_bindings = entries([ApiBindingSpec {
            namespace: "a.b.c".into(),
            pattern: vec![],
            handler: handler.clone(),
        }]);

        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();
        let result: String = lua.globals().get("result").unwrap();
        assert_eq!(result, "deep");
    }

    #[test]
    fn api_handler_returning_null() {
        let lua = Lua::new();
        let handler = Arc::new(MockHandler::new(FieldValue::Null));
        let clause = test_clause(
            vec![],
            None,
            "result = client.get_value()\nis_nil = result == nil",
        );
        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![],
            handler,
        }]);
        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();
        let is_nil: bool = lua.globals().get("is_nil").unwrap();
        assert!(is_nil);
    }

    #[test]
    fn api_handler_returning_list() {
        struct ListHandler;
        impl ApiHandler for ListHandler {
            fn call(&self, _: &str, _: &str, _: Vec<FieldValue>) -> Result<FieldValue, ApiError> {
                Ok(FieldValue::List(vec![
                    FieldValue::Number(1),
                    FieldValue::Number(2),
                ]))
            }
        }

        let lua = Lua::new();
        let clause = test_clause(vec![], None, "items = client.get_list()\nresult = #items");
        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![],
            handler: Arc::new(ListHandler),
        }]);
        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();
        let len: i64 = lua.globals().get("result").unwrap();
        assert_eq!(len, 2);
    }

    #[test]
    fn api_handler_returning_map() {
        struct MapHandler;
        impl ApiHandler for MapHandler {
            fn call(&self, _: &str, _: &str, _: Vec<FieldValue>) -> Result<FieldValue, ApiError> {
                let mut m = std::collections::HashMap::new();
                m.insert("name".into(), FieldValue::String("agent".into()));
                Ok(FieldValue::Map(m))
            }
        }

        let lua = Lua::new();
        let clause = test_clause(vec![], None, "data = client.get_map()\nresult = data.name");
        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![],
            handler: Arc::new(MapHandler),
        }]);
        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();
        let name: String = lua.globals().get("result").unwrap();
        assert_eq!(name, "agent");
    }

    #[test]
    fn api_call_with_zero_args() {
        let lua = Lua::new();
        let handler = Arc::new(MockHandler::new(FieldValue::Null));
        let clause = test_clause(vec![], None, "client.ping()");
        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![],
            handler: handler.clone(),
        }]);
        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();
        let calls = handler.call_log();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].2.is_empty());
    }

    #[test]
    fn api_call_with_many_args() {
        let lua = Lua::new();
        let handler = Arc::new(MockHandler::new(FieldValue::Null));
        let clause = test_clause(vec![], None, "client.many(1,2,3,4,5,6,7,8,9,10)");
        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![],
            handler: handler.clone(),
        }]);
        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();
        let calls = handler.call_log();
        assert_eq!(calls[0].2.len(), 10);
    }

    #[test]
    fn api_multiple_calls_in_single_script() {
        let lua = Lua::new();
        let handler = Arc::new(MockHandler::new(FieldValue::Null));
        let clause = test_clause(vec![], None, "client.first()\nclient.second()");
        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![],
            handler: handler.clone(),
        }]);
        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();
        let calls = handler.call_log();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, "first");
        assert_eq!(calls[1].1, "second");
    }

    #[test]
    fn api_return_value_used_in_subsequent_lua() {
        let lua = Lua::new();
        let handler = Arc::new(MockHandler::new(FieldValue::Number(42)));
        let clause = test_clause(vec![], None, "n = client.get_number()\nresult = n + 8");
        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![],
            handler,
        }]);
        execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap();
        let result: i64 = lua.globals().get("result").unwrap();
        assert_eq!(result, 50);
    }

    #[test]
    fn api_error_message_preserved() {
        struct FailHandler;
        impl ApiHandler for FailHandler {
            fn call(&self, _: &str, _: &str, _: Vec<FieldValue>) -> Result<FieldValue, ApiError> {
                Err(ApiError::new("specific error message"))
            }
        }

        let lua = Lua::new();
        let clause = test_clause(vec![], None, "client.fail()");
        let api_bindings = entries([ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![],
            handler: Arc::new(FailHandler),
        }]);
        let err =
            execute_clause(&lua, &clause, &FieldBindings::new(), &api_bindings, &[]).unwrap_err();
        assert!(err.to_string().contains("specific error message"));
    }

    #[test]
    fn policy_to_fieldmap_skips_variables_and_wildcards() {
        let clause = test_clause(
            vec![
                ("type".into(), Pattern::Enum("input".into())),
                ("key".into(), Pattern::Variable("k".into())),
                ("mode".into(), Pattern::Wildcard),
                ("surface".into(), Pattern::StringLiteral("tui".into())),
                ("count".into(), Pattern::NumberLiteral(5)),
                ("active".into(), Pattern::BoolLiteral(true)),
            ],
            None,
            "",
        );
        let map = clause_policy_to_fieldmap(&clause);
        // Only literals should be in the map
        assert_eq!(map.len(), 4);
        assert_eq!(map.get("type"), Some(&FieldValue::Enum("input".into())));
        assert_eq!(map.get("surface"), Some(&FieldValue::String("tui".into())));
        assert_eq!(map.get("count"), Some(&FieldValue::Number(5)));
        assert_eq!(map.get("active"), Some(&FieldValue::Bool(true)));
        assert!(!map.contains_key("key"));
        assert!(!map.contains_key("mode"));
    }
}
