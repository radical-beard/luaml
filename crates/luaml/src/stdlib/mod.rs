//! luaml stdlib infrastructure.
//!
//! The stdlib is a pluggable set of Rust-backed Lua modules installed into
//! the engine's Lua globals at construction. Each module implements
//! [`LuamlStdlibModule`], provides a bare namespace name (e.g. `"http"`,
//! `"json"`, `"fs"`), and returns the table to be assigned to that global.
//!
//! This module only contains the wiring — the trait, the `install_all` entry
//! point called from `LuamlEngine::with_lua`, and the [`promise::Promise`]
//! userdata used by async stdlib ops. Concrete modules are added behind
//! `stdlib-<name>` feature flags as they land.

use mlua::Lua;
use tokio::runtime::Handle;

pub mod promise;

/// Trait implemented by each stdlib module. Each module owns a single bare
/// namespace in the Lua globals and produces the table installed under that
/// name. Modules are stateless from the engine's perspective: they may close
/// over a [`Handle`] for spawning async work, but they do not persist state
/// across calls (state lives on userdata returned from module functions).
pub trait LuamlStdlibModule: Send + Sync {
    /// Bare namespace installed as a Lua global (e.g. `"http"`).
    fn namespace(&self) -> &'static str;

    /// Build the namespace table. Called once at engine construction. The
    /// [`Handle`] is a clone of the engine's tokio runtime handle — modules
    /// that spawn async work use it; sync modules may ignore it.
    fn install(&self, lua: &Lua, rt: &Handle) -> mlua::Result<mlua::Table>;
}

/// Install every compiled-in stdlib module into the Lua globals as bare
/// namespaces. Called by [`crate::LuamlEngine::with_lua`] after the rest of
/// engine state has been prepared. No module is installed unless its
/// `stdlib-<name>` feature is enabled.
pub(crate) fn install_all(lua: &Lua, rt: &Handle) -> mlua::Result<()> {
    let modules = collect_modules();
    let globals = lua.globals();
    for module in modules {
        let table = module.install(lua, rt)?;
        globals.set(module.namespace(), table)?;
    }
    Ok(())
}

/// Aggregate every compiled-in stdlib module. Modules register themselves by
/// being appended here behind their respective `stdlib-<name>` feature flags;
/// the list is empty by default until the first module lands.
fn collect_modules() -> Vec<Box<dyn LuamlStdlibModule>> {
    // Intentionally empty: L9–L26 modules will append to this list under
    // their own `stdlib-<name>` feature flags. Keep the explicit type so the
    // empty case type-checks even when every feature is disabled.
    let modules: Vec<Box<dyn LuamlStdlibModule>> = Vec::new();
    modules
}

#[cfg(test)]
mod tests {
    use crate::LuamlEngine;

    #[test]
    fn engine_constructs_with_stdlib_infra() {
        // stdlib has no modules yet, so no globals are injected. What we're
        // testing is that construction succeeds with the tokio runtime
        // plumbed through — a broken wiring would panic or error here.
        let engine = LuamlEngine::new().expect("engine should build");
        let _ = engine.rt_handle();
    }

    #[test]
    fn engine_rt_handle_can_spawn() {
        let engine = LuamlEngine::new().expect("engine should build");
        let handle = engine.rt_handle();
        let result = handle.block_on(async { 2 + 2 });
        assert_eq!(result, 4);
    }

    #[test]
    fn install_all_is_a_noop_until_modules_land() {
        // Sanity: no stdlib modules currently register, so no globals beyond
        // what the Lua VM ships with are installed by the engine. The
        // explicit names below are the bare namespaces L9+ modules claim; if
        // any accidentally ship enabled-by-default, this test fails first.
        let engine = LuamlEngine::new().expect("engine should build");
        let globals = engine.lua().globals();
        for name in ["http", "fs", "json", "crypto", "process", "net"] {
            let val: mlua::Value = globals.get(name).unwrap_or(mlua::Value::Nil);
            assert!(
                matches!(val, mlua::Value::Nil),
                "unexpected global {name:?} installed without feature flag"
            );
        }
    }
}
