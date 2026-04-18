pub mod api;
pub mod clause;
pub mod error;
pub mod executor;
pub mod extension;
pub mod guard;
pub mod parser;
pub mod pattern;
pub mod pattern_match;
pub mod registry;
pub mod stdlib;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod types;
#[cfg(feature = "file-watch")]
mod watcher;

pub use stdlib::promise::Promise;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mlua::Lua;
use tokio::runtime::{Builder, Runtime};

use api::{
    ApiBindingEntry, ApiBindingId, ApiBindingSpec, LocalApiBindingEntry, LocalApiBindingSpec,
};
use clause::Clause;
use error::LuamlError;
use executor::execute_clause;
use registry::{ClauseMatch, QueryResult, ScriptRegistry};
use types::{FieldBindings, FieldMap, FieldValue};

#[cfg(feature = "file-watch")]
use watcher::ScriptWatcher;

/// Result of dispatching a single matched clause. The outer dispatch call
/// never fails — per-clause failures surface via [`ClauseOutcome::result`].
#[derive(Debug)]
pub struct ClauseOutcome<'a> {
    pub script_path: &'a Path,
    pub clause: &'a Clause,
    pub bindings: FieldBindings,
    pub result: Result<ClauseSuccess, ClauseError>,
}

/// Per-clause success. `emitted` holds any back-matter cascade events the
/// clause produced (populated once L7 lands; empty otherwise).
#[derive(Debug, Default, Clone)]
pub struct ClauseSuccess {
    pub emitted: Vec<FieldMap>,
}

/// Per-clause failure. The dispatch loop isolates these so one bad clause
/// cannot stop its siblings.
#[derive(Debug, Clone)]
pub struct ClauseError {
    pub kind: ClauseErrKind,
    pub message: String,
}

/// Discriminator for per-clause failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClauseErrKind {
    /// Guard expression raised an error (reserved for callers that opt into
    /// surfacing guard errors; the default dispatch still silently skips
    /// clauses whose guard errors, matching pre-L4 behavior).
    Guard,
    /// The Lua body raised an error. Errors from `luaml.dispatch(...)` (e.g.
    /// malformed table) surface as `Body` — the API call is part of the body.
    Body,
    /// A cascade emission would have exceeded the configured depth limit.
    CascadeDepth,
    /// A cascade emission targeted an event already on the active dispatch
    /// stack (cycle detected).
    CascadeCycle,
    /// A cascade emission would have exceeded the configured total-emission
    /// budget for this dispatch tree.
    CascadeBudget,
}

/// Snapshot of engine activity. Returned by
/// [`LuamlEngine::stats`]. Counters track cumulative activity since the engine
/// was created; `scripts_by_type` and `last_error_per_script` are derived from
/// the current registry / error history at call time.
#[derive(Debug, Default, Clone)]
pub struct EngineStats {
    /// Count of currently-registered scripts grouped by the enum value of
    /// their first clause's `type:` pattern field. Scripts without a literal
    /// `type:` pattern are grouped under the empty string.
    pub scripts_by_type: std::collections::BTreeMap<String, usize>,
    /// Total number of times [`LuamlEngine::dispatch`] has been called.
    pub dispatches: u64,
    /// Total number of matched clauses across all dispatches.
    pub matched: u64,
    /// Deepest cascade depth reached across all dispatches (0 until L7 lands).
    pub cascade_depth_max: u32,
    /// Total cascade events emitted across all dispatches (0 until L7 lands).
    pub cascades_emitted: u64,
    /// Most recent per-clause error keyed by the clause's source path.
    pub last_error_per_script: std::collections::BTreeMap<PathBuf, ClauseError>,
}

/// Tuning knobs for back-matter cascading dispatch. The defaults keep cascade
/// chains bounded and deterministic; callers with known chain shapes can
/// relax them via [`LuamlEngine::set_cascade_config`].
#[derive(Debug, Clone, Copy)]
pub struct CascadeConfig {
    /// Maximum number of nested cascade levels. A clause whose emission would
    /// push past this depth records a synthetic `CascadeDepth` outcome instead.
    pub max_depth: u32,
    /// Maximum number of cascade emissions produced by a single top-level
    /// dispatch tree. Once exhausted, further emissions record a synthetic
    /// `CascadeBudget` outcome instead of re-entering dispatch.
    pub budget: u32,
}

impl Default for CascadeConfig {
    fn default() -> Self {
        Self {
            max_depth: 32,
            budget: 256,
        }
    }
}

/// Top-level engine combining script registry, API bindings, and Lua execution.
pub struct LuamlEngine {
    registry: ScriptRegistry,
    api_bindings: Vec<ApiBindingEntry>,
    local_api_bindings: Vec<LocalApiBindingEntry>,
    next_api_id: u64,
    lua: Lua,
    roots: Vec<PathBuf>,
    cascade_config: CascadeConfig,
    dispatches: Cell<u64>,
    matched: Cell<u64>,
    cascade_depth_max: Cell<u32>,
    cascades_emitted: Cell<u64>,
    last_error_per_script: RefCell<BTreeMap<PathBuf, ClauseError>>,
    /// Tokio runtime backing stdlib async ops (http, fs, process, ...).
    /// Owned by the engine so every per-engine Lua VM has a runtime to
    /// `block_on` regardless of whether the embedder holds their own.
    /// `Arc` because modules and [`Promise`]s clone handles out of it.
    rt: Arc<Runtime>,
    #[cfg(feature = "file-watch")]
    watcher: Option<ScriptWatcher>,
}

impl LuamlEngine {
    pub fn new() -> Result<Self, LuamlError> {
        Self::with_lua(Lua::new())
    }

    /// Construct an engine that adopts an existing [`Lua`] VM. Library-mode
    /// consumers (e.g. crucible's per-agent runtime) that pre-build namespace
    /// tables against their own Lua hand that Lua over so engine dispatch and
    /// consumer-held table handles share one VM — the only way to install a
    /// pre-built table into a clause environment without cross-VM copies.
    pub fn with_lua(lua: Lua) -> Result<Self, LuamlError> {
        let rt = Arc::new(
            Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(LuamlError::Io)?,
        );
        stdlib::install_all(&lua, rt.handle())?;
        Ok(Self {
            registry: ScriptRegistry::new(),
            api_bindings: Vec::new(),
            local_api_bindings: Vec::new(),
            next_api_id: 0,
            lua,
            roots: Vec::new(),
            cascade_config: CascadeConfig::default(),
            dispatches: Cell::new(0),
            matched: Cell::new(0),
            cascade_depth_max: Cell::new(0),
            cascades_emitted: Cell::new(0),
            last_error_per_script: RefCell::new(BTreeMap::new()),
            rt,
            #[cfg(feature = "file-watch")]
            watcher: None,
        })
    }

    /// Clone of the engine's tokio runtime handle. Stdlib modules and code
    /// that spawns async work on behalf of a clause use this handle so every
    /// task runs on the engine-owned runtime rather than a caller-provided
    /// one. The handle is `Clone + Send + Sync`.
    pub fn rt_handle(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
    }

    /// Replace the cascade configuration. New values apply to subsequent
    /// dispatches only; in-flight dispatch trees keep their original config.
    pub fn set_cascade_config(&mut self, cfg: CascadeConfig) {
        self.cascade_config = cfg;
    }

