Generate comprehensive tests for a pattern type: $ARGUMENTS

Read the existing tests in `pattern.rs`, `pattern_match.rs`, and `integration.rs` to understand conventions, then create tests covering:

**1. Parsing tests** (in `crates/luaml/src/pattern.rs` `#[cfg(test)]`):
- Valid syntax parses to the correct `Pattern` variant
- Invalid/malformed syntax produces `PatternParseError`
- Edge cases (empty values, extra whitespace, special characters)
- Nesting with other patterns if applicable

**2. Matching tests** (in `crates/luaml/src/pattern_match.rs` `#[cfg(test)]`):
- Successful match returns correct bindings
- Type mismatch returns `None` (especially Enum vs String distinction)
- Matching with variables, wildcards, and pins
- Matching with context (for Pin-related behavior)

**3. Integration test** (in `crates/luaml/tests/integration.rs`):
- Full pipeline: register script with the pattern -> dispatch event -> verify Lua execution
- Guard interaction: pattern with guard that uses bound variables
- Multi-clause: pattern used in base clause, inherited by child

**4. Service test** (in `crates/luaml-service/tests/protocol_integration.rs`):
- JSON-RPC roundtrip if the pattern appears in dispatched events

Follow existing test naming conventions (descriptive `snake_case`).
Use the `event()` helper and `RecordingHandler` pattern from existing tests.
Run `cargo test` after writing to verify all tests pass.
