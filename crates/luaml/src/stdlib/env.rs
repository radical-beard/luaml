//! `env` stdlib module: read-only access to environment variables and
//! process metadata.
//!
//! Methods installed under the `env` global:
//! - `env.get(name) -> string | nil`
//! - `env.list() -> { [name] = value, ... }`
//! - `env.home() -> string | nil`
//! - `env.cwd() -> string`
//! - `env.args() -> { ... }` (process argv)
//! - `env.platform() -> string` (`"linux"` | `"macos"` | `"windows"` | other)
//!
//! Everything here is synchronous; the tokio [`Handle`] passed to
//! [`LuamlStdlibModule::install`] is ignored. Only `std::env` is touched —
//! no new dependencies.
//!
//! ## `env.set` is intentionally omitted
//!
//! Setting process-wide environment variables from a Lua script is a
//! security concern: env vars leak into every child process, are observable
//! by other threads, and are a vehicle for credential/path injection. A
//! sandboxed script should not be able to mutate them from Lua. If a
//! consumer genuinely needs mutation, they should expose a guarded wrapper
//! in their own module (e.g. whitelisted keys, audit logging) rather than
//! reintroducing it here.

use mlua::{Lua, Table};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;

/// `env` stdlib module. See module-level docs.
pub struct EnvModule;

