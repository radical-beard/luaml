use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::clause::{Behavior, Clause, ExecutionPolicy, Script};
use crate::error::LuamlError;
use crate::pattern::{Pattern, parse_pattern_value};

/// Parse a .luaml file from source text into a Script.
///
/// Format:
/// ```text
/// ---
/// key: :enum_value:
/// other: "string value"
/// param: $variable
/// ? variable > 0
/// ---
/// lua_body_here()
/// ```
///
/// Guards use `? expr` syntax — a dedicated prefix, not a key-value field.
/// Multiple `?` lines are implicitly ANDed. Guards are never inherited
/// in multi-clause files; each clause must declare its own.
///
/// Multi-clause: subsequent `---` blocks introduce new clauses that inherit
/// the first clause's execution policy field overrides (but not guards).
pub fn parse_luaml(source_path: impl Into<PathBuf>, text: &str) -> Result<Script, LuamlError> {
    let source_path = source_path.into();
    let all_lines: Vec<&str> = text.lines().collect();
    let source_name = source_path.display().to_string();

    if all_lines.is_empty() || all_lines[0] != "---" {
        return Err(LuamlError::Parse {
            message: "luaml file must start with frontmatter delimiter ---".into(),
            source_name,
        });
    }

    // Find the closing `---` of the first frontmatter block.
    let first_close = all_lines[1..]
        .iter()
        .position(|l| *l == "---")
        .map(|i| i + 1)
        .ok_or_else(|| LuamlError::Parse {
            message: "luaml file did not close frontmatter with ---".into(),
            source_name: source_name.clone(),
        })?;

    // Extract `! extension` declaration from the top of frontmatter.
    let fm_lines = &all_lines[1..first_close];
    let (extension, fm_lines) = extract_extension(fm_lines, &source_name)?;

    let base = parse_frontmatter_block(fm_lines, false)?;

    // Everything after the first frontmatter close is body + potential multi-clause blocks.
    let rest = &all_lines[first_close + 1..];
    let clause_boundaries = find_clause_boundaries(rest);

    let first_body_end = clause_boundaries.first().copied().unwrap_or(rest.len());
    let first_body = rest[..first_body_end].join("\n");

    let mut clauses = Vec::new();
    clauses.push(Clause {
        policy: ExecutionPolicy {
            fields: base.fields.clone(),
        },
        guard: base.guard.clone(),
        behavior: Behavior {
            lua_source: first_body,
        },
        annotations: base.annotations.clone(),
        field_annotations: base.field_annotations.clone(),
    });

    for (i, &boundary_start) in clause_boundaries.iter().enumerate() {
        let fm_start = boundary_start + 1;
        let fm_end = rest[fm_start..]
            .iter()
            .position(|l| *l == "---")
            .map(|j| fm_start + j)
            .ok_or_else(|| LuamlError::Parse {
                message: format!(
                    "multi-clause block {} did not close frontmatter with ---",
                    i + 2
                ),
                source_name: source_name.clone(),
            })?;

        let child_fm_lines = &rest[fm_start..fm_end];
        let child = parse_frontmatter_block(child_fm_lines, true)?;

        let body_end = clause_boundaries.get(i + 1).copied().unwrap_or(rest.len());
        let body = rest[fm_end + 1..body_end].join("\n");

        let merged_fields = merge_fields(&base.fields, &child.fields);
        let merged_fa = merge_field_annotations(
            &base.field_annotations,
            &child.field_annotations,
            &merged_fields,
        );

        clauses.push(Clause {
            policy: ExecutionPolicy {
                fields: merged_fields,
            },
            guard: child.guard.clone(),
            behavior: Behavior { lua_source: body },
            annotations: child.annotations.clone(),
            field_annotations: merged_fa,
        });
    }

    Ok(Script {
        source_path,
        extension,
        clauses,
    })
}

/// Merge child clause fields with base fields. Child overrides same-key base fields.
fn merge_fields(base: &[(String, Pattern)], child: &[(String, Pattern)]) -> Vec<(String, Pattern)> {
    let mut merged = base.to_vec();
    for (key, pattern) in child {
        if let Some(existing) = merged.iter_mut().find(|(k, _)| k == key) {
            existing.1 = pattern.clone();
        } else {
            merged.push((key.clone(), pattern.clone()));
        }
    }
    merged
}

