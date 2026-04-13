use std::path::Path;

use crate::clause::{Clause, Script};
use crate::error::LuamlError;
use crate::guard::evaluate_guard;
use crate::parser::parse_luaml;
use crate::pattern::Pattern;
use crate::pattern_match::{match_field_value, match_fields};
use crate::types::{FieldBindings, FieldMap};

/// Stores registered scripts and provides matching queries.
#[derive(Clone, Debug)]
pub struct ScriptRegistry {
    scripts: Vec<Script>,
}

impl Default for ScriptRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptRegistry {
    pub fn new() -> Self {
        Self {
            scripts: Vec::new(),
        }
    }

    /// Access the registered scripts.
    pub fn scripts(&self) -> &[Script] {
        &self.scripts
    }

    /// Register a script parsed from source text.
    pub fn register_text(
        &mut self,
        source_path: impl Into<std::path::PathBuf>,
        text: &str,
    ) -> Result<(), LuamlError> {
        let script = parse_luaml(source_path, text)?;
        self.scripts.push(script);
        Ok(())
    }

    /// Register a script from a file path.
    pub fn register_file(&mut self, path: &Path) -> Result<(), LuamlError> {
        let text = std::fs::read_to_string(path)?;
        self.register_text(path, &text)
    }

    /// Register all .luaml files under a directory (recursive).
    pub fn register_dir(&mut self, dir: &Path) -> Result<usize, LuamlError> {
        let mut count = 0;
        if !dir.is_dir() {
            return Ok(0);
        }
        for entry in walkdir(dir)? {
            if entry.extension().is_some_and(|ext| ext == "luaml") {
                self.register_file(&entry)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Register a pre-parsed script directly.
    pub fn register(&mut self, script: Script) {
        self.scripts.push(script);
    }

    /// Remove all scripts registered from the given source path.
    pub fn unregister(&mut self, source_path: &Path) {
        self.scripts.retain(|s| s.source_path != source_path);
    }

    /// Replace a script: unregister the old version, then re-register from new text.
    pub fn replace(
        &mut self,
        source_path: impl Into<std::path::PathBuf>,
        text: &str,
    ) -> Result<(), LuamlError> {
        let path = source_path.into();
        self.unregister(&path);
        self.register_text(path, text)
    }

    /// All registered scripts.
    pub fn all(&self) -> &[Script] {
        &self.scripts
    }

    /// Find all clauses across all scripts that match the given event.
    /// Returns at most one match per script (first matching clause wins within a script).
    /// Evaluates guards after pattern matching.
    pub fn match_clauses<'a>(&'a self, event: &FieldMap) -> Vec<ClauseMatch<'a>> {
        let mut matches = Vec::new();
        for script in &self.scripts {
            if let Some(m) = match_first_clause(script, event) {
                matches.push(m);
            }
        }
        matches
    }

    /// Find all clauses whose pattern fields are a superset of the query fields.
    ///
    /// This is the inverse of dispatch matching: the query fields must be a SUBSET
    /// of the clause's pattern fields. For each query field, the clause must have a
    /// pattern with that name, and the pattern must accept the query value (literals
    /// must match exactly; Variable and Wildcard match any value).
    ///
    /// Guards are ignored — this answers "what clauses exist for this shape of event?"
    /// not "what would fire right now?"
    ///
    /// Returns one result per matching clause (a multi-clause script can appear
    /// multiple times).
    pub fn query_subset<'a>(&'a self, query: &FieldMap) -> Vec<QueryResult<'a>> {
        let mut results = Vec::new();
        for script in &self.scripts {
            for (clause_index, clause) in script.clauses.iter().enumerate() {
                if subset_matches(query, &clause.policy.fields) {
                    results.push(QueryResult {
                        script,
                        clause_index,
                        clause,
                    });
                }
            }
        }
        results
    }
}

/// A matched clause with its bindings.
pub struct ClauseMatch<'a> {
    pub script: &'a Script,
    pub clause: &'a Clause,
    pub bindings: FieldBindings,
}

