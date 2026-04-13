---
name: add-pattern-type
description: Guide for adding a new pattern variant to the Pattern enum and all dependent modules. Use when adding new syntax to the pattern language.
---

# Adding a New Pattern Type

Adding a variant to the `Pattern` enum is the most cross-cutting change in luaml. It touches 6+ files in a specific order. Follow these steps precisely.

## Step 1: Define the variant in `pattern.rs`

**File**: `crates/luaml/src/pattern.rs`

1. Add the new variant to the `Pattern` enum (top of file)
2. If it has substructure (like `ListPattern`), define a sub-enum
3. Add parsing logic in `parse_pattern_value()` — define what syntax produces this pattern
4. Add **unit tests** in the `#[cfg(test)]` block:
   - Valid syntax parses to the correct `Pattern` variant
   - Edge cases (whitespace, nesting, empty)
   - Invalid syntax produces `PatternParseError`

**Key constraint**: bare words are always parse errors. New syntax must be unambiguous with existing patterns.

## Step 2: Implement matching in `pattern_match.rs`

**File**: `crates/luaml/src/pattern_match.rs`

1. Add a match arm in `match_field_value()` — define which `FieldValue` variants this pattern matches and what bindings it produces
2. If the pattern needs prior binding context (like `Pin` does), also update `match_field_value_with_context()`
3. Add **unit tests**:
   - Successful match with correct bindings
   - Type mismatch returns `None` (especially Enum vs String if applicable)
   - Interaction with variables, wildcards, pins
   - Nested patterns if applicable

**Key constraint**: `:name:` (Enum) and `"name"` (String) are type-distinct. If your new pattern involves string-like values, decide and test which FieldValue variants it accepts.

## Step 3: Handle in `executor.rs` (if literal)

**File**: `crates/luaml/src/executor.rs`

The function `clause_policy_to_fieldmap()` converts literal patterns to FieldValues for API binding matching. If the new pattern is a **literal** (produces a known value, like Enum or StringLiteral), add it to this conversion.

Variable and structural patterns (Wildcard, Variable, List, Map) typically need no changes here.

## Step 4: Handle in `registry.rs` (if needed)

**File**: `crates/luaml/src/registry.rs`

`subset_matches()` delegates to `match_field_value()`, which you already updated. Usually no changes needed unless the new pattern affects `query_subset` behavior specifically.

## Step 5: Handle in service `connection.rs` (if literal)

**File**: `crates/luaml-service/src/connection.rs`

The function `field_value_to_pattern()` converts FieldValues to Patterns for the `register_api` JSON-RPC method. If the new pattern is literal, add the conversion.

Add tests in the `#[cfg(test)]` block.

## Step 6: Integration tests

**File**: `crates/luaml/tests/integration.rs`

Add end-to-end tests:
1. Register a script using the new pattern syntax
2. Dispatch an event that matches — verify Lua execution and bindings
3. Dispatch an event that should NOT match — verify no execution
4. If the pattern interacts with guards, test that combination
5. If multi-clause, test inheritance behavior

**File**: `crates/luaml-service/tests/protocol_integration.rs`

If the new pattern appears in events (FieldValue encoding), add a JSON-RPC roundtrip test.

## Step 7: Update documentation

- `README.md` — add to the Type System table
- `CLAUDE.md` — update if it affects invariants or the type system description

## Checklist

```sh
cargo fmt --all
cargo clippy --workspace
cargo test
```

All three must pass before the change is complete.

## Files touched (summary)

| File | What to change |
|---|---|
| `crates/luaml/src/pattern.rs` | Pattern enum + parse_pattern_value() + tests |
| `crates/luaml/src/pattern_match.rs` | match_field_value() + tests |
| `crates/luaml/src/executor.rs` | clause_policy_to_fieldmap() (if literal) |
| `crates/luaml/src/registry.rs` | Usually nothing |
| `crates/luaml-service/src/connection.rs` | field_value_to_pattern() (if literal) + tests |
| `crates/luaml/tests/integration.rs` | End-to-end tests |
| `crates/luaml-service/tests/protocol_integration.rs` | JSON-RPC roundtrip (if applicable) |
| `README.md` | Type System table |
