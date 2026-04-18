// NEW DEP: chrono = "0.4" (clock feature)
//! `time` stdlib module: clocks, timers, and sleeping.
//!
//! Installed under the bare `time` global. Methods:
//! - `time.now() -> number` — unix epoch seconds as `f64`. Sub-second
//!   precision is preserved (we convert from `SystemTime` via nanoseconds).
//! - `time.now_ms() -> integer` — unix epoch milliseconds.
//! - `time.sleep(seconds)` — blocking sleep. Drives
//!   [`tokio::time::sleep`] on the engine's runtime via
//!   [`Handle::block_on`], so timer wakeups cooperate with the rest of the
//!   runtime (other async work can progress while the Lua thread is parked).
//!   NOTE on panic hazard: `Handle::block_on` panics if invoked from within a
//!   thread already driving *this* runtime (e.g. a Lua function called from
//!   inside a task spawned on the same multi-thread runtime). Tests run on
//!   the outer test thread so this is fine, and the engine's executor
//!   currently runs Lua on a non-runtime thread; if a future caller dispatches
//!   Lua from inside a runtime worker, this must switch to
//!   [`tokio::task::block_in_place`] or the caller must use `time.sleep_async`.
//! - `time.sleep_async(seconds) -> Promise` — non-blocking; returns a
//!   [`Promise`] that resolves to nil when the timer fires.
//! - `time.format(epoch, fmt) -> string` — render a unix epoch (seconds, may
//!   be fractional) with a chrono format string. UTC.
//! - `time.parse(s, fmt) -> number` — inverse of `format`; yields epoch
//!   seconds as `f64`. The parsed timestamp must include enough fields to
//!   construct a UTC instant.
//! - `time.timer() -> Timer` — userdata wrapping a [`std::time::Instant`].
//!   Methods: `:elapsed_ms() -> integer`, `:reset()`.
//!
//! All clock reads go through [`std::time::SystemTime`]; formatting/parsing
//! goes through [`chrono`] in UTC. No environment-variable or local-time
//! dependence — callers that want a local-time view should pass a format
//! string that includes an offset and adjust downstream.

use std::cell::RefCell;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use mlua::{Lua, Table, UserData, UserDataMethods};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;
use crate::stdlib::promise::{Promise, PromiseResult};
use crate::types::FieldValue;

/// `time` stdlib module. See module-level docs.
pub struct TimeModule;

/// Userdata wrapping a monotonic [`Instant`]. A timer is constructed by
/// `time.timer()`, tracks elapsed time since construction (or the last
/// `:reset()`), and exposes a single read (`:elapsed_ms()`).
///
/// The instant is wrapped in a [`RefCell`] so `:reset()` can mutate the stored
/// start time — `UserData` method receivers are `&self`, so interior
/// mutability is the only way to support reset without boxing.
struct Timer {
    start: RefCell<Instant>,
}

impl UserData for Timer {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // timer:elapsed_ms() -> integer
        // Milliseconds since construction or the last reset, saturating at
        // i64::MAX for the (pathological) case where the timer has been alive
        // longer than ~292 million years.
        methods.add_method("elapsed_ms", |_, this, ()| {
            let elapsed = this.start.borrow().elapsed();
            let ms = elapsed.as_millis();
            let clamped: i64 = i64::try_from(ms).unwrap_or(i64::MAX);
            Ok(clamped)
        });

        // timer:reset()
        // Resets the start point to the current instant. Subsequent
        // `:elapsed_ms()` reads measure from this new origin.
        methods.add_method("reset", |_, this, ()| {
            *this.start.borrow_mut() = Instant::now();
            Ok(())
        });
    }
}

