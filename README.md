# luaml

A pattern-matched script engine where YAML frontmatter defines typed execution policies
and Lua bodies define behavior. Erlang/Elixir-style pattern matching with variable binding,
destructuring, and multi-clause inheritance.

## Quick Example

```luaml
---
type: :input:
surface: :tui:
key: "q"
mode: :normal:
---
api.client.quit()
```

## Type System

| Syntax | Type | Example |
|---|---|---|
| `:name:` | Enum | `type: :input:` |
| `"text"` | String | `context: "overlay.settings"` |
| `$name` | Variable | `agent_id: $id` |
| `*` | Wildcard | `key: *` |
| `42` | Number | `depth: 42` |
| `true`/`false` | Boolean | `active: true` |
| `{k: v, ...}` | Map | `ctx: {phase: :planning:, d: $d}` |
| `[a \| b]` | List | `skills: [$first \| $rest]` |
| `[a, b]` | List | `pair: [$x, $y]` |
| `^name` | Pin | `id: ^saved_id` |

Bare words are parse errors. Enums and strings are type-distinct (`:tui:` != `"tui"`).

## Multi-Clause Scripts

A single file can contain multiple clauses. Subsequent clauses inherit the first
clause's execution policy, overriding only the fields they specify:

```luaml
---
type: :lifecycle:
event: :on_step:
agent_id: $id
phase: :planning:
---
handle_planning(id)
---
phase: :executing:
---
handle_executing(id)
---
phase: *
guard: depth > 0
---
handle_deep(id)
```

## Library Mode

Embed luaml as a Rust dependency:

```rust
use luaml::{LuamlEngine, ApiBinding, Pattern, FieldValue};
use std::sync::Arc;

let mut engine = LuamlEngine::new()?;

// Register scripts
engine.register_file("scripts/quit.luaml")?;
engine.register_dir("scripts/")?;

// Register consumer APIs (pattern determines which clauses get this namespace)
engine.register_api(ApiBinding {
    namespace: "client".into(),
    pattern: vec![("surface".into(), Pattern::Enum("tui".into()))],
    handler: Arc::new(MyTuiHandler),
});

// Dispatch events
let mut event = HashMap::new();
event.insert("type".into(), FieldValue::Enum("input".into()));
event.insert("surface".into(), FieldValue::Enum("tui".into()));
event.insert("key".into(), FieldValue::String("q".into()));
event.insert("mode".into(), FieldValue::Enum("normal".into()));
engine.dispatch(&event)?;
```

## Service Mode

Run luaml as a standalone process. Consumer connects over Unix socket or TCP:

```sh
luaml-service --socket /tmp/luaml.sock
luaml-service --tcp 127.0.0.1:9900
```

Consumer communicates via JSON-RPC 2.0:

```json
{"jsonrpc":"2.0","id":1,"method":"register","params":{"source_path":"quit.luaml","text":"---\ntype: :input:\nkey: \"q\"\n---\napi.client.quit()\n"}}
{"jsonrpc":"2.0","id":2,"method":"register_api","params":{"namespace":"client","pattern":[]}}
{"jsonrpc":"2.0","id":3,"method":"dispatch","params":{"event":{"type":{"Enum":"input"},"key":{"String":"q"}}}}
```

When scripts call API functions, the service sends JSON-RPC requests back to the consumer:

```json
{"jsonrpc":"2.0","id":100,"method":"api_call","params":{"namespace":"client","method":"quit","args":[]}}
```

The consumer executes the function and responds. The service resumes Lua execution.

## Guards

Guard expressions evaluate over pattern-bound variables after a successful match:

```yaml
guard: depth > 0 and phase == "planning"
```

Supported: `==`, `~=`/`!=`, `<`, `>`, `<=`, `>=`, `and`, `or`, `not`, parentheses.

## License

MIT OR Apache-2.0
