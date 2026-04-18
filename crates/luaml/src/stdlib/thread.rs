//! `thread` stdlib module: cooperative scheduling, sleep, and promise
//! coordination.
//!
//! Installed under the bare `thread` global. Methods:
//! - `thread.sleep(ms)` — blocking sleep in milliseconds. Drives
//!   [`tokio::time::sleep`] on the engine's runtime via [`Handle::block_on`]
//!   so other async work (timers, I/O) can make progress while the Lua
//!   thread is parked. Negative / NaN durations clamp to zero.
//! - `thread.sleep_async(ms) -> Promise<nil>` — spawns the sleep on the
//!   runtime and hands back a [`Promise`] that resolves to nil when the
//!   timer fires.
//! - `thread.yield()` — cooperative yield. Implemented by blocking on a
//!   single [`tokio::task::yield_now`] so any queued runtime work gets a
//!   chance to run before the Lua clause resumes.
//! - `thread.wait_all({p1, p2, ...}) -> {r1, r2, ...}` — awaits each
//!   [`Promise`] in the list in order and returns a new sequence of their
//!   resolved values. The first erroring promise errors the whole call.
//! - `thread.wait_any({p1, p2, ...}) -> (index, result)` — polls the
//!   promises in a light loop and returns the 1-based index and value of
//!   whichever completes first. A 1 ms sleep between sweeps keeps the CPU
//!   quiet without introducing observable latency for sub-millisecond
//!   tasks (first-pass check happens before the first sleep).
//!
//! TODO: `thread.spawn(function) -> Promise` is deliberately deferred.
//! Spawning an arbitrary Lua function onto the tokio runtime requires
//! either the `send` feature of `mlua` (to make `mlua::Lua` `Send`) or a
//! pool of worker Lua VMs, because `Runtime::spawn` demands `Send +
//! 'static` and Lua state is `!Send` by default. Neither approach is
//! cheap; until there's a concrete need, scripts that want concurrent
//! work should use the async variants of IO modules (e.g.
//! `http.get_async`) and coordinate via `thread.wait_all` /
//! `thread.wait_any`.

use mlua::{AnyUserData, Lua, ObjectLike, Table, Value};
use tokio::runtime::Handle;
use tokio::time::Duration;

use super::LuamlStdlibModule;
use crate::stdlib::promise::{Promise, PromiseResult};
use crate::types::FieldValue;

/// `thread` stdlib module. See module-level docs.
pub struct ThreadModule;

