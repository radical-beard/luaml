//! `Promise` userdata: an mlua `UserData` wrapping a tokio `JoinHandle` whose
//! output is a `Result<FieldValue, String>`. Async stdlib ops (e.g.
//! `http.get_async(...)`) return a `Promise` that scripts drive with
//! `:await()`, `:try_await()`, or `:poll()`.
//!
//! The promise owns its join handle in a [`RefCell<Option<JoinHandle>>`] so
//! `:await()` can move it out (ending in exhaustion on subsequent calls) while
//! `:poll()` and `:try_await()` may observe the handle non-destructively.
//!
//! A handle to the tokio runtime is carried alongside so `:await()` can block
//! on the associated runtime regardless of which thread the Lua clause runs on.
//! This matches the engine's own construction: the engine owns the `Runtime`,
//! modules and promises hold `Handle`s.
//!
//! The task's output type is `Result<FieldValue, String>` rather than
//! `mlua::Result<FieldValue>` because `mlua::Error` is not `Send + Sync`
//! by default, and `Runtime::spawn` requires `Send + 'static`. Stdlib modules
//! collapse their internal errors into a string at the task boundary; the
//! string is converted to `mlua::Error::runtime` when surfaced into Lua.
use std::cell::RefCell;

use mlua::{IntoLua, UserData, UserDataMethods, Value as LuaValue};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::executor::field_value_to_lua;
use crate::types::FieldValue;

/// Output type of the task wrapped by a [`Promise`]. Stdlib modules build
/// their async wrappers to resolve to this type so error messages can cross
/// thread boundaries without invoking `mlua::Error`'s non-`Send` internals.
pub type PromiseResult = Result<FieldValue, String>;

/// A promise produced by an async stdlib operation. See the module docs.
///
/// Construct with [`Promise::new`]; expose by returning it from an
/// `mlua` function (it implements `UserData` so it converts to `LuaValue`
/// automatically).
pub struct Promise {
    inner: RefCell<Option<JoinHandle<PromiseResult>>>,
    rt: Handle,
}

impl Promise {
    /// Wrap a spawned task. The join handle's output is a `Result<FieldValue,
    /// String>`; a successful value becomes a Lua value via [`field_value_to_lua`],
    /// while the `String` error is surfaced as an `mlua::Error::runtime`.
    pub fn new(handle: JoinHandle<PromiseResult>, rt: Handle) -> Self {
        Self {
            inner: RefCell::new(Some(handle)),
            rt,
        }
    }
}

impl UserData for Promise {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // p:await() — blocks the current Lua thread on the runtime until the
        // spawned task completes. On success returns the converted FieldValue
        // as a Lua value; on task error raises `mlua::Error::runtime(msg)`.
        // After the first await the promise is exhausted and further awaits
        // error.
        methods.add_method("await", |lua, this, ()| {
            let handle = this
                .inner
                .borrow_mut()
                .take()
                .ok_or_else(|| mlua::Error::runtime("promise already awaited or resolved"))?;
            let joined = this
                .rt
                .block_on(handle)
                .map_err(|e| mlua::Error::runtime(format!("promise join error: {e}")))?;
            let value = joined.map_err(mlua::Error::runtime)?;
            field_value_to_lua(lua, &value)
        });

        // p:try_await() — non-blocking check. Returns two values:
        //   (true,  <value>) if the task completed successfully,
        //   (false, <err>)   if the task errored or was cancelled,
        //   (nil,   nil)     if the task is still pending (no state change).
        // On successful completion the promise is exhausted; on error it is
        // also exhausted (the error is surfaced once, not re-raised). While
        // pending, the join handle is left in place for a later retry.
        methods.add_method("try_await", |lua, this, ()| {
            let mut slot = this.inner.borrow_mut();
            let Some(handle) = slot.as_ref() else {
                return Err(mlua::Error::runtime("promise already awaited or resolved"));
            };
            if !handle.is_finished() {
                return Ok((LuaValue::Nil, LuaValue::Nil));
            }
            let handle = slot.take().expect("handle present after is_finished");
            drop(slot);
            match this.rt.block_on(handle) {
                Ok(Ok(value)) => {
                    let v = field_value_to_lua(lua, &value)?;
                    Ok((true.into_lua(lua)?, v))
                }
                Ok(Err(err)) => {
                    let msg = err.into_lua(lua)?;
                    Ok((false.into_lua(lua)?, msg))
                }
                Err(join_err) => {
                    let msg = format!("promise join error: {join_err}").into_lua(lua)?;
                    Ok((false.into_lua(lua)?, msg))
                }
            }
        });

