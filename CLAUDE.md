# luaml

A pattern-matched script engine. YAML frontmatter defines typed execution policies; Lua bodies define behavior.

## The Rule

**Frontmatter defines when. Lua defines what.**

Every frontmatter field is a typed pattern. The engine matches incoming events against clause patterns and executes the first match. No special fields, no domain-specific logic in the engine.

## Terminology

- **Execution policy** — the frontmatter: typed pattern fields that determine when a clause fires
- **Behavior** — the Lua code body attached to an execution policy
- **Clause** — one execution policy + one behavior (the atomic unit of matching and execution)
- **Script** — a `.luaml` file containing one or more clauses
- **Guard** — a `? expr` line in frontmatter; boolean filter evaluated after pattern match succeeds
- **Annotation** — an `@key: value` line in frontmatter; pure metadata, never affects matching
- **Binding** — a variable captured by a `$name` pattern during matching
- **Pin** — a `^name` pattern that matches against an existing binding value

## Workspace Layout

Two crates in a Cargo workspace:

### `crates/luaml/` — Core library
| Module | Purpose |
|---|---|
| `lib.rs` | `LuamlEngine` public API — register, query, dispatch |
| `types.rs` | `FieldValue` enum, `FieldMap`, `FieldBindings` type aliases |
| `pattern.rs` | `Pattern` enum + `parse_pattern_value()` parser |
| `pattern_match.rs` | `match_field_value()` + `match_fields()` matching algorithm |
| `guard.rs` | Guard expression tokenizer, parser, and evaluator |
| `executor.rs` | Sandboxed Lua execution + API namespace injection |
| `registry.rs` | `ScriptRegistry` — script storage, `match_clauses()`, `query_subset()` |
| `parser.rs` | YAML frontmatter + multi-clause parser — `parse_luaml()` |
| `clause.rs` | `ExecutionPolicy`, `Behavior`, `Clause`, `Script` structs |
| `api.rs` | `ApiHandler` trait + `ApiBinding` struct |
| `error.rs` | `LuamlError`, `PatternParseError` |
| `watcher.rs` | `ScriptWatcher` hot-reload (feature-gated: `file-watch`) |

### `crates/luaml-service/` — JSON-RPC 2.0 server
| Module | Purpose |
|---|---|
| `server.rs` | `ListenAddr` parsing, TCP/Unix listener loops |
| `connection.rs` | Per-connection request dispatch — handles register, dispatch, query, etc. |
| `protocol.rs` | JSON-RPC request/response/error types and param structs |
| `remote_api.rs` | `RemoteApiHandler` — wraps API calls as JSON-RPC to the consumer |

## Dispatch Flow

```
dispatch(event: FieldMap)
  -> registry.match_clauses(event)
       -> for each Script, try clauses in order:
            match_fields(clause.policy.fields, event)  [pattern_match.rs]
              -> if no match, try next clause
            evaluate_guard(clause.guard, bindings)      [guard.rs]
              -> if guard fails, try next clause
            first matching clause wins for this script
  -> for each matched clause:
       execute_clause(clause, bindings, api_bindings)   [executor.rs]
         -> create sandboxed Lua env with metatable to globals
         -> inject bindings as Lua globals
         -> for each ApiBinding, check binding.pattern against clause policy
              -> if match, inject namespace as Lua table with __index proxy
         -> execute Lua body (wrapped in IIFE so `return` exits clause)
         -> on api call: handler.call(namespace, method, args) -> FieldValue
```

## Type System

**`FieldValue`** is the single runtime value type (Enum, String, Number, Float, Bool, List, Map, Null).
**`Pattern`** is the AST type (Wildcard, Enum, StringLiteral, NumberLiteral, BoolLiteral, Variable, Pin, List, Map).

Enums and strings are **type-distinct**: `:tui:` (Enum) only matches `FieldValue::Enum`, never `FieldValue::String`. This is the most common source of bugs.

Type aliases in `types.rs`:
- `FieldMap = HashMap<String, FieldValue>` — events, execution policies
- `FieldBindings = HashMap<String, FieldValue>` — captured variables from pattern matching

## API Injection Model

Consumer-provided functionality is injected via the `ApiHandler` trait (`api.rs`):
- `ApiHandler::call(namespace, method, args) -> Result<FieldValue, ApiError>`
- `ApiBinding` pairs a namespace + pattern + handler
- The pattern is checked against each clause's execution policy — if it matches, the namespace is injected into that clause's Lua env
- Empty patterns match all clauses (global injection)
- **Library mode**: consumer implements `ApiHandler` directly
- **Service mode**: `RemoteApiHandler` wraps calls in JSON-RPC, sends `api_call` to consumer, blocks for response

## Building & Testing

```sh
cargo build                    # build workspace
cargo test                     # all tests (~500)
cargo test -p luaml            # core crate only
cargo test -p luaml-service    # service crate only
cargo clippy --workspace       # lint
cargo fmt --all                # format
cargo fmt --all -- --check     # format check (CI)
```

**Feature flags**: `file-watch` enables `watcher.rs` (depends on `notify` + `notify-debouncer-mini`)

**Integration tests**:
- `crates/luaml/tests/integration.rs` — full pipeline: parse -> match -> execute -> verify
- `crates/luaml-service/tests/protocol_integration.rs` — JSON-RPC session over in-memory I/O

**Test helpers**:
- `event()` — construct a FieldMap from pairs (defined in `lib.rs` tests and `integration.rs`)
- `RecordingHandler` — captures API calls for assertion (implements `ApiHandler`)

## Coding Standards

- Edition 2024 Rust, MIT/Apache-2.0
- **No domain-specific logic** in the engine — no crucible, TUI, agent, or consumer knowledge
- `FieldValue` is the single value type; no `serde_json::Value` in core paths
- API injection via `ApiHandler` trait — never hardcode consumer APIs
- `Vec<(String, Pattern)>` for ordered field collections (ordering matters for matching)
- `thiserror` for error types
- No unnecessary abstraction — prefer explicit code over clever indirection
- Tests for every pattern syntax and matching edge case
- Each module has `#[cfg(test)] mod tests` with thorough coverage

## Key Invariants

- **One match per script**: first matching clause wins within a script; all matching scripts execute
- **Guards are per-clause**: never inherited in multi-clause files
- **Annotations are not inherited**: child clauses in multi-clause files get their own annotations
- **Dispatch stops on error**: if script A errors, script B never runs
- **Scripts are void**: `return` exits early but produces no value (executor wraps body in IIFE)
- **Pin requires existing binding**: `^name` patterns return None if the variable isn't already bound
- **Extra input fields are ignored**: an event with fields {a, b, c} can match a pattern with only {a}
- **Missing input fields fail**: a pattern field that doesn't exist in the event is a no-match