/// A clause discovered by subset query (no bindings, no guard evaluation).
pub struct QueryResult<'a> {
    pub script: &'a Script,
    pub clause_index: usize,
    pub clause: &'a Clause,
}

/// Check whether all query fields exist in the clause's pattern fields and match.
/// Variable and Wildcard patterns accept any query value.
/// Literal patterns must match the query value exactly.
fn subset_matches(query: &FieldMap, pattern_fields: &[(String, Pattern)]) -> bool {
    for (query_field, query_value) in query {
        let Some((_, pattern)) = pattern_fields.iter().find(|(k, _)| k == query_field) else {
            return false; // clause doesn't have this field
        };
        if match_field_value(pattern, query_value).is_none() {
            return false; // pattern doesn't accept this value
        }
    }
    true
}

/// Find the first clause in a script that matches the event.
fn match_first_clause<'a>(script: &'a Script, event: &FieldMap) -> Option<ClauseMatch<'a>> {
    for clause in &script.clauses {
        // Pattern match the clause's execution policy against the event.
        let Some(bindings) = match_fields(&clause.policy.fields, event) else {
            continue;
        };

        // Evaluate guard if present.
        if let Some(guard_expr) = &clause.guard {
            match evaluate_guard(guard_expr, &bindings) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => continue, // Guard evaluation error = no match
            }
        }

        return Some(ClauseMatch {
            script,
            clause,
            bindings,
        });
    }
    None
}