        // p:poll() — observe the join handle without consuming it.
        // Returns "pending" (still running), "ready" (done but not yet
        // awaited), or "resolved" (already awaited / exhausted).
        // Note: "errored" vs successful ready is not distinguished here —
        // that requires consuming the handle. Use try_await() to surface the
        // error/value while transitioning out of "ready".
        methods.add_method("poll", |_lua, this, ()| {
            let slot = this.inner.borrow();
            let state = match slot.as_ref() {
                None => "resolved",
                Some(h) if h.is_finished() => "ready",
                Some(_) => "pending",
            };
            Ok(state)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    fn rt() -> tokio::runtime::Runtime {
        Builder::new_multi_thread().enable_all().build().unwrap()
    }

    fn ok_value(v: FieldValue) -> PromiseResult {
        Ok(v)
    }

    fn err_value(msg: &'static str) -> PromiseResult {
        Err(msg.to_string())
    }

    #[test]
    fn await_returns_value() {
        let rt = rt();
        let lua = Lua::new();
        let handle = rt.spawn(async { ok_value(FieldValue::Number(42)) });
        let promise = Promise::new(handle, rt.handle().clone());
        lua.globals().set("p", promise).unwrap();
        let v: i64 = lua.load("return p:await()").eval().unwrap();
        assert_eq!(v, 42);
    }

    #[test]
    fn await_twice_errors() {
        let rt = rt();
        let lua = Lua::new();
        let handle = rt.spawn(async { ok_value(FieldValue::String("hi".into())) });
        let promise = Promise::new(handle, rt.handle().clone());
        lua.globals().set("p", promise).unwrap();
        let _: String = lua.load("return p:await()").eval().unwrap();
        let second = lua.load("return p:await()").eval::<String>();
        assert!(second.is_err(), "second await must error");
    }

    #[test]
    fn await_surfaces_task_error_as_runtime_error() {
        let rt = rt();
        let lua = Lua::new();
        let handle = rt.spawn(async { err_value("kaboom") });
        let promise = Promise::new(handle, rt.handle().clone());
        lua.globals().set("p", promise).unwrap();
        let res = lua.load("return p:await()").eval::<LuaValue>();
        let err = res.unwrap_err().to_string();
        assert!(err.contains("kaboom"), "err should carry msg: {err}");
    }

    #[test]
    fn poll_transitions_from_pending_to_ready_to_resolved() {
        let rt = rt();
        let lua = Lua::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = rt.spawn(async move {
            let _ = rx.await;
            ok_value(FieldValue::Bool(true))
        });
        let promise = Promise::new(handle, rt.handle().clone());
        lua.globals().set("p", promise).unwrap();

        let state: String = lua.load("return p:poll()").eval().unwrap();
        assert_eq!(state, "pending");

        // Release the task and give the runtime a beat to settle.
        tx.send(()).unwrap();
        // Spin briefly until the task is finished. In practice this returns
        // immediately on a multi-thread runtime once the send resolves.
        for _ in 0..100 {
            let s: String = lua.load("return p:poll()").eval().unwrap();
            if s == "ready" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let state: String = lua.load("return p:poll()").eval().unwrap();
        assert_eq!(state, "ready");

        let _: bool = lua.load("return p:await()").eval().unwrap();
        let state: String = lua.load("return p:poll()").eval().unwrap();
        assert_eq!(state, "resolved");
    }

    #[test]
    fn try_await_pending_returns_nils() {
        let rt = rt();
        let lua = Lua::new();
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = rt.spawn(async move {
            let _ = rx.await;
            ok_value(FieldValue::Null)
        });
        let promise = Promise::new(handle, rt.handle().clone());
        lua.globals().set("p", promise).unwrap();
        let (ok, err): (LuaValue, LuaValue) = lua.load("return p:try_await()").eval().unwrap();
        assert!(matches!(ok, LuaValue::Nil));
        assert!(matches!(err, LuaValue::Nil));
    }

    #[test]
    fn try_await_ready_success_returns_true_value() {
        let rt = rt();
        let lua = Lua::new();
        let handle = rt.spawn(async { ok_value(FieldValue::Number(7)) });
        let promise = Promise::new(handle, rt.handle().clone());
        lua.globals().set("p", promise).unwrap();

        // Spin until ready.
        for _ in 0..100 {
            let s: String = lua.load("return p:poll()").eval().unwrap();
            if s == "ready" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let (ok, value): (bool, i64) = lua.load("return p:try_await()").eval().unwrap();
        assert!(ok);
        assert_eq!(value, 7);
    }

    #[test]
    fn try_await_ready_error_returns_false_message() {
        let rt = rt();
        let lua = Lua::new();
        let handle = rt.spawn(async { err_value("boom") });
        let promise = Promise::new(handle, rt.handle().clone());
        lua.globals().set("p", promise).unwrap();

        for _ in 0..100 {
            let s: String = lua.load("return p:poll()").eval().unwrap();
            if s == "ready" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let (ok, msg): (bool, String) = lua.load("return p:try_await()").eval().unwrap();
        assert!(!ok);
        assert!(msg.contains("boom"), "message should carry error: {msg}");
    }
}
