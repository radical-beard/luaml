//! End-to-end integration tests exercising the full luaml pipeline:
//! parse → match → execute → verify side effects.

use std::sync::{Arc, Mutex};

use luaml::LuamlEngine;
use luaml::api::{ApiBindingSpec, ApiError, ApiHandler};
use luaml::pattern::Pattern;
use luaml::types::{FieldMap, FieldValue};

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
fn end_to_end_simple_dispatch() {
    let mut engine = LuamlEngine::new().unwrap();
    engine
        .register(
            "quit.luaml",
            "---\ntype: :input:\nkey: \"q\"\n---\nquit_called = true\n",
        )
        .unwrap();

    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("key", FieldValue::String("q".into())),
    ]));

    assert_eq!(results.len(), 1);
    assert!(engine.lua().globals().get::<bool>("quit_called").unwrap());
}

#[test]
fn end_to_end_variable_binding_flow() {
    // Pattern captures $d → guard checks d > 0 → Lua uses d
    let mut engine = LuamlEngine::new().unwrap();
    engine
        .register(
            "depth.luaml",
            "---\ntype: :lifecycle:\ndepth: $d\n? d > 0\n---\ncaptured_depth = d\n",
        )
        .unwrap();

    // d=5 passes guard
    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("lifecycle".into())),
        ("depth", FieldValue::Number(5)),
    ]));
    assert_eq!(results.len(), 1);
    let val: i64 = engine.lua().globals().get("captured_depth").unwrap();
    assert_eq!(val, 5);
}

#[test]
fn end_to_end_multi_clause_dispatch() {
    let mut engine = LuamlEngine::new().unwrap();
    engine
        .register(
            "keys.luaml",
            "\
---
type: :input:
key: :escape:
---
matched = \"escape\"
---
key: :tab:
---
matched = \"tab\"
---
key: $other
---
matched = \"other\"
",
        )
        .unwrap();

    // escape → first clause
    engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("key", FieldValue::Enum("escape".into())),
    ]));
    assert_eq!(
        engine.lua().globals().get::<String>("matched").unwrap(),
        "escape"
    );

    // tab → second clause
    engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("key", FieldValue::Enum("tab".into())),
    ]));
    assert_eq!(
        engine.lua().globals().get::<String>("matched").unwrap(),
        "tab"
    );

    // anything else → wildcard clause
    engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("key", FieldValue::Enum("enter".into())),
    ]));
    assert_eq!(
        engine.lua().globals().get::<String>("matched").unwrap(),
        "other"
    );
}

#[test]
fn end_to_end_guard_rejects_then_accepts() {
    let mut engine = LuamlEngine::new().unwrap();
    engine
        .register(
            "guard.luaml",
            "\
---
type: :lifecycle:
depth: $d
? d > 2
---
result = \"deep\"
---
depth: $d
? d >= 0
---
result = \"shallow\"
",
        )
        .unwrap();

    // d=1 fails first guard, falls to second clause
    engine.dispatch(&event(&[
        ("type", FieldValue::Enum("lifecycle".into())),
        ("depth", FieldValue::Number(1)),
    ]));
    assert_eq!(
        engine.lua().globals().get::<String>("result").unwrap(),
        "shallow"
    );

    // d=5 passes first guard
    engine.dispatch(&event(&[
        ("type", FieldValue::Enum("lifecycle".into())),
        ("depth", FieldValue::Number(5)),
    ]));
    assert_eq!(
        engine.lua().globals().get::<String>("result").unwrap(),
        "deep"
    );
}

#[test]
fn end_to_end_api_callback_flow() {
    let mut engine = LuamlEngine::new().unwrap();
    let handler = Arc::new(RecordingHandler::new(FieldValue::String("saved".into())));

    engine
        .register(
            "save.luaml",
            "---\ntype: :input:\nsurface: :tui:\nkey: \"s\"\n---\nresult = client.save(\"file.txt\", 42)\n",
        )
        .unwrap();

    engine.register_api(ApiBindingSpec {
        namespace: "client".into(),
        pattern: vec![("surface".into(), Pattern::Enum("tui".into()))],
        handler: handler.clone(),
    });

    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("surface", FieldValue::Enum("tui".into())),
        ("key", FieldValue::String("s".into())),
    ]));

    assert_eq!(results.len(), 1);

    let calls = handler.call_log();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "client");
    assert_eq!(calls[0].1, "save");
    assert_eq!(
        calls[0].2,
        vec![
            FieldValue::String("file.txt".into()),
            FieldValue::Number(42)
        ]
    );

    let val: String = engine.lua().globals().get("result").unwrap();
    assert_eq!(val, "saved");
}

#[test]
fn end_to_end_multiple_scripts_with_api() {
    let mut engine = LuamlEngine::new().unwrap();
    let handler = Arc::new(RecordingHandler::new(FieldValue::Null));

    engine
        .register(
            "a.luaml",
            "---\ntype: :input:\nsurface: :tui:\n---\nsvc.action_a()\n",
        )
        .unwrap();
    engine
        .register(
            "b.luaml",
            "---\ntype: :input:\nsurface: :tui:\n---\nsvc.action_b()\n",
        )
        .unwrap();
    // This script has surface: :runner: so the API shouldn't be injected
    engine
        .register(
            "c.luaml",
            "---\ntype: :input:\nsurface: :runner:\n---\nc_ran = true\n",
        )
        .unwrap();

    engine.register_api(ApiBindingSpec {
        namespace: "svc".into(),
        pattern: vec![("surface".into(), Pattern::Enum("tui".into()))],
        handler: handler.clone(),
    });

    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("surface", FieldValue::Enum("tui".into())),
    ]));

    // Only a and b match (surface: :tui:), c has surface: :runner:
    assert_eq!(results.len(), 2);

    let calls = handler.call_log();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, "action_a");
    assert_eq!(calls[1].1, "action_b");
}