    /// Snapshot of the current cascade configuration.
    pub fn cascade_config(&self) -> CascadeConfig {
        self.cascade_config
    }

    /// Register a script from source text.
    pub fn register(
        &mut self,
        source_path: impl Into<PathBuf>,
        text: &str,
    ) -> Result<(), LuamlError> {
        self.registry.register_text(source_path, text)
    }

    /// Register a script from a file path.
    pub fn register_file(&mut self, path: impl AsRef<Path>) -> Result<(), LuamlError> {
        self.registry.register_file(path.as_ref())
    }

    /// Register all .luaml files under a directory (recursive).
    pub fn register_dir(&mut self, dir: impl AsRef<Path>) -> Result<usize, LuamlError> {
        self.registry.register_dir(dir.as_ref())
    }

    /// Record the given roots (de-duplicating against previously-recorded
    /// roots) and register every `.luaml` file under each. Returns the total
    /// number of newly registered scripts.
    pub fn register_roots(&mut self, roots: &[PathBuf]) -> Result<usize, LuamlError> {
        let mut total = 0;
        for root in roots {
            total += self.add_root(root.clone())?;
        }
        Ok(total)
    }

    /// The roots currently tracked by the engine.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Clear the registry and re-register every `.luaml` file under every
    /// tracked root. Returns the total number of scripts registered. API
    /// bindings and the root list survive the reload.
    pub fn reload_roots(&mut self) -> Result<usize, LuamlError> {
        self.registry.clear();
        let mut total = 0;
        for root in self.roots.clone() {
            total += self.registry.register_dir(&root)?;
        }
        Ok(total)
    }

    /// Append a root (idempotent — duplicates are ignored) and register every
    /// `.luaml` file under it. Returns the number of newly registered scripts;
    /// returns 0 if the root was already tracked.
    pub fn add_root(&mut self, root: PathBuf) -> Result<usize, LuamlError> {
        if self.roots.iter().any(|r| r == &root) {
            return Ok(0);
        }
        let count = self.registry.register_dir(&root)?;
        self.roots.push(root);
        Ok(count)
    }

    /// Remove every script that was registered from the given source path.
    /// Returns true if at least one script was removed.
    pub fn unregister(&mut self, source_path: impl AsRef<Path>) -> bool {
        self.registry.unregister(source_path.as_ref())
    }

    /// Re-read the file at `path` and atomically swap it for any previously
    /// registered entry at the same source path. Returns true if an old entry
    /// was replaced, false if this is a fresh registration.
    pub fn replace_file(&mut self, path: impl AsRef<Path>) -> Result<bool, LuamlError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)?;
        self.registry.replace(path, &text)
    }

    /// Drop every registered script. API bindings remain registered.
    pub fn clear(&mut self) {
        self.registry.clear();
    }

    /// Register an API namespace binding (namespace + pattern + handler).
    /// Returns an opaque id the caller can use later to remove or atomically
    /// replace the binding.
    pub fn register_api(&mut self, spec: ApiBindingSpec) -> ApiBindingId {
        let id = ApiBindingId(self.next_api_id);
        self.next_api_id += 1;
        self.api_bindings.push(ApiBindingEntry { id, spec });
        id
    }

    /// Remove the binding previously registered under `id`. Returns true if a
    /// binding was removed.
    pub fn unregister_api(&mut self, id: ApiBindingId) -> bool {
        let before = self.api_bindings.len();
        self.api_bindings.retain(|e| e.id != id);
        self.api_bindings.len() != before
    }

    /// Atomically swap the spec for the binding previously registered under
    /// `id`. Returns true if a binding was replaced, false if the id is
    /// unknown.
    pub fn replace_api(&mut self, id: ApiBindingId, spec: ApiBindingSpec) -> bool {
        for entry in &mut self.api_bindings {
            if entry.id == id {
                entry.spec = spec;
                return true;
            }
        }
        false
    }

    /// Register a local-mode API namespace binding.
    ///
    /// Local bindings produce the namespace table directly in the engine's Lua
    /// VM via a builder closure — for consumers whose handlers hold non-`Send`
    /// state or need direct `mlua` access. Returns an opaque id for removal /
    /// hot-swap. See [`LocalApiBindingSpec`] for details.
    pub fn register_local_api(&mut self, spec: LocalApiBindingSpec) -> ApiBindingId {
        let id = ApiBindingId(self.next_api_id);
        self.next_api_id += 1;
        self.local_api_bindings
            .push(LocalApiBindingEntry { id, spec });
        id
    }

    /// Remove a local-mode binding previously registered under `id`. Returns
    /// true if a binding was removed.
    pub fn unregister_local_api(&mut self, id: ApiBindingId) -> bool {
        let before = self.local_api_bindings.len();
        self.local_api_bindings.retain(|e| e.id != id);
        self.local_api_bindings.len() != before
    }

    /// Find all matching clauses without executing them.
    pub fn query(&self, event: &FieldMap) -> Vec<ClauseMatch<'_>> {
        self.registry.match_clauses(event)
    }

    /// Find all clauses whose pattern fields are a superset of the query fields.
    /// See [`ScriptRegistry::query_subset`] for details.
    pub fn query_subset(&self, query: &FieldMap) -> Vec<QueryResult<'_>> {
        self.registry.query_subset(query)
    }

    /// Match and execute all clauses that match the event. Returns one
    /// outcome per matched clause. The dispatch call itself never errors:
    /// per-clause failures (Lua body raised, emission shape invalid, cascade
    /// limits exceeded) are captured in each outcome's `result` so a single
    /// bad clause cannot stop its siblings.
    ///
    /// If the engine is watching roots, any pending file changes are drained
    /// and applied to the registry before matching. Callers never invoke a
    /// separate reload step.
    pub fn dispatch(&mut self, event: &FieldMap) -> Vec<ClauseOutcome<'_>> {
        #[cfg(feature = "file-watch")]
        if let Some(watcher) = self.watcher.as_ref() {
            let _ = watcher.apply_pending(&mut self.registry);
        }

        self.dispatches.set(self.dispatches.get() + 1);
        let mut outcomes = Vec::new();
        let mut ctx = CascadeContext::new(self.cascade_config);
        self.dispatch_inner(event, &mut ctx, &mut outcomes, 0);
        outcomes
    }

    fn dispatch_inner<'a>(
        &'a self,
        event: &FieldMap,
        ctx: &mut CascadeContext,
        outcomes: &mut Vec<ClauseOutcome<'a>>,
        depth: u32,
    ) {
        self.cascade_depth_max
            .set(self.cascade_depth_max.get().max(depth));

        if depth >= ctx.config.max_depth {
            outcomes.push(synthetic_cascade_outcome(
                ClauseErrKind::CascadeDepth,
                format!(
                    "cascade depth {} exceeded configured max_depth {}",
                    depth, ctx.config.max_depth
                ),
            ));
            return;
        }

        let sig = event_signature(event);
        if !ctx.mark_visited(sig.clone()) {
            outcomes.push(synthetic_cascade_outcome(
                ClauseErrKind::CascadeCycle,
                format!("cascade cycle detected at depth {} (repeated event)", depth),
            ));
            return;
        }

        let matches = self.registry.match_clauses(event);
        self.matched.set(self.matched.get() + matches.len() as u64);

        for m in matches {
            let (outcome, emissions) = self.execute_matched(m);
            outcomes.push(outcome);

            for emission_event in emissions {
                if ctx.budget_remaining == 0 {
                    outcomes.push(synthetic_cascade_outcome(
                        ClauseErrKind::CascadeBudget,
                        format!(
                            "cascade budget {} exhausted at depth {}",
                            ctx.config.budget,
                            depth + 1
                        ),
                    ));
                    ctx.unmark_visited(&sig);
                    return;
                }
                ctx.budget_remaining -= 1;
                self.cascades_emitted.set(self.cascades_emitted.get() + 1);
                self.dispatch_inner(&emission_event, ctx, outcomes, depth + 1);
            }
        }

        ctx.unmark_visited(&sig);
    }

    fn execute_matched<'a>(&'a self, m: ClauseMatch<'a>) -> (ClauseOutcome<'a>, Vec<FieldMap>) {
        let exec = execute_clause(
            &self.lua,
            m.clause,
            &m.bindings,
            &self.api_bindings,
            &self.local_api_bindings,
        );
        let (result, emissions) = match exec {
            Ok(emissions) => (
                Ok(ClauseSuccess {
                    emitted: emissions.clone(),
                }),
                emissions,
            ),
            Err(e) => (
                Err(ClauseError {
                    kind: ClauseErrKind::Body,
                    message: e.to_string(),
                }),
                Vec::new(),
            ),
        };
        if let Err(err) = &result {
            self.last_error_per_script
                .borrow_mut()
                .insert(m.script.source_path.clone(), err.clone());
        }
        (
            ClauseOutcome {
                script_path: &m.script.source_path,
                clause: m.clause,
                bindings: m.bindings,
                result,
            },
            emissions,
        )
    }

    /// Snapshot of engine activity since creation. See [`EngineStats`].
    pub fn stats(&self) -> EngineStats {
        let mut by_type = BTreeMap::<String, usize>::new();
        for script in self.registry.scripts() {
            let type_key = script
                .clauses
                .first()
                .and_then(|c| {
                    c.policy.fields.iter().find_map(|(k, p)| {
                        if k == "type" {
                            match p {
                                pattern::Pattern::Enum(s) => Some(s.clone()),
                                pattern::Pattern::StringLiteral(s) => Some(s.clone()),
                                _ => None,
                            }
                        } else {
                            None
                        }
                    })
                })
                .unwrap_or_default();
            *by_type.entry(type_key).or_insert(0) += 1;
        }
        EngineStats {
            scripts_by_type: by_type,
            dispatches: self.dispatches.get(),
            matched: self.matched.get(),
            cascade_depth_max: self.cascade_depth_max.get(),
            cascades_emitted: self.cascades_emitted.get(),
            last_error_per_script: self.last_error_per_script.borrow().clone(),
        }
    }

    /// Start watching every tracked root for `.luaml` file changes. Calls to
    /// [`dispatch`](Self::dispatch) automatically apply pending changes before
    /// matching — the caller runs no reconciliation loop.
    ///
    /// Calling `watch` while already watching replaces the active watcher so
    /// newly-added roots (via [`add_root`](Self::add_root) or
    /// [`register_roots`](Self::register_roots)) are picked up.
    #[cfg(feature = "file-watch")]
    pub fn watch(&mut self, debounce: std::time::Duration) -> Result<(), LuamlError> {
        let dirs: Vec<&Path> = self.roots.iter().map(|p| p.as_path()).collect();
        let watcher = ScriptWatcher::new(&dirs, debounce)?;
        self.watcher = Some(watcher);
        Ok(())
    }

    /// Stop watching roots. Does nothing if the engine was not watching.
    #[cfg(feature = "file-watch")]
    pub fn unwatch(&mut self) {
        self.watcher = None;
    }

    /// Whether the engine is currently watching roots for file changes.
    #[cfg(feature = "file-watch")]
    pub fn is_watching(&self) -> bool {
        self.watcher.is_some()
    }

    /// Access the underlying script registry.
    pub fn registry(&self) -> &ScriptRegistry {
        &self.registry
    }

    /// Access the underlying Lua VM.
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Find all scripts belonging to a named extension.
    pub fn scripts_for_extension(&self, name: &str) -> Vec<&clause::Script> {
        self.registry.scripts_for_extension(name)
    }

    /// Return distinct extension names across all registered scripts.
    pub fn extension_names(&self) -> Vec<&str> {
        self.registry.extension_names()
    }
}