/// Merge field annotations: child inherits parent's field annotations for inherited fields,
/// but child re-declarations override parent annotations for that field.
fn merge_field_annotations(
    base: &BTreeMap<String, Vec<(String, String)>>,
    child: &BTreeMap<String, Vec<(String, String)>>,
    merged_fields: &[(String, Pattern)],
) -> BTreeMap<String, Vec<(String, String)>> {
    let mut merged = BTreeMap::new();
    for (field_name, _) in merged_fields {
        // Child annotations take priority over base
        if let Some(annotations) = child.get(field_name) {
            merged.insert(field_name.clone(), annotations.clone());
        } else if let Some(annotations) = base.get(field_name) {
            merged.insert(field_name.clone(), annotations.clone());
        }
    }
    merged
}

/// Extract `! extension-name` declaration from the top of frontmatter lines.
///
/// Returns `(Some(name), remaining_lines)` if a `!` line is found, or
/// `(None, all_lines)` if no `!` line is present. Only one `!` line is
/// allowed, and it must appear before any other content.
fn extract_extension<'a>(
    lines: &'a [&'a str],
    source_name: &str,
) -> Result<(Option<String>, &'a [&'a str]), LuamlError> {
    let mut extension: Option<String> = None;
    let mut consumed = 0;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            consumed += 1;
            continue;
        }
        if let Some(name) = trimmed.strip_prefix('!') {
            let name = name.trim();
            if name.is_empty() {
                return Err(LuamlError::Parse {
                    message: "extension declaration `!` has no name".into(),
                    source_name: source_name.into(),
                });
            }
            if extension.is_some() {
                return Err(LuamlError::Parse {
                    message: "only one extension declaration `!` is allowed per script".into(),
                    source_name: source_name.into(),
                });
            }
            extension = Some(name.to_string());
            consumed += 1;
        } else {
            // First non-empty, non-`!` line — stop consuming.
            break;
        }
    }

    Ok((extension, &lines[consumed..]))
}

/// Parsed result from a frontmatter block.
struct FrontmatterBlock {
    fields: Vec<(String, Pattern)>,
    guard: Option<String>,
    annotations: Vec<(String, String)>,
    field_annotations: BTreeMap<String, Vec<(String, String)>>,
}

/// Parse a frontmatter block into typed pattern fields, optional guard, and annotations.
///
/// Guard lines use `? expr` syntax. Multiple `?` lines are ANDed together.
///
/// Annotation lines use `@key: value` syntax. Positional rules:
/// - In the base clause (`is_child=false`): annotations before the first field → top-level
/// - In child clauses (`is_child=true`): all annotations follow the "annotate next field" rule
/// - Annotations between fields → annotate the next field line
/// - Guards discard any pending annotations (guards aren't annotated)
fn parse_frontmatter_block(lines: &[&str], is_child: bool) -> Result<FrontmatterBlock, LuamlError> {
    let mut fields = Vec::new();
    let mut guards = Vec::new();
    let mut top_annotations = Vec::new();
    let mut field_annotations: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut pending_annotations: Vec<(String, String)> = Vec::new();
    let mut first_field_seen = false;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Extension declarations must be consumed before this function is called.
        if trimmed.starts_with('!') {
            return Err(LuamlError::Parse {
                message: "extension `!` must appear at the top of the first frontmatter block"
                    .into(),
                source_name: "frontmatter".into(),
            });
        }

        // Annotation lines: `@key: value`
        if let Some(rest) = trimmed.strip_prefix('@') {
            let Some((key, value)) = rest.split_once(':') else {
                return Err(LuamlError::Parse {
                    message: format!("annotation line missing value: {trimmed}"),
                    source_name: "frontmatter".into(),
                });
            };
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            pending_annotations.push((key, value));
            continue;
        }

        // Guard lines: `? expr`
        if let Some(expr) = trimmed.strip_prefix('?') {
            let expr = expr.trim();
            if expr.is_empty() {
                return Err(LuamlError::Parse {
                    message: "guard line '?' has no expression".into(),
                    source_name: "frontmatter".into(),
                });
            }
            guards.push(expr.to_string());
            // Discard pending annotations — guards aren't annotated
            pending_annotations.clear();
            continue;
        }

        // Pattern field line: `key: value`
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(LuamlError::Parse {
                message: format!("invalid frontmatter line: {line}"),
                source_name: "frontmatter".into(),
            });
        };

        let key = key.trim();
        let value = value.trim();

        if value.is_empty() {
            return Err(LuamlError::Parse {
                message: format!("frontmatter field '{key}' has no value"),
                source_name: "frontmatter".into(),
            });
        }

        // Flush pending annotations.
        // In base clause: annotations before the first field are top-level.
        // In child clauses: all annotations annotate the next field.
        if !first_field_seen && !is_child {
            top_annotations = std::mem::take(&mut pending_annotations);
            first_field_seen = true;
        } else {
            first_field_seen = true;
            if !pending_annotations.is_empty() {
                field_annotations.insert(key.to_string(), std::mem::take(&mut pending_annotations));
            }
        }

        let pattern = parse_pattern_value(value)?;
        fields.push((key.to_string(), pattern));
    }

    // Any trailing annotations (after last field) become top-level if no fields seen,
    // otherwise they're orphaned — silently discard.
    if !first_field_seen {
        top_annotations = pending_annotations;
    }

    let guard = if guards.is_empty() {
        None
    } else {
        Some(guards.join(" and "))
    };

    Ok(FrontmatterBlock {
        fields,
        guard,
        annotations: top_annotations,
        field_annotations,
    })
}

