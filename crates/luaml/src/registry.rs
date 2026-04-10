use crate::clause::Script;
use crate::error::LuamlError;
use std::path::Path;

/// Stores registered scripts and provides matching queries.
pub struct ScriptRegistry {
    scripts: Vec<Script>,
}

impl ScriptRegistry {
    pub fn new() -> Self {
        Self { scripts: Vec::new() }
    }

    pub fn register(&mut self, script: Script) {
        self.scripts.push(script);
    }

    pub fn all(&self) -> &[Script] {
        &self.scripts
    }

    pub fn register_file(&mut self, _path: &Path) -> Result<(), LuamlError> {
        // TODO: implement
        Ok(())
    }
}