/// Per-dispatch-tree state: tracks the visited-event stack for cycle detection
/// and the remaining emission budget. A fresh context is created for each
/// top-level [`LuamlEngine::dispatch`] call so limits apply per-tree.
struct CascadeContext {
    config: CascadeConfig,
    visited: std::collections::HashSet<EventSig>,
    budget_remaining: u32,
}

impl CascadeContext {
    fn new(config: CascadeConfig) -> Self {
        Self {
            config,
            visited: std::collections::HashSet::new(),
            budget_remaining: config.budget,
        }
    }

    /// Returns true if `sig` was freshly inserted; false if the event is
    /// already on the active dispatch stack (cycle).
    fn mark_visited(&mut self, sig: EventSig) -> bool {
        self.visited.insert(sig)
    }

    fn unmark_visited(&mut self, sig: &EventSig) {
        self.visited.remove(sig);
    }
}

/// Canonicalized event signature used for cycle detection. Same key/value set
/// compares equal regardless of insertion order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EventSig(Vec<(String, FieldValueSig)>);

/// Hashable projection of `FieldValue` for signature building. `Float` is
/// encoded via `to_bits` so NaN/+0/-0 round-trip deterministically.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FieldValueSig {
    Enum(String),
    String(String),
    Number(i64),
    Float(u64),
    Bool(bool),
    Null,
    List(Vec<FieldValueSig>),
    Map(Vec<(String, FieldValueSig)>),
}

impl FieldValueSig {
    fn from_value(v: &FieldValue) -> Self {
        match v {
            FieldValue::Enum(s) => FieldValueSig::Enum(s.clone()),
            FieldValue::String(s) => FieldValueSig::String(s.clone()),
            FieldValue::Number(n) => FieldValueSig::Number(*n),
            FieldValue::Float(f) => FieldValueSig::Float(f.to_bits()),
            FieldValue::Bool(b) => FieldValueSig::Bool(*b),
            FieldValue::Null => FieldValueSig::Null,
            FieldValue::List(items) => {
                FieldValueSig::List(items.iter().map(FieldValueSig::from_value).collect())
            }
            FieldValue::Map(map) => {
                let mut pairs: Vec<(String, FieldValueSig)> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), FieldValueSig::from_value(v)))
                    .collect();
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                FieldValueSig::Map(pairs)
            }
        }
    }
}

fn event_signature(event: &FieldMap) -> EventSig {
    let mut pairs: Vec<(String, FieldValueSig)> = event
        .iter()
        .map(|(k, v)| (k.clone(), FieldValueSig::from_value(v)))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    EventSig(pairs)
}