impl LuamlStdlibModule for ThreadModule {
    fn namespace(&self) -> &'static str {
        "thread"
    }

    fn install(&self, lua: &Lua, rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // thread.sleep(ms)
        // Blocking sleep in milliseconds routed through the tokio runtime.
        // Takes a millisecond argument which is the natural unit for
        // coordination code. Non-finite or negative values clamp to zero
        // so the Lua side never panics. The sleep future is awaited
        // inside an async block so it is polled inside the runtime's
        // entered context — constructing `tokio::time::sleep` outside
        // `block_on` panics with "no reactor running" on Handle (the
        // reactor is only set as current while block_on is driving).
        {
            let rt = rt.clone();
            table.set(
                "sleep",
                lua.create_function(move |_, ms: f64| {
                    let dur = duration_from_ms(ms);
                    rt.block_on(async move { tokio::time::sleep(dur).await });
                    Ok(())
                })?,
            )?;
        }

        // thread.sleep_async(ms) -> Promise
        // Spawns the sleep on the runtime and returns a Promise that
        // resolves to nil when the timer fires. Non-finite / negative
        // inputs clamp to zero (same as `thread.sleep`).
        {
            let rt = rt.clone();
            table.set(
                "sleep_async",
                lua.create_function(move |_, ms: f64| {
                    let dur = duration_from_ms(ms);
                    let join: tokio::task::JoinHandle<PromiseResult> = rt.spawn(async move {
                        tokio::time::sleep(dur).await;
                        Ok(FieldValue::Null)
                    });
                    Ok(Promise::new(join, rt.clone()))
                })?,
            )?;
        }

        // thread.yield()
        // Cooperative yield. Drives a single `tokio::task::yield_now` on
        // the engine's runtime so queued async work (e.g. awaiting timers
        // or I/O) can make progress before this clause resumes. Kept
        // blocking and trivial — scripts that need real concurrency pair
        // this with `sleep_async` + `wait_*`.
        {
            let rt = rt.clone();
            table.set(
                "yield",
                lua.create_function(move |_, ()| {
                    rt.block_on(async { tokio::task::yield_now().await });
                    Ok(())
                })?,
            )?;
        }

        // thread.wait_all({promises...}) -> {results...}
        // Awaits the promises in the order they appear in the input list
        // and returns a new sequence of their resolved values. The first
        // promise whose `:await()` errors aborts the whole call and
        // surfaces that error — later promises are left in whatever state
        // they happen to be in (pending, ready, or resolved) and are not
        // cancelled. Each entry must be Promise userdata; anything else
        // errors with a type mismatch.
        table.set(
            "wait_all",
            lua.create_function(|lua, promises: Table| {
                let out = lua.create_table()?;
                for (i, value) in promises.sequence_values::<Value>().enumerate() {
                    let value = value?;
                    let ud = promise_userdata(&value, "thread.wait_all", i + 1)?;
                    let resolved: Value = ud.call_method("await", ())?;
                    out.set(i + 1, resolved)?;
                }
                Ok(out)
            })?,
        )?;

        // thread.wait_any({promises...}) -> (index, result)
        // Polls the promises in a light loop, returning the 1-based index
        // and resolved value of whichever completes first. Uses `:poll()`
        // to peek at readiness (non-consuming) and `:try_await()` to
        // consume the ready one. Between sweeps we block_on a 1 ms sleep
        // to keep the CPU quiet — small enough to be invisible next to
        // any real async work, large enough to avoid spinning. Empty
        // input errors; any entry that is not Promise userdata errors
        // immediately. Promises already in the "resolved" state (already
        // awaited elsewhere) are skipped on readiness checks but count
        // toward the input list's length.
        {
            let rt = rt.clone();
            table.set(
                "wait_any",
                lua.create_function(move |lua, promises: Table| {
                    // Snapshot once so `sequence_values` iteration order is
                    // stable across polling sweeps and so type errors
                    // surface before we start sleeping.
                    let mut entries: Vec<AnyUserData> = Vec::new();
                    for (i, value) in promises.sequence_values::<Value>().enumerate() {
                        let value = value?;
                        let ud = promise_userdata(&value, "thread.wait_any", i + 1)?;
                        entries.push(ud);
                    }
                    if entries.is_empty() {
                        return Err(mlua::Error::runtime(
                            "thread.wait_any: list of promises is empty",
                        ));
                    }

                    loop {
                        for (i, ud) in entries.iter().enumerate() {
                            let state: String = ud.call_method("poll", ())?;
                            if state == "ready" {
                                // try_await returns (ok, value_or_msg).
                                // Mirror Promise::await's error semantics:
                                // task errors surface as runtime errors.
                                let (ok, payload): (Value, Value) =
                                    ud.call_method("try_await", ())?;
                                match ok {
                                    Value::Boolean(true) => {
                                        let out = lua.create_table()?;
                                        out.set(1, (i + 1) as i64)?;
                                        out.set(2, payload)?;
                                        return Ok(out);
                                    }
                                    Value::Boolean(false) => {
                                        let msg = match payload {
                                            Value::String(s) => s.to_str()?.to_owned(),
                                            other => format!("{other:?}"),
                                        };
                                        return Err(mlua::Error::runtime(msg));
                                    }
                                    _ => {
                                        // Race: poll said "ready" but
                                        // try_await saw pending/resolved.
                                        // Treat as still pending and loop.
                                    }
                                }
                            }
                        }
                        rt.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
                    }
                })?,
            )?;
        }

        Ok(table)
    }
}

/// Coerce a Lua value at index `i` of a promise list into the underlying
/// [`AnyUserData`], producing a clear runtime error when the caller hands
/// us the wrong type. We don't assert the concrete type is [`Promise`]
/// here — `call_method` will surface a readable `attempt to call a nil
/// value` error if the userdata lacks the expected method. Asserting the
/// type would force an unnecessary Rust-side borrow just to look at the
/// type id.
fn promise_userdata(value: &Value, who: &str, i: usize) -> mlua::Result<AnyUserData> {
    match value {
        Value::UserData(ud) => Ok(ud.clone()),
        other => Err(mlua::Error::runtime(format!(
            "{who}: entry {i} is not a promise (got {})",
            other.type_name()
        ))),
    }
}

