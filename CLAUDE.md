# luaml

A pattern-matched script engine. YAML frontmatter defines typed execution policies; Lua bodies define behavior.

## The Rule

**Frontmatter defines when. Lua defines what.**

Every frontmatter field is a typed pattern. The engine matches incoming events against clause patterns and executes the first match. No special fields, no domain-specific logic in the engine.

## Terminology

- **Execution policy**: The frontmatter — typed pattern fields that determine when a clause executes
- **Behavior**: The Lua code body attached to an execution policy
- **Clause**: One execution policy + one behavior — the atomic unit
- **Script**: A .luaml file containing one or more clauses

## Type System

Enums (`:name:`) and strings (`"text"`) are type-distinct. Variables require `$` prefix. Bare words are parse errors.

## Building & Testing

```sh
cargo build
cargo test
cargo clippy --workspace
cargo fmt --all
```

## Coding Standards

- Edition 2024 Rust, MIT/Apache-2.0
- No domain-specific logic in the engine (no crucible, TUI, agent knowledge)
- Patterns and matching are the core; keep them fast and well-tested
- FieldValue is the single value type; no serde_json::Value in core paths
- API injection via trait — never hardcode consumer APIs
- No unnecessary abstraction. Prefer explicit code over clever indirection.
- Tests for every pattern syntax and matching edge case
