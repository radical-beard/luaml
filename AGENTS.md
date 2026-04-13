# AGENTS.md

Operating rules and boundaries for AI agents working in this repository.

## Before You Start

Read these before any non-trivial task:
1. `CLAUDE.md` — architecture, dispatch flow, type system, invariants
2. `README.md` — user-facing API and service documentation
3. The source + test file for whatever module you're changing

## Commands

```sh
cargo build                    # build workspace
cargo test                     # all tests
cargo test -p luaml            # core crate only
cargo test -p luaml-service    # service crate only
cargo clippy --workspace       # lint
cargo fmt --all                # format
cargo fmt --all -- --check     # format check
```

## Testing Requirements

Every new feature or bug fix must have tests. Where they go:

| Change | Unit tests in | Integration tests in |
|---|---|---|
| Pattern type/syntax | `pattern.rs`, `pattern_match.rs` | `crates/luaml/tests/integration.rs` |
| Guard operator | `guard.rs` | `crates/luaml/tests/integration.rs` |
| Engine API | `lib.rs` | `crates/luaml/tests/integration.rs` |
| JSON-RPC method | `connection.rs` | `crates/luaml-service/tests/protocol_integration.rs` |
| Parser change | `parser.rs` | `crates/luaml/tests/integration.rs` |

**Naming**: descriptive `snake_case` (e.g. `match_field_value_enum_rejects_string`, `engine_guard_filters_dispatch`).

**Helpers**: use `event()` for constructing test events and `RecordingHandler` for capturing API calls. Both are defined in `lib.rs` tests and `integration.rs`.

## Project Structure Rules

- Two crates: `luaml` (core library) and `luaml-service` (JSON-RPC server)
- Core crate must **never** depend on the service crate
- No `serde_json::Value` in the core crate's public API — `FieldValue` is the boundary type
- Service crate converts at the JSON-RPC boundary (see `json_map_to_field_map` and `field_value_to_pattern` in `connection.rs`)

## Code Style

- Edition 2024 Rust idioms (`let-else`, `if let` chains)
- No domain-specific logic — the engine knows nothing about its consumers
- `thiserror` for error types
- `Vec<(String, Pattern)>` for ordered field collections — ordering matters
- `FieldMap` and `FieldBindings` are `HashMap` aliases from `types.rs`

## Git Workflow

- One logical change per commit
- Run `cargo fmt --all && cargo clippy --workspace && cargo test` before committing
- Descriptive commit messages: what changed and why

## Boundaries

### Always Do
- Run `cargo test` after any change
- Add tests for new functionality
- Maintain domain isolation (no consumer/application knowledge in the engine)
- Use `FieldValue` as the single value type in core paths
- Preserve the one-match-per-script invariant
- Use existing error types from `error.rs`

### Ask First
- Adding new variants to `Pattern` or `FieldValue` (ripple effects across parser, matcher, executor, serde)
- Adding new JSON-RPC methods (requires protocol types + handler + routing + tests)
- Changing the pattern matching algorithm (performance-sensitive, 100+ tests)
- Adding new crate dependencies
- Changing the multi-clause inheritance model

### Never Do
- Put domain-specific logic in the engine
- Use `serde_json::Value` in the core crate's public API
- Break "frontmatter defines when, Lua defines what"
- Skip tests
- Add consumer-specific APIs to the engine
- Hardcode API namespaces or function names
