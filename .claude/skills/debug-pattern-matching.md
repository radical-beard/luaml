---
name: debug-pattern-matching
description: Troubleshooting guide for pattern matching issues in luaml. Use when dispatched events don't match expected scripts, or when debugging why a clause fires or doesn't fire.
---

# Debugging Pattern Matching

When an event doesn't match the script you expect (or matches one you don't), work through these common causes in order.

## Common Issue 1: Type Mismatch (Enum vs String)

**Symptom**: Event has the right text value but pattern doesn't match.

**Cause**: `:tui:` (Enum) and `"tui"` (String) are **different types**. `Pattern::Enum` only matches `FieldValue::Enum`. `Pattern::StringLiteral` only matches `FieldValue::String`.

**Check**: Is the event sending `FieldValue::Enum("tui")` while the pattern expects `"tui"` (string), or vice versa?

**In JSON-RPC**: `{"Enum": "tui"}` and `{"String": "tui"}` are different. The pattern `:tui:` in a script requires `{"Enum": "tui"}` in the event.

## Common Issue 2: Missing Input Field

**Symptom**: Pattern has a field the event doesn't include.

**Cause**: `match_fields()` requires every pattern field to exist in the input. If the pattern has `mode: :normal:` but the event doesn't include a `mode` key at all, it's a no-match.

**Note**: Extra event fields are fine — an event with {type, surface, key, mode} matches a pattern with only {type}.

## Common Issue 3: Guard Failure

**Symptom**: Pattern matches but clause doesn't execute.

**Cause**: Guards evaluate after pattern match. Guard failure is **silent** — it means "no match," not an error. Guard errors (e.g., referencing an unbound variable) also produce no-match.

**Check**: Are the guard variable names correct? Guards use the variable name without `$` (e.g., `? d > 0` for a binding from `$d`). Print the bindings from `query()` to verify.

## Common Issue 4: Multi-Clause Inheritance

**Symptom**: A child clause doesn't match even though it looks right.

**Cause**: Child clauses inherit **all** fields from the base (first) clause. If the base has `type: :input:` and the child only specifies `key: :tab:`, the child still requires `type: :input:` in the event.

**Check**: Read the base clause. Every field there is also required by every child clause (unless the child explicitly overrides it).

## Common Issue 5: Pin Variable Not Found

**Symptom**: Pattern with `^name` never matches.

**Cause**: Pin patterns require the variable to already exist in the binding context. If `^saved_id` refers to a variable that wasn't bound by an earlier pattern field in the same clause, `match_field_value_with_context()` returns `None`.

**Note**: Within a single clause, fields are matched in order. A pin can reference a variable bound by a field earlier in the same clause's pattern.

## Common Issue 6: One Match Per Script

**Symptom**: Only one clause fires from a multi-clause script, even when multiple could match.

**Cause**: By design, the engine finds the **first** matching clause per script. If clause 1 and clause 3 both match, only clause 1 executes.

**Check**: The clause order in the file matters. More specific patterns should come before more general ones (wildcards, variables).

## Common Issue 7: Dispatch Stops on Error

**Symptom**: Scripts after a failing one never execute.

**Cause**: If script A errors during Lua execution, `dispatch()` returns the error immediately. Script B never runs.

**Check**: Look at the error. Is a Lua function undefined? Is an API call failing? Fix the failing script.

## Debugging Approach

### Use `query()` instead of `dispatch()`

`query()` finds matches without executing Lua. This isolates matching from execution:

```rust
let matches = engine.query(&event);
for m in &matches {
    println!("matched: {} bindings: {:?}", m.script.source_path.display(), m.bindings);
}
```

If `query()` returns matches but `dispatch()` fails, the problem is in Lua execution, not matching.
If `query()` returns no matches, the problem is in the patterns/event.

### Use `query_subset()` for discovery

`query_subset()` finds clauses whose patterns are a **superset** of the query fields. It ignores guards and doesn't require all pattern fields to be present in the query:

```rust
let candidates = engine.query_subset(&partial_event);
```

This helps answer "what clauses exist for events shaped like this?"

### Write a minimal reproduction test

```rust
#[test]
fn debug_my_issue() {
    let mut engine = LuamlEngine::new().unwrap();
    engine.register("test.luaml", r#"
---
type: :input:
key: $k
---
print("matched: " .. k)
"#).unwrap();

    let event = event(&[
        ("type", FieldValue::Enum("input".into())),
        ("key", FieldValue::String("q".into())),
    ]);

    let matches = engine.query(&event);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].bindings["k"], FieldValue::String("q".into()));
}
```

Use the `event()` helper (defined in `lib.rs` tests) for clean event construction.

### Inspect API binding matching

If the pattern matches but API calls aren't available in Lua, the issue may be in API binding pattern matching. API bindings have their own pattern that's checked against the clause's **execution policy** (not the event). Check that the ApiBinding's pattern matches the clause's literal fields.
