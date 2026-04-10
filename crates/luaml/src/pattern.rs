use crate::error::{PatternParseError, parse_err};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    Wildcard,
    Enum(String),
    StringLiteral(String),
    NumberLiteral(i64),
    BoolLiteral(bool),
    Variable(String),
    Pin(String),
    List(ListPattern),
    Map(Vec<(String, Pattern)>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListPattern {
    Empty,
    HeadTail {
        head: Box<Pattern>,
        tail: Box<Pattern>,
    },
    Elements(Vec<Pattern>),
}

// TODO: implement parse_pattern_value and helpers
pub fn parse_pattern_value(_input: &str) -> Result<Pattern, PatternParseError> {
    Err(parse_err("not yet implemented", _input))
}