/// Clamp a millisecond sleep argument to a non-negative [`Duration`].
/// Negative, NaN, and extremely large values all degrade gracefully
/// rather than panicking inside [`Duration::from_millis`] (which takes a
/// `u64` and would otherwise wrap or overflow on cast).
fn duration_from_ms(ms: f64) -> Duration {
    if !ms.is_finite() || ms <= 0.0 {
        return Duration::ZERO;
    }
    // Duration::from_millis takes u64; clamp so far-future values become
    // Duration::MAX rather than panicking on an out-of-range cast.
    let max_ms = u64::MAX as f64;
    if ms >= max_ms {
        return Duration::MAX;
    }
    Duration::from_millis(ms as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use std::time::Instant;
    use tokio::runtime::Builder;

    fn rt() -> tokio::runtime::Runtime {
        Builder::new_multi_thread().enable_all().build().unwrap()
    }

    fn install_thread(lua: &Lua, rt: &Handle) {
        let table = ThreadModule.install(lua, rt).expect("install thread");
        lua.globals().set("thread", table).unwrap();
    }

    #[test]
    fn sleep_actually_sleeps() {
        let rt = rt();
        let lua = Lua::new();
        install_thread(&lua, rt.handle());

        let start = Instant::now();
        lua.load("thread.sleep(10)").exec().unwrap();
        let elapsed = start.elapsed();
        // Generous lower bound: OS schedulers routinely undershoot by a
        // few hundred microseconds, so assert >= 8 ms rather than the
        // requested 10 to keep the test non-flaky on loaded CI.
        assert!(
            elapsed >= std::time::Duration::from_millis(8),
            "expected >= ~10ms elapsed, got {elapsed:?}"
        );
    }

    #[test]
    fn wait_all_resolves_two_promises_in_order() {
        let rt = rt();
        let lua = Lua::new();
        install_thread(&lua, rt.handle());

        // Build two already-ready promises and stash them in globals.
        let p1 = Promise::new(
            rt.spawn(async { Ok::<_, String>(FieldValue::Number(11)) }),
            rt.handle().clone(),
        );
        let p2 = Promise::new(
            rt.spawn(async { Ok::<_, String>(FieldValue::String("two".into())) }),
            rt.handle().clone(),
        );
        lua.globals().set("p1", p1).unwrap();
        lua.globals().set("p2", p2).unwrap();

        let (a, b): (i64, String) = lua
            .load("local r = thread.wait_all({p1, p2}); return r[1], r[2]")
            .eval()
            .unwrap();
        assert_eq!(a, 11);
        assert_eq!(b, "two");
    }

    #[test]
    fn sleep_async_awaited_actually_sleeps() {
        let rt = rt();
        let lua = Lua::new();
        install_thread(&lua, rt.handle());

        let start = Instant::now();
        lua.load("local p = thread.sleep_async(15); p:await()")
            .exec()
            .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(12),
            "expected >= ~15ms elapsed via sleep_async, got {elapsed:?}"
        );
    }

    #[test]
    fn wait_any_returns_index_and_value_of_first_ready() {
        let rt = rt();
        let lua = Lua::new();
        install_thread(&lua, rt.handle());

        // p1 resolves immediately; p2 waits long enough that p1 wins.
        let p1 = Promise::new(
            rt.spawn(async { Ok::<_, String>(FieldValue::Number(7)) }),
            rt.handle().clone(),
        );
        let p2 = Promise::new(
            rt.spawn(async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<_, String>(FieldValue::Number(99))
            }),
            rt.handle().clone(),
        );
        lua.globals().set("p1", p1).unwrap();
        lua.globals().set("p2", p2).unwrap();

        let (idx, val): (i64, i64) = lua
            .load("local r = thread.wait_any({p1, p2}); return r[1], r[2]")
            .eval()
            .unwrap();
        assert_eq!(idx, 1);
        assert_eq!(val, 7);
    }

    #[test]
    fn yield_is_a_noop_that_does_not_error() {
        let rt = rt();
        let lua = Lua::new();
        install_thread(&lua, rt.handle());
        lua.load("thread.yield()").exec().unwrap();
    }

    #[test]
    fn wait_all_errors_on_non_promise_entry() {
        let rt = rt();
        let lua = Lua::new();
        install_thread(&lua, rt.handle());
        let err = lua
            .load("thread.wait_all({42})")
            .exec()
            .expect_err("non-promise entry must error");
        let msg = err.to_string();
        assert!(msg.contains("wait_all"), "should name the caller: {msg}");
    }
}
