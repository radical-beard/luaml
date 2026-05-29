use std::collections::HashMap;

use crate::pattern::{ListPattern, Pattern};
use crate::types::{FieldBindings, FieldValue};

/// The reserved enum value that matches any enum (and a missing field in
/// `match_fields`). Used by scripts that declare `surface: :global:` to
/// indicate they are universal — loaded into every engine and eligible on
/// any surface the dispatcher emits.
pub const GLOBAL_ENUM: &str = "global";

fn is_global_enum(pattern: &Pattern) -> bool {
    matches!(pattern, Pattern::Enum(name) if name == GLOBAL_ENUM)
}

/// Match a single pattern against a single FieldValue.
/// Returns `Some(bindings)` on match, `None` on mismatch.
/// Enums and strings are type-distinct: Pattern::Enum only matches FieldValue::Enum,
/// Pattern::StringLiteral only matches FieldValue::String.
///
/// Special case: `Pattern::Enum("global")` matches any `FieldValue::Enum`.
/// See `match_fields` for the absence-tolerant behavior that pairs with it.
pub fn match_field_value(pattern: &Pattern, value: &FieldValue) -> Option<FieldBindings> {
    match pattern {
        Pattern::Wildcard => Some(FieldBindings::new()),

        Pattern::Enum(expected) if expected == GLOBAL_ENUM => match value {
            FieldValue::Enum(_) => Some(FieldBindings::new()),
            _ => None,
        },

        Pattern::Enum(expected) => match value {
            FieldValue::Enum(actual) if expected == actual => Some(FieldBindings::new()),
            _ => None,
        },

        Pattern::StringLiteral(expected) => match value {
            FieldValue::String(actual) if expected == actual => Some(FieldBindings::new()),
            _ => None,
        },

        Pattern::NumberLiteral(expected) => match value {
            FieldValue::Number(actual) if expected == actual => Some(FieldBindings::new()),
            _ => None,
        },

        Pattern::BoolLiteral(expected) => match value {
            FieldValue::Bool(actual) if expected == actual => Some(FieldBindings::new()),
            _ => None,
        },

        Pattern::Variable(name) => {
            let mut bindings = FieldBindings::new();
            bindings.insert(name.clone(), value.clone());
            Some(bindings)
        }

        Pattern::Pin(_name) => {
            // Pin matching requires an existing binding context.
            // See match_field_value_with_context.
            None
        }

        Pattern::List(list_pat) => match_list(list_pat, value),
        Pattern::Map(fields) => match_map(fields, value),
    }
}

/// Match a pattern with an existing binding context (for Pin patterns).
pub fn match_field_value_with_context(
    pattern: &Pattern,
    value: &FieldValue,
    context: &FieldBindings,
) -> Option<FieldBindings> {
    match pattern {
        Pattern::Pin(name) => {
            let expected = context.get(name)?;
            if expected == value {
                Some(FieldBindings::new())
            } else {
                None
            }
        }
        _ => match_field_value(pattern, value),
    }
}

/// Match a set of pattern fields against a FieldMap (execution policy vs event).
/// Every pattern field must have a matching value in the input map.
/// Extra fields in the input are ignored.
///
/// A pattern of `Pattern::Enum("global")` is tolerant of an absent key —
/// it matches whether the key is missing from the input or carries any
/// enum value.
pub fn match_fields(
    pattern_fields: &[(String, Pattern)],
    input: &HashMap<String, FieldValue>,
) -> Option<FieldBindings> {
    let mut all_bindings = FieldBindings::new();
    for (key, pattern) in pattern_fields {
        let value = match input.get(key) {
            Some(v) => v,
            None => {
                if is_global_enum(pattern) {
                    continue;
                }
                return None;
            }
        };
        let bindings = match_field_value_with_context(pattern, value, &all_bindings)?;
        all_bindings.extend(bindings);
    }
    Some(all_bindings)
}

