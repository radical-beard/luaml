//! luaml stdlib infrastructure.
//!
//! The stdlib is a pluggable set of Rust-backed Lua modules installed into
//! the engine's Lua globals at construction. Each module implements
//! [`LuamlStdlibModule`], provides a bare namespace name (e.g. `"http"`,
//! `"json"`, `"fs"`), and returns the table to be assigned to that global.
//!
//! This module contains the wiring — the trait, the `install_all` entry
//! point called from `LuamlEngine::with_lua`, the [`promise::Promise`]
//! userdata used by async stdlib ops, and the list of compiled-in modules.

use mlua::Lua;
use tokio::runtime::Handle;

pub mod codec;
pub mod console;
pub mod crypto;
pub mod env;
pub mod fs;
pub mod http;
pub mod json;
pub mod math;
pub mod path;
pub mod process;
pub mod promise;
pub mod regex;
pub mod rpc;
pub mod tcp;
pub mod thread;
pub mod time;
pub mod udp;
pub mod url;
pub mod vec;

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
/// engine state has been prepared.
pub(crate) fn install_all(lua: &Lua, rt: &Handle) -> mlua::Result<()> {
    let modules = collect_modules();
    let globals = lua.globals();
    for module in modules {
        let table = module.install(lua, rt)?;
        globals.set(module.namespace(), table)?;
    }
    Ok(())
}

/// Aggregate every compiled-in stdlib module. `math` installs last because it
/// extends (not replaces) Lua's built-in `math` global — its `install` reads
/// the existing `math` table and augments it with additional functions.
fn collect_modules() -> Vec<Box<dyn LuamlStdlibModule>> {
    vec![
        Box::new(codec::CodecModule),
        Box::new(console::ConsoleModule),
        Box::new(crypto::CryptoModule),
        Box::new(env::EnvModule),
        Box::new(fs::FsModule),
        Box::new(http::HttpModule),
        Box::new(json::JsonModule),
        Box::new(path::PathModule),
        Box::new(process::ProcessModule),
        Box::new(regex::RegexModule),
        Box::new(rpc::RpcModule),
        Box::new(tcp::TcpModule),
        Box::new(thread::ThreadModule),
        Box::new(time::TimeModule),
        Box::new(udp::UdpModule),
        Box::new(url::UrlModule),
        Box::new(vec::VecModule),
        Box::new(math::MathModule),
    ]
}

#[cfg(test)]
mod tests {
    use crate::LuamlEngine;

    #[test]
    fn engine_constructs_with_stdlib_infra() {
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
    fn all_stdlib_namespaces_installed() {
        let engine = LuamlEngine::new().expect("engine should build");
        let globals = engine.lua().globals();
        for name in [
            "codec", "console", "crypto", "env", "fs", "http", "json", "path",
            "process", "regex", "rpc", "tcp", "thread", "time", "udp", "url",
            "vec",
        ] {
            let val: mlua::Value = globals.get(name).unwrap_or(mlua::Value::Nil);
            assert!(
                !matches!(val, mlua::Value::Nil),
                "stdlib namespace {name:?} missing from globals"
            );
        }
    }
}
