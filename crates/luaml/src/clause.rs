use crate::pattern::Pattern;
use std::path::PathBuf;

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
#[derive(Clone, Debug)]
pub struct Clause {
    pub policy: ExecutionPolicy,
    pub guard: Option<String>,
    pub behavior: Behavior,
}

/// A single .luaml file containing one or more clauses.
#[derive(Clone, Debug)]
pub struct Script {
    pub source_path: PathBuf,
    pub clauses: Vec<Clause>,
}