#[test]
fn end_to_end_map_destructuring_to_lua() {
    let mut engine = LuamlEngine::new().unwrap();
    engine
        .register(
            "map.luaml",
            "---\ntype: :lifecycle:\ncontext: {phase: :planning:, depth: $d}\n---\ncaptured = d\n",
        )
        .unwrap();

    let mut ctx = FieldMap::new();
    ctx.insert("phase".into(), FieldValue::Enum("planning".into()));
    ctx.insert("depth".into(), FieldValue::Number(3));

    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("lifecycle".into())),
        ("context", FieldValue::Map(ctx)),
    ]));

    assert_eq!(results.len(), 1);
    let val: i64 = engine.lua().globals().get("captured").unwrap();
    assert_eq!(val, 3);
}

#[test]
fn end_to_end_list_destructuring_to_lua() {
    let mut engine = LuamlEngine::new().unwrap();
    engine
        .register(
            "list.luaml",
            "---\ntype: :data:\nitems: [$first | $rest]\n---\ncaptured_first = first\ncaptured_rest_len = #rest\n",
        )
        .unwrap();

    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("data".into())),
        (
            "items",
            FieldValue::List(vec![
                FieldValue::String("alpha".into()),
                FieldValue::String("beta".into()),
                FieldValue::String("gamma".into()),
            ]),
        ),
    ]));

    assert_eq!(results.len(), 1);
    let first: String = engine.lua().globals().get("captured_first").unwrap();
    assert_eq!(first, "alpha");
    let rest_len: i64 = engine.lua().globals().get("captured_rest_len").unwrap();
    assert_eq!(rest_len, 2);
}

#[test]
fn end_to_end_type_distinction_preserved() {
    let mut engine = LuamlEngine::new().unwrap();
    // Script matches Enum :tui:
    engine
        .register(
            "enum.luaml",
            "---\ntype: :input:\nsurface: :tui:\n---\nmatched_enum = true\n",
        )
        .unwrap();

    // Enum event matches
    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("surface", FieldValue::Enum("tui".into())),
    ]));
    assert_eq!(results.len(), 1);

    // String "tui" does NOT match Enum :tui:
    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("surface", FieldValue::String("tui".into())),
    ]));
    assert_eq!(results.len(), 0);
}

#[test]
fn end_to_end_lua_error_surfaces_in_outcome() {
    let mut engine = LuamlEngine::new().unwrap();
    engine
        .register("err.luaml", "---\ntype: :input:\n---\nerror(\"boom\")\n")
        .unwrap();

    let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
    assert_eq!(outcomes.len(), 1);
    let err = outcomes[0].result.as_ref().unwrap_err();
    assert_eq!(err.kind, luaml::ClauseErrKind::Body);
    assert!(err.message.contains("boom"));
}

#[test]
fn end_to_end_empty_body_no_op() {
    let mut engine = LuamlEngine::new().unwrap();
    engine
        .register("empty.luaml", "---\ntype: :input:\n---\n")
        .unwrap();

    let results = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
    assert_eq!(results.len(), 1);
}

#[test]
fn end_to_end_nested_namespace_api() {
    let mut engine = LuamlEngine::new().unwrap();
    let handler = Arc::new(RecordingHandler::new(FieldValue::Number(7)));

    engine
        .register(
            "nested.luaml",
            "---\ntype: :input:\nsurface: :tui:\n---\nresult = a.b.compute(3, 4)\n",
        )
        .unwrap();

    engine.register_api(ApiBindingSpec {
        namespace: "a.b".into(),
        pattern: vec![("surface".into(), Pattern::Enum("tui".into()))],
        handler: handler.clone(),
    });

    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("surface", FieldValue::Enum("tui".into())),
    ]));

    assert_eq!(results.len(), 1);
    let calls = handler.call_log();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "a.b");
    assert_eq!(calls[0].1, "compute");

    let val: i64 = engine.lua().globals().get("result").unwrap();
    assert_eq!(val, 7);
}

#[test]
fn end_to_end_register_dir() {
    use std::fs;

    let dir = std::env::temp_dir().join("luaml_integration_test_dir");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("sub")).unwrap();

    fs::write(
        dir.join("keys.luaml"),
        "---\ntype: :input:\nkey: \"q\"\n---\nkeys_matched = true\n",
    )
    .unwrap();
    fs::write(
        dir.join("sub/lifecycle.luaml"),
        "---\ntype: :lifecycle:\n---\nlifecycle_matched = true\n",
    )
    .unwrap();

    let mut engine = LuamlEngine::new().unwrap();
    let count = engine.register_dir(&dir).unwrap();
    assert_eq!(count, 2);

    // Dispatch input event
    let results = engine.dispatch(&event(&[
        ("type", FieldValue::Enum("input".into())),
        ("key", FieldValue::String("q".into())),
    ]));
    assert_eq!(results.len(), 1);
    assert!(engine.lua().globals().get::<bool>("keys_matched").unwrap());

    // Dispatch lifecycle event
    let results = engine.dispatch(&event(&[("type", FieldValue::Enum("lifecycle".into()))]));
    assert_eq!(results.len(), 1);
    assert!(
        engine
            .lua()
            .globals()
            .get::<bool>("lifecycle_matched")
            .unwrap()
    );

    let _ = fs::remove_dir_all(&dir);
}
