pub mod api;
pub mod clause;
pub mod error;
pub mod executor;
pub mod guard;
pub mod parser;
pub mod pattern;
pub mod pattern_match;
pub mod registry;
pub mod types;
#[cfg(feature = "file-watch")]
pub mod watcher;

use std::path::{Path, PathBuf};

use mlua::Lua;

use api::ApiBinding;
use clause::Clause;
use error::LuamlError;
use executor::execute_clause;
use registry::{ClauseMatch, QueryResult, ScriptRegistry};
use types::{FieldBindings, FieldMap};

/// Result of dispatching a single matched clause.
#[derive(Debug)]
pub struct DispatchResult<'a> {
    pub script_path: &'a Path,
    pub clause: &'a Clause,
    pub bindings: FieldBindings,
}

/// Top-level engine combining script registry, API bindings, and Lua execution.
pub struct LuamlEngine {
    registry: ScriptRegistry,
    api_bindings: Vec<ApiBinding>,
    lua: Lua,
}

impl LuamlEngine {
    pub fn new() -> Result<Self, LuamlError> {
        Ok(Self {
            registry: ScriptRegistry::new(),
            api_bindings: Vec::new(),
            lua: Lua::new(),
        })
    }

    /// Register a script from source text.
    pub fn register(
        &mut self,
        source_path: impl Into<PathBuf>,
        text: &str,
    ) -> Result<(), LuamlError> {
        self.registry.register_text(source_path, text)
    }

    /// Register a script from a file path.
    pub fn register_file(&mut self, path: impl AsRef<Path>) -> Result<(), LuamlError> {
        self.registry.register_file(path.as_ref())
    }

    /// Register all .luaml files under a directory (recursive).
    pub fn register_dir(&mut self, dir: impl AsRef<Path>) -> Result<usize, LuamlError> {
        self.registry.register_dir(dir.as_ref())
    }

    /// Register an API binding (namespace + pattern + handler).
    pub fn register_api(&mut self, binding: ApiBinding) {
        self.api_bindings.push(binding);
    }