/// Synthetic outcome used for cascade-layer failures (depth exceeded, cycle
/// detected, budget exhausted). Carries no real clause or script because the
/// failure belongs to the cascade machinery, not to any particular script.
fn synthetic_cascade_outcome<'a>(kind: ClauseErrKind, message: String) -> ClauseOutcome<'a> {
    ClauseOutcome {
        script_path: &SYNTHETIC_PATH,
        clause: &SYNTHETIC_CLAUSE,
        bindings: FieldBindings::new(),
        result: Err(ClauseError { kind, message }),
    }
}

static SYNTHETIC_PATH: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(PathBuf::new);

static SYNTHETIC_CLAUSE: std::sync::LazyLock<Clause> = std::sync::LazyLock::new(|| Clause {
    policy: clause::ExecutionPolicy { fields: Vec::new() },
    guard: None,
    behavior: clause::Behavior {
        lua_source: String::new(),
    },
    annotations: Vec::new(),
    field_annotations: std::collections::BTreeMap::new(),
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiError, ApiHandler};
    use crate::types::FieldValue;
    use std::sync::{Arc, Mutex};

    fn event(pairs: &[(&str, FieldValue)]) -> FieldMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    struct RecordingHandler {
        calls: Mutex<Vec<(String, String, Vec<FieldValue>)>>,
        return_value: FieldValue,
    }

    impl RecordingHandler {
        fn new(return_value: FieldValue) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                return_value,
            }
        }

        fn call_log(&self) -> Vec<(String, String, Vec<FieldValue>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ApiHandler for RecordingHandler {
        fn call(
            &self,
            namespace: &str,
            method: &str,
            args: Vec<FieldValue>,
        ) -> Result<FieldValue, ApiError> {
            self.calls
                .lock()
                .unwrap()
                .push((namespace.into(), method.into(), args));
            Ok(self.return_value.clone())
        }
    }

    #[test]
    fn engine_register_and_dispatch() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nkey: \"q\"\n---\nresult = \"quit\"\n",
            )
            .unwrap();

        let results = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("q".into())),
        ]));

        assert_eq!(results.len(), 1);
        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "quit");
    }

    #[test]
    fn engine_no_match_returns_empty() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("test.luaml", "---\ntype: :input:\n---\nprint('hi')\n")
            .unwrap();

        let results = engine.dispatch(&event(&[("type", FieldValue::Enum("lifecycle".into()))]));

        assert_eq!(results.len(), 0);
    }

    #[test]
    fn engine_query_without_execution() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nkey: $k\n---\nresult = k\n",
            )
            .unwrap();

        let matches = engine.query(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("x".into())),
        ]));

        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].bindings.get("k"),
            Some(&FieldValue::String("x".into()))
        );

        // result should NOT be set since we only queried
        let val: mlua::Value = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, mlua::Value::Nil);
    }

    #[test]
    fn engine_multiple_scripts_dispatch() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :input:\n---\na_ran = true\n")
            .unwrap();
        engine
            .register("b.luaml", "---\ntype: :input:\n---\nb_ran = true\n")
            .unwrap();

        let results = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));

        assert_eq!(results.len(), 2);
        assert!(engine.lua().globals().get::<bool>("a_ran").unwrap());
        assert!(engine.lua().globals().get::<bool>("b_ran").unwrap());
    }

    #[test]
    fn engine_dispatch_with_api_binding() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::String("done".into())));

        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nsurface: :tui:\n---\nresult = client.save(\"file.txt\")\n",
            )
            .unwrap();

        engine.register_api(ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![("surface".into(), pattern::Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        });

        let results = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("surface", FieldValue::Enum("tui".into())),
        ]));

        assert_eq!(results.len(), 1);

        let calls = handler.call_log();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "client");
        assert_eq!(calls[0].1, "save");
        assert_eq!(calls[0].2, vec![FieldValue::String("file.txt".into())]);

        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "done");
    }

    #[test]
    fn engine_api_not_injected_for_mismatched_clause() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::Null));

        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nsurface: :runner:\n---\nresult = client == nil\n",
            )
            .unwrap();

        // API only available for :tui: surface
        engine.register_api(ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![("surface".into(), pattern::Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        });

        engine.dispatch(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("surface", FieldValue::Enum("runner".into())),
        ]));

        // client should be nil in the :runner: clause
        assert!(engine.lua().globals().get::<bool>("result").unwrap());
        assert_eq!(handler.call_log().len(), 0);
    }

    #[test]
    fn engine_multi_clause_dispatch() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "multi.luaml",
                "\
---
type: :input:
key: :escape:
---
result = \"escape\"
---
key: :tab:
---
result = \"tab\"
",
            )
            .unwrap();

        // escape matches first clause
        let results = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::Enum("escape".into())),
        ]));
        assert_eq!(results.len(), 1);
        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "escape");

        // tab matches second clause
        let results = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::Enum("tab".into())),
        ]));
        assert_eq!(results.len(), 1);
        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "tab");
    }

    #[test]
    fn engine_bindings_available_in_lua() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nkey: $pressed\n---\nresult = pressed\n",
            )
            .unwrap();

        let results = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("q".into())),
        ]));

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].bindings.get("pressed"),
            Some(&FieldValue::String("q".into()))
        );
        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "q");
    }

    #[test]
    fn engine_dispatch_result_has_script_path() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("my/script.luaml", "---\ntype: :input:\n---\nprint('x')\n")
            .unwrap();

        let results = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].script_path, Path::new("my/script.luaml"));
    }

    #[test]
    fn engine_guard_filters_dispatch() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "test.luaml",
                "---\ntype: :lifecycle:\ndepth: $d\n? d > 0\n---\nresult = d\n",
            )
            .unwrap();

        // depth=0 fails guard — no dispatch
        let results = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("lifecycle".into())),
            ("depth", FieldValue::Number(0)),
        ]));
        assert_eq!(results.len(), 0);

        // depth=3 passes guard
        let results = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("lifecycle".into())),
            ("depth", FieldValue::Number(3)),
        ]));
        assert_eq!(results.len(), 1);
        let val: i64 = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, 3);
    }

    #[test]
    fn engine_lua_error_surfaces_in_outcome() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("test.luaml", "---\ntype: :input:\n---\nerror(\"boom\")\n")
            .unwrap();

        let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));

        assert_eq!(outcomes.len(), 1);
        let err = outcomes[0].result.as_ref().unwrap_err();
        assert_eq!(err.kind, ClauseErrKind::Body);
        assert!(err.message.contains("boom"));
    }

    #[test]
    fn engine_register_dir() {
        use std::fs;

        let dir = std::env::temp_dir().join("luaml_test_register_dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();

        fs::write(
            dir.join("a.luaml"),
            "---\ntype: :input:\n---\na_ran = true\n",
        )
        .unwrap();
        fs::write(
            dir.join("sub/b.luaml"),
            "---\ntype: :input:\n---\nb_ran = true\n",
        )
        .unwrap();
        // Non-luaml file should be ignored
        fs::write(dir.join("c.txt"), "not a script").unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        let count = engine.register_dir(&dir).unwrap();
        assert_eq!(count, 2);

        let results = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(results.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_new_creates_fresh_state() {
        let engine = LuamlEngine::new().unwrap();
        assert_eq!(engine.registry().all().len(), 0);
    }

    #[test]
    fn engine_incremental_registration() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :input:\n---\na_ran = true\n")
            .unwrap();

        // First dispatch: only a matches
        let results = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(results.len(), 1);

        // Register another script
        engine
            .register("b.luaml", "---\ntype: :input:\n---\nb_ran = true\n")
            .unwrap();

        // Second dispatch: both match
        let results = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn engine_stats_tracks_dispatches_matched_and_last_error() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("ok.luaml", "---\ntype: :input:\n---\nok_ran = true\n")
            .unwrap();
        engine
            .register("bad.luaml", "---\ntype: :input:\n---\nerror(\"broken\")\n")
            .unwrap();

        // Initial stats show nothing happened yet.
        let before = engine.stats();
        assert_eq!(before.dispatches, 0);
        assert_eq!(before.matched, 0);
        assert!(before.last_error_per_script.is_empty());

        engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));

        let after = engine.stats();
        assert_eq!(after.dispatches, 2);
        // Two scripts matched on each of two dispatches.
        assert_eq!(after.matched, 4);
        // Only the erroring script records a last-error entry.
        assert_eq!(after.last_error_per_script.len(), 1);
        let (path, err) = after.last_error_per_script.iter().next().unwrap();
        assert!(path.ends_with("bad.luaml"));
        assert_eq!(err.kind, ClauseErrKind::Body);
        assert!(err.message.contains("broken"));
    }

    #[test]
    fn engine_stats_scripts_by_type() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :input:\n---\n")
            .unwrap();
        engine
            .register("b.luaml", "---\ntype: :input:\n---\n")
            .unwrap();
        engine
            .register("c.luaml", "---\ntype: :tool:\n---\n")
            .unwrap();

        let stats = engine.stats();
        assert_eq!(stats.scripts_by_type.get("input"), Some(&2));
        assert_eq!(stats.scripts_by_type.get("tool"), Some(&1));
    }

    #[test]
    fn engine_builder_composes_scripts_and_apis() {
        use crate::testing::EngineBuilder;

        let handler = Arc::new(RecordingHandler::new(FieldValue::Number(7)));
        let mut engine = EngineBuilder::new()
            .with_script("t.luaml", "---\ntype: :input:\n---\nresult = svc.ping()\n")
            .with_api("svc", handler.clone())
            .build()
            .unwrap();

        let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].result.is_ok());
        let val: i64 = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, 7);
        assert_eq!(handler.call_log().len(), 1);
    }

    #[test]
    fn engine_builder_event_helper_builds_fieldmap() {
        use crate::testing::{enum_value, event as test_event, str_value};

        let ev = test_event([("type", enum_value("input")), ("key", str_value("q"))]);
        assert_eq!(ev.get("type"), Some(&FieldValue::Enum("input".into())));
        assert_eq!(ev.get("key"), Some(&FieldValue::String("q".into())));
    }

    #[test]
    fn engine_register_api_after_scripts() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::Number(99)));

        // Register script first
        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nsurface: :tui:\n---\nresult = svc.ping()\n",
            )
            .unwrap();

        // Register API after scripts
        engine.register_api(ApiBindingSpec {
            namespace: "svc".into(),
            pattern: vec![("surface".into(), pattern::Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        });

        let results = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("surface", FieldValue::Enum("tui".into())),
        ]));
        assert_eq!(results.len(), 1);
        assert_eq!(handler.call_log().len(), 1);
        let val: i64 = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, 99);
    }

    #[test]
    fn engine_unregister_api_removes_namespace_from_fresh_dispatch() {
        // Register an API, immediately unregister it before any dispatch, then
        // dispatch a script that references it. Because no proxy has ever been
        // injected, the reference is a nil-index error reported as a per-clause
        // Body failure.
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::Number(1)));
        engine
            .register("t.luaml", "---\ntype: :input:\n---\nsvc.ping()\n")
            .unwrap();

        let id = engine.register_api(ApiBindingSpec {
            namespace: "svc".into(),
            pattern: vec![],
            handler: handler.clone(),
        });
        assert!(engine.unregister_api(id));

        let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(outcomes.len(), 1);
        let err = outcomes[0].result.as_ref().unwrap_err();
        assert_eq!(err.kind, ClauseErrKind::Body);
        // Handler must never have been called.
        assert_eq!(handler.call_log().len(), 0);
    }

    #[test]
    fn engine_unregister_api_twice_is_noop() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::Null));
        let id = engine.register_api(ApiBindingSpec {
            namespace: "svc".into(),
            pattern: vec![],
            handler,
        });
        assert!(engine.unregister_api(id));
        // Second call finds no binding with that id.
        assert!(!engine.unregister_api(id));
    }

    #[test]
    fn engine_replace_api_swaps_handler() {
        let mut engine = LuamlEngine::new().unwrap();
        let first = Arc::new(RecordingHandler::new(FieldValue::Number(1)));
        let second = Arc::new(RecordingHandler::new(FieldValue::Number(2)));
        engine
            .register("t.luaml", "---\ntype: :input:\n---\nresult = svc.ping()\n")
            .unwrap();

        let id = engine.register_api(ApiBindingSpec {
            namespace: "svc".into(),
            pattern: vec![],
            handler: first.clone(),
        });

        engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        let v: i64 = engine.lua().globals().get("result").unwrap();
        assert_eq!(v, 1);
        assert_eq!(first.call_log().len(), 1);
        assert_eq!(second.call_log().len(), 0);

        assert!(engine.replace_api(
            id,
            ApiBindingSpec {
                namespace: "svc".into(),
                pattern: vec![],
                handler: second.clone(),
            },
        ));

        engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        let v: i64 = engine.lua().globals().get("result").unwrap();
        assert_eq!(v, 2);
        assert_eq!(first.call_log().len(), 1);
        assert_eq!(second.call_log().len(), 1);
    }

    #[test]
    fn engine_replace_api_unknown_id_returns_false() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::Null));
        let id = engine.register_api(ApiBindingSpec {
            namespace: "svc".into(),
            pattern: vec![],
            handler,
        });
        assert!(engine.unregister_api(id));
        assert!(!engine.replace_api(
            id,
            ApiBindingSpec {
                namespace: "svc".into(),
                pattern: vec![],
                handler: Arc::new(RecordingHandler::new(FieldValue::Null)),
            },
        ));
    }

    #[test]
    fn engine_dispatch_multiple_matching_with_api() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::Null));

        engine
            .register(
                "a.luaml",
                "---\ntype: :input:\nsurface: :tui:\n---\nsvc.method_a()\n",
            )
            .unwrap();
        engine
            .register(
                "b.luaml",
                "---\ntype: :input:\nsurface: :tui:\n---\nsvc.method_b()\n",
            )
            .unwrap();

        engine.register_api(ApiBindingSpec {
            namespace: "svc".into(),
            pattern: vec![("surface".into(), pattern::Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        });

        let results = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("surface", FieldValue::Enum("tui".into())),
        ]));
        assert_eq!(results.len(), 2);

        let calls = handler.call_log();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, "method_a");
        assert_eq!(calls[1].1, "method_b");
    }

    #[test]
    fn engine_query_vs_dispatch_consistency() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "test.luaml",
                "---\ntype: :input:\nkey: $k\n---\nresult = k\n",
            )
            .unwrap();

        let ev = event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("z".into())),
        ]);

        let query_bindings = engine.query(&ev)[0].bindings.clone();
        let dispatch_results = engine.dispatch(&ev);

        assert_eq!(dispatch_results.len(), 1);
        assert_eq!(
            query_bindings.get("k"),
            dispatch_results[0].bindings.get("k")
        );
    }

    #[test]
    fn engine_dispatch_isolates_errors() {
        let mut engine = LuamlEngine::new().unwrap();
        // First script errors
        engine
            .register("a.luaml", "---\ntype: :input:\n---\nerror(\"fail\")\n")
            .unwrap();
        // Second script succeeds — must still run despite sibling's failure
        engine
            .register("b.luaml", "---\ntype: :input:\n---\nb_ran = true\n")
            .unwrap();

        let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(outcomes.len(), 2);

        let a = outcomes
            .iter()
            .find(|o| o.script_path.ends_with("a.luaml"))
            .unwrap();
        let a_err = a.result.as_ref().unwrap_err();
        assert_eq!(a_err.kind, ClauseErrKind::Body);
        assert!(a_err.message.contains("fail"));

        let b = outcomes
            .iter()
            .find(|o| o.script_path.ends_with("b.luaml"))
            .unwrap();
        assert!(b.result.is_ok());

        // Sibling MUST have run despite the earlier failure.
        let val: bool = engine.lua().globals().get("b_ran").unwrap();
        assert!(val);
    }

    #[test]
    fn engine_query_subset_basic() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "a.luaml",
                "---\ntype: :input:\nsurface: :tui:\nmode: :leader:\n---\na()\n",
            )
            .unwrap();
        engine
            .register("b.luaml", "---\ntype: :input:\nsurface: :tui:\n---\nb()\n")
            .unwrap();
        engine
            .register("c.luaml", "---\ntype: :lifecycle:\n---\nc()\n")
            .unwrap();

        // Empty query returns all clauses
        let results = engine.query_subset(&FieldMap::new());
        assert_eq!(results.len(), 3);

        // Filter to input type
        let results = engine.query_subset(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(results.len(), 2);

        // Filter to TUI input leader mode — only script a has all three
        let results = engine.query_subset(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("surface", FieldValue::Enum("tui".into())),
            ("mode", FieldValue::Enum("leader".into())),
        ]));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].clause.behavior.lua_source, "a()");
    }

    #[test]
    fn engine_register_dir_empty_directory() {
        use std::fs;
        let dir = std::env::temp_dir().join("luaml_test_empty_dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        let count = engine.register_dir(&dir).unwrap();
        assert_eq!(count, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_register_dir_nonexistent() {
        let mut engine = LuamlEngine::new().unwrap();
        let count = engine
            .register_dir("/tmp/luaml_test_nonexistent_dir_xyz")
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn engine_register_invalid_text() {
        let mut engine = LuamlEngine::new().unwrap();
        let err = engine.register("bad.luaml", "not valid luaml");
        assert!(err.is_err());
    }

    #[test]
    fn engine_registry_accessor() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :a:\n---\na()\n")
            .unwrap();
        engine
            .register("b.luaml", "---\ntype: :b:\n---\nb()\n")
            .unwrap();
        assert_eq!(engine.registry().all().len(), 2);
    }

    #[test]
    fn engine_unregister_removes_script() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :input:\n---\na_ran = true\n")
            .unwrap();
        engine
            .register("b.luaml", "---\ntype: :input:\n---\nb_ran = true\n")
            .unwrap();

        assert!(engine.unregister("a.luaml"));
        assert_eq!(engine.registry().all().len(), 1);

        let results = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(results.len(), 1);
        assert!(engine.lua().globals().get::<bool>("b_ran").unwrap());
        let a_ran: mlua::Value = engine.lua().globals().get("a_ran").unwrap();
        assert_eq!(a_ran, mlua::Value::Nil);
    }

    #[test]
    fn engine_unregister_nonexistent_returns_false() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :input:\n---\na()\n")
            .unwrap();
        assert!(!engine.unregister("nonexistent.luaml"));
        assert_eq!(engine.registry().all().len(), 1);
    }

    #[test]
    fn engine_replace_file_swaps_script() {
        use std::fs;
        let dir = std::env::temp_dir().join("luaml_test_replace_file");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.luaml");
        fs::write(&path, "---\ntype: :input:\n---\nresult = \"old\"\n").unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        engine.register_file(&path).unwrap();

        fs::write(&path, "---\ntype: :input:\n---\nresult = \"new\"\n").unwrap();
        assert!(engine.replace_file(&path).unwrap());
        assert_eq!(engine.registry().all().len(), 1);

        engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "new");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_replace_file_fresh_returns_false() {
        use std::fs;
        let dir = std::env::temp_dir().join("luaml_test_replace_file_fresh");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.luaml");
        fs::write(&path, "---\ntype: :input:\n---\nresult = \"fresh\"\n").unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        assert!(!engine.replace_file(&path).unwrap());
        assert_eq!(engine.registry().all().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_replace_file_missing_file_errors() {
        let mut engine = LuamlEngine::new().unwrap();
        let err = engine.replace_file("/tmp/luaml_nonexistent_xyz.luaml");
        assert!(err.is_err());
    }

    #[test]
    fn engine_register_roots_records_and_loads() {
        use std::fs;
        let base = std::env::temp_dir().join("luaml_test_register_roots");
        let _ = fs::remove_dir_all(&base);
        let a = base.join("a");
        let b = base.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("x.luaml"), "---\ntype: :input:\n---\nx()\n").unwrap();
        fs::write(b.join("y.luaml"), "---\ntype: :input:\n---\ny()\n").unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        let total = engine.register_roots(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(total, 2);
        assert_eq!(engine.roots(), &[a, b]);
        assert_eq!(engine.registry().all().len(), 2);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn engine_register_roots_idempotent_on_duplicates() {
        use std::fs;
        let dir = std::env::temp_dir().join("luaml_test_register_roots_dupe");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x.luaml"), "---\ntype: :input:\n---\nx()\n").unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        let first = engine.register_roots(&[dir.clone(), dir.clone()]).unwrap();
        assert_eq!(first, 1);
        assert_eq!(engine.roots().len(), 1);
        assert_eq!(engine.registry().all().len(), 1);

        let second = engine.register_roots(&[dir.clone()]).unwrap();
        assert_eq!(second, 0);
        assert_eq!(engine.roots().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_add_root_loads_and_is_idempotent() {
        use std::fs;
        let dir = std::env::temp_dir().join("luaml_test_add_root");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x.luaml"), "---\ntype: :input:\n---\nx()\n").unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        assert_eq!(engine.add_root(dir.clone()).unwrap(), 1);
        assert_eq!(engine.add_root(dir.clone()).unwrap(), 0);
        assert_eq!(engine.roots().len(), 1);
        assert_eq!(engine.registry().all().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_reload_roots_picks_up_new_files() {
        use std::fs;
        let dir = std::env::temp_dir().join("luaml_test_reload_roots");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x.luaml"), "---\ntype: :input:\n---\nx()\n").unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        engine.add_root(dir.clone()).unwrap();
        assert_eq!(engine.registry().all().len(), 1);

        fs::write(dir.join("y.luaml"), "---\ntype: :input:\n---\ny()\n").unwrap();
        let total = engine.reload_roots().unwrap();
        assert_eq!(total, 2);
        assert_eq!(engine.registry().all().len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_reload_roots_preserves_api_bindings() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::Null));
        engine.register_api(ApiBindingSpec {
            namespace: "svc".into(),
            pattern: vec![],
            handler: handler.clone(),
        });
        assert_eq!(engine.reload_roots().unwrap(), 0);

        engine
            .register("a.luaml", "---\ntype: :input:\n---\nsvc.ping()\n")
            .unwrap();
        engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(handler.call_log().len(), 1);
    }

    #[test]
    fn engine_reload_roots_drops_removed_files() {
        use std::fs;
        let dir = std::env::temp_dir().join("luaml_test_reload_drops");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x.luaml"), "---\ntype: :input:\n---\nx()\n").unwrap();
        fs::write(dir.join("y.luaml"), "---\ntype: :input:\n---\ny()\n").unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        engine.add_root(dir.clone()).unwrap();
        assert_eq!(engine.registry().all().len(), 2);

        fs::remove_file(dir.join("y.luaml")).unwrap();
        assert_eq!(engine.reload_roots().unwrap(), 1);
        assert_eq!(engine.registry().all().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "file-watch")]
    #[test]
    fn engine_watch_is_idempotent() {
        use std::fs;
        let dir = std::env::temp_dir().join("luaml_test_watch_idempotent");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        engine.add_root(dir.clone()).unwrap();
        assert!(!engine.is_watching());

        engine.watch(std::time::Duration::from_millis(50)).unwrap();
        assert!(engine.is_watching());

        engine.watch(std::time::Duration::from_millis(50)).unwrap();
        assert!(engine.is_watching());

        engine.unwatch();
        assert!(!engine.is_watching());

        engine.unwatch();
        assert!(!engine.is_watching());

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "file-watch")]
    #[test]
    fn engine_watch_applies_file_changes_on_dispatch() {
        use std::fs;
        let dir = std::env::temp_dir().join("luaml_test_watch_applies");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.luaml");
        fs::write(&path, "---\ntype: :input:\n---\nresult = \"old\"\n").unwrap();

        let mut engine = LuamlEngine::new().unwrap();
        engine.add_root(dir.clone()).unwrap();
        engine.watch(std::time::Duration::from_millis(50)).unwrap();

        engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "old");

        fs::write(&path, "---\ntype: :input:\n---\nresult = \"new\"\n").unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
            let current: String = engine.lua().globals().get("result").unwrap();
            if current == "new" {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("watcher did not pick up the file change in time");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "file-watch")]
    #[test]
    fn engine_watch_without_roots_is_noop() {
        let mut engine = LuamlEngine::new().unwrap();
        engine.watch(std::time::Duration::from_millis(50)).unwrap();
        assert!(engine.is_watching());
        engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
    }

    #[cfg(feature = "file-watch")]
    #[test]
    fn engine_dispatch_without_watch_does_not_touch_registry() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :input:\n---\nran = true\n")
            .unwrap();
        assert!(!engine.is_watching());
        let results = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(results.len(), 1);
        assert!(engine.lua().globals().get::<bool>("ran").unwrap());
    }

    #[test]
    fn engine_clear_drops_scripts_preserves_api() {
        let mut engine = LuamlEngine::new().unwrap();
        let handler = Arc::new(RecordingHandler::new(FieldValue::String("ok".into())));

        engine
            .register(
                "a.luaml",
                "---\ntype: :input:\nsurface: :tui:\n---\nresult = svc.ping()\n",
            )
            .unwrap();
        engine.register_api(ApiBindingSpec {
            namespace: "svc".into(),
            pattern: vec![("surface".into(), pattern::Pattern::Enum("tui".into()))],
            handler: handler.clone(),
        });

        engine.clear();
        assert_eq!(engine.registry().all().len(), 0);

        engine
            .register(
                "b.luaml",
                "---\ntype: :input:\nsurface: :tui:\n---\nresult = svc.ping()\n",
            )
            .unwrap();
        engine.dispatch(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("surface", FieldValue::Enum("tui".into())),
        ]));
        assert_eq!(handler.call_log().len(), 1);
    }

    // ── L7: cascade via luaml.dispatch ─────────────────────────────

    #[test]
    fn cascade_single_emission_matches_downstream_clause() {
        let mut engine = LuamlEngine::new().unwrap();
        // Script A fires on :input: and enqueues a :lifecycle: event.
        engine
            .register(
                "a.luaml",
                "---\ntype: :input:\nkey: \"q\"\n---\n\
                 luaml.dispatch({ type = luaml.enum(\"lifecycle\"), event = luaml.enum(\"on_quit\") })\n",
            )
            .unwrap();
        // Script B fires on the :lifecycle: event the first one emits.
        engine
            .register(
                "b.luaml",
                "---\ntype: :lifecycle:\nevent: :on_quit:\n---\nresult = \"b_ran\"\n",
            )
            .unwrap();

        let outcomes = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("input".into())),
            ("key", FieldValue::String("q".into())),
        ]));

        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[0].result.is_ok());
        assert!(outcomes[1].result.is_ok());

        let val: String = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, "b_ran");
    }

    #[test]
    fn cascade_without_dispatch_call_is_no_op() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register("a.luaml", "---\ntype: :input:\n---\nresult = \"done\"\n")
            .unwrap();

        let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("input".into()))]));
        assert_eq!(outcomes.len(), 1);
        if let Ok(ref s) = outcomes[0].result {
            assert!(s.emitted.is_empty());
        } else {
            panic!("expected success, got {:?}", outcomes[0].result);
        }
    }

    #[test]
    fn cascade_multiple_dispatches_in_one_script() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "a.luaml",
                "---\ntype: :kick:\n---\n\
                 luaml.dispatch({ type = luaml.enum(\"tick\"), n = 1 })\n\
                 luaml.dispatch({ type = luaml.enum(\"tick\"), n = 2 })\n",
            )
            .unwrap();
        engine
            .register(
                "b.luaml",
                "---\ntype: :tick:\nn: $count\n---\nresult = (result or 0) + count\n",
            )
            .unwrap();

        engine.dispatch(&event(&[("type", FieldValue::Enum("kick".into()))]));

        let val: i64 = engine.lua().globals().get("result").unwrap();
        assert_eq!(val, 3);
    }

    #[test]
    fn cascade_depth_limit_surfaces_synthetic_outcome() {
        let mut engine = LuamlEngine::new().unwrap();
        engine.set_cascade_config(CascadeConfig {
            max_depth: 3,
            budget: 100,
        });
        // A self-cascading script (same event each hop, but with a counter that
        // makes each hop's signature unique so the cycle detector won't fire
        // first — we want depth, not cycle).
        engine
            .register(
                "a.luaml",
                "---\ntype: :tick:\nhops: $h\n---\n\
                 luaml.dispatch({ type = luaml.enum(\"tick\"), hops = h + 1 })\n",
            )
            .unwrap();

        let outcomes = engine.dispatch(&event(&[
            ("type", FieldValue::Enum("tick".into())),
            ("hops", FieldValue::Number(0)),
        ]));

        let depth_hits: Vec<&ClauseOutcome> = outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.result,
                    Err(ClauseError {
                        kind: ClauseErrKind::CascadeDepth,
                        ..
                    })
                )
            })
            .collect();
        assert_eq!(
            depth_hits.len(),
            1,
            "expected exactly one depth-limit outcome; got {:#?}",
            outcomes
        );
    }

    #[test]
    fn cascade_cycle_detector_flags_self_loop() {
        let mut engine = LuamlEngine::new().unwrap();
        // Emit the exact same event — same signature — triggering cycle detection.
        engine
            .register(
                "a.luaml",
                "---\ntype: :tick:\n---\n\
                 luaml.dispatch({ type = luaml.enum(\"tick\") })\n",
            )
            .unwrap();

        let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("tick".into()))]));

        let cycle_hits: Vec<&ClauseOutcome> = outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.result,
                    Err(ClauseError {
                        kind: ClauseErrKind::CascadeCycle,
                        ..
                    })
                )
            })
            .collect();
        assert_eq!(
            cycle_hits.len(),
            1,
            "expected exactly one cycle outcome; got {:#?}",
            outcomes
        );
    }

    #[test]
    fn cascade_budget_caps_wide_fanout() {
        let mut engine = LuamlEngine::new().unwrap();
        engine.set_cascade_config(CascadeConfig {
            max_depth: 100,
            budget: 3,
        });
        engine
            .register(
                "a.luaml",
                "---\ntype: :kick:\n---\n\
                 for i = 1, 10 do luaml.dispatch({ type = luaml.enum(\"worker\"), n = i }) end\n",
            )
            .unwrap();
        engine
            .register(
                "b.luaml",
                "---\ntype: :worker:\n---\nresult = (result or 0) + 1\n",
            )
            .unwrap();

        let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("kick".into()))]));

        let budget_hits: Vec<&ClauseOutcome> = outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.result,
                    Err(ClauseError {
                        kind: ClauseErrKind::CascadeBudget,
                        ..
                    })
                )
            })
            .collect();
        assert!(
            !budget_hits.is_empty(),
            "expected at least one budget outcome; got {:#?}",
            outcomes
        );

        let val: i64 = engine.lua().globals().get("result").unwrap();
        // With budget 3 we should see at most 3 worker runs.
        assert!(val <= 3, "budget not enforced: got {val} runs");
    }

    #[test]
    fn cascade_skipped_when_body_errors() {
        let mut engine = LuamlEngine::new().unwrap();
        // Body emits, then raises. The emission is enqueued in the Rust buffer,
        // but because the body errored the engine must discard the queue and
        // not cascade. `a`'s outcome is Err(Body); `b` never runs.
        engine
            .register(
                "a.luaml",
                "---\ntype: :kick:\n---\n\
                 luaml.dispatch({ type = luaml.enum(\"downstream\") })\n\
                 error(\"boom\")\n",
            )
            .unwrap();
        engine
            .register(
                "b.luaml",
                "---\ntype: :downstream:\n---\nran_count = (ran_count or 0) + 1\n",
            )
            .unwrap();

        let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("kick".into()))]));

        // Exactly one outcome (a's, Err). b never runs.
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            outcomes[0].result,
            Err(ClauseError {
                kind: ClauseErrKind::Body,
                ..
            })
        ));

        let ran_count: i64 = engine.lua().globals().get("ran_count").unwrap_or(0);
        assert_eq!(ran_count, 0, "b should not have run when a errored");
    }

    #[test]
    fn cascade_sibling_errors_dont_stop_cascades() {
        let mut engine = LuamlEngine::new().unwrap();
        // `a` matches :kick: and enqueues :downstream:.
        engine
            .register(
                "a.luaml",
                "---\ntype: :kick:\n---\n\
                 luaml.dispatch({ type = luaml.enum(\"downstream\") })\n",
            )
            .unwrap();
        // `b` also matches :kick: but errors — should not block the cascade from `a`.
        engine
            .register("b.luaml", "---\ntype: :kick:\n---\nerror(\"b broke\")\n")
            .unwrap();
        engine
            .register("c.luaml", "---\ntype: :downstream:\n---\nran = true\n")
            .unwrap();

        let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("kick".into()))]));

        let ok_count = outcomes.iter().filter(|o| o.result.is_ok()).count();
        let err_count = outcomes.iter().filter(|o| o.result.is_err()).count();
        assert_eq!(
            ok_count, 2,
            "a and c should succeed; got outcomes: {:#?}",
            outcomes
        );
        assert_eq!(
            err_count, 1,
            "b should error; got outcomes: {:#?}",
            outcomes
        );

        let ran: bool = engine.lua().globals().get("ran").unwrap();
        assert!(ran, "downstream c should have run even though b errored");
    }

    #[test]
    fn cascade_literal_and_variable_fields_both_work() {
        let mut engine = LuamlEngine::new().unwrap();
        // `a` emits a table with string values and a luaml.enum wrapper.
        engine
            .register(
                "a.luaml",
                "---\ntype: :kick:\n---\n\
                 local phase = \"ready\"\n\
                 luaml.dispatch({\n\
                   type = luaml.enum(\"lifecycle\"),\n\
                   phase = phase,\n\
                   count = 7,\n\
                 })\n",
            )
            .unwrap();
        // `b` matches on the enum + binds `phase` (string) and `count` (number).
        engine
            .register(
                "b.luaml",
                "---\ntype: :lifecycle:\nphase: $p\ncount: $c\n---\n\
                 result_phase = p\nresult_count = c\n",
            )
            .unwrap();

        let outcomes = engine.dispatch(&event(&[("type", FieldValue::Enum("kick".into()))]));
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| o.result.is_ok()), "{:#?}", outcomes);

        let phase: String = engine.lua().globals().get("result_phase").unwrap();
        assert_eq!(phase, "ready");
        let count: i64 = engine.lua().globals().get("result_count").unwrap();
        assert_eq!(count, 7);
    }

    #[test]
    fn cascade_stats_track_depth_and_count() {
        let mut engine = LuamlEngine::new().unwrap();
        engine
            .register(
                "a.luaml",
                "---\ntype: :k1:\n---\nluaml.dispatch({ type = luaml.enum(\"k2\") })\n",
            )
            .unwrap();
        engine
            .register(
                "b.luaml",
                "---\ntype: :k2:\n---\nluaml.dispatch({ type = luaml.enum(\"k3\") })\n",
            )
            .unwrap();
        engine
            .register("c.luaml", "---\ntype: :k3:\n---\nran = true\n")
            .unwrap();

        engine.dispatch(&event(&[("type", FieldValue::Enum("k1".into()))]));

        let stats = engine.stats();
        assert!(
            stats.cascade_depth_max >= 2,
            "cascade_depth_max should be at least 2, got {}",
            stats.cascade_depth_max
        );
        assert!(
            stats.cascades_emitted >= 2,
            "cascades_emitted should be at least 2, got {}",
            stats.cascades_emitted
        );
    }
}
