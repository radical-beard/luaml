//! Guard expression evaluator for pattern-matched script clauses.
//!
//! Guard expressions are simple boolean expressions that run over
//! pattern-bound variables after a successful pattern match.
//!
//! Syntax (within frontmatter):
//!   ? depth > 0
//!   ? phase == "planning"
//!   ? depth >= 1 and phase ~= "idle"
//!   ? not skip
//!
//! Supported operators:
//!   ==, ~= (not equal), !=, <, >, <=, >=
//!   and, or, not
//!   Parentheses for grouping

use crate::types::{FieldBindings, FieldValue};

/// Evaluate a guard expression string against a set of bindings.
/// Returns `true` if the guard passes, `false` if it fails.
pub fn evaluate_guard(expr: &str, bindings: &FieldBindings) -> Result<bool, String> {
    let tokens = tokenize(expr)?;
    let mut parser = Parser::new(&tokens);
    let ast = parser.parse_expr()?;
    if parser.pos < parser.tokens.len() {
        return Err(format!(
            "unexpected token after expression: {:?}",
            parser.tokens[parser.pos]
        ));
    }
    eval_bool(&ast, bindings)
}

// ── Tokens ──────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    StringLit(String),
    NumberLit(f64),
    BoolLit(bool),
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,
    And,
    Or,
    Not,
    Nil,
    LParen,
    RParen,
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '=' if i + 1 < chars.len() && chars[i + 1] == '=' => {
                tokens.push(Token::Eq);
                i += 2;
            }
            '~' if i + 1 < chars.len() && chars[i + 1] == '=' => {
                tokens.push(Token::Neq);
                i += 2;
            }
            '!' if i + 1 < chars.len() && chars[i + 1] == '=' => {
                tokens.push(Token::Neq);
                i += 2;
            }
            '<' if i + 1 < chars.len() && chars[i + 1] == '=' => {
                tokens.push(Token::Lte);
                i += 2;
            }
            '>' if i + 1 < chars.len() && chars[i + 1] == '=' => {
                tokens.push(Token::Gte);
                i += 2;
            }
            '<' => {
                tokens.push(Token::Lt);
                i += 1;
            }
            '>' => {
                tokens.push(Token::Gt);
                i += 1;
            }
            '"' | '\'' => {
                let quote = chars[i];
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != quote {
                    i += 1;
                }
                if i >= chars.len() {
                    return Err("unterminated string literal in guard expression".into());
                }
                let s: String = chars[start..i].iter().collect();
                tokens.push(Token::StringLit(s));
                i += 1;
            }
            c if c.is_ascii_digit()
                || (c == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) =>
            {
                let start = i;
                if c == '-' {
                    i += 1;
                }
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                let n: f64 = s
                    .parse()
                    .map_err(|_| format!("invalid number in guard expression: {s}"))?;
                tokens.push(Token::NumberLit(n));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                match word.as_str() {
                    "and" => tokens.push(Token::And),
                    "or" => tokens.push(Token::Or),
                    "not" => tokens.push(Token::Not),
                    "true" => tokens.push(Token::BoolLit(true)),
                    "false" => tokens.push(Token::BoolLit(false)),
                    "nil" => tokens.push(Token::Nil),
                    _ => tokens.push(Token::Ident(word)),
                }
            }
            c => return Err(format!("unexpected character in guard expression: '{c}'")),
        }
    }
    Ok(tokens)
}

