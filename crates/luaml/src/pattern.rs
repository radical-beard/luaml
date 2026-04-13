use crate::error::{PatternParseError, parse_err};

/// A single node in the pattern AST.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// Matches anything, binds nothing. Syntax: `*` or `_`
    Wildcard,
    /// Matches only when the runtime value is an Enum with this name.
    /// Syntax: `:name:` — type-distinct from StringLiteral.
    Enum(String),
    /// Matches only when the runtime value is a String equal to this literal.
    /// Syntax: `"text"` or `'text'`
    StringLiteral(String),
    /// Matches a number literal exactly.
    NumberLiteral(i64),
    /// Matches a boolean literal exactly.
    BoolLiteral(bool),
    /// Matches anything and binds the value to a named variable.
    /// Syntax: `$name`
    Variable(String),
    /// Matches a value equal to a previously bound variable.
    /// Syntax: `^name`
    Pin(String),
    /// Matches a list with destructuring.
    List(ListPattern),
    /// Matches a map/table and destructures named fields.
    /// Syntax: `{key1: pattern1, key2: pattern2}`
    Map(Vec<(String, Pattern)>),
}

/// List destructuring patterns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListPattern {
    /// Matches only an empty list. Syntax: `[]`
    Empty,
    /// Head/tail destructuring. Syntax: `[$head | $tail]`
    HeadTail {
        head: Box<Pattern>,
        tail: Box<Pattern>,
    },
    /// Fixed-length element matching. Syntax: `[$a, $b, $c]`
    Elements(Vec<Pattern>),
}

/// Parse a single pattern value from a string.
///
/// Syntax:
/// - `*` or `_` → Wildcard
/// - `:name:` → Enum
/// - `"text"` or `'text'` → StringLiteral
/// - `true` / `false` → BoolLiteral
/// - Integer → NumberLiteral
/// - `$name` → Variable
/// - `^name` → Pin
/// - `[]` → List(Empty)
/// - `[$head | $tail]` → List(HeadTail)
/// - `[$a, $b]` → List(Elements)
/// - `{key: pat, ...}` → Map
/// - Bare words → parse error
pub fn parse_pattern_value(input: &str) -> Result<Pattern, PatternParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(parse_err("empty pattern", input));
    }

    // Wildcard
    if trimmed == "*" || trimmed == "_" {
        return Ok(Pattern::Wildcard);
    }

    // Enum: :name:
    if trimmed.starts_with(':') && trimmed.ends_with(':') && trimmed.len() >= 3 {
        let inner = &trimmed[1..trimmed.len() - 1];
        if is_identifier(inner) {
            return Ok(Pattern::Enum(inner.to_string()));
        }
        return Err(parse_err("invalid enum name", input));
    }

    // Quoted string literal
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        let inner = &trimmed[1..trimmed.len() - 1];
        return Ok(Pattern::StringLiteral(inner.to_string()));
    }

    // Boolean literals
    if trimmed == "true" {
        return Ok(Pattern::BoolLiteral(true));
    }
    if trimmed == "false" {
        return Ok(Pattern::BoolLiteral(false));
    }

    // Number literals
    if let Ok(n) = trimmed.parse::<i64>() {
        return Ok(Pattern::NumberLiteral(n));
    }

    // Variable: $name
    if let Some(name) = trimmed.strip_prefix('$') {
        let name = name.trim();
        if is_identifier(name) {
            return Ok(Pattern::Variable(name.to_string()));
        }
        return Err(parse_err("invalid variable name", input));
    }

    // Pin operator: ^name
    if let Some(name) = trimmed.strip_prefix('^') {
        let name = name.trim();
        if is_identifier(name) {
            return Ok(Pattern::Pin(name.to_string()));
        }
        return Err(parse_err("invalid pin variable name", input));
    }

    // List pattern
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return parse_list_pattern(trimmed);
    }

    // Map pattern
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return parse_map_pattern(trimmed);
    }

    // Bare word = error (no backward compat)
    Err(parse_err(
        "bare words not allowed — use :enum:, \"string\", $var, or *",
        input,
    ))
}

