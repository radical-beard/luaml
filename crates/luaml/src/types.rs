use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The runtime value type. Type-distinct: Enum("tui") != String("tui").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FieldValue {
    Enum(String),
    String(String),
    Number(i64),
    Float(f64),
    Bool(bool),
    List(Vec<FieldValue>),
    Map(FieldMap),
    Null,
}

/// Ordered map of string keys to FieldValues. Used for events, execution policies, etc.
pub type FieldMap = HashMap<String, FieldValue>;

/// Variable bindings produced by successful pattern matching.
pub type FieldBindings = HashMap<String, FieldValue>;

#[cfg(test)]
mod tests {
    use super::*;

    // ── Serde roundtrip ────────────────────────────────────────────

    fn roundtrip(value: FieldValue) {
        let json = serde_json::to_string(&value).unwrap();
        let back: FieldValue = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back, "roundtrip failed for {value:?}");
    }

    #[test]
    fn serde_roundtrip_enum() {
        roundtrip(FieldValue::Enum("tui".into()));
    }

    #[test]
    fn serde_roundtrip_string() {
        roundtrip(FieldValue::String("hello".into()));
    }

    #[test]
    fn serde_roundtrip_number() {
        roundtrip(FieldValue::Number(42));
    }

    #[test]
    fn serde_roundtrip_number_negative() {
        roundtrip(FieldValue::Number(-99));
    }

    #[test]
    fn serde_roundtrip_number_zero() {
        roundtrip(FieldValue::Number(0));
    }

    #[test]
    fn serde_roundtrip_number_i64_max() {
        roundtrip(FieldValue::Number(i64::MAX));
    }

    #[test]
    fn serde_roundtrip_number_i64_min() {
        roundtrip(FieldValue::Number(i64::MIN));
    }

    #[test]
    fn serde_roundtrip_float() {
        roundtrip(FieldValue::Float(3.14));
    }

    #[test]
    fn serde_roundtrip_float_negative() {
        roundtrip(FieldValue::Float(-2.718));
    }

    #[test]
    fn serde_roundtrip_float_zero() {
        roundtrip(FieldValue::Float(0.0));
    }

    #[test]
    fn serde_float_infinity_does_not_roundtrip() {
        // serde_json serializes Infinity as {"Float": null}, which fails to deserialize
        let json = serde_json::to_string(&FieldValue::Float(f64::INFINITY)).unwrap();
        let back = serde_json::from_str::<FieldValue>(&json);
        assert!(back.is_err(), "Infinity should not survive JSON roundtrip");
    }

    #[test]
    fn serde_float_neg_infinity_does_not_roundtrip() {
        let json = serde_json::to_string(&FieldValue::Float(f64::NEG_INFINITY)).unwrap();
        let back = serde_json::from_str::<FieldValue>(&json);
        assert!(back.is_err(), "-Infinity should not survive JSON roundtrip");
    }

    #[test]
    fn serde_float_nan_does_not_roundtrip() {
        let json = serde_json::to_string(&FieldValue::Float(f64::NAN)).unwrap();
        let back = serde_json::from_str::<FieldValue>(&json);
        assert!(back.is_err(), "NaN should not survive JSON roundtrip");
    }

    #[test]
    fn serde_roundtrip_bool_true() {
        roundtrip(FieldValue::Bool(true));
    }

    #[test]
    fn serde_roundtrip_bool_false() {
        roundtrip(FieldValue::Bool(false));
    }

    #[test]
    fn serde_roundtrip_null() {
        roundtrip(FieldValue::Null);
    }

    #[test]
    fn serde_roundtrip_list_empty() {
        roundtrip(FieldValue::List(vec![]));
    }

    #[test]
    fn serde_roundtrip_list_of_numbers() {
        roundtrip(FieldValue::List(vec![
            FieldValue::Number(1),
            FieldValue::Number(2),
        ]));
    }

    #[test]
    fn serde_roundtrip_list_of_mixed() {
        roundtrip(FieldValue::List(vec![
            FieldValue::Enum("a".into()),
            FieldValue::String("b".into()),
            FieldValue::Number(3),
            FieldValue::Bool(true),
            FieldValue::Null,
        ]));
    }

    #[test]
    fn serde_roundtrip_map_empty() {
        roundtrip(FieldValue::Map(HashMap::new()));
    }

    #[test]
    fn serde_roundtrip_map_single_entry() {
        let mut map = HashMap::new();
        map.insert("key".into(), FieldValue::String("val".into()));
        roundtrip(FieldValue::Map(map));
    }

    #[test]
    fn serde_roundtrip_list_of_maps() {
        let mut m1 = HashMap::new();
        m1.insert("a".into(), FieldValue::Number(1));
        let mut m2 = HashMap::new();
        m2.insert("b".into(), FieldValue::Number(2));
        roundtrip(FieldValue::List(vec![
            FieldValue::Map(m1),
            FieldValue::Map(m2),
        ]));
    }

    #[test]
    fn serde_roundtrip_map_of_lists() {
        let mut map = HashMap::new();
        map.insert(
            "items".into(),
            FieldValue::List(vec![FieldValue::Number(1), FieldValue::Number(2)]),
        );
        roundtrip(FieldValue::Map(map));
    }

    #[test]
    fn serde_roundtrip_deeply_nested() {
        // Map -> List -> Map -> List -> String (5 levels)
        let inner_list = FieldValue::List(vec![FieldValue::String("deep".into())]);
        let mut inner_map = HashMap::new();
        inner_map.insert("c".into(), inner_list);
        let mid_list = FieldValue::List(vec![FieldValue::Map(inner_map)]);
        let mut outer_map = HashMap::new();
        outer_map.insert("b".into(), mid_list);
        roundtrip(FieldValue::Map(outer_map));
    }

    // ── Type distinction ───────────────────────────────────────────

    #[test]
    fn enum_ne_string_same_content() {
        assert_ne!(
            FieldValue::Enum("tui".into()),
            FieldValue::String("tui".into())
        );
    }

    #[test]
    fn number_ne_float_same_value() {
        assert_ne!(FieldValue::Number(3), FieldValue::Float(3.0));
    }

    #[test]
    fn null_ne_bool_false() {
        assert_ne!(FieldValue::Null, FieldValue::Bool(false));
    }

    #[test]
    fn empty_string_ne_null() {
        assert_ne!(FieldValue::String("".into()), FieldValue::Null);
    }

    // ── Deserialization from raw JSON ──────────────────────────────

    #[test]
    fn deserialize_enum_from_json() {
        let v: FieldValue = serde_json::from_str(r#"{"Enum":"tui"}"#).unwrap();
        assert_eq!(v, FieldValue::Enum("tui".into()));
    }

    #[test]
    fn deserialize_string_from_json() {
        let v: FieldValue = serde_json::from_str(r#"{"String":"hello"}"#).unwrap();
        assert_eq!(v, FieldValue::String("hello".into()));
    }

    #[test]
    fn deserialize_null_from_json() {
        let v: FieldValue = serde_json::from_str(r#""Null""#).unwrap();
        assert_eq!(v, FieldValue::Null);
    }

    #[test]
    fn deserialize_invalid_variant() {
        let result = serde_json::from_str::<FieldValue>(r#"{"Unknown":"x"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_empty_string_value() {
        let v: FieldValue = serde_json::from_str(r#"{"String":""}"#).unwrap();
        assert_eq!(v, FieldValue::String("".into()));
    }
}