impl LuamlStdlibModule for TimeModule {
    fn namespace(&self) -> &'static str {
        "time"
    }

    fn install(&self, lua: &Lua, rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // time.now() -> number
        // Unix epoch seconds as f64, including sub-second precision. We go
        // through nanoseconds rather than SystemTime::as_secs_f64 directly so
        // the conversion is explicit about what is kept.
        table.set(
            "now",
            lua.create_function(|_, ()| {
                let dur = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| mlua::Error::runtime(format!("time.now: {e}")))?;
                Ok(dur.as_secs_f64())
            })?,
        )?;

        // time.now_ms() -> integer
        // Unix epoch milliseconds. Overflows cap at i64::MAX; on a realistic
        // clock that can't happen until the year ~292,278,994 AD.
        table.set(
            "now_ms",
            lua.create_function(|_, ()| {
                let dur = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| mlua::Error::runtime(format!("time.now_ms: {e}")))?;
                let ms = dur.as_millis();
                let clamped: i64 = i64::try_from(ms).unwrap_or(i64::MAX);
                Ok(clamped)
            })?,
        )?;

        // time.sleep(seconds)
        // Blocking sleep routed through the tokio runtime so other async
        // tasks (timers, I/O) can progress while the Lua thread is parked.
        // Negative / NaN durations clamp to zero — matches the behaviour of
        // most sleep APIs and avoids panicking inside Duration::from_secs_f64.
        {
            let rt = rt.clone();
            table.set(
                "sleep",
                lua.create_function(move |_, seconds: f64| {
                    let dur = duration_from_secs(seconds);
                    rt.block_on(async move { tokio::time::sleep(dur).await });
                    Ok(())
                })?,
            )?;
        }

        // time.sleep_async(seconds) -> Promise
        // Spawns the sleep on the runtime and hands back a Promise that
        // resolves to nil. Negative / NaN durations clamp to zero.
        {
            let rt = rt.clone();
            table.set(
                "sleep_async",
                lua.create_function(move |_, seconds: f64| {
                    let dur = duration_from_secs(seconds);
                    let join: tokio::task::JoinHandle<PromiseResult> = rt.spawn(async move {
                        tokio::time::sleep(dur).await;
                        Ok(FieldValue::Null)
                    });
                    Ok(Promise::new(join, rt.clone()))
                })?,
            )?;
        }

        // time.format(epoch, fmt) -> string
        // Renders a unix epoch (fractional seconds) with the provided chrono
        // format string, in UTC. Splits the f64 into whole seconds + nanos so
        // sub-second precision survives (DateTime::from_timestamp takes an
        // i64 seconds and u32 nanos).
        table.set(
            "format",
            lua.create_function(|_, (epoch, fmt): (f64, String)| {
                if !epoch.is_finite() {
                    return Err(mlua::Error::runtime("time.format: epoch is not finite"));
                }
                let (secs, nanos) = split_secs_nanos(epoch);
                let dt = DateTime::<Utc>::from_timestamp(secs, nanos).ok_or_else(|| {
                    mlua::Error::runtime(format!("time.format: epoch out of range: {epoch}"))
                })?;
                Ok(dt.format(&fmt).to_string())
            })?,
        )?;

        // time.parse(s, fmt) -> number
        // Inverse of `format`. Parses the string as a NaiveDateTime in the
        // given format, interprets it as UTC, and returns epoch seconds
        // (fractional). The format must supply enough fields to construct a
        // datetime; a date-only format will error because chrono refuses to
        // guess the time components.
        table.set(
            "parse",
            lua.create_function(|_, (s, fmt): (String, String)| {
                let naive = NaiveDateTime::parse_from_str(&s, &fmt).map_err(|e| {
                    mlua::Error::runtime(format!("time.parse: {e}"))
                })?;
                let dt = Utc.from_utc_datetime(&naive);
                // timestamp() is i64 seconds; timestamp_subsec_nanos is u32.
                // Combine without losing sub-second precision.
                let secs = dt.timestamp() as f64;
                let frac = dt.timestamp_subsec_nanos() as f64 / 1_000_000_000.0;
                Ok(secs + frac)
            })?,
        )?;

        // time.timer() -> Timer
        // Constructs a fresh timer anchored at Instant::now().
        table.set(
            "timer",
            lua.create_function(|_, ()| {
                Ok(Timer {
                    start: RefCell::new(Instant::now()),
                })
            })?,
        )?;

        Ok(table)
    }
}

/// Clamp a fractional-second sleep duration to a non-negative [`Duration`].
/// Negative, NaN, and extremely large values all degrade gracefully rather
/// than panicking inside [`Duration::from_secs_f64`] (which would otherwise
/// abort on negative / NaN input).
fn duration_from_secs(seconds: f64) -> Duration {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Duration::ZERO;
    }
    // Duration::from_secs_f64 panics if the value doesn't fit; clamp to u64
    // seconds' worth of nanoseconds first.
    let max = Duration::MAX.as_secs_f64();
    if seconds >= max {
        return Duration::MAX;
    }
    Duration::from_secs_f64(seconds)
}

/// Split an f64 epoch-seconds value into (whole seconds, sub-second
/// nanoseconds) suitable for [`DateTime::<Utc>::from_timestamp`]. Negative
/// fractional parts are normalised so the nanos component is always
/// non-negative and the seconds component carries the sign — matches chrono's
/// API contract that wants `0 <= nanos < 1_000_000_000`.
fn split_secs_nanos(epoch: f64) -> (i64, u32) {
    let whole = epoch.trunc();
    let frac = epoch - whole;
    let mut secs = whole as i64;
    let mut nanos = (frac * 1_000_000_000.0).round() as i64;
    if nanos < 0 {
        secs -= 1;
        nanos += 1_000_000_000;
    }
    // Rounding at the boundary can spill nanos to 1e9 exactly; carry into
    // seconds so the u32 cast below is always in range.
    if nanos >= 1_000_000_000 {
        secs += 1;
        nanos -= 1_000_000_000;
    }
    (secs, nanos as u32)
}

