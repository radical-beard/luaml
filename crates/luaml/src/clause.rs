use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::pattern::Pattern;

/// The frontmatter — typed pattern fields that determine when a clause executes.
#[derive(Clone, Debug)]
pub struct ExecutionPolicy {
    pub fields: Vec<(String, Pattern)>,
}

/// The Lua code body attached to an execution policy.
#[derive(Clone, Debug)]
pub struct Behavior {
    pub lua_source: String,
}

/// One execution policy + one behavior — the atomic unit of matching and execution.
///
/// Annotations are pure metadata (`@key: value` lines in frontmatter).
/// They never affect pattern matching or execution — consumers read them
/// for display, schema generation, etc.
///
/// - `annotations`: top-level annotations (before the first field, i.e. before `type:`)
/// - `field_annotations`: per-field annotations (immediately before a field line)
#[derive(Clone, Debug)]
pub struct Clause {
    pub policy: ExecutionPolicy,
    pub guard: Option<String>,
    pub behavior: Behavior,
    pub annotations: Vec<(String, String)>,
    pub field_annotations: BTreeMap<String, Vec<(String, String)>>,
}

/// A single .luaml file containing one or more clauses.
#[derive(Clone, Debug)]
pub struct Script {
    pub source_path: PathBuf,
    pub clauses: Vec<Clause>,
}