// ── AST ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Expr {
    BoolLit(bool),
    NumberLit(f64),
    StringLit(String),
    Nil,
    Var(String),
    Not(Box<Expr>),
    Compare(Box<Expr>, CmpOp, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

#[derive(Clone, Debug)]
enum CmpOp {
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,
}

// ── Parser ──────────────────────────────────────────────────────────

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&Token> {
        let tok = self.tokens.get(self.pos);
        self.pos += 1;
        tok
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.peek() == Some(&Token::Or) {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_not()?;
        while self.peek() == Some(&Token::And) {
            self.advance();
            let right = self.parse_not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        if self.peek() == Some(&Token::Not) {
            self.advance();
            let inner = self.parse_not()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_compare()
    }

    fn parse_compare(&mut self) -> Result<Expr, String> {
        let left = self.parse_primary()?;
        let op = match self.peek() {
            Some(Token::Eq) => CmpOp::Eq,
            Some(Token::Neq) => CmpOp::Neq,
            Some(Token::Lt) => CmpOp::Lt,
            Some(Token::Gt) => CmpOp::Gt,
            Some(Token::Lte) => CmpOp::Lte,
            Some(Token::Gte) => CmpOp::Gte,
            _ => return Ok(left),
        };
        self.advance();
        let right = self.parse_primary()?;
        Ok(Expr::Compare(Box::new(left), op, Box::new(right)))
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.advance() {
            Some(Token::BoolLit(b)) => Ok(Expr::BoolLit(*b)),
            Some(Token::NumberLit(n)) => Ok(Expr::NumberLit(*n)),
            Some(Token::StringLit(s)) => Ok(Expr::StringLit(s.clone())),
            Some(Token::Nil) => Ok(Expr::Nil),
            Some(Token::Ident(name)) => Ok(Expr::Var(name.clone())),
            Some(Token::LParen) => {
                let inner = self.parse_expr()?;
                if self.advance() != Some(&Token::RParen) {
                    return Err("expected closing ')' in guard expression".into());
                }
                Ok(inner)
            }
            Some(tok) => Err(format!("unexpected token in guard expression: {tok:?}")),
            None => Err("unexpected end of guard expression".into()),
        }
    }
}

// ── Evaluator ───────────────────────────────────────────────────────

fn eval_bool(expr: &Expr, bindings: &FieldBindings) -> Result<bool, String> {
    match expr {
        Expr::BoolLit(b) => Ok(*b),
        Expr::NumberLit(n) => Ok(*n != 0.0),
        Expr::StringLit(s) => Ok(!s.is_empty()),
        Expr::Nil => Ok(false),
        Expr::Var(name) => match bindings.get(name) {
            Some(FieldValue::Bool(b)) => Ok(*b),
            Some(FieldValue::Number(n)) => Ok(*n != 0),
            Some(FieldValue::Float(f)) => Ok(*f != 0.0),
            Some(FieldValue::String(s)) => Ok(!s.is_empty()),
            Some(FieldValue::Enum(_)) => Ok(true),
            Some(FieldValue::Null) | None => Ok(false),
            Some(_) => Ok(true), // lists/maps are truthy
        },
        Expr::Not(inner) => Ok(!eval_bool(inner, bindings)?),
        Expr::And(left, right) => Ok(eval_bool(left, bindings)? && eval_bool(right, bindings)?),
        Expr::Or(left, right) => Ok(eval_bool(left, bindings)? || eval_bool(right, bindings)?),
        Expr::Compare(left, op, right) => eval_compare(left, op, right, bindings),
    }
}

/// Resolve an expression to a comparable value.
fn resolve_value(expr: &Expr, bindings: &FieldBindings) -> Result<GuardValue, String> {
    match expr {
        Expr::BoolLit(b) => Ok(GuardValue::Bool(*b)),
        Expr::NumberLit(n) => Ok(GuardValue::Float(*n)),
        Expr::StringLit(s) => Ok(GuardValue::String(s.clone())),
        Expr::Nil => Ok(GuardValue::Null),
        Expr::Var(name) => match bindings.get(name) {
            Some(fv) => Ok(GuardValue::from_field_value(fv)),
            None => Ok(GuardValue::Null),
        },
        _ => Err("complex expression in comparison position".into()),
    }
}

/// Internal value type for guard comparisons.
#[derive(Debug)]
enum GuardValue {
    Bool(bool),
    Float(f64),
    String(String),
    Null,
}

impl GuardValue {
    fn from_field_value(fv: &FieldValue) -> Self {
        match fv {
            FieldValue::Bool(b) => GuardValue::Bool(*b),
            FieldValue::Number(n) => GuardValue::Float(*n as f64),
            FieldValue::Float(f) => GuardValue::Float(*f),
            FieldValue::String(s) => GuardValue::String(s.clone()),
            FieldValue::Enum(s) => GuardValue::String(s.clone()),
            FieldValue::Null => GuardValue::Null,
            FieldValue::List(_) | FieldValue::Map(_) => GuardValue::Null,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            GuardValue::Float(f) => Some(*f),
            GuardValue::String(s) => s.parse::<f64>().ok(),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            GuardValue::String(s) => Some(s),
            _ => None,
        }
    }
}

fn eval_compare(
    left: &Expr,
    op: &CmpOp,
    right: &Expr,
    bindings: &FieldBindings,
) -> Result<bool, String> {
    let lv = resolve_value(left, bindings)?;
    let rv = resolve_value(right, bindings)?;

    match op {
        CmpOp::Eq => Ok(guard_eq(&lv, &rv)),
        CmpOp::Neq => Ok(!guard_eq(&lv, &rv)),
        CmpOp::Lt => Ok(guard_cmp(&lv, &rv).is_some_and(|o| o == std::cmp::Ordering::Less)),
        CmpOp::Gt => Ok(guard_cmp(&lv, &rv).is_some_and(|o| o == std::cmp::Ordering::Greater)),
        CmpOp::Lte => Ok(guard_cmp(&lv, &rv).is_some_and(|o| o != std::cmp::Ordering::Greater)),
        CmpOp::Gte => Ok(guard_cmp(&lv, &rv).is_some_and(|o| o != std::cmp::Ordering::Less)),
    }
}

fn guard_eq(a: &GuardValue, b: &GuardValue) -> bool {
    match (a, b) {
        (GuardValue::Null, GuardValue::Null) => true,
        (GuardValue::Bool(a), GuardValue::Bool(b)) => a == b,
        (GuardValue::Float(a), GuardValue::Float(b)) => a == b,
        (GuardValue::String(a), GuardValue::String(b)) => a == b,
        // Cross-type number/string coercion
        (GuardValue::Float(n), GuardValue::String(s))
        | (GuardValue::String(s), GuardValue::Float(n)) => s.parse::<f64>().ok() == Some(*n),
        _ => false,
    }
}

fn guard_cmp(a: &GuardValue, b: &GuardValue) -> Option<std::cmp::Ordering> {
    match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => a.partial_cmp(&b),
        _ => match (a.as_str(), b.as_str()) {
            (Some(a), Some(b)) => Some(a.cmp(b)),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FieldValue;

    fn bindings(pairs: &[(&str, FieldValue)]) -> FieldBindings {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn simple_comparison() {
        let b = bindings(&[("depth", FieldValue::Number(2))]);
        assert!(evaluate_guard("depth > 0", &b).unwrap());
        assert!(evaluate_guard("depth >= 2", &b).unwrap());
        assert!(!evaluate_guard("depth < 2", &b).unwrap());
        assert!(evaluate_guard("depth == 2", &b).unwrap());
        assert!(evaluate_guard("depth ~= 3", &b).unwrap());
    }

    #[test]
    fn string_comparison() {
        let b = bindings(&[("phase", FieldValue::String("planning".into()))]);
        assert!(evaluate_guard("phase == \"planning\"", &b).unwrap());
        assert!(!evaluate_guard("phase == \"idle\"", &b).unwrap());
        assert!(evaluate_guard("phase ~= \"idle\"", &b).unwrap());
    }

    #[test]
    fn enum_as_string_in_guard() {
        // Enums resolve to strings in guard comparisons
        let b = bindings(&[("phase", FieldValue::Enum("planning".into()))]);
        assert!(evaluate_guard("phase == \"planning\"", &b).unwrap());
    }

    #[test]
    fn boolean_logic() {
        let b = bindings(&[
            ("depth", FieldValue::Number(2)),
            ("phase", FieldValue::String("planning".into())),
        ]);
        assert!(evaluate_guard("depth > 0 and phase == \"planning\"", &b).unwrap());
        assert!(!evaluate_guard("depth > 0 and phase == \"idle\"", &b).unwrap());
        assert!(evaluate_guard("depth > 0 or phase == \"idle\"", &b).unwrap());
    }

    #[test]
    fn not_operator() {
        let b = bindings(&[("skip", FieldValue::Bool(false))]);
        assert!(evaluate_guard("not skip", &b).unwrap());
        assert!(!evaluate_guard("not not skip", &b).unwrap());
    }

    #[test]
    fn missing_variable_is_falsy() {
        let b = FieldBindings::new();
        assert!(!evaluate_guard("missing", &b).unwrap());
        assert!(evaluate_guard("not missing", &b).unwrap());
        assert!(evaluate_guard("missing == nil", &b).unwrap());
    }

    #[test]
    fn parentheses() {
        let b = bindings(&[
            ("a", FieldValue::Bool(true)),
            ("b", FieldValue::Bool(false)),
            ("c", FieldValue::Bool(true)),
        ]);
        assert!(evaluate_guard("a and (b or c)", &b).unwrap());
        assert!(!evaluate_guard("(a and b) or false", &b).unwrap());
    }

    #[test]
    fn number_truthiness() {
        let b = bindings(&[
            ("zero", FieldValue::Number(0)),
            ("nonzero", FieldValue::Number(5)),
        ]);
        assert!(!evaluate_guard("zero", &b).unwrap());
        assert!(evaluate_guard("nonzero", &b).unwrap());
    }

    #[test]
    fn not_equal_bang_syntax() {
        let b = bindings(&[("x", FieldValue::Number(1))]);
        assert!(evaluate_guard("x != 2", &b).unwrap());
        assert!(!evaluate_guard("x != 1", &b).unwrap());
    }

    #[test]
    fn empty_guard_is_error() {
        assert!(evaluate_guard("", &FieldBindings::new()).is_err());
    }

    #[test]
    fn float_comparison() {
        let b = bindings(&[("score", FieldValue::Float(3.5))]);
        assert!(evaluate_guard("score > 3", &b).unwrap());
        assert!(!evaluate_guard("score > 4", &b).unwrap());
    }

    // ── Operator precedence ────────────────────────────────────────

    #[test]
    fn and_binds_tighter_than_or() {
        // a or (b and c) — a=false, b=true, c=true → true
        let b = bindings(&[
            ("a", FieldValue::Bool(false)),
            ("b", FieldValue::Bool(true)),
            ("c", FieldValue::Bool(true)),
        ]);
        assert!(evaluate_guard("a or b and c", &b).unwrap());
    }

    #[test]
    fn mixed_and_or_without_parens() {
        // (a and b) or (c and d) — a=true, b=false, c=true, d=true → true
        let b = bindings(&[
            ("a", FieldValue::Bool(true)),
            ("b", FieldValue::Bool(false)),
            ("c", FieldValue::Bool(true)),
            ("d", FieldValue::Bool(true)),
        ]);
        assert!(evaluate_guard("a and b or c and d", &b).unwrap());
    }

    #[test]
    fn or_with_falsy_first() {
        let b = bindings(&[
            ("a", FieldValue::Bool(false)),
            ("b", FieldValue::Bool(true)),
        ]);
        assert!(evaluate_guard("a or b", &b).unwrap());
    }

    // ── Not operator depth ─────────────────────────────────────────

    #[test]
    fn double_not_true() {
        let b = bindings(&[("x", FieldValue::Bool(true))]);
        assert!(evaluate_guard("not not x", &b).unwrap());
    }

    #[test]
    fn triple_not() {
        let b = bindings(&[("x", FieldValue::Bool(true))]);
        assert!(!evaluate_guard("not not not x", &b).unwrap());
    }

    #[test]
    fn not_with_comparison() {
        let b = bindings(&[("x", FieldValue::Number(3))]);
        assert!(evaluate_guard("not (x > 5)", &b).unwrap());
    }

    // ── Truthiness rules for every FieldValue variant ──────────────

    #[test]
    fn string_truthiness_non_empty() {
        let b = bindings(&[("s", FieldValue::String("hello".into()))]);
        assert!(evaluate_guard("s", &b).unwrap());
    }

    #[test]
    fn string_truthiness_empty() {
        let b = bindings(&[("s", FieldValue::String("".into()))]);
        assert!(!evaluate_guard("s", &b).unwrap());
    }

    #[test]
    fn enum_truthiness() {
        let b = bindings(&[("e", FieldValue::Enum("any".into()))]);
        assert!(evaluate_guard("e", &b).unwrap());
    }

    #[test]
    fn null_truthiness() {
        let b = bindings(&[("n", FieldValue::Null)]);
        assert!(!evaluate_guard("n", &b).unwrap());
    }

    #[test]
    fn bool_truthiness_true() {
        let b = bindings(&[("x", FieldValue::Bool(true))]);
        assert!(evaluate_guard("x", &b).unwrap());
    }

    #[test]
    fn bool_truthiness_false() {
        let b = bindings(&[("x", FieldValue::Bool(false))]);
        assert!(!evaluate_guard("x", &b).unwrap());
    }

    #[test]
    fn float_truthiness_nonzero() {
        let b = bindings(&[("x", FieldValue::Float(1.5))]);
        assert!(evaluate_guard("x", &b).unwrap());
    }

    #[test]
    fn float_truthiness_zero() {
        let b = bindings(&[("x", FieldValue::Float(0.0))]);
        assert!(!evaluate_guard("x", &b).unwrap());
    }

    #[test]
    fn list_truthiness() {
        let b = bindings(&[("x", FieldValue::List(vec![FieldValue::Number(1)]))]);
        assert!(evaluate_guard("x", &b).unwrap());
    }

    #[test]
    fn empty_list_truthiness() {
        // Empty list is still Some(_) => true
        let b = bindings(&[("x", FieldValue::List(vec![]))]);
        assert!(evaluate_guard("x", &b).unwrap());
    }

    #[test]
    fn map_truthiness() {
        let mut m = std::collections::HashMap::new();
        m.insert("a".into(), FieldValue::Number(1));
        let b = bindings(&[("x", FieldValue::Map(m))]);
        assert!(evaluate_guard("x", &b).unwrap());
    }

    #[test]
    fn empty_map_truthiness() {
        let b = bindings(&[("x", FieldValue::Map(std::collections::HashMap::new()))]);
        assert!(evaluate_guard("x", &b).unwrap());
    }

    // ── Comparison edge cases ──────────────────────────────────────

    #[test]
    fn string_lexicographic_lt() {
        let b = bindings(&[
            ("a", FieldValue::String("apple".into())),
            ("b", FieldValue::String("banana".into())),
        ]);
        assert!(evaluate_guard("a < b", &b).unwrap());
    }

    #[test]
    fn string_lexicographic_gt() {
        let b = bindings(&[
            ("a", FieldValue::String("zebra".into())),
            ("b", FieldValue::String("apple".into())),
        ]);
        assert!(evaluate_guard("a > b", &b).unwrap());
    }

    #[test]
    fn cross_type_number_vs_string_eq() {
        // Number(3) → Float(3.0) in GuardValue; String "3" coerces to 3.0 via as_f64
        let b = bindings(&[("count", FieldValue::Number(3))]);
        assert!(evaluate_guard("count == \"3\"", &b).unwrap());
    }

    #[test]
    fn cross_type_number_vs_string_lt() {
        let b = bindings(&[("count", FieldValue::Number(3))]);
        assert!(evaluate_guard("count < \"5\"", &b).unwrap());
    }

    #[test]
    fn float_vs_integer_eq() {
        // Float(3.0) compared to NumberLit(3) — both become Float in GuardValue
        let b = bindings(&[("x", FieldValue::Float(3.0))]);
        assert!(evaluate_guard("x == 3", &b).unwrap());
    }

    #[test]
    fn float_vs_integer_neq() {
        let b = bindings(&[("x", FieldValue::Float(3.1))]);
        assert!(!evaluate_guard("x == 3", &b).unwrap());
    }

    #[test]
    fn null_eq_nil() {
        let b = bindings(&[("x", FieldValue::Null)]);
        assert!(evaluate_guard("x == nil", &b).unwrap());
    }

    #[test]
    fn null_neq_nil() {
        let b = bindings(&[("x", FieldValue::Number(5))]);
        assert!(evaluate_guard("x ~= nil", &b).unwrap());
    }

    #[test]
    fn two_literal_comparison() {
        assert!(evaluate_guard("3 > 2", &FieldBindings::new()).unwrap());
    }

    #[test]
    fn two_variable_comparison() {
        let b = bindings(&[("a", FieldValue::Number(5)), ("b", FieldValue::Number(3))]);
        assert!(evaluate_guard("a > b", &b).unwrap());
    }

    #[test]
    fn bool_eq_true() {
        let b = bindings(&[("x", FieldValue::Bool(true))]);
        assert!(evaluate_guard("x == true", &b).unwrap());
    }

    #[test]
    fn bool_eq_false() {
        let b = bindings(&[("x", FieldValue::Bool(true))]);
        assert!(!evaluate_guard("x == false", &b).unwrap());
    }

    #[test]
    fn incomparable_types_lt() {
        // Bool vs Number — guard_cmp returns None → false
        let b = bindings(&[("x", FieldValue::Bool(true)), ("y", FieldValue::Number(1))]);
        assert!(!evaluate_guard("x < y", &b).unwrap());
    }

    // ── Complex expressions ────────────────────────────────────────

    #[test]
    fn chained_comparisons_with_and() {
        let b = bindings(&[
            ("a", FieldValue::Number(3)),
            ("b", FieldValue::Number(2)),
            ("c", FieldValue::Number(1)),
        ]);
        assert!(evaluate_guard("a > b and b > c", &b).unwrap());
    }

    #[test]
    fn deeply_nested_parentheses() {
        let b = bindings(&[("x", FieldValue::Bool(true))]);
        assert!(evaluate_guard("((((x))))", &b).unwrap());
    }

    #[test]
    fn complex_mixed_expression() {
        let b = bindings(&[
            ("a", FieldValue::Number(1)),
            ("b", FieldValue::Number(0)),
            ("c", FieldValue::Bool(true)),
        ]);
        assert!(evaluate_guard("(a > 0 and b > 0) or (c == true)", &b).unwrap());
    }

    // ── Tokenizer edge cases ───────────────────────────────────────

    #[test]
    fn unterminated_string() {
        let result = evaluate_guard("name == \"hello", &FieldBindings::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unterminated"));
    }

    #[test]
    fn invalid_token_at_symbol() {
        let result = evaluate_guard("@", &FieldBindings::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unexpected character"));
    }

    #[test]
    fn negative_number_literal() {
        let b = bindings(&[("x", FieldValue::Number(0))]);
        assert!(evaluate_guard("x > -5", &b).unwrap());
    }

    #[test]
    fn float_literal_in_guard() {
        let b = bindings(&[("x", FieldValue::Float(4.0))]);
        assert!(evaluate_guard("x > 3.14", &b).unwrap());
    }

    #[test]
    fn string_with_spaces_in_guard() {
        let b = bindings(&[("name", FieldValue::String("hello world".into()))]);
        assert!(evaluate_guard("name == \"hello world\"", &b).unwrap());
    }

    #[test]
    fn single_quoted_string_in_guard() {
        let b = bindings(&[("name", FieldValue::String("hello".into()))]);
        assert!(evaluate_guard("name == 'hello'", &b).unwrap());
    }

    #[test]
    fn whitespace_only_is_error() {
        let result = evaluate_guard("   ", &FieldBindings::new());
        assert!(result.is_err());
    }

    #[test]
    fn single_variable_truthiness_guard() {
        let b = bindings(&[("active", FieldValue::Bool(true))]);
        assert!(evaluate_guard("active", &b).unwrap());
    }

    // ── Error cases ────────────────────────────────────────────────

    #[test]
    fn unmatched_close_paren() {
        let result = evaluate_guard("x)", &bindings(&[("x", FieldValue::Bool(true))]));
        assert!(result.is_err());
    }

    #[test]
    fn unmatched_open_paren() {
        let result = evaluate_guard("(x", &bindings(&[("x", FieldValue::Bool(true))]));
        assert!(result.is_err());
    }

    #[test]
    fn trailing_operator() {
        let result = evaluate_guard("x >", &bindings(&[("x", FieldValue::Number(1))]));
        assert!(result.is_err());
    }

    #[test]
    fn double_operator() {
        let result = evaluate_guard("x > > 1", &bindings(&[("x", FieldValue::Number(1))]));
        assert!(result.is_err());
    }

    #[test]
    fn complex_expr_in_comparison() {
        let b = bindings(&[("a", FieldValue::Bool(true)), ("b", FieldValue::Bool(true))]);
        let result = evaluate_guard("(a and b) > 1", &b);
        assert!(result.is_err());
    }
}