#[cfg(test)]
mod tests {
    //! Smoke tests. A note on runtime choice:
    //!
    //! `time.sleep` calls `Handle::block_on` on the runtime handed to the
    //! module. That panics if it runs on a worker thread of *that same*
    //! runtime. Tests here run on the outer test thread (Cargo's default
    //! thread pool), and the runtime is constructed inside the test, so
    //! `block_on` is legal — the test thread is not a runtime worker.
    //! Callers that invoke Lua from inside a runtime task should use
    //! `sleep_async` instead.
    use super::*;
    use mlua::Lua;
    use tokio::runtime::{Builder, Runtime};

    fn rt() -> Runtime {
        Builder::new_multi_thread().enable_all().build().unwrap()
    }

    fn install(rt: &Runtime) -> Lua {
        let lua = Lua::new();
        let table = TimeModule
            .install(&lua, rt.handle())
            .expect("install time module");
        lua.globals().set("time", table).unwrap();
        lua
    }

    #[test]
    fn now_returns_positive_epoch() {
        let rt = rt();
        let lua = install(&rt);
        let v: f64 = lua.load("return time.now()").eval().unwrap();
        assert!(v > 0.0, "epoch seconds should be positive, got {v}");
        // Sanity: should at least be past year 2000 (epoch 946_684_800).
        assert!(v > 946_684_800.0, "epoch should be post-2000, got {v}");
    }

    #[test]
    fn now_ms_returns_positive_integer() {
        let rt = rt();
        let lua = install(&rt);
        let v: i64 = lua.load("return time.now_ms()").eval().unwrap();
        assert!(v > 0, "epoch ms should be positive, got {v}");
        assert!(v > 946_684_800_000, "epoch ms should be post-2000, got {v}");
    }

    #[test]
    fn format_epoch_zero_is_1970() {
        let rt = rt();
        let lua = install(&rt);
        let v: String = lua
            .load("return time.format(0, '%Y')")
            .eval()
            .unwrap();
        assert_eq!(v, "1970");
        // And a fuller check to confirm the format string is honoured.
        let full: String = lua
            .load("return time.format(0, '%Y-%m-%d %H:%M:%S')")
            .eval()
            .unwrap();
        assert_eq!(full, "1970-01-01 00:00:00");
    }

    #[test]
    fn parse_format_round_trip() {
        let rt = rt();
        let lua = install(&rt);
        // Use a second-aligned timestamp so the round trip is exact.
        let original: f64 = 1_700_000_000.0;
        let script = format!(
            "local s = time.format({original}, '%Y-%m-%d %H:%M:%S'); \
             return time.parse(s, '%Y-%m-%d %H:%M:%S')"
        );
        let round: f64 = lua.load(script).eval().unwrap();
        assert_eq!(round, original, "round trip must preserve whole seconds");
    }

    #[test]
    fn timer_elapsed_ms_after_sleep_is_positive() {
        let rt = rt();
        let lua = install(&rt);
        // sleep(0.02) parks the Lua thread for ~20ms; the timer should then
        // report a strictly positive elapsed_ms. We don't assert an upper
        // bound because scheduler jitter can stretch it arbitrarily on CI.
        let ms: i64 = lua
            .load(
                r#"
                local t = time.timer()
                time.sleep(0.02)
                return t:elapsed_ms()
            "#,
            )
            .eval()
            .unwrap();
        assert!(ms > 0, "elapsed after sleep should be > 0, got {ms}");
    }

    #[test]
    fn timer_reset_starts_from_zero_again() {
        let rt = rt();
        let lua = install(&rt);
        let (before, after): (i64, i64) = lua
            .load(
                r#"
                local t = time.timer()
                time.sleep(0.02)
                local before = t:elapsed_ms()
                t:reset()
                local after = t:elapsed_ms()
                return before, after
            "#,
            )
            .eval()
            .unwrap();
        assert!(before > 0, "before reset should be > 0, got {before}");
        assert!(
            after <= before,
            "after reset elapsed ({after}ms) should be <= pre-reset ({before}ms)"
        );
    }

    #[test]
    fn sleep_async_returns_promise_resolving_to_nil() {
        let rt = rt();
        let lua = install(&rt);
        // The promise resolves to FieldValue::Null, which surfaces to Lua
        // as nil. `:await()` then returns nil, so reading the value back in
        // Lua should give `type(v) == 'nil'`.
        let kind: String = lua
            .load(
                r#"
                local p = time.sleep_async(0.01)
                local v = p:await()
                return type(v)
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(kind, "nil");
    }
}