fn parse_list_pattern(input: &str) -> Result<Pattern, PatternParseError> {
    let inner = input[1..input.len() - 1].trim();

    // Empty list
    if inner.is_empty() {
        return Ok(Pattern::List(ListPattern::Empty));
    }

    // Head|tail destructuring
    if let Some(pipe_pos) = find_pipe(inner) {
        let head_str = inner[..pipe_pos].trim();
        let tail_str = inner[pipe_pos + 1..].trim();
        let head = parse_pattern_value(head_str)?;
        let tail = parse_pattern_value(tail_str)?;
        return Ok(Pattern::List(ListPattern::HeadTail {
            head: Box::new(head),
            tail: Box::new(tail),
        }));
    }

    // Fixed elements: [$a, $b, $c]
    let elements = split_top_level(inner, ',')?;
    let patterns: Result<Vec<Pattern>, _> = elements
        .iter()
        .map(|e| parse_pattern_value(e.trim()))
        .collect();
    Ok(Pattern::List(ListPattern::Elements(patterns?)))
}

fn parse_map_pattern(input: &str) -> Result<Pattern, PatternParseError> {
    let inner = input[1..input.len() - 1].trim();
    if inner.is_empty() {
        return Ok(Pattern::Map(Vec::new()));
    }

    let pairs = split_top_level(inner, ',')?;
    let mut fields = Vec::new();
    for pair in &pairs {
        let pair = pair.trim();
        let colon_pos = pair
            .find(':')
            .ok_or_else(|| parse_err("map pattern entry missing ':'", pair))?;
        let key = pair[..colon_pos].trim();
        let val = pair[colon_pos + 1..].trim();
        if !is_identifier(key) {
            return Err(parse_err("invalid map key", key));
        }
        let pattern = parse_pattern_value(val)?;
        fields.push((key.to_string(), pattern));
    }
    Ok(Pattern::Map(fields))
}

