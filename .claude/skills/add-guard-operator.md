---
name: add-guard-operator
description: Guide for adding a new operator to the guard expression evaluator. Use when extending the guard language with new comparison or logical operators.
---

# Adding a Guard Operator

Guard expressions live entirely in `crates/luaml/src/guard.rs`. The module has its own tokenizer, parser, and evaluator. Adding an operator touches all three layers.

## Architecture of guard.rs

The module processes guard expressions in three phases:
1. **Tokenize**: `tokenize()` converts a string into a `Vec<Token>`
2. **Parse**: `Parser` struct builds an expression tree respecting operator precedence
3. **Evaluate**: `eval_bool()` / `eval_value()` evaluate the tree against `FieldBindings`

Current operator precedence (lowest to highest):
1. `or`
2. `and`
3. `not` (unary)
4. Comparison: `==`, `~=`/`!=`, `<`, `>`, `<=`, `>=`

## Step 1: Add Token variant

Add a new variant to the `Token` enum (top of guard.rs).

If the operator uses a symbol (like `>=`), it's a punctuation token. If it uses a keyword (like `and`), it's a keyword token.

## Step 2: Add tokenization rule

Update `tokenize()` to recognize the new operator text and emit the token.

For symbol operators: add to the character-matching logic (watch for multi-character sequences like `>=`).
For keyword operators: add to the identifier/keyword matching.

## Step 3: Add parsing

Decide where your operator fits in the precedence hierarchy:
- Same precedence as existing comparison operators? Add to the same parsing function.
- New precedence level? Create a new parsing function and wire it into the chain.

The parser uses recursive descent. Each precedence level has a function that calls the next-higher level.

## Step 4: Add evaluation logic

Update `eval_bool()` or `eval_value()` to handle the new operator.

Consider:
- What types does it accept? (Number-only? String-comparable? Any FieldValue?)
- What happens with type mismatches? (Error? False?)
- Follow existing patterns: comparison operators return bool, logical operators compose bools.

## Step 5: Add tests

guard.rs has 50+ tests. Follow the existing naming pattern. Add tests for:

1. **Basic operation**: operator with valid operands returns correct result
2. **Type handling**: operator with each applicable FieldValue type
3. **Precedence**: operator combined with `and`, `or`, `not`, parentheses
4. **Edge cases**: operands of different types, null values
5. **Error cases**: if the operator should reject certain types

## Step 6: Integration test

**File**: `crates/luaml/tests/integration.rs`

Add a test combining the new guard operator with pattern matching and Lua execution:
1. Register a script with a guard using the new operator
2. Dispatch an event where the guard passes — verify execution
3. Dispatch an event where the guard fails — verify no execution

## Checklist

```sh
cargo fmt --all
cargo clippy --workspace
cargo test
```

## Files touched (summary)

| File | What to change |
|---|---|
| `crates/luaml/src/guard.rs` | Token + tokenize() + parser + evaluator + tests |
| `crates/luaml/tests/integration.rs` | End-to-end guard test |
| `README.md` | Guards operator list (if user-facing) |