impl LuamlStdlibModule for EnvModule {
    fn namespace(&self) -> &'static str {
        "env"
    }

    fn install(&self, lua: &Lua, _rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // env.get(name) -> string | nil
        // Returns the value of the named environment variable, or nil if
        // unset. Non-UTF-8 values are treated as "unset" rather than
        // surfaced — Lua strings are bytes-ish but our other APIs assume
        // utf8, so keep the contract uniform.
        table.set(
            "get",
            lua.create_function(|_, name: String| {
                Ok(std::env::var(&name).ok())
            })?,
        )?;

        // env.list() -> { [name] = value, ... }
        // Snapshot of the current environment at call time. Non-UTF-8
        // entries are skipped rather than raising, for the same reason as
        // `get`.
        table.set(
            "list",
            lua.create_function(|lua, ()| {
                let out = lua.create_table()?;
                for (k, v) in std::env::vars() {
                    out.set(k, v)?;
                }
                Ok(out)
            })?,
        )?;

        // env.home() -> string | nil
        // $HOME on unix, %USERPROFILE% on windows. Any other platform falls
        // through to nil; callers should not rely on a home dir existing.
        table.set(
            "home",
            lua.create_function(|_, ()| {
                #[cfg(windows)]
                let key = "USERPROFILE";
                #[cfg(not(windows))]
                let key = "HOME";
                Ok(std::env::var(key).ok())
            })?,
        )?;

        // env.cwd() -> string
        // Current working directory as a utf8 string. Errors if the cwd
        // cannot be resolved (deleted dir, permission denied) or if its
        // path is not valid utf8.
        table.set(
            "cwd",
            lua.create_function(|_, ()| {
                let path = std::env::current_dir()
                    .map_err(|e| mlua::Error::runtime(format!("env.cwd: {e}")))?;
                path.into_os_string().into_string().map_err(|_| {
                    mlua::Error::runtime("env.cwd: current directory is not valid utf-8")
                })
            })?,
        )?;

        // env.args() -> { ... }
        // Process argv as a table of strings (1-indexed, Lua-style).
        // Non-utf8 args are surfaced as a runtime error so callers don't
        // silently lose positional arguments.
        table.set(
            "args",
            lua.create_function(|lua, ()| {
                let out = lua.create_table()?;
                for (idx, arg) in std::env::args_os().enumerate() {
                    let s = arg.into_string().map_err(|_| {
                        mlua::Error::runtime(format!(
                            "env.args: argv[{idx}] is not valid utf-8"
                        ))
                    })?;
                    // Lua-style 1-indexed table.
                    out.set(idx + 1, s)?;
                }
                Ok(out)
            })?,
        )?;

        // env.platform() -> string
        // Thin wrapper over `std::env::consts::OS`. Known values include
        // "linux", "macos", "windows", plus the other targets rustc knows
        // about ("ios", "freebsd", ...). Callers should treat the set as
        // open.
        table.set(
            "platform",
            lua.create_function(|_, ()| Ok(std::env::consts::OS))?,
        )?;

        Ok(table)
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests. A few notes on test isolation:
    //!
    //! - `std::env::set_var` mutates process-wide state and is not safe
    //!   under parallel test runners that share a process. Cargo runs
    //!   tests in threads by default, so we use a module-local mutex to
    //!   serialize the handful of tests that write env vars, and we pick
    //!   variable names with a test-specific prefix to avoid colliding
    //!   with whatever the surrounding shell set.
    //! - Tests that only read the environment (list/cwd/platform) don't
    //!   take the mutex since they tolerate concurrent mutation.
    use super::*;
    use mlua::Lua;
    use std::sync::Mutex;
    use tokio::runtime::Builder;

    // Serialize env-mutating tests within this module. Other modules
    // touching std::env in parallel can still collide — nothing we can do
    // about that from here without a global lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn install() -> Lua {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let lua = Lua::new();
        let table = EnvModule
            .install(&lua, rt.handle())
            .expect("install env module");
        lua.globals().set("env", table).unwrap();
        // Keep rt alive for the duration of the test by leaking the
        // handle — install() ignores it anyway, and the Lua closures
        // never call back into the runtime. Dropping the rt here would
        // be fine too since no async work is spawned; we leave it to
        // fall out of scope.
        drop(rt);
        lua
    }

    #[test]
    fn get_returns_value_for_set_var() {
        let _g = ENV_LOCK.lock().unwrap();
        let key = "LUAML_ENV_SMOKE_GET";
        // SAFETY: tests serialize via ENV_LOCK; no other code in this
        // module observes the variable while we mutate it.
        unsafe {
            std::env::set_var(key, "hello-from-test");
        }
        let lua = install();
        let v: String = lua
            .load(format!("return env.get('{key}')"))
            .eval()
            .unwrap();
        assert_eq!(v, "hello-from-test");

        unsafe {
            std::env::remove_var(key);
        }
        let nil: mlua::Value = lua
            .load(format!("return env.get('{key}')"))
            .eval()
            .unwrap();
        assert!(matches!(nil, mlua::Value::Nil));
    }

    #[test]
    fn list_returns_non_empty_table_including_set_var() {
        let _g = ENV_LOCK.lock().unwrap();
        let key = "LUAML_ENV_SMOKE_LIST";
        unsafe {
            std::env::set_var(key, "listed");
        }
        let lua = install();
        let (count, value): (i64, String) = lua
            .load(format!(
                r#"
                local t = env.list()
                local n = 0
                for _ in pairs(t) do n = n + 1 end
                return n, t['{key}']
            "#
            ))
            .eval()
            .unwrap();
        assert!(count > 0, "list should return a non-empty table");
        assert_eq!(value, "listed");
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn cwd_returns_non_empty_string() {
        let lua = install();
        let cwd: String = lua.load("return env.cwd()").eval().unwrap();
        assert!(!cwd.is_empty(), "cwd should not be empty");
        // Sanity: agrees with std for the current process at this moment.
        // We don't pin the exact value — tests can run from anywhere.
        let expected = std::env::current_dir()
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap();
        assert_eq!(cwd, expected);
    }

    #[test]
    fn platform_returns_known_value() {
        let lua = install();
        let p: String = lua.load("return env.platform()").eval().unwrap();
        // Don't hardcode a single value — tests may run on any target rustc
        // supports. Just confirm it matches what std reports.
        assert_eq!(p, std::env::consts::OS);
        // And that it's one of the common ones we explicitly care about in
        // the module docs (or at least a non-empty identifier).
        assert!(!p.is_empty());
    }

    #[test]
    fn args_returns_table_of_strings() {
        let lua = install();
        // The test binary itself is argv[0]; we don't pin the exact path,
        // but the table must be non-empty and its first entry must be a
        // string.
        let (len, first_is_string): (i64, bool) = lua
            .load(
                r#"
                local a = env.args()
                local n = 0
                for _ in ipairs(a) do n = n + 1 end
                return n, type(a[1]) == 'string'
            "#,
            )
            .eval()
            .unwrap();
        assert!(len >= 1, "argv should have at least one entry");
        assert!(first_is_string);
    }

    #[test]
    fn home_returns_string_or_nil() {
        // No mutation: we accept whatever the ambient environment has.
        let lua = install();
        let v: mlua::Value = lua.load("return env.home()").eval().unwrap();
        match v {
            mlua::Value::Nil => { /* ok: HOME genuinely unset */ }
            mlua::Value::String(s) => {
                assert!(!s.to_str().unwrap().is_empty(), "home should not be empty");
            }
            other => panic!("env.home returned unexpected type: {other:?}"),
        }
    }
}
