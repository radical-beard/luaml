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