/// Find the `|` pipe separator in a list pattern, respecting nesting.
fn find_pipe(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            '|' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Split a string by a delimiter, respecting nested brackets and quotes.
pub fn split_top_level(s: &str, delim: char) -> Result<Vec<String>, PatternParseError> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '"';

    for ch in s.chars() {
        if in_quotes {
            current.push(ch);
            if ch == quote_char {
                in_quotes = false;
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                in_quotes = true;
                quote_char = ch;
                current.push(ch);
            }
            '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ']' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            c if c == delim && depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    Ok(parts)
}

/// Check if a string is a valid identifier (variable/enum name).
pub fn is_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_alphabetic() && first != '_' {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Collect all variable names bound by a pattern.
pub fn collect_variables(pattern: &Pattern) -> Vec<String> {
    let mut vars = Vec::new();
    collect_vars_inner(pattern, &mut vars);
    vars
}

fn collect_vars_inner(pattern: &Pattern, vars: &mut Vec<String>) {
    match pattern {
        Pattern::Variable(name) => vars.push(name.clone()),
        Pattern::List(ListPattern::Empty) => {}
        Pattern::List(ListPattern::HeadTail { head, tail }) => {
            collect_vars_inner(head, vars);
            collect_vars_inner(tail, vars);
        }
        Pattern::List(ListPattern::Elements(elements)) => {
            for elem in elements {
                collect_vars_inner(elem, vars);
            }
        }
        Pattern::Map(fields) => {
            for (_, pat) in fields {
                collect_vars_inner(pat, vars);
            }
        }
        Pattern::Wildcard
        | Pattern::Enum(_)
        | Pattern::StringLiteral(_)
        | Pattern::NumberLiteral(_)
        | Pattern::BoolLiteral(_)
        | Pattern::Pin(_) => {}
    }
}

/// Check for duplicate variable names across a set of pattern fields.
pub fn check_duplicate_variables(fields: &[(String, Pattern)]) -> Result<(), PatternParseError> {
    let mut all = Vec::new();
    for (_, pat) in fields {
        all.extend(collect_variables(pat));
    }
    let mut seen = std::collections::HashSet::new();
    for var in &all {
        if !seen.insert(var.as_str()) {
            return Err(parse_err(
                format!("duplicate variable name '{var}' in pattern"),
                format!("{all:?}"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Wildcard ----

    #[test]
    fn parse_wildcard_star() {
        assert_eq!(parse_pattern_value("*").unwrap(), Pattern::Wildcard);
    }

    #[test]
    fn parse_wildcard_underscore() {
        assert_eq!(parse_pattern_value("_").unwrap(), Pattern::Wildcard);
    }

    // ---- Enums ----

    #[test]
    fn parse_enum() {
        assert_eq!(
            parse_pattern_value(":input:").unwrap(),
            Pattern::Enum("input".into())
        );
    }

    #[test]
    fn parse_enum_with_underscore() {
        assert_eq!(
            parse_pattern_value(":on_client_start:").unwrap(),
            Pattern::Enum("on_client_start".into())
        );
    }

    #[test]
    fn parse_enum_single_char() {
        assert_eq!(
            parse_pattern_value(":q:").unwrap(),
            Pattern::Enum("q".into())
        );
    }

    #[test]
    fn parse_invalid_enum_errors() {
        assert!(parse_pattern_value(":123:").is_err());
        assert!(parse_pattern_value("::").is_err());
    }

    // ---- String literals ----

    #[test]
    fn parse_double_quoted_string() {
        assert_eq!(
            parse_pattern_value("\"overlay.settings\"").unwrap(),
            Pattern::StringLiteral("overlay.settings".into())
        );
    }

    #[test]
    fn parse_single_quoted_string() {
        assert_eq!(
            parse_pattern_value("'hello'").unwrap(),
            Pattern::StringLiteral("hello".into())
        );
    }

    // ---- Booleans ----

    #[test]
    fn parse_bool_true() {
        assert_eq!(
            parse_pattern_value("true").unwrap(),
            Pattern::BoolLiteral(true)
        );
    }

    #[test]
    fn parse_bool_false() {
        assert_eq!(
            parse_pattern_value("false").unwrap(),
            Pattern::BoolLiteral(false)
        );
    }

    // ---- Numbers ----

    #[test]
    fn parse_number_literal() {
        assert_eq!(
            parse_pattern_value("42").unwrap(),
            Pattern::NumberLiteral(42)
        );
    }

    #[test]
    fn parse_negative_number() {
        assert_eq!(
            parse_pattern_value("-7").unwrap(),
            Pattern::NumberLiteral(-7)
        );
    }

    // ---- Variables ($name) ----

    #[test]
    fn parse_variable() {
        assert_eq!(
            parse_pattern_value("$agent_id").unwrap(),
            Pattern::Variable("agent_id".into())
        );
    }

    #[test]
    fn parse_single_char_variable() {
        assert_eq!(
            parse_pattern_value("$x").unwrap(),
            Pattern::Variable("x".into())
        );
    }

    #[test]
    fn parse_variable_with_space() {
        assert_eq!(
            parse_pattern_value("$ id").unwrap(),
            Pattern::Variable("id".into())
        );
    }

    #[test]
    fn parse_invalid_variable_errors() {
        assert!(parse_pattern_value("$123").is_err());
        assert!(parse_pattern_value("$").is_err());
    }

    // ---- Bare words are errors ----

    #[test]
    fn bare_word_is_error() {
        let result = parse_pattern_value("agent_id");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .contains("bare words not allowed")
        );
    }

    #[test]
    fn bare_word_tui_is_error() {
        assert!(parse_pattern_value("tui").is_err());
    }

    #[test]
    fn bare_word_input_is_error() {
        assert!(parse_pattern_value("input").is_err());
    }

    // ---- Pin ----

    #[test]
    fn parse_pin_operator() {
        assert_eq!(
            parse_pattern_value("^expected").unwrap(),
            Pattern::Pin("expected".into())
        );
    }

    #[test]
    fn parse_pin_with_space() {
        assert_eq!(
            parse_pattern_value("^ name").unwrap(),
            Pattern::Pin("name".into())
        );
    }

    // ---- Empty list ----

    #[test]
    fn parse_empty_list() {
        assert_eq!(
            parse_pattern_value("[]").unwrap(),
            Pattern::List(ListPattern::Empty)
        );
    }

    // ---- Head|tail ----

    #[test]
    fn parse_head_tail() {
        assert_eq!(
            parse_pattern_value("[$first | $rest]").unwrap(),
            Pattern::List(ListPattern::HeadTail {
                head: Box::new(Pattern::Variable("first".into())),
                tail: Box::new(Pattern::Variable("rest".into())),
            })
        );
    }

    #[test]
    fn parse_head_tail_with_literal_head() {
        assert_eq!(
            parse_pattern_value("[\"review\" | $rest]").unwrap(),
            Pattern::List(ListPattern::HeadTail {
                head: Box::new(Pattern::StringLiteral("review".into())),
                tail: Box::new(Pattern::Variable("rest".into())),
            })
        );
    }

    #[test]
    fn parse_head_tail_with_enum_head() {
        assert_eq!(
            parse_pattern_value("[:review: | $rest]").unwrap(),
            Pattern::List(ListPattern::HeadTail {
                head: Box::new(Pattern::Enum("review".into())),
                tail: Box::new(Pattern::Variable("rest".into())),
            })
        );
    }

    #[test]
    fn parse_head_tail_wildcard_tail() {
        assert_eq!(
            parse_pattern_value("[$first | _]").unwrap(),
            Pattern::List(ListPattern::HeadTail {
                head: Box::new(Pattern::Variable("first".into())),
                tail: Box::new(Pattern::Wildcard),
            })
        );
    }

    // ---- Fixed elements ----

    #[test]
    fn parse_fixed_elements() {
        assert_eq!(
            parse_pattern_value("[$a, $b, $c]").unwrap(),
            Pattern::List(ListPattern::Elements(vec![
                Pattern::Variable("a".into()),
                Pattern::Variable("b".into()),
                Pattern::Variable("c".into()),
            ]))
        );
    }

    #[test]
    fn parse_mixed_element_types() {
        assert_eq!(
            parse_pattern_value("[\"literal\", $x, 42]").unwrap(),
            Pattern::List(ListPattern::Elements(vec![
                Pattern::StringLiteral("literal".into()),
                Pattern::Variable("x".into()),
                Pattern::NumberLiteral(42),
            ]))
        );
    }

    // ---- Map patterns ----

    #[test]
    fn parse_empty_map() {
        assert_eq!(parse_pattern_value("{}").unwrap(), Pattern::Map(Vec::new()));
    }

    #[test]
    fn parse_map_with_variable_values() {
        assert_eq!(
            parse_pattern_value("{phase: $p, idle: $is_idle}").unwrap(),
            Pattern::Map(vec![
                ("phase".into(), Pattern::Variable("p".into())),
                ("idle".into(), Pattern::Variable("is_idle".into())),
            ])
        );
    }

    #[test]
    fn parse_map_with_literal_value() {
        assert_eq!(
            parse_pattern_value("{status: \"active\"}").unwrap(),
            Pattern::Map(vec![(
                "status".into(),
                Pattern::StringLiteral("active".into())
            )])
        );
    }

    #[test]
    fn parse_map_with_enum_value() {
        assert_eq!(
            parse_pattern_value("{phase: :planning:}").unwrap(),
            Pattern::Map(vec![("phase".into(), Pattern::Enum("planning".into()))])
        );
    }

    #[test]
    fn parse_nested_map() {
        assert_eq!(
            parse_pattern_value("{inner: {x: $val}}").unwrap(),
            Pattern::Map(vec![(
                "inner".into(),
                Pattern::Map(vec![("x".into(), Pattern::Variable("val".into()))])
            )])
        );
    }

    #[test]
    fn parse_map_with_list_value() {
        assert_eq!(
            parse_pattern_value("{items: [$first | $rest]}").unwrap(),
            Pattern::Map(vec![(
                "items".into(),
                Pattern::List(ListPattern::HeadTail {
                    head: Box::new(Pattern::Variable("first".into())),
                    tail: Box::new(Pattern::Variable("rest".into())),
                })
            )])
        );
    }

    // ---- Error cases ----

    #[test]
    fn parse_empty_string_errors() {
        assert!(parse_pattern_value("").is_err());
    }

    #[test]
    fn parse_invalid_pin_errors() {
        assert!(parse_pattern_value("^123").is_err());
    }

    #[test]
    fn parse_map_missing_colon_errors() {
        assert!(parse_pattern_value("{bad}").is_err());
    }

    // ---- Variable collection and duplicate detection ----

    #[test]
    fn collect_variables_from_nested_pattern() {
        let pat = Pattern::Map(vec![
            ("a".into(), Pattern::Variable("x".into())),
            (
                "b".into(),
                Pattern::List(ListPattern::HeadTail {
                    head: Box::new(Pattern::Variable("h".into())),
                    tail: Box::new(Pattern::Variable("t".into())),
                }),
            ),
        ]);
        let vars = collect_variables(&pat);
        assert_eq!(vars, vec!["x", "h", "t"]);
    }

    #[test]
    fn duplicate_variable_detection() {
        let fields = vec![
            ("a".into(), Pattern::Variable("x".into())),
            ("b".into(), Pattern::Variable("x".into())),
        ];
        assert!(check_duplicate_variables(&fields).is_err());
    }

    #[test]
    fn no_duplicate_passes() {
        let fields = vec![
            ("a".into(), Pattern::Variable("x".into())),
            ("b".into(), Pattern::Variable("y".into())),
        ];
        assert!(check_duplicate_variables(&fields).is_ok());
    }

    // ---- Whitespace tolerance ----

    #[test]
    fn parse_with_extra_whitespace() {
        assert_eq!(
            parse_pattern_value("  $agent_id  ").unwrap(),
            Pattern::Variable("agent_id".into())
        );
    }

    #[test]
    fn parse_list_with_spaces() {
        assert_eq!(
            parse_pattern_value("[  $first  |  $rest  ]").unwrap(),
            Pattern::List(ListPattern::HeadTail {
                head: Box::new(Pattern::Variable("first".into())),
                tail: Box::new(Pattern::Variable("rest".into())),
            })
        );
    }

    #[test]
    fn parse_map_with_spaces() {
        assert_eq!(
            parse_pattern_value("{  key :  $val  }").unwrap(),
            Pattern::Map(vec![("key".into(), Pattern::Variable("val".into()))])
        );
    }

    #[test]
    fn parse_enum_with_spaces() {
        assert_eq!(
            parse_pattern_value("  :lifecycle:  ").unwrap(),
            Pattern::Enum("lifecycle".into())
        );
    }

    // ── String literal edge cases ──────────────────────────────────

    #[test]
    fn parse_empty_double_quoted_string() {
        assert_eq!(
            parse_pattern_value("\"\"").unwrap(),
            Pattern::StringLiteral("".into())
        );
    }

    #[test]
    fn parse_empty_single_quoted_string() {
        assert_eq!(
            parse_pattern_value("''").unwrap(),
            Pattern::StringLiteral("".into())
        );
    }

    #[test]
    fn parse_string_with_spaces() {
        assert_eq!(
            parse_pattern_value("\"hello world\"").unwrap(),
            Pattern::StringLiteral("hello world".into())
        );
    }

    #[test]
    fn parse_string_with_colon() {
        assert_eq!(
            parse_pattern_value("\"foo:bar\"").unwrap(),
            Pattern::StringLiteral("foo:bar".into())
        );
    }

    #[test]
    fn parse_string_with_special_chars() {
        assert_eq!(
            parse_pattern_value("\"a$b^c\"").unwrap(),
            Pattern::StringLiteral("a$b^c".into())
        );
    }

    #[test]
    fn parse_string_with_brackets() {
        assert_eq!(
            parse_pattern_value("\"[1,2]\"").unwrap(),
            Pattern::StringLiteral("[1,2]".into())
        );
    }

    #[test]
    fn parse_string_with_braces() {
        assert_eq!(
            parse_pattern_value("\"{x: 1}\"").unwrap(),
            Pattern::StringLiteral("{x: 1}".into())
        );
    }

    #[test]
    fn parse_string_with_pipe() {
        assert_eq!(
            parse_pattern_value("\"|\"").unwrap(),
            Pattern::StringLiteral("|".into())
        );
    }

    // ── Number edge cases ──────────────────────────────────────────

    #[test]
    fn parse_zero() {
        assert_eq!(parse_pattern_value("0").unwrap(), Pattern::NumberLiteral(0));
    }

    #[test]
    fn parse_i64_max() {
        let s = i64::MAX.to_string();
        assert_eq!(
            parse_pattern_value(&s).unwrap(),
            Pattern::NumberLiteral(i64::MAX)
        );
    }

    #[test]
    fn parse_i64_min() {
        let s = i64::MIN.to_string();
        assert_eq!(
            parse_pattern_value(&s).unwrap(),
            Pattern::NumberLiteral(i64::MIN)
        );
    }

    #[test]
    fn parse_leading_zero_accepted() {
        // i64::parse accepts leading zeros
        assert_eq!(
            parse_pattern_value("007").unwrap(),
            Pattern::NumberLiteral(7)
        );
    }

    #[test]
    fn parse_float_like_number_is_bare_word() {
        // 3.14 does not parse as i64 and is not quoted — falls through to bare word error
        assert!(parse_pattern_value("3.14").is_err());
    }

    #[test]
    fn parse_number_overflow_is_bare_word() {
        // Exceeds i64 range — falls through to bare word error
        let big = "99999999999999999999999";
        assert!(parse_pattern_value(big).is_err());
    }

    // ── Enum edge cases ────────────────────────────────────────────

    #[test]
    fn parse_enum_long_name() {
        assert_eq!(
            parse_pattern_value(":a_very_long_enum_name_that_goes_on:").unwrap(),
            Pattern::Enum("a_very_long_enum_name_that_goes_on".into())
        );
    }

    #[test]
    fn parse_enum_numeric_chars_inside() {
        assert_eq!(
            parse_pattern_value(":item2:").unwrap(),
            Pattern::Enum("item2".into())
        );
    }

    #[test]
    fn parse_enum_starts_with_underscore() {
        assert_eq!(
            parse_pattern_value(":_private:").unwrap(),
            Pattern::Enum("_private".into())
        );
    }

    #[test]
    fn parse_single_colon_is_error() {
        // ":" has len 1, not >= 3, doesn't match enum pattern, falls through to bare word
        assert!(parse_pattern_value(":").is_err());
    }

    #[test]
    fn parse_enum_with_spaces_inside() {
        assert!(parse_pattern_value(":has space:").is_err());
    }

    #[test]
    fn parse_enum_with_hyphen() {
        assert!(parse_pattern_value(":my-enum:").is_err());
    }

    // ── Variable/Pin edge cases ────────────────────────────────────

    #[test]
    fn parse_variable_starts_with_underscore() {
        assert_eq!(
            parse_pattern_value("$_hidden").unwrap(),
            Pattern::Variable("_hidden".into())
        );
    }

    #[test]
    fn parse_variable_with_digits() {
        assert_eq!(
            parse_pattern_value("$var123").unwrap(),
            Pattern::Variable("var123".into())
        );
    }

    #[test]
    fn parse_pin_starts_with_underscore() {
        assert_eq!(
            parse_pattern_value("^_prev").unwrap(),
            Pattern::Pin("_prev".into())
        );
    }

    #[test]
    fn parse_pin_with_digits() {
        assert_eq!(
            parse_pattern_value("^val42").unwrap(),
            Pattern::Pin("val42".into())
        );
    }

    #[test]
    fn parse_dollar_only_is_error() {
        assert!(parse_pattern_value("$").is_err());
    }

    #[test]
    fn parse_caret_only_is_error() {
        assert!(parse_pattern_value("^").is_err());
    }

    // ── List pattern edge cases ────────────────────────────────────

    #[test]
    fn parse_single_element_list() {
        assert_eq!(
            parse_pattern_value("[$x]").unwrap(),
            Pattern::List(ListPattern::Elements(vec![Pattern::Variable("x".into())]))
        );
    }

    #[test]
    fn parse_list_with_nested_list() {
        assert_eq!(
            parse_pattern_value("[[$a, $b], $c]").unwrap(),
            Pattern::List(ListPattern::Elements(vec![
                Pattern::List(ListPattern::Elements(vec![
                    Pattern::Variable("a".into()),
                    Pattern::Variable("b".into()),
                ])),
                Pattern::Variable("c".into()),
            ]))
        );
    }

    #[test]
    fn parse_list_with_nested_map() {
        assert_eq!(
            parse_pattern_value("[{key: $v}]").unwrap(),
            Pattern::List(ListPattern::Elements(vec![Pattern::Map(vec![(
                "key".into(),
                Pattern::Variable("v".into()),
            )])]))
        );
    }

    #[test]
    fn parse_head_tail_with_wildcard_head() {
        assert_eq!(
            parse_pattern_value("[* | $rest]").unwrap(),
            Pattern::List(ListPattern::HeadTail {
                head: Box::new(Pattern::Wildcard),
                tail: Box::new(Pattern::Variable("rest".into())),
            })
        );
    }

    #[test]
    fn parse_head_tail_with_number_head() {
        assert_eq!(
            parse_pattern_value("[42 | $rest]").unwrap(),
            Pattern::List(ListPattern::HeadTail {
                head: Box::new(Pattern::NumberLiteral(42)),
                tail: Box::new(Pattern::Variable("rest".into())),
            })
        );
    }

    #[test]
    fn parse_list_with_whitespace_only() {
        assert_eq!(
            parse_pattern_value("[  ]").unwrap(),
            Pattern::List(ListPattern::Empty)
        );
    }

    // ── Map pattern edge cases ─────────────────────────────────────

    #[test]
    fn parse_map_single_entry() {
        assert_eq!(
            parse_pattern_value("{key: $val}").unwrap(),
            Pattern::Map(vec![("key".into(), Pattern::Variable("val".into()))])
        );
    }

    #[test]
    fn parse_map_nested_map_and_list() {
        assert_eq!(
            parse_pattern_value("{a: {b: [$x | $y]}}").unwrap(),
            Pattern::Map(vec![(
                "a".into(),
                Pattern::Map(vec![(
                    "b".into(),
                    Pattern::List(ListPattern::HeadTail {
                        head: Box::new(Pattern::Variable("x".into())),
                        tail: Box::new(Pattern::Variable("y".into())),
                    })
                )])
            )])
        );
    }

    #[test]
    fn parse_map_three_levels_deep() {
        assert_eq!(
            parse_pattern_value("{a: {b: {c: $v}}}").unwrap(),
            Pattern::Map(vec![(
                "a".into(),
                Pattern::Map(vec![(
                    "b".into(),
                    Pattern::Map(vec![("c".into(), Pattern::Variable("v".into()))])
                )])
            )])
        );
    }

    #[test]
    fn parse_map_key_with_underscore() {
        assert_eq!(
            parse_pattern_value("{my_key: $v}").unwrap(),
            Pattern::Map(vec![("my_key".into(), Pattern::Variable("v".into()))])
        );
    }

    #[test]
    fn parse_map_key_starts_with_underscore() {
        assert_eq!(
            parse_pattern_value("{_private: $v}").unwrap(),
            Pattern::Map(vec![("_private".into(), Pattern::Variable("v".into()))])
        );
    }

    #[test]
    fn parse_map_key_numeric_is_error() {
        assert!(parse_pattern_value("{123: $v}").is_err());
    }

    #[test]
    fn parse_map_key_empty_is_error() {
        assert!(parse_pattern_value("{: $v}").is_err());
    }

    #[test]
    fn parse_map_duplicate_keys_allowed_by_parser() {
        // Parser allows duplicate keys — check_duplicate_variables catches variable conflicts separately
        let result = parse_pattern_value("{a: $x, a: $y}").unwrap();
        if let Pattern::Map(fields) = result {
            assert_eq!(fields.len(), 2);
        } else {
            panic!("expected Map");
        }
    }

    #[test]
    fn parse_map_value_is_wildcard() {
        assert_eq!(
            parse_pattern_value("{key: *}").unwrap(),
            Pattern::Map(vec![("key".into(), Pattern::Wildcard)])
        );
    }

    #[test]
    fn parse_map_value_is_bool() {
        assert_eq!(
            parse_pattern_value("{active: true}").unwrap(),
            Pattern::Map(vec![("active".into(), Pattern::BoolLiteral(true))])
        );
    }

    // ── split_top_level ────────────────────────────────────────────

    #[test]
    fn split_simple() {
        let parts = split_top_level("a, b, c", ',').unwrap();
        assert_eq!(parts, vec!["a", " b", " c"]);
    }

    #[test]
    fn split_nested_brackets() {
        let parts = split_top_level("[1,2], [3,4]", ',').unwrap();
        assert_eq!(parts, vec!["[1,2]", " [3,4]"]);
    }

    #[test]
    fn split_nested_braces() {
        let parts = split_top_level("{a:1, b:2}, {c:3}", ',').unwrap();
        assert_eq!(parts, vec!["{a:1, b:2}", " {c:3}"]);
    }

    #[test]
    fn split_with_quotes() {
        let parts = split_top_level("\"a,b\", c", ',').unwrap();
        assert_eq!(parts, vec!["\"a,b\"", " c"]);
    }

    #[test]
    fn split_empty_input() {
        let parts = split_top_level("", ',').unwrap();
        assert!(parts.is_empty());
    }

    #[test]
    fn split_no_delimiter() {
        let parts = split_top_level("abc", ',').unwrap();
        assert_eq!(parts, vec!["abc"]);
    }

    // ── is_identifier ──────────────────────────────────────────────

    #[test]
    fn identifier_valid_simple() {
        assert!(is_identifier("abc"));
    }

    #[test]
    fn identifier_valid_with_digits() {
        assert!(is_identifier("abc123"));
    }

    #[test]
    fn identifier_valid_underscore_start() {
        assert!(is_identifier("_foo"));
    }

    #[test]
    fn identifier_empty_is_false() {
        assert!(!is_identifier(""));
    }

    #[test]
    fn identifier_starts_with_digit() {
        assert!(!is_identifier("1abc"));
    }

    #[test]
    fn identifier_with_hyphen() {
        assert!(!is_identifier("a-b"));
    }

    // ── collect_variables + check_duplicate_variables ───────────────

    #[test]
    fn collect_variables_wildcard() {
        assert!(collect_variables(&Pattern::Wildcard).is_empty());
    }

    #[test]
    fn collect_variables_enum() {
        assert!(collect_variables(&Pattern::Enum("x".into())).is_empty());
    }

    #[test]
    fn collect_variables_single_var() {
        assert_eq!(collect_variables(&Pattern::Variable("x".into())), vec!["x"]);
    }

    #[test]
    fn collect_variables_pin() {
        assert!(collect_variables(&Pattern::Pin("x".into())).is_empty());
    }

    #[test]
    fn collect_variables_empty_list() {
        assert!(collect_variables(&Pattern::List(ListPattern::Empty)).is_empty());
    }

    #[test]
    fn collect_variables_elements_list() {
        let pat = Pattern::List(ListPattern::Elements(vec![
            Pattern::Variable("a".into()),
            Pattern::Variable("b".into()),
            Pattern::Variable("c".into()),
        ]));
        assert_eq!(collect_variables(&pat), vec!["a", "b", "c"]);
    }

    #[test]
    fn check_duplicate_variables_empty() {
        assert!(check_duplicate_variables(&[]).is_ok());
    }

    #[test]
    fn check_duplicate_variables_across_nested() {
        // Same variable in nested map of one field and flat of another
        let fields = vec![
            (
                "a".into(),
                Pattern::Map(vec![("inner".into(), Pattern::Variable("x".into()))]),
            ),
            ("b".into(), Pattern::Variable("x".into())),
        ];
        assert!(check_duplicate_variables(&fields).is_err());
    }
}
