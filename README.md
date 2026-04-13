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

The frontmatter matches events where `type` is the enum `:input:`, `surface` is `:tui:`,
`key` is the string `"q"`, and `mode` is `:normal:`. When all fields match, the Lua body runs.

## Core Concept

**Frontmatter defines when. Lua defines what.**

The engine matches incoming events (key-value maps) against the patterns in each script's
frontmatter. When a match is found, the attached Lua code executes. No special fields,
no routing logic — just typed patterns and Lua.

## Type System

| Syntax | Type | Example |
|---|---|---|
| `:name:` | Enum | `type: :input:` |
| `"text"` | String | `context: "overlay.settings"` |
| `$name` | Variable | `agent_id: $id` (captures value) |
| `*` or `_` | Wildcard | `key: *` (matches anything) |
| `42` | Number | `depth: 42` |
| `true`/`false` | Boolean | `active: true` |
| `{k: v, ...}` | Map | `ctx: {phase: :planning:, d: $d}` |
| `[$h \| $t]` | List (head\|tail) | `skills: [$first \| $rest]` |
| `[$a, $b]` | List (fixed) | `pair: [$x, $y]` |
| `^name` | Pin | `id: ^saved_id` (matches existing binding) |

**Type distinction**: Enums and strings are different types. `:tui:` (Enum) will **never**
match `"tui"` (String), even though the text is the same. This is enforced throughout the engine.

**Bare words are parse errors**: `agent_id` without quotes or colons is invalid. Use
`:agent_id:` (enum), `"agent_id"` (string), or `$agent_id` (variable).

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
? depth > 0
---
handle_deep(id)
```

The second clause inherits `type: :lifecycle:`, `event: :on_step:`, and `agent_id: $id`
from the first, overriding only `phase`. The third clause also inherits the base fields
and adds a guard.

**Guards are per-clause** — they are never inherited from the base clause.

## Guards

Guard expressions filter matches using `?` prefix lines in the frontmatter:

```luaml
---
type: :lifecycle:
depth: $d
phase: $p
? d > 0
? p ~= "idle"
---
handle(d, p)
```

- Multiple `?` lines are implicitly **ANDed**
- Guards evaluate over pattern-bound variables after a successful match
- Guard failure means no match for that clause (not an error)
- Supported operators: `==`, `~=`/`!=`, `<`, `>`, `<=`, `>=`, `and`, `or`, `not`, parentheses

## Annotations

Annotations are metadata lines prefixed with `@` in the frontmatter. They never affect
pattern matching or execution — consumers read them for display, schema generation, etc.

```luaml
---
@title: "Quit Command"
@category: "navigation"
type: :input:
@description: "The key to press"
key: "q"
---
api.client.quit()
```

- **Top-level annotations** appear before any field (e.g., `@title`, `@category` above)
- **Field annotations** appear immediately before a field (e.g., `@description` before `key`)
- Top-level annotations are **not inherited** by child clauses in multi-clause scripts
- Field annotations are inherited for inherited fields

## Library Mode

Embed luaml as a Rust dependency:

```rust
use luaml::{LuamlEngine, ApiBinding, Pattern, FieldValue};
use luaml::api::{ApiHandler, ApiError};
use std::collections::HashMap;
use std::sync::Arc;

// 1. Implement an API handler
struct MyHandler;
impl ApiHandler for MyHandler {
    fn call(&self, _ns: &str, method: &str, args: Vec<FieldValue>) -> Result<FieldValue, ApiError> {
        match method {
            "quit" => { /* handle quit */ Ok(FieldValue::Null) }
            "save" => { /* handle save */ Ok(FieldValue::Bool(true)) }
            _ => Err(ApiError::new(format!("unknown method: {method}"))),
        }
    }
}

// 2. Create engine and register scripts
let mut engine = LuamlEngine::new()?;
engine.register_file("scripts/quit.luaml")?;
engine.register_dir("scripts/")?;  // all .luaml files, recursive

// 3. Register API bindings (pattern determines which clauses get this namespace)
engine.register_api(ApiBinding {
    namespace: "client".into(),
    pattern: vec![("surface".into(), Pattern::Enum("tui".into()))],
    handler: Arc::new(MyHandler),
});