fn match_list(list_pat: &ListPattern, value: &FieldValue) -> Option<FieldBindings> {
    let array = match value {
        FieldValue::List(arr) => arr,
        _ => return None,
    };

    match list_pat {
        ListPattern::Empty => {
            if array.is_empty() {
                Some(FieldBindings::new())
            } else {
                None
            }
        }

        ListPattern::HeadTail { head, tail } => {
            if array.is_empty() {
                return None;
            }
            let head_val = &array[0];
            let tail_val = FieldValue::List(array[1..].to_vec());

            let mut bindings = match_field_value(head, head_val)?;
            let tail_bindings = match_field_value(tail, &tail_val)?;
            bindings.extend(tail_bindings);
            Some(bindings)
        }

        ListPattern::Elements(patterns) => {
            if array.len() != patterns.len() {
                return None;
            }
            let mut bindings = FieldBindings::new();
            for (pat, val) in patterns.iter().zip(array.iter()) {
                let sub = match_field_value(pat, val)?;
                bindings.extend(sub);
            }
            Some(bindings)
        }
    }
}

fn match_map(fields: &[(String, Pattern)], value: &FieldValue) -> Option<FieldBindings> {
    let obj = match value {
        FieldValue::Map(m) => m,
        _ => return None,
    };

    let mut bindings = FieldBindings::new();
    for (key, pat) in fields {
        let val = obj.get(key)?;
        let sub = match_field_value(pat, val)?;
        bindings.extend(sub);
    }
    Some(bindings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FieldValue;
    use std::collections::HashMap;

    fn fv_enum(s: &str) -> FieldValue {
        FieldValue::Enum(s.into())
    }
    fn fv_str(s: &str) -> FieldValue {
        FieldValue::String(s.into())
    }
    fn fv_num(n: i64) -> FieldValue {
        FieldValue::Number(n)
    }
    fn fv_bool(b: bool) -> FieldValue {
        FieldValue::Bool(b)
    }
    fn fv_list(items: Vec<FieldValue>) -> FieldValue {
        FieldValue::List(items)
    }
    fn fv_map(pairs: Vec<(&str, FieldValue)>) -> FieldValue {
        FieldValue::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    // ---- Wildcard ----

    #[test]
    fn wildcard_matches_anything() {
        assert!(match_field_value(&Pattern::Wildcard, &fv_enum("hello")).is_some());
        assert!(match_field_value(&Pattern::Wildcard, &fv_str("hello")).is_some());
        assert!(match_field_value(&Pattern::Wildcard, &fv_num(42)).is_some());
        assert!(match_field_value(&Pattern::Wildcard, &fv_bool(true)).is_some());
        assert!(match_field_value(&Pattern::Wildcard, &FieldValue::Null).is_some());
        assert!(match_field_value(&Pattern::Wildcard, &fv_list(vec![])).is_some());
    }

    #[test]
    fn wildcard_binds_nothing() {
        let bindings = match_field_value(&Pattern::Wildcard, &fv_str("x")).unwrap();
        assert!(bindings.is_empty());
    }

    // ---- Enum matching (type-distinct) ----

    #[test]
    fn enum_matches_same_enum() {
        let pat = Pattern::Enum("tui".into());
        assert!(match_field_value(&pat, &fv_enum("tui")).is_some());
    }

    #[test]
    fn enum_rejects_different_enum() {
        let pat = Pattern::Enum("tui".into());
        assert!(match_field_value(&pat, &fv_enum("runner")).is_none());
    }

    #[test]
    fn enum_rejects_string_with_same_content() {
        let pat = Pattern::Enum("tui".into());
        assert!(match_field_value(&pat, &fv_str("tui")).is_none());
    }

    #[test]
    fn enum_rejects_number() {
        let pat = Pattern::Enum("tui".into());
        assert!(match_field_value(&pat, &fv_num(42)).is_none());
    }

    // ---- :global: universal enum wildcard ----

    #[test]
    fn global_enum_matches_any_enum_value() {
        let pat = Pattern::Enum("global".into());
        assert!(match_field_value(&pat, &fv_enum("tui")).is_some());
        assert!(match_field_value(&pat, &fv_enum("daemon")).is_some());
        assert!(match_field_value(&pat, &fv_enum("runner")).is_some());
        assert!(match_field_value(&pat, &fv_enum("global")).is_some());
    }

    #[test]
    fn global_enum_still_rejects_non_enum_values() {
        let pat = Pattern::Enum("global".into());
        assert!(match_field_value(&pat, &fv_str("tui")).is_none());
        assert!(match_field_value(&pat, &fv_num(42)).is_none());
        assert!(match_field_value(&pat, &fv_bool(true)).is_none());
    }

    #[test]
    fn global_enum_matches_missing_field_in_match_fields() {
        let pattern = vec![("surface".to_string(), Pattern::Enum("global".into()))];
        let input: HashMap<String, FieldValue> = HashMap::new();
        assert!(match_fields(&pattern, &input).is_some());
    }

    #[test]
    fn non_global_enum_still_requires_field() {
        let pattern = vec![("surface".to_string(), Pattern::Enum("tui".into()))];
        let input: HashMap<String, FieldValue> = HashMap::new();
        assert!(match_fields(&pattern, &input).is_none());
    }

    #[test]
    fn global_enum_with_present_mismatched_key_uses_wildcard() {
        let pattern = vec![("surface".to_string(), Pattern::Enum("global".into()))];
        let mut input: HashMap<String, FieldValue> = HashMap::new();
        input.insert("surface".into(), fv_enum("daemon"));
        assert!(match_fields(&pattern, &input).is_some());
    }

    // ---- String literal matching (type-distinct) ----

    #[test]
    fn string_matches_same_string() {
        let pat = Pattern::StringLiteral("overlay.settings".into());
        assert!(match_field_value(&pat, &fv_str("overlay.settings")).is_some());
    }

    #[test]
    fn string_rejects_different_string() {
        let pat = Pattern::StringLiteral("overlay.settings".into());
        assert!(match_field_value(&pat, &fv_str("overlay.fuzzy")).is_none());
    }

    #[test]
    fn string_rejects_enum_with_same_content() {
        let pat = Pattern::StringLiteral("tui".into());
        assert!(match_field_value(&pat, &fv_enum("tui")).is_none());
    }

    // ---- Number matching ----

    #[test]
    fn number_matches_equal() {
        assert!(match_field_value(&Pattern::NumberLiteral(42), &fv_num(42)).is_some());
    }

    #[test]
    fn number_rejects_different() {
        assert!(match_field_value(&Pattern::NumberLiteral(42), &fv_num(43)).is_none());
    }

    #[test]
    fn number_rejects_string() {
        assert!(match_field_value(&Pattern::NumberLiteral(42), &fv_str("42")).is_none());
    }

    // ---- Bool matching ----

    #[test]
    fn bool_matches_true() {
        assert!(match_field_value(&Pattern::BoolLiteral(true), &fv_bool(true)).is_some());
    }

    #[test]
    fn bool_rejects_mismatch() {
        assert!(match_field_value(&Pattern::BoolLiteral(true), &fv_bool(false)).is_none());
    }

    // ---- Variable binding ----

    #[test]
    fn variable_captures_enum() {
        let pat = Pattern::Variable("x".into());
        let bindings = match_field_value(&pat, &fv_enum("tui")).unwrap();
        assert_eq!(bindings["x"], fv_enum("tui"));
    }

    #[test]
    fn variable_captures_string() {
        let pat = Pattern::Variable("x".into());
        let bindings = match_field_value(&pat, &fv_str("hello")).unwrap();
        assert_eq!(bindings["x"], fv_str("hello"));
    }

    #[test]
    fn variable_captures_number() {
        let pat = Pattern::Variable("n".into());
        let bindings = match_field_value(&pat, &fv_num(42)).unwrap();
        assert_eq!(bindings["n"], fv_num(42));
    }

    #[test]
    fn variable_captures_list() {
        let pat = Pattern::Variable("items".into());
        let val = fv_list(vec![fv_num(1), fv_num(2)]);
        let bindings = match_field_value(&pat, &val).unwrap();
        assert_eq!(bindings["items"], fv_list(vec![fv_num(1), fv_num(2)]));
    }

    // ---- Empty list ----

    #[test]
    fn empty_list_matches_empty() {
        let pat = Pattern::List(ListPattern::Empty);
        assert!(match_field_value(&pat, &fv_list(vec![])).is_some());
    }

    #[test]
    fn empty_list_rejects_non_empty() {
        let pat = Pattern::List(ListPattern::Empty);
        assert!(match_field_value(&pat, &fv_list(vec![fv_num(1)])).is_none());
    }

    #[test]
    fn empty_list_rejects_non_list() {
        let pat = Pattern::List(ListPattern::Empty);
        assert!(match_field_value(&pat, &fv_str("not list")).is_none());
    }

    // ---- Head|tail ----

    #[test]
    fn head_tail_binds_first_and_rest() {
        let pat = Pattern::List(ListPattern::HeadTail {
            head: Box::new(Pattern::Variable("h".into())),
            tail: Box::new(Pattern::Variable("t".into())),
        });
        let val = fv_list(vec![fv_str("a"), fv_str("b"), fv_str("c")]);
        let bindings = match_field_value(&pat, &val).unwrap();
        assert_eq!(bindings["h"], fv_str("a"));
        assert_eq!(bindings["t"], fv_list(vec![fv_str("b"), fv_str("c")]));
    }

    #[test]
    fn head_tail_single_element() {
        let pat = Pattern::List(ListPattern::HeadTail {
            head: Box::new(Pattern::Variable("h".into())),
            tail: Box::new(Pattern::Variable("t".into())),
        });
        let val = fv_list(vec![fv_str("only")]);
        let bindings = match_field_value(&pat, &val).unwrap();
        assert_eq!(bindings["h"], fv_str("only"));
        assert_eq!(bindings["t"], fv_list(vec![]));
    }

    #[test]
    fn head_tail_rejects_empty() {
        let pat = Pattern::List(ListPattern::HeadTail {
            head: Box::new(Pattern::Variable("h".into())),
            tail: Box::new(Pattern::Variable("t".into())),
        });
        assert!(match_field_value(&pat, &fv_list(vec![])).is_none());
    }

    #[test]
    fn head_tail_with_enum_head_matches() {
        let pat = Pattern::List(ListPattern::HeadTail {
            head: Box::new(Pattern::Enum("review".into())),
            tail: Box::new(Pattern::Variable("rest".into())),
        });
        let val = fv_list(vec![fv_enum("review"), fv_enum("code")]);
        let bindings = match_field_value(&pat, &val).unwrap();
        assert_eq!(bindings["rest"], fv_list(vec![fv_enum("code")]));
    }

    #[test]
    fn head_tail_with_enum_head_rejects_mismatch() {
        let pat = Pattern::List(ListPattern::HeadTail {
            head: Box::new(Pattern::Enum("review".into())),
            tail: Box::new(Pattern::Variable("rest".into())),
        });
        let val = fv_list(vec![fv_enum("test"), fv_enum("code")]);
        assert!(match_field_value(&pat, &val).is_none());
    }

    // ---- Fixed elements ----

    #[test]
    fn fixed_elements_match_exact_length() {
        let pat = Pattern::List(ListPattern::Elements(vec![
            Pattern::Variable("a".into()),
            Pattern::Variable("b".into()),
        ]));
        let val = fv_list(vec![fv_str("x"), fv_str("y")]);
        let bindings = match_field_value(&pat, &val).unwrap();
        assert_eq!(bindings["a"], fv_str("x"));
        assert_eq!(bindings["b"], fv_str("y"));
    }

    #[test]
    fn fixed_elements_reject_wrong_length() {
        let pat = Pattern::List(ListPattern::Elements(vec![
            Pattern::Variable("a".into()),
            Pattern::Variable("b".into()),
        ]));
        assert!(match_field_value(&pat, &fv_list(vec![fv_num(1), fv_num(2), fv_num(3)])).is_none());
        assert!(match_field_value(&pat, &fv_list(vec![fv_num(1)])).is_none());
    }

    // ---- Map matching ----

    #[test]
    fn map_matches_and_extracts() {
        let pat = Pattern::Map(vec![
            ("phase".into(), Pattern::Variable("p".into())),
            ("idle".into(), Pattern::Variable("is_idle".into())),
        ]);
        let val = fv_map(vec![
            ("phase", fv_enum("working")),
            ("idle", fv_bool(false)),
            ("extra", fv_num(99)),
        ]);
        let bindings = match_field_value(&pat, &val).unwrap();
        assert_eq!(bindings["p"], fv_enum("working"));
        assert_eq!(bindings["is_idle"], fv_bool(false));
    }

    #[test]
    fn map_rejects_missing_key() {
        let pat = Pattern::Map(vec![
            ("phase".into(), Pattern::Variable("p".into())),
            ("missing".into(), Pattern::Variable("m".into())),
        ]);
        let val = fv_map(vec![("phase", fv_str("x"))]);
        assert!(match_field_value(&pat, &val).is_none());
    }

    #[test]
    fn map_with_enum_guard() {
        let pat = Pattern::Map(vec![
            ("status".into(), Pattern::Enum("active".into())),
            ("name".into(), Pattern::Variable("n".into())),
        ]);
        let val = fv_map(vec![
            ("status", fv_enum("active")),
            ("name", fv_str("agent-1")),
        ]);
        let bindings = match_field_value(&pat, &val).unwrap();
        assert_eq!(bindings["n"], fv_str("agent-1"));

        let val2 = fv_map(vec![
            ("status", fv_enum("idle")),
            ("name", fv_str("agent-1")),
        ]);
        assert!(match_field_value(&pat, &val2).is_none());
    }

    #[test]
    fn map_rejects_non_map() {
        let pat = Pattern::Map(vec![("a".into(), Pattern::Wildcard)]);
        assert!(match_field_value(&pat, &fv_str("string")).is_none());
        assert!(match_field_value(&pat, &fv_list(vec![fv_num(1)])).is_none());
    }

    #[test]
    fn empty_map_matches_any_map() {
        let pat = Pattern::Map(Vec::new());
        assert!(match_field_value(&pat, &fv_map(vec![])).is_some());
        assert!(match_field_value(&pat, &fv_map(vec![("a", fv_num(1))])).is_some());
    }

    // ---- Nested patterns ----

    #[test]
    fn nested_map_in_map() {
        let pat = Pattern::Map(vec![(
            "intent".into(),
            Pattern::Map(vec![(
                "requested_work".into(),
                Pattern::List(ListPattern::HeadTail {
                    head: Box::new(Pattern::Variable("first".into())),
                    tail: Box::new(Pattern::Variable("rest".into())),
                }),
            )]),
        )]);
        let val = fv_map(vec![(
            "intent",
            fv_map(vec![(
                "requested_work",
                fv_list(vec![fv_str("review"), fv_str("test"), fv_str("refactor")]),
            )]),
        )]);
        let bindings = match_field_value(&pat, &val).unwrap();
        assert_eq!(bindings["first"], fv_str("review"));
        assert_eq!(
            bindings["rest"],
            fv_list(vec![fv_str("test"), fv_str("refactor")])
        );
    }

    // ---- match_fields ----

    #[test]
    fn match_fields_matches_subset() {
        let fields = vec![
            ("type".into(), Pattern::Enum("input".into())),
            ("surface".into(), Pattern::Enum("tui".into())),
        ];
        let mut input = HashMap::new();
        input.insert("type".into(), fv_enum("input"));
        input.insert("surface".into(), fv_enum("tui"));
        input.insert("key".into(), fv_str("q"));
        assert!(match_fields(&fields, &input).is_some());
    }

    #[test]
    fn match_fields_rejects_missing_key() {
        let fields = vec![
            ("type".into(), Pattern::Enum("input".into())),
            ("surface".into(), Pattern::Enum("tui".into())),
        ];
        let mut input = HashMap::new();
        input.insert("type".into(), fv_enum("input"));
        assert!(match_fields(&fields, &input).is_none());
    }

    #[test]
    fn match_fields_captures_bindings() {
        let fields = vec![
            ("type".into(), Pattern::Enum("input".into())),
            ("key".into(), Pattern::Variable("pressed".into())),
        ];
        let mut input = HashMap::new();
        input.insert("type".into(), fv_enum("input"));
        input.insert("key".into(), fv_str("q"));
        let bindings = match_fields(&fields, &input).unwrap();
        assert_eq!(bindings["pressed"], fv_str("q"));
    }

    #[test]
    fn match_fields_uses_previous_bindings_for_pins() {
        let fields = vec![
            ("first".into(), Pattern::Variable("x".into())),
            ("second".into(), Pattern::Pin("x".into())),
        ];
        let mut input = HashMap::new();
        input.insert("first".into(), fv_str("same"));
        input.insert("second".into(), fv_str("same"));

        let bindings = match_fields(&fields, &input).unwrap();
        assert_eq!(bindings["x"], fv_str("same"));
    }

    #[test]
    fn match_fields_rejects_pin_mismatch() {
        let fields = vec![
            ("first".into(), Pattern::Variable("x".into())),
            ("second".into(), Pattern::Pin("x".into())),
        ];
        let mut input = HashMap::new();
        input.insert("first".into(), fv_str("same"));
        input.insert("second".into(), fv_str("different"));

        assert!(match_fields(&fields, &input).is_none());
    }

    // ---- Pin matching ----

    #[test]
    fn pin_matches_with_context() {
        let context = HashMap::from([("expected".into(), fv_str("hello"))]);
        let pat = Pattern::Pin("expected".into());
        assert!(match_field_value_with_context(&pat, &fv_str("hello"), &context).is_some());
    }

    #[test]
    fn pin_rejects_different_value() {
        let context = HashMap::from([("expected".into(), fv_str("hello"))]);
        let pat = Pattern::Pin("expected".into());
        assert!(match_field_value_with_context(&pat, &fv_str("world"), &context).is_none());
    }

    #[test]
    fn pin_rejects_missing_context_var() {
        let context = HashMap::new();
        let pat = Pattern::Pin("missing".into());
        assert!(match_field_value_with_context(&pat, &fv_str("x"), &context).is_none());
    }

    #[test]
    fn pin_type_distinct() {
        let context = HashMap::from([("x".into(), fv_enum("tui"))]);
        let pat = Pattern::Pin("x".into());
        // Enum("tui") should match Enum("tui")
        assert!(match_field_value_with_context(&pat, &fv_enum("tui"), &context).is_some());
        // Enum("tui") should NOT match String("tui")
        assert!(match_field_value_with_context(&pat, &fv_str("tui"), &context).is_none());
    }

    // ---- Null ----

    #[test]
    fn variable_captures_null() {
        let pat = Pattern::Variable("x".into());
        let bindings = match_field_value(&pat, &FieldValue::Null).unwrap();
        assert_eq!(bindings["x"], FieldValue::Null);
    }

    // ── Systematic cross-type rejection ────────────────────────────

    #[test]
    fn enum_rejects_bool() {
        assert!(match_field_value(&Pattern::Enum("x".into()), &fv_bool(true)).is_none());
    }

    #[test]
    fn enum_rejects_list() {
        assert!(match_field_value(&Pattern::Enum("x".into()), &fv_list(vec![])).is_none());
    }

    #[test]
    fn enum_rejects_map() {
        assert!(match_field_value(&Pattern::Enum("x".into()), &fv_map(vec![])).is_none());
    }

    #[test]
    fn enum_rejects_null() {
        assert!(match_field_value(&Pattern::Enum("x".into()), &FieldValue::Null).is_none());
    }

    #[test]
    fn enum_rejects_float() {
        assert!(match_field_value(&Pattern::Enum("x".into()), &FieldValue::Float(1.0)).is_none());
    }

    #[test]
    fn string_rejects_number() {
        assert!(match_field_value(&Pattern::StringLiteral("42".into()), &fv_num(42)).is_none());
    }

    #[test]
    fn string_rejects_bool() {
        assert!(
            match_field_value(&Pattern::StringLiteral("true".into()), &fv_bool(true)).is_none()
        );
    }

    #[test]
    fn string_rejects_float() {
        assert!(
            match_field_value(
                &Pattern::StringLiteral("1.0".into()),
                &FieldValue::Float(1.0)
            )
            .is_none()
        );
    }

    #[test]
    fn string_rejects_null() {
        assert!(match_field_value(&Pattern::StringLiteral("".into()), &FieldValue::Null).is_none());
    }

    #[test]
    fn string_rejects_list() {
        assert!(
            match_field_value(&Pattern::StringLiteral("[]".into()), &fv_list(vec![])).is_none()
        );
    }

    #[test]
    fn string_rejects_map() {
        assert!(match_field_value(&Pattern::StringLiteral("{}".into()), &fv_map(vec![])).is_none());
    }

    #[test]
    fn number_rejects_float_same_value() {
        // Type-distinct: Number(3) does not match Float(3.0)
        assert!(match_field_value(&Pattern::NumberLiteral(3), &FieldValue::Float(3.0)).is_none());
    }

    #[test]
    fn number_rejects_bool() {
        assert!(match_field_value(&Pattern::NumberLiteral(1), &fv_bool(true)).is_none());
    }

    #[test]
    fn number_rejects_enum() {
        assert!(match_field_value(&Pattern::NumberLiteral(42), &fv_enum("42")).is_none());
    }

    #[test]
    fn number_rejects_null() {
        assert!(match_field_value(&Pattern::NumberLiteral(0), &FieldValue::Null).is_none());
    }

    #[test]
    fn bool_rejects_number() {
        assert!(match_field_value(&Pattern::BoolLiteral(true), &fv_num(1)).is_none());
    }

    #[test]
    fn bool_rejects_string() {
        assert!(match_field_value(&Pattern::BoolLiteral(true), &fv_str("true")).is_none());
    }

    #[test]
    fn bool_rejects_enum() {
        assert!(match_field_value(&Pattern::BoolLiteral(true), &fv_enum("true")).is_none());
    }

    #[test]
    fn bool_rejects_null() {
        assert!(match_field_value(&Pattern::BoolLiteral(false), &FieldValue::Null).is_none());
    }

    // ── Variable binding completeness ──────────────────────────────

    #[test]
    fn variable_captures_bool() {
        let b = match_field_value(&Pattern::Variable("x".into()), &fv_bool(true)).unwrap();
        assert_eq!(b["x"], fv_bool(true));
    }

    #[test]
    fn variable_captures_float() {
        let b =
            match_field_value(&Pattern::Variable("x".into()), &FieldValue::Float(1.5)).unwrap();
        assert_eq!(b["x"], FieldValue::Float(1.5));
    }

    #[test]
    fn variable_captures_map() {
        let val = fv_map(vec![("a", fv_num(1))]);
        let b = match_field_value(&Pattern::Variable("x".into()), &val).unwrap();
        assert_eq!(b["x"], val);
    }

    #[test]
    fn variable_captures_empty_list() {
        let b = match_field_value(&Pattern::Variable("x".into()), &fv_list(vec![])).unwrap();
        assert_eq!(b["x"], fv_list(vec![]));
    }

    #[test]
    fn variable_captures_empty_map() {
        let b = match_field_value(&Pattern::Variable("x".into()), &fv_map(vec![])).unwrap();
        assert_eq!(b["x"], fv_map(vec![]));
    }

    // ── List pattern depth ─────────────────────────────────────────

    #[test]
    fn head_tail_with_nested_list_values() {
        let pat = Pattern::List(ListPattern::HeadTail {
            head: Box::new(Pattern::Variable("h".into())),
            tail: Box::new(Pattern::Variable("t".into())),
        });
        // Head is itself a list
        let val = fv_list(vec![fv_list(vec![fv_num(1), fv_num(2)]), fv_num(3)]);
        let b = match_field_value(&pat, &val).unwrap();
        assert_eq!(b["h"], fv_list(vec![fv_num(1), fv_num(2)]));
        assert_eq!(b["t"], fv_list(vec![fv_num(3)]));
    }

    #[test]
    fn head_tail_with_many_elements() {
        let pat = Pattern::List(ListPattern::HeadTail {
            head: Box::new(Pattern::Variable("h".into())),
            tail: Box::new(Pattern::Variable("t".into())),
        });
        let items: Vec<FieldValue> = (0..100).map(fv_num).collect();
        let val = FieldValue::List(items);
        let b = match_field_value(&pat, &val).unwrap();
        assert_eq!(b["h"], fv_num(0));
        if let FieldValue::List(tail) = &b["t"] {
            assert_eq!(tail.len(), 99);
        } else {
            panic!("tail should be a list");
        }
    }

    #[test]
    fn head_tail_literal_head_type_mismatch() {
        let pat = Pattern::List(ListPattern::HeadTail {
            head: Box::new(Pattern::StringLiteral("x".into())),
            tail: Box::new(Pattern::Variable("rest".into())),
        });
        // List starts with Enum("x"), not String("x")
        let val = fv_list(vec![fv_enum("x"), fv_num(1)]);
        assert!(match_field_value(&pat, &val).is_none());
    }

    #[test]
    fn elements_with_wildcards() {
        let pat = Pattern::List(ListPattern::Elements(vec![
            Pattern::Wildcard,
            Pattern::Variable("x".into()),
            Pattern::Wildcard,
        ]));
        let val = fv_list(vec![fv_num(1), fv_num(2), fv_num(3)]);
        let b = match_field_value(&pat, &val).unwrap();
        assert_eq!(b["x"], fv_num(2));
        assert_eq!(b.len(), 1); // only $x bound
    }

    #[test]
    fn elements_empty_matches_empty_list() {
        let pat = Pattern::List(ListPattern::Elements(vec![]));
        assert!(match_field_value(&pat, &fv_list(vec![])).is_some());
    }

    #[test]
    fn elements_empty_rejects_non_empty() {
        let pat = Pattern::List(ListPattern::Elements(vec![]));
        assert!(match_field_value(&pat, &fv_list(vec![fv_num(1)])).is_none());
    }

    #[test]
    fn elements_with_nested_map() {
        let pat = Pattern::List(ListPattern::Elements(vec![Pattern::Map(vec![(
            "key".into(),
            Pattern::Variable("v".into()),
        )])]));
        let val = fv_list(vec![fv_map(vec![("key", fv_str("value"))])]);
        let b = match_field_value(&pat, &val).unwrap();
        assert_eq!(b["v"], fv_str("value"));
    }

    // ── Pin pattern completeness ───────────────────────────────────

    #[test]
    fn pin_matches_number_context() {
        let ctx = HashMap::from([("x".into(), fv_num(42))]);
        assert!(
            match_field_value_with_context(&Pattern::Pin("x".into()), &fv_num(42), &ctx).is_some()
        );
    }

    #[test]
    fn pin_matches_bool_context() {
        let ctx = HashMap::from([("x".into(), fv_bool(true))]);
        assert!(
            match_field_value_with_context(&Pattern::Pin("x".into()), &fv_bool(true), &ctx)
                .is_some()
        );
    }

    #[test]
    fn pin_matches_null_context() {
        let ctx = HashMap::from([("x".into(), FieldValue::Null)]);
        assert!(
            match_field_value_with_context(&Pattern::Pin("x".into()), &FieldValue::Null, &ctx)
                .is_some()
        );
    }

    #[test]
    fn pin_rejects_wrong_type_same_content() {
        // Context has Enum("x"), value is String("x") — type-distinct
        let ctx = HashMap::from([("v".into(), fv_enum("x"))]);
        assert!(
            match_field_value_with_context(&Pattern::Pin("v".into()), &fv_str("x"), &ctx).is_none()
        );
    }

    // ── match_fields edge cases ────────────────────────────────────

    #[test]
    fn empty_pattern_matches_anything() {
        let input = HashMap::from([("a".to_string(), fv_num(1))]);
        assert!(match_fields(&[], &input).is_some());
    }

    #[test]
    fn empty_pattern_empty_input() {
        assert!(match_fields(&[], &HashMap::new()).is_some());
    }

    #[test]
    fn pattern_against_empty_input() {
        let fields = vec![("a".into(), Pattern::Variable("x".into()))];
        assert!(match_fields(&fields, &HashMap::new()).is_none());
    }

    #[test]
    fn match_fields_multiple_bindings() {
        let fields = vec![
            ("a".into(), Pattern::Variable("x".into())),
            ("b".into(), Pattern::Variable("y".into())),
            ("c".into(), Pattern::Variable("z".into())),
        ];
        let input = HashMap::from([
            ("a".to_string(), fv_num(1)),
            ("b".to_string(), fv_num(2)),
            ("c".to_string(), fv_num(3)),
        ]);
        let b = match_fields(&fields, &input).unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(b["x"], fv_num(1));
        assert_eq!(b["y"], fv_num(2));
        assert_eq!(b["z"], fv_num(3));
    }

    #[test]
    fn match_fields_mix_of_literals_and_variables() {
        let fields = vec![
            ("type".into(), Pattern::Enum("input".into())),
            ("key".into(), Pattern::Variable("k".into())),
        ];
        let input = HashMap::from([
            ("type".to_string(), fv_enum("input")),
            ("key".to_string(), fv_str("q")),
        ]);
        let b = match_fields(&fields, &input).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b["k"], fv_str("q"));
    }

    // ── Empty map pattern rejects non-map ──────────────────────────

    #[test]
    fn empty_map_pattern_rejects_non_map() {
        let pat = Pattern::Map(Vec::new());
        assert!(match_field_value(&pat, &fv_str("string")).is_none());
        assert!(match_field_value(&pat, &fv_num(42)).is_none());
        assert!(match_field_value(&pat, &fv_list(vec![])).is_none());
    }
}
