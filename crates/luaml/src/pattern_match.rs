use crate::pattern::Pattern;
use crate::types::{FieldBindings, FieldValue};

/// Match a pattern against a field value, returning bindings on success.
pub fn match_field_value(_pattern: &Pattern, _value: &FieldValue) -> Option<FieldBindings> {
    // TODO: implement
    None
}