/// Find indices of `---` lines in the body that start new clause blocks.
fn find_clause_boundaries(lines: &[&str]) -> Vec<usize> {
    let mut boundaries = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i] == "---"
            && let Some(close) = lines[i + 1..].iter().position(|l| *l == "---")
        {
            let between = &lines[i + 1..i + 1 + close];
            if looks_like_frontmatter(between) {
                boundaries.push(i);
                i = i + 1 + close + 1;
                continue;
            }
        }
        i += 1;
    }
    boundaries
}

/// Heuristic: do these lines look like YAML frontmatter?
fn looks_like_frontmatter(lines: &[&str]) -> bool {
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains(':') || trimmed.starts_with('?') || trimmed.starts_with('@') {
            return true;
        }
        return false;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::{ListPattern, Pattern};

    #[test]
    fn parse_single_clause() {
        let input = "\
---
type: :input:
surface: :tui:
key: \"q\"
mode: :normal:
---
api.client.quit()
";
        let script = parse_luaml("quit.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 1);

        let clause = &script.clauses[0];
        assert_eq!(clause.policy.fields.len(), 4);
        assert_eq!(
            clause.policy.fields[0],
            ("type".into(), Pattern::Enum("input".into()))
        );
        assert_eq!(
            clause.policy.fields[1],
            ("surface".into(), Pattern::Enum("tui".into()))
        );
        assert_eq!(
            clause.policy.fields[2],
            ("key".into(), Pattern::StringLiteral("q".into()))
        );
        assert_eq!(
            clause.policy.fields[3],
            ("mode".into(), Pattern::Enum("normal".into()))
        );
        assert_eq!(clause.behavior.lua_source, "api.client.quit()");
        assert!(clause.guard.is_none());
    }

    #[test]
    fn parse_with_guard() {
        let input = "\
---
type: :lifecycle:
event: :on_step:
agent_id: $id
depth: $d
? d > 0
---
print(id)
";
        let script = parse_luaml("guard.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 1);

        let clause = &script.clauses[0];
        assert_eq!(clause.guard, Some("d > 0".to_string()));
        // agent_id should be a Variable
        assert_eq!(
            clause.policy.fields.iter().find(|(k, _)| k == "agent_id"),
            Some(&("agent_id".into(), Pattern::Variable("id".into())))
        );
    }

    #[test]
    fn parse_with_variable_binding() {
        let input = "\
---
type: :input:
key: $pressed
---
print(pressed)
";
        let script = parse_luaml("var.luaml", input).unwrap();
        let clause = &script.clauses[0];
        assert_eq!(
            clause.policy.fields.iter().find(|(k, _)| k == "key"),
            Some(&("key".into(), Pattern::Variable("pressed".into())))
        );
    }

    #[test]
    fn parse_with_wildcard() {
        let input = "\
---
type: :input:
key: *
---
print('catch-all')
";
        let script = parse_luaml("wild.luaml", input).unwrap();
        assert_eq!(
            script.clauses[0]
                .policy
                .fields
                .iter()
                .find(|(k, _)| k == "key"),
            Some(&("key".into(), Pattern::Wildcard))
        );
    }

    #[test]
    fn parse_multi_clause_with_inheritance() {
        let input = "\
---
type: :input:
surface: :tui:
context: \"overlay.settings\"
key: :escape:
mode: :normal:
---
api.client.settings_stop_edit()
---
key: :tab:
---
api.client.settings_next_sub()
---
key: :j:
---
api.client.settings_move(1)
";
        let script = parse_luaml("multi.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 3);

        // Clause 1: key = :escape:
        assert_eq!(
            script.clauses[0]
                .policy
                .fields
                .iter()
                .find(|(k, _)| k == "key"),
            Some(&("key".into(), Pattern::Enum("escape".into())))
        );
        assert_eq!(
            script.clauses[0].behavior.lua_source,
            "api.client.settings_stop_edit()"
        );

        // Clause 2: key = :tab: (overridden), inherits type/surface/context/mode
        let c2 = &script.clauses[1];
        assert_eq!(
            c2.policy.fields.iter().find(|(k, _)| k == "key"),
            Some(&("key".into(), Pattern::Enum("tab".into())))
        );
        assert_eq!(
            c2.policy.fields.iter().find(|(k, _)| k == "type"),
            Some(&("type".into(), Pattern::Enum("input".into())))
        );
        assert_eq!(
            c2.policy.fields.iter().find(|(k, _)| k == "surface"),
            Some(&("surface".into(), Pattern::Enum("tui".into())))
        );
        assert_eq!(
            c2.policy.fields.iter().find(|(k, _)| k == "context"),
            Some(&(
                "context".into(),
                Pattern::StringLiteral("overlay.settings".into())
            ))
        );
        assert_eq!(
            c2.policy.fields.iter().find(|(k, _)| k == "mode"),
            Some(&("mode".into(), Pattern::Enum("normal".into())))
        );
        assert_eq!(c2.behavior.lua_source, "api.client.settings_next_sub()");

        // Clause 3: key = :j: (overridden), inherits everything else
        assert_eq!(
            script.clauses[2]
                .policy
                .fields
                .iter()
                .find(|(k, _)| k == "key"),
            Some(&("key".into(), Pattern::Enum("j".into())))
        );
    }

    #[test]
    fn parse_multi_clause_guard_not_inherited() {
        let input = "\
---
type: :lifecycle:
event: :on_step:
depth: $d
? d > 0
---
handle_deep()
---
depth: 0
---
handle_shallow()
";
        let script = parse_luaml("guard_inherit.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 2);

        // Clause 1: has guard from base
        assert_eq!(script.clauses[0].guard, Some("d > 0".to_string()));

        // Clause 2: does NOT inherit guard — guards are per-clause
        assert_eq!(script.clauses[1].guard, None);
        // depth field overridden to literal 0
        assert_eq!(
            script.clauses[1]
                .policy
                .fields
                .iter()
                .find(|(k, _)| k == "depth"),
            Some(&("depth".into(), Pattern::NumberLiteral(0)))
        );
    }

    #[test]
    fn parse_multi_clause_independent_guards() {
        let input = "\
---
type: :lifecycle:
? x > 0
---
first()
---
? x < 10
---
second()
";
        let script = parse_luaml("guard_override.luaml", input).unwrap();

        assert_eq!(script.clauses[0].guard, Some("x > 0".to_string()));
        assert_eq!(script.clauses[1].guard, Some("x < 10".to_string()));
    }

    #[test]
    fn parse_with_map_destructuring() {
        let input = "\
---
type: :lifecycle:
context: {phase: :planning:, depth: $d}
---
print(d)
";
        let script = parse_luaml("map.luaml", input).unwrap();
        let clause = &script.clauses[0];

        match clause.policy.fields.iter().find(|(k, _)| k == "context") {
            Some((_, Pattern::Map(entries))) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(
                    entries[0],
                    ("phase".into(), Pattern::Enum("planning".into()))
                );
                assert_eq!(entries[1], ("depth".into(), Pattern::Variable("d".into())));
            }
            other => panic!("expected Map pattern, got: {other:?}"),
        }
    }

    #[test]
    fn parse_with_list_destructuring() {
        let input = "\
---
type: :lifecycle:
skills: [$first | $rest]
---
print(first)
";
        let script = parse_luaml("list.luaml", input).unwrap();
        let clause = &script.clauses[0];

        match clause.policy.fields.iter().find(|(k, _)| k == "skills") {
            Some((_, Pattern::List(ListPattern::HeadTail { head, tail }))) => {
                assert_eq!(**head, Pattern::Variable("first".into()));
                assert_eq!(**tail, Pattern::Variable("rest".into()));
            }
            other => panic!("expected HeadTail, got: {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_frontmatter() {
        let err = parse_luaml("bad.luaml", "type: :tool:\n---\nprint('x')")
            .expect_err("should reject missing ---");
        assert!(err.to_string().contains("must start"));
    }

    #[test]
    fn rejects_unclosed_frontmatter() {
        let err = parse_luaml("bad.luaml", "---\ntype: :tool:")
            .expect_err("should reject unclosed frontmatter");
        assert!(err.to_string().contains("did not close"));
    }

    #[test]
    fn bare_words_rejected_in_frontmatter() {
        let err = parse_luaml("bad.luaml", "---\ntype: input\n---\nprint('x')")
            .expect_err("bare words should be rejected");
        assert!(err.to_string().contains("bare words"));
    }

    #[test]
    fn multiline_lua_body() {
        let input = "\
---
type: :lifecycle:
---
local x = 1
local y = 2
print(x + y)
";
        let script = parse_luaml("multi.luaml", input).unwrap();
        assert!(
            script.clauses[0]
                .behavior
                .lua_source
                .contains("local x = 1")
        );
        assert!(
            script.clauses[0]
                .behavior
                .lua_source
                .contains("print(x + y)")
        );
    }

    #[test]
    fn multi_clause_new_field_in_child() {
        let input = "\
---
type: :input:
key: :a:
---
handle_a()
---
key: :b:
extra: :yes:
---
handle_b()
";
        let script = parse_luaml("extra.luaml", input).unwrap();

        // Clause 2 has the extra field that clause 1 doesn't
        assert!(
            script.clauses[1]
                .policy
                .fields
                .iter()
                .any(|(k, _)| k == "extra")
        );
        assert!(
            !script.clauses[0]
                .policy
                .fields
                .iter()
                .any(|(k, _)| k == "extra")
        );
    }

    #[test]
    fn empty_body_single_clause() {
        let input = "\
---
type: :lifecycle:
---
";
        let script = parse_luaml("empty.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 1);
        assert!(script.clauses[0].behavior.lua_source.trim().is_empty());
    }

    // ── Frontmatter edge cases ─────────────────────────────────────

    #[test]
    fn frontmatter_with_blank_lines() {
        let input = "---\ntype: :input:\n\nkey: \"q\"\n---\nprint('x')\n";
        let script = parse_luaml("blank.luaml", input).unwrap();
        assert_eq!(script.clauses[0].policy.fields.len(), 2);
    }

    #[test]
    fn frontmatter_trailing_whitespace() {
        let input = "---\ntype: :input:  \n---\nprint('x')\n";
        let script = parse_luaml("trail.luaml", input).unwrap();
        assert_eq!(
            script.clauses[0].policy.fields[0],
            ("type".into(), Pattern::Enum("input".into()))
        );
    }

    #[test]
    fn frontmatter_tab_indented() {
        let input = "---\n\ttype: :input:\n---\nprint('x')\n";
        let script = parse_luaml("tab.luaml", input).unwrap();
        assert_eq!(script.clauses[0].policy.fields.len(), 1);
    }

    #[test]
    fn frontmatter_value_with_colon() {
        // split_once(':') splits on first colon: key="key", value="\"foo:bar\""
        let input = "---\nkey: \"foo:bar\"\n---\nprint('x')\n";
        let script = parse_luaml("colon.luaml", input).unwrap();
        assert_eq!(
            script.clauses[0].policy.fields[0],
            ("key".into(), Pattern::StringLiteral("foo:bar".into()))
        );
    }

    #[test]
    fn frontmatter_empty_value_rejected() {
        let input = "---\nkey:\n---\nprint('x')\n";
        assert!(parse_luaml("empty_val.luaml", input).is_err());
    }

    #[test]
    fn frontmatter_whitespace_only_value() {
        let input = "---\nkey:   \n---\nprint('x')\n";
        assert!(parse_luaml("ws_val.luaml", input).is_err());
    }

    // ── File structure edge cases ──────────────────────────────────

    #[test]
    fn file_no_trailing_newline() {
        let input = "---\ntype: :input:\n---\nprint('x')";
        let script = parse_luaml("no_trail.luaml", input).unwrap();
        assert_eq!(script.clauses[0].behavior.lua_source, "print('x')");
    }

    #[test]
    fn file_empty() {
        assert!(parse_luaml("empty.luaml", "").is_err());
    }

    #[test]
    fn file_only_frontmatter() {
        let input = "---\ntype: :input:\n---";
        let script = parse_luaml("only_fm.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 1);
        assert!(script.clauses[0].behavior.lua_source.is_empty());
    }

    #[test]
    fn file_frontmatter_and_newlines() {
        let input = "---\ntype: :input:\n---\n\n\n";
        let script = parse_luaml("newlines.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 1);
        // Body is two empty lines joined by \n
        assert_eq!(script.clauses[0].behavior.lua_source, "\n");
    }

    #[test]
    fn file_with_windows_line_endings() {
        // text.lines() strips \r from line endings, but "---\r" != "---"
        // So \r\n input needs to be tested for what actually happens
        let input = "---\r\ntype: :input:\r\n---\r\nprint('x')\r\n";
        // lines() handles \r\n properly — each line is "---", "type: :input:", etc.
        let script = parse_luaml("crlf.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 1);
    }

    // ── Multi-clause edge cases ────────────────────────────────────

    #[test]
    fn multi_clause_five_clauses() {
        let input = "\
---
type: :input:
key: :a:
---
handle_a()
---
key: :b:
---
handle_b()
---
key: :c:
---
handle_c()
---
key: :d:
---
handle_d()
---
key: :e:
---
handle_e()
";
        let script = parse_luaml("five.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 5);
        // All clauses inherit type: :input:
        for clause in &script.clauses {
            assert!(clause.policy.fields.iter().any(|(k, _)| k == "type"));
        }
    }

    #[test]
    fn multi_clause_child_no_overrides() {
        // Child block has guard but no field overrides
        let input = "\
---
type: :lifecycle:
depth: $d
---
handle_all()
---
? d > 5
---
handle_deep()
";
        let script = parse_luaml("no_override.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 2);
        // Second clause inherits all base fields
        assert_eq!(
            script.clauses[1].policy.fields.len(),
            script.clauses[0].policy.fields.len()
        );
        assert_eq!(script.clauses[1].guard, Some("d > 5".to_string()));
    }

    #[test]
    fn body_contains_triple_dash_not_frontmatter() {
        // Body has "---" but the lines between are not frontmatter-like (no colon)
        let input = "---\ntype: :input:\n---\nprint('before')\n---\nthis is not frontmatter\n---\nprint('after')\n";
        let script = parse_luaml("dash_body.luaml", input).unwrap();
        // The "---" block without frontmatter-like content should NOT create a new clause
        assert_eq!(script.clauses.len(), 1);
    }

    #[test]
    fn body_contains_triple_dash_that_looks_like_frontmatter() {
        // Body has "---" with frontmatter-like content — this WILL be treated as a new clause boundary
        let input =
            "---\ntype: :input:\n---\nprint('before')\n---\nkey: :tab:\n---\nprint('after')\n";
        let script = parse_luaml("dash_fm_body.luaml", input).unwrap();
        // The frontmatter-like "---" block creates a new clause
        assert_eq!(script.clauses.len(), 2);
    }

    // ── merge_fields ───────────────────────────────────────────────

    #[test]
    fn merge_child_overrides_base() {
        let base = vec![("key".into(), Pattern::Enum("a".into()))];
        let child = vec![("key".into(), Pattern::Enum("b".into()))];
        let merged = merge_fields(&base, &child);
        assert_eq!(merged, vec![("key".into(), Pattern::Enum("b".into()))]);
    }

    #[test]
    fn merge_child_adds_new() {
        let base = vec![("x".into(), Pattern::Enum("a".into()))];
        let child = vec![("y".into(), Pattern::Enum("b".into()))];
        let merged = merge_fields(&base, &child);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_empty_child() {
        let base = vec![("x".into(), Pattern::Enum("a".into()))];
        let merged = merge_fields(&base, &[]);
        assert_eq!(merged, base);
    }

    #[test]
    fn merge_empty_base() {
        let child = vec![("x".into(), Pattern::Enum("a".into()))];
        let merged = merge_fields(&[], &child);
        assert_eq!(merged, child);
    }

    // ── Guard `?` syntax ──────────────────────────────────────────

    #[test]
    fn multiple_guard_lines_anded() {
        let input = "\
---
type: :lifecycle:
depth: $d
context: {phase: $p}
? d > 0
? p ~= \"idle\"
---
handle()
";
        let script = parse_luaml("multi_guard.luaml", input).unwrap();
        assert_eq!(
            script.clauses[0].guard,
            Some("d > 0 and p ~= \"idle\"".to_string())
        );
    }

    #[test]
    fn guard_line_empty_rejected() {
        let input = "---\ntype: :input:\n?\n---\nprint('x')\n";
        let err = parse_luaml("bad_guard.luaml", input).expect_err("bare ? should be rejected");
        assert!(err.to_string().contains("no expression"));
    }

    #[test]
    fn guard_not_inherited_in_child() {
        let input = "\
---
type: :lifecycle:
depth: $d
? d > 0
---
base_body()
---
depth: 0
---
child_body()
";
        let script = parse_luaml("no_inherit.luaml", input).unwrap();
        assert_eq!(script.clauses[0].guard, Some("d > 0".to_string()));
        assert_eq!(script.clauses[1].guard, None);
    }

    // ── Annotation parsing ────────────────────────────────────────

    #[test]
    fn top_level_annotations_before_type() {
        let input = "\
---
@tool.description: Search by meaning
@tool.max_depth: 0
type: :tool:
name: \"semantic_search\"
---
search()
";
        let script = parse_luaml("ann.luaml", input).unwrap();
        let clause = &script.clauses[0];
        assert_eq!(clause.annotations.len(), 2);
        assert_eq!(
            clause.annotations[0],
            ("tool.description".into(), "Search by meaning".into())
        );
        assert_eq!(clause.annotations[1], ("tool.max_depth".into(), "0".into()));
        // No field annotations on type or name
        assert!(clause.field_annotations.is_empty());
    }

    #[test]
    fn field_annotations_before_fields() {
        let input = "\
---
type: :tool:
name: \"read\"
@tool.param.description: File path
@tool.param.type: string
path: $path
---
read()
";
        let script = parse_luaml("field_ann.luaml", input).unwrap();
        let clause = &script.clauses[0];
        // No top-level annotations
        assert!(clause.annotations.is_empty());
        // path field has two annotations
        let path_ann = clause.field_annotations.get("path").unwrap();
        assert_eq!(path_ann.len(), 2);
        assert_eq!(
            path_ann[0],
            ("tool.param.description".into(), "File path".into())
        );
        assert_eq!(path_ann[1], ("tool.param.type".into(), "string".into()));
    }

    #[test]
    fn mixed_top_and_field_annotations() {
        let input = "\
---
@tool.description: Search
type: :tool:
name: \"search\"
@tool.param.description: Query text
@tool.param.type: string
query: $q
@tool.param.description: Max results
@tool.param.type: number
limit: $l
---
search()
";
        let script = parse_luaml("mixed.luaml", input).unwrap();
        let clause = &script.clauses[0];
        assert_eq!(clause.annotations.len(), 1);
        assert_eq!(clause.annotations[0].0, "tool.description");
        assert_eq!(clause.field_annotations.get("query").unwrap().len(), 2);
        assert_eq!(clause.field_annotations.get("limit").unwrap().len(), 2);
        assert!(clause.field_annotations.get("name").is_none());
    }

    #[test]
    fn annotations_dont_affect_matching() {
        // Annotations should not appear as pattern fields
        let input = "\
---
@description.short: Quit
type: :input:
@tool.param.type: string
input: \"q\"
---
quit()
";
        let script = parse_luaml("ann_match.luaml", input).unwrap();
        let clause = &script.clauses[0];
        // Only two pattern fields: type and input
        assert_eq!(clause.policy.fields.len(), 2);
        assert_eq!(
            clause.policy.fields[0],
            ("type".into(), Pattern::Enum("input".into()))
        );
        assert_eq!(
            clause.policy.fields[1],
            ("input".into(), Pattern::StringLiteral("q".into()))
        );
    }

    #[test]
    fn multi_clause_inherits_field_annotations() {
        let input = "\
---
@tool.description: Read file
type: :tool:
name: \"read\"
@tool.param.description: File path
@tool.param.type: string
path: $path
@tool.param.description: Start line
@tool.param.type: number
start_line: $start
---
read_range()
---
path: $path
---
read_all()
";
        let script = parse_luaml("inherit_ann.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 2);

        // First clause has top-level annotation and field annotations
        assert_eq!(script.clauses[0].annotations.len(), 1);
        assert!(script.clauses[0].field_annotations.contains_key("path"));
        assert!(
            script.clauses[0]
                .field_annotations
                .contains_key("start_line")
        );

        // Second clause: inherits path's field annotations from parent
        // (path is inherited from base), but no start_line (not in this clause)
        let c2 = &script.clauses[1];
        assert!(c2.annotations.is_empty()); // top-level NOT inherited
        let path_ann = c2.field_annotations.get("path").unwrap();
        assert_eq!(path_ann.len(), 2);
        assert_eq!(path_ann[0].0, "tool.param.description");
        // start_line is still inherited as a field (from base), so its annotations come along
        assert!(c2.field_annotations.contains_key("start_line"));
    }

    #[test]
    fn child_clause_overrides_field_annotations() {
        let input = "\
---
type: :tool:
name: \"process\"
@tool.param.description: Original description
@tool.param.type: string
action: $a
---
process()
---
@tool.param.description: Overridden description
action: :create:
---
create()
";
        let script = parse_luaml("override_ann.luaml", input).unwrap();
        // First clause: original annotation
        let c1_ann = script.clauses[0].field_annotations.get("action").unwrap();
        assert_eq!(c1_ann[0].1, "Original description");

        // Second clause: overridden annotation (child re-declared it)
        let c2_ann = script.clauses[1].field_annotations.get("action").unwrap();
        assert_eq!(c2_ann.len(), 1); // only the child's annotation
        assert_eq!(c2_ann[0].1, "Overridden description");
    }

    #[test]
    fn annotation_missing_value_rejected() {
        let input = "---\n@nodesc\ntype: :input:\n---\nprint('x')\n";
        assert!(parse_luaml("bad_ann.luaml", input).is_err());
    }

    #[test]
    fn annotation_empty_value_allowed() {
        // @key: (empty after colon) — allowed, value is empty string
        let input = "---\n@note:\ntype: :input:\n---\nprint('x')\n";
        let script = parse_luaml("empty_ann.luaml", input).unwrap();
        assert_eq!(script.clauses[0].annotations[0], ("note".into(), "".into()));
    }

    #[test]
    fn annotation_with_colon_in_value() {
        let input = "---\n@desc: key: value pair\ntype: :input:\n---\nprint('x')\n";
        let script = parse_luaml("colon_ann.luaml", input).unwrap();
        assert_eq!(script.clauses[0].annotations[0].1, "key: value pair");
    }

    #[test]
    fn guard_discards_pending_annotations() {
        let input = "\
---
type: :lifecycle:
depth: $d
@note: this gets discarded
? d > 0
---
handle()
";
        let script = parse_luaml("guard_discard.luaml", input).unwrap();
        let clause = &script.clauses[0];
        // Annotation before guard is discarded
        assert!(clause.annotations.is_empty());
        assert!(clause.field_annotations.is_empty());
        assert_eq!(clause.guard, Some("d > 0".into()));
    }

    #[test]
    fn looks_like_frontmatter_with_annotations() {
        // A block with only annotations should be recognized as frontmatter
        let input = "---\ntype: :input:\n---\nprint('before')\n---\n@override: quit\nkey: :tab:\n---\nprint('after')\n";
        let script = parse_luaml("ann_fm.luaml", input).unwrap();
        assert_eq!(script.clauses.len(), 2);
    }

    // ── Extension `!` syntax ──────────────────────────────────────

    #[test]
    fn extension_declaration_sets_script_extension() {
        let input = "---\n! my-ext\ntype: :provider:\nprovider: \"test\"\n---\nprint('hello')\n";
        let script = parse_luaml("ext.luaml", input).unwrap();
        assert_eq!(script.extension, Some("my-ext".into()));
        assert_eq!(script.clauses.len(), 1);
        // Extension line is NOT a pattern field
        assert_eq!(script.clauses[0].policy.fields.len(), 2);
    }

    #[test]
    fn no_extension_declaration() {
        let input = "---\ntype: :input:\nkey: \"q\"\n---\nprint('hello')\n";
        let script = parse_luaml("no-ext.luaml", input).unwrap();
        assert_eq!(script.extension, None);
    }

    #[test]
    fn extension_with_leading_blank_lines() {
        let input = "---\n\n! my-ext\ntype: :tool:\n---\nprint('hello')\n";
        let script = parse_luaml("ext-blank.luaml", input).unwrap();
        assert_eq!(script.extension, Some("my-ext".into()));
    }

    #[test]
    fn extension_error_empty_name() {
        let input = "---\n!\ntype: :tool:\n---\nprint('hello')\n";
        let result = parse_luaml("ext-empty.luaml", input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no name"),
            "expected 'no name' error, got: {err}"
        );
    }

    #[test]
    fn extension_error_duplicate() {
        let input = "---\n! first\n! second\ntype: :tool:\n---\nprint('hello')\n";
        let result = parse_luaml("ext-dup.luaml", input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("only one"),
            "expected 'only one' error, got: {err}"
        );
    }

    #[test]
    fn extension_error_after_field() {
        let input = "---\ntype: :tool:\n! my-ext\nname: \"test\"\n---\nprint('hello')\n";
        let result = parse_luaml("ext-after.luaml", input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("top of the first"),
            "expected position error, got: {err}"
        );
    }

    #[test]
    fn extension_not_in_child_clause() {
        let input = "---\n! my-ext\ntype: :input:\nkey: \"a\"\n---\nprint('a')\n---\nkey: \"b\"\n---\nprint('b')\n";
        let script = parse_luaml("ext-child.luaml", input).unwrap();
        assert_eq!(script.extension, Some("my-ext".into()));
        assert_eq!(script.clauses.len(), 2);
    }

}
