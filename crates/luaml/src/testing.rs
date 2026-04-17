//! Helpers for consumer tests. Available under `#[cfg(test)]` within luaml
//! and behind the `testing` feature flag for downstream crates.
//!
//! The helpers here exist so that a consumer (for example, crucible) can
//! build a configured `LuamlEngine` in one expression and construct a
//! `FieldMap` without naming every `FieldValue` variant by hand. They exist
//! solely for tests and dev tooling — production callers use the normal
//! [`crate::LuamlEngine`] surface.

use std::path::PathBuf;
use std::sync::Arc;

use crate::api::{ApiBindingSpec, ApiHandler};
use crate::error::LuamlError;
use crate::pattern::Pattern;
use crate::types::{FieldMap, FieldValue};
use crate::LuamlEngine;

/// Fluent builder for a pre-configured [`LuamlEngine`]. Reduces boilerplate in
/// consumer tests that used to call `LuamlEngine::new`, then `register`, then
/// `register_api` in sequence.
pub struct EngineBuilder {
    scripts: Vec<(PathBuf, String)>,
    apis: Vec<ApiBindingSpec>,
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineBuilder {
    pub fn new() -> Self {
        Self {
            scripts: Vec::new(),
            apis: Vec::new(),
        }
    }

    /// Queue a script to be registered when the engine is built.
    pub fn with_script(
        mut self,
        path: impl Into<PathBuf>,
        text: impl Into<String>,
    ) -> Self {
        self.scripts.push((path.into(), text.into()));
        self
    }

    /// Queue an API binding with an empty pattern (matches every clause).
    pub fn with_api(
        self,
        namespace: impl Into<String>,
        handler: Arc<dyn ApiHandler>,
    ) -> Self {
        self.with_api_pattern(namespace, Vec::new(), handler)
    }

    /// Queue an API binding with a pattern (matches only clauses whose policy
    /// fields satisfy the pattern).
    pub fn with_api_pattern(
        mut self,
        namespace: impl Into<String>,
        pattern: Vec<(String, Pattern)>,
        handler: Arc<dyn ApiHandler>,
    ) -> Self {
        self.apis.push(ApiBindingSpec {
            namespace: namespace.into(),
            pattern,
            handler,
        });
        self
    }

    /// Consume the builder and produce a ready-to-dispatch engine. Script
    /// registration errors surface here; API registration cannot fail.
    pub fn build(self) -> Result<LuamlEngine, LuamlError> {
        let mut engine = LuamlEngine::new()?;
        for (path, text) in self.scripts {
            engine.register(path, &text)?;
        }
        for spec in self.apis {
            engine.register_api(spec);
        }
        Ok(engine)
    }
}

/// Build a [`FieldMap`] from `(&str, FieldValue)` pairs. Saves consumers from
/// writing `.into()` on every key.
pub fn event<I, K>(pairs: I) -> FieldMap
where
    I: IntoIterator<Item = (K, FieldValue)>,
    K: Into<String>,
{
    pairs.into_iter().map(|(k, v)| (k.into(), v)).collect()
}

/// `FieldValue::Enum`. Enums carry semantic categories (`type: :input:`,
/// `surface: :tui:`) and are distinct from strings under luaml's type-aware
/// equality.
pub fn enum_value(s: impl Into<String>) -> FieldValue {
    FieldValue::Enum(s.into())
}

/// `FieldValue::String`. Use for free-form text.
pub fn str_value(s: impl Into<String>) -> FieldValue {
    FieldValue::String(s.into())
}