/// Recursively collect file paths under a directory.
fn walkdir(dir: &Path) -> Result<Vec<std::path::PathBuf>, LuamlError> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walkdir(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FieldValue;

    fn event(pairs: &[(&str, FieldValue)]) -> FieldMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn register_script(registry: &mut ScriptRegistry, text: &str) {
        registry
            .register_text("test.luaml", text)
            .expect("script should parse");
    }

    #[test]
    fn match_single_clause() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\nkey: \"q\"\n---\nquit()\n");

        let matches = reg.match_clauses(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("q".into())),
        ]));
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn no_match_wrong_value() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\nkey: \"q\"\n---\nquit()\n");

        let matches = reg.match_clauses(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("x".into())),
        ]));
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn no_match_missing_field() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\nkey: \"q\"\n---\nquit()\n");

        let matches = reg.match_clauses(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn extra_event_fields_ok() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\n---\nhandle()\n");

        let matches = reg.match_clauses(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("q".into())),
            ("mode", FieldValue::Enum("normal".into())),
        ]));
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn variable_binding_captured() {
        let mut reg = ScriptRegistry::new();
        register_script(
            &mut reg,
            "---\ntype: :input:\nkey: $pressed\n---\nprint(pressed)\n",
        );

        let matches = reg.match_clauses(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("q".into())),
        ]));
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].bindings.get("pressed"),
            Some(&FieldValue::String("q".into()))
        );
    }

    #[test]
    fn multi_clause_first_match_wins() {
        let mut reg = ScriptRegistry::new();
        register_script(
            &mut reg,
            "\
---
type: :input:
key: :escape:
---
handle_escape()
---
key: $other
---
handle_other()
",
        );

        // escape matches clause 1
        let matches = reg.match_clauses(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::Enum("escape".into())),
        ]));
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].clause.behavior.lua_source, "handle_escape()");

        // tab matches clause 2 (wildcard)
        let matches = reg.match_clauses(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::Enum("tab".into())),
        ]));
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].clause.behavior.lua_source, "handle_other()");
        assert_eq!(
            matches[0].bindings.get("other"),
            Some(&FieldValue::Enum("tab".into()))
        );
    }

    #[test]
    fn guard_filters_matches() {
        let mut reg = ScriptRegistry::new();
        register_script(
            &mut reg,
            "---\ntype: :lifecycle:\ndepth: $d\n? d > 0\n---\nhandle()\n",
        );

        // depth=2 passes guard
        let matches = reg.match_clauses(&event(&[
            ("type", FieldValue::Enum("lifecycle".into())),
            ("depth", FieldValue::Number(2)),
        ]));
        assert_eq!(matches.len(), 1);

        // depth=0 fails guard
        let matches = reg.match_clauses(&event(&[
            ("type", FieldValue::Enum("lifecycle".into())),
            ("depth", FieldValue::Number(0)),
        ]));
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn multiple_scripts_match() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\n---\nfirst()\n");
        register_script(&mut reg, "---\ntype: :input:\n---\nsecond()\n");

        let matches = reg.match_clauses(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn type_distinct_matching() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\n---\nhandle()\n");

        // Enum matches Enum
        let matches = reg.match_clauses(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(matches.len(), 1);

        // String("input") does NOT match Enum("input")
        let matches = reg.match_clauses(&event(&[("type", FieldValue::String("input".into()))]));
        assert_eq!(matches.len(), 0);
    }

    // ── Registration edge cases ────────────────────────────────────

    #[test]
    fn register_text_invalid_source_returns_error() {
        let mut reg = ScriptRegistry::new();
        assert!(reg.register_text("bad.luaml", "not valid luaml").is_err());
    }

    #[test]
    fn register_text_same_path_twice() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\n---\nfirst()\n");
        register_script(&mut reg, "---\ntype: :input:\n---\nsecond()\n");
        // Duplicates are not deduplicated
        assert_eq!(reg.all().len(), 2);
    }

    #[test]
    fn register_pre_parsed_script() {
        use crate::clause::{Behavior, Clause, ExecutionPolicy, Script};
        use std::collections::BTreeMap;
        let mut reg = ScriptRegistry::new();
        let script = Script {
            source_path: "direct.luaml".into(),
            clauses: vec![Clause {
                policy: ExecutionPolicy { fields: vec![] },
                guard: None,
                behavior: Behavior {
                    lua_source: "x()".into(),
                },
                annotations: Vec::new(),
                field_annotations: BTreeMap::new(),
            }],
        };
        reg.register(script);
        assert_eq!(reg.all().len(), 1);
    }

    #[test]
    fn all_returns_all_scripts() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :a:\n---\na()\n");
        register_script(&mut reg, "---\ntype: :b:\n---\nb()\n");
        register_script(&mut reg, "---\ntype: :c:\n---\nc()\n");
        assert_eq!(reg.all().len(), 3);
    }

    // ── Matching edge cases ────────────────────────────────────────

    #[test]
    fn match_clauses_empty_registry() {
        let reg = ScriptRegistry::new();
        let matches = reg.match_clauses(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn match_clauses_empty_event() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\n---\nhandle()\n");
        // Empty event has no "type" field → no match
        let matches = reg.match_clauses(&FieldMap::new());
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn match_clauses_wildcard_only_script() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\nkey: *\n---\ncatch_all()\n");
        // Matches any event that has a "key" field
        let matches = reg.match_clauses(&event(&[("key", FieldValue::Enum("anything".into()))]));
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn guard_error_skips_clause() {
        let mut reg = ScriptRegistry::new();
        // Guard has syntax error ">" — evaluate_guard will return Err
        register_script(
            &mut reg,
            "---\ntype: :input:\nkey: $k\n? >\n---\nhandle()\n",
        );
        let matches = reg.match_clauses(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("q".into())),
        ]));
        // Guard error → skipped, not propagated
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn at_most_one_match_per_script() {
        let mut reg = ScriptRegistry::new();
        // Both clauses match type: :input:, but only first should be returned
        register_script(
            &mut reg,
            "\
---
type: :input:
key: $k
---
first()
---
key: $k2
---
second()
",
        );
        let matches = reg.match_clauses(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("q".into())),
        ]));
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].clause.behavior.lua_source, "first()");
    }

    // ── Subset query tests ────────────────────────────────────────

    #[test]
    fn query_subset_empty_query_returns_all_clauses() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\n---\na()\n");
        register_script(&mut reg, "---\ntype: :lifecycle:\n---\nb()\n");
        let results = reg.query_subset(&FieldMap::new());
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn query_subset_type_filter() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\n---\na()\n");
        register_script(&mut reg, "---\ntype: :lifecycle:\n---\nb()\n");
        let results = reg.query_subset(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].clause.behavior.lua_source, "a()");
    }

    #[test]
    fn query_subset_multiple_fields() {
        let mut reg = ScriptRegistry::new();
        // This clause has type + surface + mode
        register_script(
            &mut reg,
            "---\ntype: :input:\nsurface: :tui:\nmode: :leader:\n---\nleader()\n",
        );
        // This clause has type + surface only
        register_script(
            &mut reg,
            "---\ntype: :input:\nsurface: :tui:\n---\nall_tui()\n",
        );
        // This clause has type only
        register_script(&mut reg, "---\ntype: :input:\n---\nall_input()\n");

        // Query {type: :input:, surface: :tui:, mode: :leader:} — only the first has all three
        let results = reg.query_subset(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("surface", FieldValue::Enum("tui".into())),
            ("mode", FieldValue::Enum("leader".into())),
        ]));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].clause.behavior.lua_source, "leader()");
    }

    #[test]
    fn query_subset_variable_matches_any_value() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\nkey: $k\n---\nhandle()\n");

        let results = reg.query_subset(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("anything".into())),
        ]));
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn query_subset_wildcard_matches_any_value() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\nkey: *\n---\nhandle()\n");

        let results = reg.query_subset(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::Enum("tab".into())),
        ]));
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn query_subset_literal_mismatch_excludes() {
        let mut reg = ScriptRegistry::new();
        register_script(
            &mut reg,
            "---\ntype: :input:\nmode: :leader:\n---\nhandle()\n",
        );

        // Query with mode: :normal: — literal :leader: does not match :normal:
        let results = reg.query_subset(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("mode", FieldValue::Enum("normal".into())),
        ]));
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn query_subset_field_not_in_clause_excludes() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\n---\nhandle()\n");

        // Query includes "surface" which the clause doesn't have
        let results = reg.query_subset(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("surface", FieldValue::Enum("tui".into())),
        ]));
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn query_subset_guard_ignored() {
        let mut reg = ScriptRegistry::new();
        register_script(
            &mut reg,
            "---\ntype: :lifecycle:\ndepth: $d\n? d > 0\n---\nhandle()\n",
        );

        // depth=0 would fail the guard, but subset query ignores guards
        let results = reg.query_subset(&event(&[
            ("type", FieldValue::Enum("lifecycle".into())),
            ("depth", FieldValue::Number(0)),
        ]));
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn query_subset_returns_all_matching_clauses_per_script() {
        let mut reg = ScriptRegistry::new();
        register_script(
            &mut reg,
            "\
---
type: :tool:
name: \"search\"
query: $q
limit: $l
---
search_with_limit()
---
query: $q
---
search_default()
",
        );
        // Query {type: :tool:} — both clauses have type (inherited)
        let results = reg.query_subset(&event(&[("type", FieldValue::Enum("tool".into()))]));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn query_subset_clause_index_correct() {
        let mut reg = ScriptRegistry::new();
        register_script(
            &mut reg,
            "\
---
type: :tool:
name: \"search\"
query: $q
limit: $l
---
first()
---
query: $q
---
second()
",
        );
        let results = reg.query_subset(&event(&[("type", FieldValue::Enum("tool".into()))]));
        assert_eq!(results[0].clause_index, 0);
        assert_eq!(results[1].clause_index, 1);
    }

    #[test]
    fn query_subset_empty_registry() {
        let reg = ScriptRegistry::new();
        let results = reg.query_subset(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn query_subset_type_distinct_matching() {
        let mut reg = ScriptRegistry::new();
        register_script(&mut reg, "---\ntype: :input:\n---\nhandle()\n");

        // Enum pattern :input: does NOT match String("input")
        let results = reg.query_subset(&event(&[("type", FieldValue::String("input".into()))]));
        assert_eq!(results.len(), 0);

        // Enum matches Enum
        let results = reg.query_subset(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn query_subset_string_literal_match() {
        let mut reg = ScriptRegistry::new();
        register_script(
            &mut reg,
            "---\ntype: :tool:\nname: \"semantic_search\"\n---\nhandle()\n",
        );

        // Exact string match
        let results = reg.query_subset(&event(&[
            ("type", FieldValue::Enum("tool".into())),
            ("name", FieldValue::String("semantic_search".into())),
        ]));
        assert_eq!(results.len(), 1);

        // Different string
        let results = reg.query_subset(&event(&[
            ("type", FieldValue::Enum("tool".into())),
            ("name", FieldValue::String("remote_read".into())),
        ]));
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn query_subset_across_multiple_scripts() {
        let mut reg = ScriptRegistry::new();
        register_script(
            &mut reg,
            "---\ntype: :input:\nsurface: :tui:\nmode: :leader:\n---\na()\n",
        );
        register_script(
            &mut reg,
            "---\ntype: :input:\nsurface: :tui:\nmode: :normal:\n---\nb()\n",
        );
        register_script(
            &mut reg,
            "---\ntype: :input:\nsurface: :runner:\n---\nc()\n",
        );

        // Query for all TUI input handlers
        let results = reg.query_subset(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("surface", FieldValue::Enum("tui".into())),
        ]));
        assert_eq!(results.len(), 2);
    }

    // ── Unregister / replace tests ────────────────────────────────

    #[test]
    fn unregister_removes_script() {
        let mut reg = ScriptRegistry::new();
        reg.register_text("a.luaml", "---\ntype: :input:\n---\na()\n")
            .unwrap();
        reg.register_text("b.luaml", "---\ntype: :input:\n---\nb()\n")
            .unwrap();
        assert_eq!(reg.all().len(), 2);

        reg.unregister(Path::new("a.luaml"));
        assert_eq!(reg.all().len(), 1);
        assert_eq!(reg.all()[0].source_path, Path::new("b.luaml"));
    }

    #[test]
    fn unregister_nonexistent_is_noop() {
        let mut reg = ScriptRegistry::new();
        reg.register_text("a.luaml", "---\ntype: :input:\n---\na()\n")
            .unwrap();
        reg.unregister(Path::new("nonexistent.luaml"));
        assert_eq!(reg.all().len(), 1);
    }

    #[test]
    fn replace_updates_script() {
        let mut reg = ScriptRegistry::new();
        reg.register_text("a.luaml", "---\ntype: :input:\n---\nold()\n")
            .unwrap();
        assert_eq!(reg.all()[0].clauses[0].behavior.lua_source, "old()");

        reg.replace("a.luaml", "---\ntype: :input:\n---\nnew()\n")
            .unwrap();
        assert_eq!(reg.all().len(), 1);
        assert_eq!(reg.all()[0].clauses[0].behavior.lua_source, "new()");
    }

    #[test]
    fn replace_nonexistent_registers_new() {
        let mut reg = ScriptRegistry::new();
        reg.replace("new.luaml", "---\ntype: :input:\n---\nfresh()\n")
            .unwrap();
        assert_eq!(reg.all().len(), 1);
        assert_eq!(reg.all()[0].clauses[0].behavior.lua_source, "fresh()");
    }

    #[test]
    fn unregister_then_dispatch_no_match() {
        let mut reg = ScriptRegistry::new();
        reg.register_text("a.luaml", "---\ntype: :input:\n---\na()\n")
            .unwrap();
        reg.unregister(Path::new("a.luaml"));
        let matches = reg.match_clauses(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(matches.len(), 0);
    }
}