    /// Find all matching clauses without executing them.
    pub fn query(&self, event: &FieldMap) -> Vec<ClauseMatch<'_>> {
        self.registry.match_clauses(event)
    }

    /// Find all clauses whose pattern fields are a superset of the query fields.
    /// See [`ScriptRegistry::query_subset`] for details.
    pub fn query_subset(&self, query: &FieldMap) -> Vec<QueryResult<'_>> {
        self.registry.query_subset(query)
    }

    /// Match and execute all clauses that match the event.
    /// Returns one result per executed clause.
    pub fn dispatch(&self, event: &FieldMap) -> Result<Vec<DispatchResult<'_>>, LuamlError> {
        let matches = self.registry.match_clauses(event);
        let mut results = Vec::with_capacity(matches.len());

        for m in &matches {
            execute_clause(&self.lua, m.clause, &m.bindings, &self.api_bindings)?;
            results.push(DispatchResult {
                script_path: &m.script.source_path,
                clause: m.clause,
                bindings: m.bindings.clone(),
            });
        }

        Ok(results)
    }

    /// Access the underlying script registry.
    pub fn registry(&self) -> &ScriptRegistry {
        &self.registry
    }

    /// Access the underlying Lua VM.
    pub fn lua(&self) -> &Lua {
        &self.lua
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiError, ApiHandler};
    use crate::types::FieldValue;
    use std::sync::{Arc, Mutex};

    fn event(pairs: &[(&str, FieldValue)]) -> FieldMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    struct RecordingHandler {
        calls: Mutex<Vec<(String, String, Vec<FieldValue>)>>,
        return_value: FieldValue,
    }

    impl RecordingHandler {
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

    impl ApiHandler for RecordingHandler {
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
    fn engine_register_and_dispatch() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nkey: \"q\"\n---\nresult = \"quit\"\n",
            )
            .unwrap();

        let results = engine
            .dispatch(&event(&[
                ("type", FieldValue::Enum("input".into())),
                ("key", FieldValue::String("q".into())),
            ]))
            .unwrap();

        assert_eq!(results.len(), 1);
        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "quit");
    }

    #[test]
    fn engine_no_match_returns_empty() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("test.luaml", "---\ntype: :input:\n---\nprint('hi')\n")
            .unwrap();

        let results = engine
            .dispatch(&event(&[("type", FieldValue::Enum("lifecycle".into()))]))
            .unwrap();

        assert_eq!(results.len(), 0);
    }

    #[test]
    fn engine_query_without_execution() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nkey: $k\n---\nresult = k\n",
            )
            .unwrap();

        let matches = engine.query(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("x".into())),
        ]));

        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].bindings.get("k"),
            Some(&FieldValue::String("x".into()))
        );

        // result should NOT be set since we only queried
        let val: mlua::Value = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, mlua::Value::Nil);
    }

    #[test]
    fn engine_multiple_scripts_dispatch() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :input:\n---\na_ran = true\n")
            .unwrap();
        engine
            .register("b.luaml", "---\ntype: :input:\n---\nb_ran = true\n")
            .unwrap();

        let results = engine
            .dispatch(&event(&[("type", FieldValue::Enum("input".into()))]))
            .unwrap();

        assert_eq!(results.len(), 2);
        assert!(engine.lua().globals().get::<bool>("a_ran").unwrap());
        assert!(engine.lua().globals().get::<bool>("b_ran").unwrap());
    }

    #[test]
    fn engine_dispatch_with_api_binding() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::String("done".into())));

        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nsurface: :tui:\n---\nresult = client.save(\"file.txt\")\n",
            )
            .unwrap();

        engine.register_api(ApiBinding {
            namespace: "client".into(),
            pattern: vec![("surface".into(), pattern::Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        });

        let results = engine
            .dispatch(&event(&[
                ("type", FieldValue::Enum("input".into())),
                ("surface", FieldValue::Enum("tui".into())),
            ]))
            .unwrap();

        assert_eq!(results.len(), 1);

        let calls = handler.call_log();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "client");
        assert_eq!(calls[0].1, "save");
        assert_eq!(calls[0].2, vec![FieldValue::String("file.txt".into())]);

        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "done");
    }

    #[test]
    fn engine_api_not_injected_for_mismatched_clause() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::Null));

        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nsurface: :runner:\n---\nresult = client == nil\n",
            )
            .unwrap();

        // API only available for :tui: surface
        engine.register_api(ApiBinding {
            namespace: "client".into(),
            pattern: vec![("surface".into(), pattern::Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        });

        engine
            .dispatch(&event(&[
                ("type", FieldValue::Enum("input".into())),
                ("surface", FieldValue::Enum("runner".into())),
            ]))
            .unwrap();

        // client should be nil in the :runner: clause
        assert!(engine.lua().globals().get::<bool>("result").unwrap());
        assert_eq!(handler.call_log().len(), 0);
    }

    #[test]
    fn engine_multi_clause_dispatch() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "multi.luaml",
                "\
---
type: :input:
key: :escape:
---
result = \"escape\"
---
key: :tab:
---
result = \"tab\"
",
            )
            .unwrap();

        // escape matches first clause
        let results = engine
            .dispatch(&event(&[
                ("type", FieldValue::Enum("input".into())),
                ("key", FieldValue::Enum("escape".into())),
            ]))
            .unwrap();
        assert_eq!(results.len(), 1);
        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "escape");

        // tab matches second clause
        let results = engine
            .dispatch(&event(&[
                ("type", FieldValue::Enum("input".into())),
                ("key", FieldValue::Enum("tab".into())),
            ]))
            .unwrap();
        assert_eq!(results.len(), 1);
        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "tab");
    }

    #[test]
    fn engine_bindings_available_in_lua() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nkey: $pressed\n---\nresult = pressed\n",
            )
            .unwrap();

        let results = engine
            .dispatch(&event(&[
                ("type", FieldValue::Enum("input".into())),
                ("key", FieldValue::String("q".into())),
            ]))
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].bindings.get("pressed"),
            Some(&FieldValue::String("q".into()))
        );
        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "q");
    }

    #[test]
    fn engine_dispatch_result_has_script_path() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("my/script.luaml", "---\ntype: :input:\n---\nprint('x')\n")
            .unwrap();

        let results = engine
            .dispatch(&event(&[("type", FieldValue::Enum("input".into()))]))
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].script_path, Path::new("my/script.luaml"));
    }

    #[test]
    fn engine_guard_filters_dispatch() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "test.luaml",
                "---\ntype: :lifecycle:\ndepth: $d\n? d > 0\n---\nresult = d\n",
            )
            .unwrap();

        // depth=0 fails guard — no dispatch
        let results = engine
            .dispatch(&event(&[
                ("type", FieldValue::Enum("lifecycle".into())),
                ("depth", FieldValue::Number(0)),
            ]))
            .unwrap();
        assert_eq!(results.len(), 0);

        // depth=3 passes guard
        let results = engine
            .dispatch(&event(&[
                ("type", FieldValue::Enum("lifecycle".into())),
                ("depth", FieldValue::Number(3)),
            ]))
            .unwrap();
        assert_eq!(results.len(), 1);
        let val: i64 = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, 3);
    }

    #[test]
    fn engine_lua_error_propagates() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("test.luaml", "---\ntype: :input:\n---\nerror(\"boom\")\n")
            .unwrap();

        let err = engine
            .dispatch(&event(&[("type", FieldValue::Enum("input".into()))]))
            .unwrap_err();

        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn engine_register_dir() {
        use std::fs;

        let dir = std::env::temp_dir().join("luaml_test_register_dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();

        fs::write(
            dir.join("a.luaml"),
            "---\ntype: :input:\n---\na_ran = true\n",
        )
        .unwrap();
        fs::write(
            dir.join("sub/b.luaml"),
            "---\ntype: :input:\n---\nb_ran = true\n",
        )
        .unwrap();
        // Non-luaml file should be ignored
        fs::write(dir.join("c.txt"), "not a script").unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        let count = engine.register_dir(&dir).unwrap();
        assert_eq!(count, 2);

        let results = engine
            .dispatch(&event(&[("type", FieldValue::Enum("input".into()))]))
            .unwrap();
        assert_eq!(results.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_new_creates_fresh_state() {
        let engine = LuamlEngine::new().unwrap();
        assert_eq!(engine.registry().all().len(), 0);
    }

    #[test]
    fn engine_incremental_registration() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :input:\n---\na_ran = true\n")
            .unwrap();

        // First dispatch: only a matches
        let results = engine
            .dispatch(&event(&[("type", FieldValue::Enum("input".into()))]))
            .unwrap();
        assert_eq!(results.len(), 1);

        // Register another script
        engine
            .register("b.luaml", "---\ntype: :input:\n---\nb_ran = true\n")
            .unwrap();

        // Second dispatch: both match
        let results = engine
            .dispatch(&event(&[("type", FieldValue::Enum("input".into()))]))
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn engine_register_api_after_scripts() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::Number(99)));

        // Register script first
        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nsurface: :tui:\n---\nresult = svc.ping()\n",
            )
            .unwrap();

        // Register API after scripts
        engine.register_api(ApiBinding {
            namespace: "svc".into(),
            pattern: vec![("surface".into(), pattern::Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        });

        let results = engine
            .dispatch(&event(&[
                ("type", FieldValue::Enum("input".into())),
                ("surface", FieldValue::Enum("tui".into())),
            ]))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(handler.call_log().len(), 1);
        let val: i64 = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, 99);
    }

    #[test]
    fn engine_dispatch_multiple_matching_with_api() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::Null));

        engine
            .register(
                "a.luaml",
                "---\ntype: :input:\nsurface: :tui:\n---\nsvc.method_a()\n",
            )
            .unwrap();
        engine
            .register(
                "b.luaml",
                "---\ntype: :input:\nsurface: :tui:\n---\nsvc.method_b()\n",
            )
            .unwrap();

        engine.register_api(ApiBinding {
            namespace: "svc".into(),
            pattern: vec![("surface".into(), pattern::Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        });

        let results = engine
            .dispatch(&event(&[
                ("type", FieldValue::Enum("input".into())),
                ("surface", FieldValue::Enum("tui".into())),
            ]))
            .unwrap();
        assert_eq!(results.len(), 2);

        let calls = handler.call_log();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, "method_a");
        assert_eq!(calls[1].1, "method_b");
    }

    #[test]
    fn engine_query_vs_dispatch_consistency() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nkey: $k\n---\nresult = k\n",
            )
            .unwrap();

        let ev = event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("z".into())),
        ]);

        let query_matches = engine.query(&ev);
        let dispatch_results = engine.dispatch(&ev).unwrap();

        assert_eq!(query_matches.len(), dispatch_results.len());
        assert_eq!(
            query_matches[0].bindings.get("k"),
            dispatch_results[0].bindings.get("k")
        );
    }

    #[test]
    fn engine_dispatch_error_stops_execution() {
        let mut engine = LuamlEngine::new().unwrap();
        // First script errors
        engine
            .register("a.luaml", "---\ntype: :input:\n---\nerror(\"fail\")\n")
            .unwrap();
        // Second script would succeed
        engine
            .register("b.luaml", "---\ntype: :input:\n---\nb_ran = true\n")
            .unwrap();

        let err = engine
            .dispatch(&event(&[("type", FieldValue::Enum("input".into()))]))
            .unwrap_err();
        assert!(err.to_string().contains("fail"));

        // b should NOT have run
        let val: mlua::Value = engine.lua().globals().get("b_ran").unwrap();
        assert_eq!(val, mlua::Value::Nil);
    }

    #[test]
    fn engine_query_subset_basic() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "a.luaml",
                "---\ntype: :input:\nsurface: :tui:\nmode: :leader:\n---\na()\n",
            )
            .unwrap();
        engine
            .register("b.luaml", "---\ntype: :input:\nsurface: :tui:\n---\nb()\n")
            .unwrap();
        engine
            .register("c.luaml", "---\ntype: :lifecycle:\n---\nc()\n")
            .unwrap();

        // Empty query returns all clauses
        let results = engine.query_subset(&FieldMap::new());
        assert_eq!(results.len(), 3);

        // Filter to input type
        let results = engine.query_subset(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(results.len(), 2);

        // Filter to TUI input leader mode — only script a has all three
        let results = engine.query_subset(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("surface", FieldValue::Enum("tui".into())),
            ("mode", FieldValue::Enum("leader".into())),
        ]));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].clause.behavior.lua_source, "a()");
    }

    #[test]
    fn engine_register_dir_empty_directory() {
        use std::fs;
        let dir = std::env::temp_dir().join("luaml_test_empty_dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        let count = engine.register_dir(&dir).unwrap();
        assert_eq!(count, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_register_dir_nonexistent() {
        let mut engine = LuamlEngine::new().unwrap();
        let count = engine
            .register_dir("/tmp/luaml_test_nonexistent_dir_xyz")
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn engine_register_invalid_text() {
        let mut engine = LuamlEngine::new().unwrap();
        let err = engine.register("bad.luaml", "not valid luaml");
        assert!(err.is_err());
    }

    #[test]
    fn engine_registry_accessor() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :a:\n---\na()\n")
            .unwrap();
        engine
            .register("b.luaml", "---\ntype: :b:\n---\nb()\n")
            .unwrap();
        assert_eq!(engine.registry().all().len(), 2);
    }
}