// 4. Dispatch events
let mut event = HashMap::new();
event.insert("type".into(), FieldValue::Enum("input".into()));
event.insert("surface".into(), FieldValue::Enum("tui".into()));
event.insert("key".into(), FieldValue::String("q".into()));
event.insert("mode".into(), FieldValue::Enum("normal".into()));

let results = engine.dispatch(&event)?;
// results contains one entry per matched+executed clause
```

### Query Without Executing

```rust
// Find matching clauses without running Lua
let matches = engine.query(&event);

// Find clauses whose patterns are a superset of these fields (discovery)
let candidates = engine.query_subset(&event);
```

`query()` returns matches with bindings. `query_subset()` finds clauses that *could* match
an event with at least these fields — useful for introspection and schema generation.

## Service Mode

Run luaml as a standalone JSON-RPC 2.0 server:

```sh
luaml-service --listen tcp:127.0.0.1:9100     # TCP (default)
luaml-service --listen unix:/tmp/luaml.sock    # Unix socket
```

Each connection gets its own engine instance. Communication is newline-delimited JSON-RPC.

### Methods

#### `register` — Register a script from text

```json
{"jsonrpc":"2.0","id":1,"method":"register","params":{
  "source_path": "quit.luaml",
  "text": "---\ntype: :input:\nkey: \"q\"\n---\napi.client.quit()\n"
}}
```

Response: `{"jsonrpc":"2.0","id":1,"result":{"ok":true}}`

#### `register_api` — Register an API namespace

```json
{"jsonrpc":"2.0","id":2,"method":"register_api","params":{
  "namespace": "client",
  "pattern": {"surface": {"Enum": "tui"}}
}}
```

The pattern uses the FieldValue JSON encoding (see below). Empty pattern `{}` matches all clauses.

#### `dispatch` — Match and execute

```json
{"jsonrpc":"2.0","id":3,"method":"dispatch","params":{
  "event": {
    "type": {"Enum": "input"},
    "surface": {"Enum": "tui"},
    "key": {"String": "q"}
  }
}}
```

Response includes matched script paths and variable bindings:

```json
{"jsonrpc":"2.0","id":3,"result":{
  "matches": [{"script_path": "quit.luaml", "bindings": {}}]
}}
```

#### `query` / `query_subset` — Find matches without executing

Same request format as `dispatch`. Returns matches without running Lua.

### API Callback Flow

When a Lua script calls an API function (e.g., `api.client.quit()`), the service sends a
JSON-RPC request **back to the consumer**:

```json
{"jsonrpc":"2.0","id":100,"method":"api_call","params":{
  "namespace": "client",
  "method": "quit",
  "args": []
}}
```

The consumer executes the function and responds. The service resumes Lua execution with the result.

### Error Codes

| Code | Meaning |
|---|---|
| `-32700` | JSON parse error |
| `-32601` | Unknown method |
| `-32602` | Invalid parameters |
| `-32000` | luaml error (pattern, guard, or Lua) |

## FieldValue JSON Encoding

Values in JSON-RPC use externally-tagged encoding:

| Type | JSON | Example |
|---|---|---|
| Enum | `{"Enum": "name"}` | `{"Enum": "input"}` |
| String | `{"String": "text"}` | `{"String": "hello"}` |
| Number | `{"Number": 42}` | `{"Number": 0}` |
| Float | `{"Float": 3.14}` | `{"Float": 1.0}` |
| Bool | `{"Bool": true}` | `{"Bool": false}` |
| List | `{"List": [...]}` | `{"List": [{"Number": 1}]}` |
| Map | `{"Map": {...}}` | `{"Map": {"key": {"String": "val"}}}` |
| Null | `"Null"` | `"Null"` |

## Hot Reload

Enable the `file-watch` feature to watch script directories for changes:

```toml
luaml = { version = "...", features = ["file-watch"] }
```

```rust
use luaml::watcher::ScriptWatcher;
use luaml::registry::ScriptRegistry;
use std::time::Duration;

let watcher = ScriptWatcher::new(&[Path::new("scripts/")], Duration::from_millis(100))?;

// Before each dispatch, apply pending changes to the registry:
let changed = watcher.process_pending(&mut registry)?;
```

The watcher debounces filesystem events and queues re-registrations. Call `process_pending()`
on the `ScriptRegistry` before dispatch to apply changes.

## License

MIT OR Apache-2.0
