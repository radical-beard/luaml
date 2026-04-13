Trace the dispatch path for this event/script scenario: $ARGUMENTS

Analyze the dispatch flow step by step:

1. **Parse the script(s)**: identify all clauses, their execution policies (including inherited fields), guards, and annotations

2. **Construct the event as a FieldMap**: identify all field types — is each value an Enum (`:name:`), String (`"text"`), Number, Bool, etc.?

3. **For each clause, trace `match_fields()`**:
   - Does the event have every field the clause requires? Missing field = no match
   - For each field, does the pattern accept this value type? Enum vs String mismatch = no match
   - What bindings are produced by Variable (`$name`) patterns?

4. **For matching clauses, trace guard evaluation**:
   - What bindings are available from step 3?
   - Substitute bindings into the guard expression
   - Does the guard evaluate to true?

5. **Report results**:
   - Which clauses match, in what order, with what bindings
   - If nothing matches, explain exactly which field/type/guard caused the failure

6. **Write a test case** that demonstrates the traced behavior — both the matching and non-matching cases.
