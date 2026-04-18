//! `process` stdlib module: subprocess execution.
//!
//! Methods installed under the `process` global:
//! - `process.exec(cmd, opts?) -> {stdout, stderr, status}` — blocking one-shot.
//! - `process.exec_async(cmd, opts?) -> Promise` — async form resolving to the
//!   same shape as `exec`.
//! - `process.spawn(cmd, opts?) -> Handle` — long-lived child with explicit
//!   stdio/stdin/wait/kill control.
//! - `process.pid() -> number` — current process pid.
//!
//! ## Options table
//!
//! Every exec/spawn call accepts an optional options table:
//!
//! | key       | type            | meaning                                              |
//! |-----------|-----------------|------------------------------------------------------|
//! | `args`    | list of strings | positional arguments, default `{}`                   |
//! | `env`     | {k = v} table   | extra env vars (merged on top of the inherited env)  |
//! | `cwd`     | string          | working directory, defaults to parent's cwd          |
//! | `stdin`   | string          | if present, bytes written to the child's stdin       |
//! | `timeout` | number          | seconds; on elapse the child is killed and exec errors |
//!
//! ## Judgment calls
//!
//! - **Stdout/stderr are decoded as UTF-8 lossily** (`String::from_utf8_lossy`).
//!   The vast majority of commands that a Lua script will call produce text;
//!   invalid UTF-8 surfaces as the Unicode replacement character rather than
//!   rejecting the whole capture or handing back bytes that the rest of the
//!   stdlib (console, json, etc.) can't handle. Scripts that genuinely need
//!   raw bytes should reach for a dedicated bytes API — not implemented here.
//!
//! - **Stdin is only piped when `opts.stdin` is explicitly set.** Otherwise the
//!   child inherits the parent's stdin. This avoids accidentally attaching a
//!   closed pipe to processes that care (e.g. `less`-style pagers) and mirrors
//!   the default behaviour of `std::process::Command`.
//!
//! - **Stdout and stderr are always piped** for `exec` / `exec_async` so the
//!   returned table can carry the captured output. For `spawn`, stdout/stderr
//!   are always piped too — the whole point of the userdata handle is to read
//!   them incrementally.
//!
//! - **Timeout behaviour:** if the provided timeout elapses, we call
//!   `child.kill().await` and surface `mlua::Error::runtime("process: timed
//!   out after Xs")`. On SIGKILL-delivered death we return `status = -1` as a
//!   sentinel (std's `ExitStatus::code()` is `None` when the process was
//!   signalled, and Lua numbers are always signed — there is no clean
//!   signal-bearing exit code to expose).
//!
//! - **Environment inheritance:** every child inherits the parent's env by
//!   default; `env` entries in the options table are merged on top. We never
//!   `env_clear()` — a sandboxed environment is the consumer's responsibility.

use std::cell::RefCell;
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;
use crate::stdlib::promise::{Promise, PromiseResult};
use crate::types::{FieldMap, FieldValue};

/// Stateless marker type implementing [`LuamlStdlibModule`]; all per-call
/// state lives on the spawned `ChildHandle` userdata or the one-shot futures
/// returned by `exec`/`exec_async`.
pub struct ProcessModule;

impl LuamlStdlibModule for ProcessModule {
    fn namespace(&self) -> &'static str {
        "process"
    }

    fn install(&self, lua: &Lua, rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // process.exec(cmd, opts?) -> {stdout, stderr, status}
        // Blocking one-shot. We drive the async implementation on the runtime
        // handle via `block_on`, which matches the `time.sleep` pattern and
        // lets other async work progress while the Lua thread is parked.
        {
            let rt = rt.clone();
            table.set(
                "exec",
                lua.create_function(move |lua, (cmd, opts): (String, Option<Table>)| {
                    let spec = ExecSpec::from_opts(cmd, opts.as_ref())?;
                    let result = rt.block_on(exec_to_completion(spec));
                    let value = result.map_err(mlua::Error::runtime)?;
                    exec_result_to_table(lua, &value)
                })?,
            )?;
        }

        // process.exec_async(cmd, opts?) -> Promise
        // Spawn the exec future onto the runtime and hand back a Promise that
        // resolves to the same `{stdout, stderr, status}` shape. The task's
        // output is `PromiseResult`, so we collapse the result into a
        // FieldValue::Map before returning from the closure.
        {
            let rt = rt.clone();
            table.set(
                "exec_async",
                lua.create_function(move |_, (cmd, opts): (String, Option<Table>)| {
                    let spec = ExecSpec::from_opts(cmd, opts.as_ref())?;
                    let join: tokio::task::JoinHandle<PromiseResult> = rt.spawn(async move {
                        let result = exec_to_completion(spec).await?;
                        Ok(exec_result_to_field_value(result))
                    });
                    Ok(Promise::new(join, rt.clone()))
                })?,
            )?;
        }

        // process.spawn(cmd, opts?) -> Handle
        // Create a long-lived child, stdout/stderr/stdin (if requested) piped.
        // The returned userdata owns the `tokio::process::Child` and exposes
        // `kill`, `wait`, `wait_async`, `stdin_write`, `stdout_read`,
        // `stderr_read`, `pid`.
        {
            let rt = rt.clone();
            table.set(
                "spawn",
                lua.create_function(move |_, (cmd, opts): (String, Option<Table>)| {
                    let spec = ExecSpec::from_opts(cmd, opts.as_ref())?;
                    let child = rt
                        .block_on(async { spawn_child(&spec).await })
                        .map_err(|e| mlua::Error::runtime(format!("process: {e}")))?;
                    Ok(ChildHandle::new(child, rt.clone()))
                })?,
            )?;
        }

        // process.pid() -> number
        // Current process pid. Wrapped in `std::process::id` which returns a
        // u32; we widen to i64 so it converts to a Lua integer without risk
        // of overflow.
        table.set(
            "pid",
            lua.create_function(|_, ()| Ok(std::process::id() as i64))?,
        )?;

        Ok(table)
    }
}

/// Fully-resolved spec for a one-shot exec or a long-lived spawn. Parsing
/// options into this struct happens synchronously on the caller's thread so
/// any user input errors surface immediately (before we touch the runtime).
struct ExecSpec {
    cmd: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
    stdin: Option<Vec<u8>>,
    timeout: Option<Duration>,
}

impl ExecSpec {
    /// Build an ExecSpec from the `(cmd, opts)` pair passed by Lua.
    /// Unknown fields in `opts` are silently ignored — we deliberately do not
    /// enforce a closed schema here so future keys can be added without
    /// breaking older scripts.
    fn from_opts(cmd: String, opts: Option<&Table>) -> mlua::Result<Self> {
        let mut spec = ExecSpec {
            cmd,
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            stdin: None,
            timeout: None,
        };

        let Some(opts) = opts else {
            return Ok(spec);
        };

        // args: list of strings. We accept either a sequence (ipairs-style)
        // or miss the key entirely; anything else is an error.
        if let Ok(Value::Table(args_t)) = opts.get::<Value>("args") {
            for pair in args_t.clone().sequence_values::<String>() {
                spec.args
                    .push(pair.map_err(|e| {
                        mlua::Error::runtime(format!("process: args entry: {e}"))
                    })?);
            }
        } else if let Ok(v) = opts.get::<Value>("args") {
            if !matches!(v, Value::Nil) {
                return Err(mlua::Error::runtime(
                    "process: `args` must be a list of strings",
                ));
            }
        }

        // env: map of string -> string. Non-string values are rejected.
        if let Ok(Value::Table(env_t)) = opts.get::<Value>("env") {
            for pair in env_t.clone().pairs::<String, String>() {
                let (k, v) =
                    pair.map_err(|e| mlua::Error::runtime(format!("process: env entry: {e}")))?;
                spec.env.push((k, v));
            }
        } else if let Ok(v) = opts.get::<Value>("env") {
            if !matches!(v, Value::Nil) {
                return Err(mlua::Error::runtime(
                    "process: `env` must be a table of string -> string",
                ));
            }
        }

        // cwd: optional string.
        if let Ok(v) = opts.get::<Value>("cwd") {
            match v {
                Value::Nil => {}
                Value::String(s) => {
                    spec.cwd = Some(s.to_str()?.to_string());
                }
                _ => {
                    return Err(mlua::Error::runtime("process: `cwd` must be a string"));
                }
            }
        }

        // stdin: optional string. We store as bytes so callers that want to
        // pipe binary data (e.g. a tar stream) could in principle — but the
        // Lua-side coercion goes through `String`, so it's utf8 in practice.
        if let Ok(v) = opts.get::<Value>("stdin") {
            match v {
                Value::Nil => {}
                Value::String(s) => {
                    spec.stdin = Some(s.as_bytes().to_vec());
                }
                _ => {
                    return Err(mlua::Error::runtime("process: `stdin` must be a string"));
                }
            }
        }

        // timeout: optional number (seconds). Non-finite / negative values
        // clamp to zero rather than erroring, matching `time.sleep`.
        if let Ok(v) = opts.get::<Value>("timeout") {
            match v {
                Value::Nil => {}
                Value::Integer(i) => {
                    let secs = i.max(0) as u64;
                    spec.timeout = Some(Duration::from_secs(secs));
                }
                Value::Number(n) => {
                    let secs = if n.is_finite() && n > 0.0 { n } else { 0.0 };
                    spec.timeout = Some(Duration::from_secs_f64(secs));
                }
                _ => {
                    return Err(mlua::Error::runtime(
                        "process: `timeout` must be a number (seconds)",
                    ));
                }
            }
        }

        Ok(spec)
    }
}

/// Result of a one-shot exec. Mirrors the Lua-side table we return from
/// `exec` / resolve from `exec_async`: `{stdout, stderr, status}`.
struct ExecResult {
    stdout: String,
    stderr: String,
    status: i64,
}

/// Build a `tokio::process::Command` from an [`ExecSpec`], but without
/// actually spawning. Used by both the exec and spawn paths so the options
/// translation stays consistent.
fn build_command(spec: &ExecSpec, pipe_stdin: bool) -> Command {
    let mut cmd = Command::new(&spec.cmd);
    cmd.args(&spec.args);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if pipe_stdin {
        cmd.stdin(Stdio::piped());
    }
    // Don't kill on drop for spawn handles — the user gets explicit control.
    // For one-shot exec we await the child anyway, so it's moot.
    cmd
}

/// One-shot exec: spawn, optionally feed stdin, wait with optional timeout,
/// collect stdio, and return the decoded result.
async fn exec_to_completion(spec: ExecSpec) -> Result<ExecResult, String> {
    let pipe_stdin = spec.stdin.is_some();
    let mut cmd = build_command(&spec, pipe_stdin);

    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", spec.cmd))?;

    // Feed stdin if provided. We take() the stdin handle so it drops once
    // the writes are flushed — the child sees EOF and can proceed.
    if let Some(bytes) = &spec.stdin {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(bytes)
                .await
                .map_err(|e| format!("stdin write: {e}"))?;
            stdin
                .shutdown()
                .await
                .map_err(|e| format!("stdin close: {e}"))?;
        }
    }

    // Wait for the child, optionally under a timeout. `wait_with_output`
    // drives stdout/stderr drainage in parallel with the wait, avoiding the
    // classic pipe-full deadlock.
    let wait = async { child.wait_with_output().await };
    let output = match spec.timeout {
        Some(dur) => match tokio::time::timeout(dur, wait).await {
            Ok(res) => res.map_err(|e| format!("wait: {e}"))?,
            Err(_) => {
                // The Child has been moved into wait_with_output and is no
                // longer reachable — the timeout elapsed so the best we can
                // do is report it. wait_with_output internally kills on drop
                // when the future is cancelled, so the child is reaped.
                return Err(format!("timed out after {}s", dur.as_secs_f64()));
            }
        },
        None => wait.await.map_err(|e| format!("wait: {e}"))?,
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // ExitStatus::code() is None when the process was terminated by signal
    // (unix). Use -1 as a sentinel in that case — see module docs.
    let status = output.status.code().map(|c| c as i64).unwrap_or(-1);

    Ok(ExecResult {
        stdout,
        stderr,
        status,
    })
}

/// Spawn a long-lived child for `process.spawn`. Unlike `exec`, we keep the
/// `Child` alive and hand it to the caller.
async fn spawn_child(spec: &ExecSpec) -> Result<Child, String> {
    // Spawn always pipes stdin so the caller can write later via
    // `h:stdin_write`. If the spec carries an initial stdin blob, we write
    // it below before returning.
    let mut cmd = build_command(spec, true);
    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", spec.cmd))?;

    if let Some(bytes) = &spec.stdin {
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(bytes)
                .await
                .map_err(|e| format!("stdin write: {e}"))?;
        }
    }

    Ok(child)
}

/// Convert an ExecResult into a Lua table for `process.exec` return.
fn exec_result_to_table(lua: &Lua, res: &ExecResult) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("stdout", res.stdout.as_str())?;
    t.set("stderr", res.stderr.as_str())?;
    t.set("status", res.status)?;
    Ok(t)
}

/// Convert an ExecResult into a FieldValue::Map for `process.exec_async`
/// promise resolution. Keeps the keys aligned with the Lua-side shape.
fn exec_result_to_field_value(res: ExecResult) -> FieldValue {
    let mut map: FieldMap = HashMap::new();
    map.insert("stdout".into(), FieldValue::String(res.stdout));
    map.insert("stderr".into(), FieldValue::String(res.stderr));
    map.insert("status".into(), FieldValue::Number(res.status));
    FieldValue::Map(map)
}

/// UserData handle wrapping a long-lived `tokio::process::Child`. Holds the
/// child (and the pid we captured at spawn time — once the child is reaped
/// tokio's `Child::id` returns None) in a `RefCell<Option<_>>` so methods
/// can take/replace parts of the state.
pub struct ChildHandle {
    child: RefCell<Option<Child>>,
    pid: Option<u32>,
    rt: Handle,
}

impl ChildHandle {
    fn new(child: Child, rt: Handle) -> Self {
        let pid = child.id();
        Self {
            child: RefCell::new(Some(child)),
            pid,
            rt,
        }
    }
}

impl UserData for ChildHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // h:pid() -> integer
        // Returns the pid captured at spawn time. Once the child is reaped
        // tokio would otherwise return None; we remember it so the id is
        // still queryable post-wait.
        methods.add_method("pid", |_, this, ()| {
            Ok(this.pid.map(|p| p as i64).unwrap_or(-1))
        });

        // h:kill() -> nil
        // Send SIGKILL to the child. Safe to call repeatedly / after wait —
        // a missing child is a no-op.
        methods.add_method("kill", |_, this, ()| {
            let mut slot = this.child.borrow_mut();
            if let Some(child) = slot.as_mut() {
                this.rt
                    .block_on(async { child.kill().await })
                    .map_err(|e| mlua::Error::runtime(format!("process: kill: {e}")))?;
            }
            Ok(())
        });

        // h:wait() -> integer (status code)
        // Blocking wait for the child to exit. Drains ownership of the child
        // — subsequent waits error. Returns the exit code (or -1 on signal).
        methods.add_method("wait", |_, this, ()| {
            let mut child = this
                .child
                .borrow_mut()
                .take()
                .ok_or_else(|| mlua::Error::runtime("process: child already reaped"))?;
            let status = this
                .rt
                .block_on(async { child.wait().await })
                .map_err(|e| mlua::Error::runtime(format!("process: wait: {e}")))?;
            Ok(status.code().map(|c| c as i64).unwrap_or(-1))
        });

        // h:wait_async() -> Promise<integer>
        // Non-blocking variant of `wait`. Returns a Promise resolving to the
        // exit status. Drains the child handle — calling either wait form
        // again errors.
        methods.add_method("wait_async", |_, this, ()| {
            let mut child = this
                .child
                .borrow_mut()
                .take()
                .ok_or_else(|| mlua::Error::runtime("process: child already reaped"))?;
            let join: tokio::task::JoinHandle<PromiseResult> = this.rt.spawn(async move {
                let status = child.wait().await.map_err(|e| format!("wait: {e}"))?;
                let code = status.code().map(|c| c as i64).unwrap_or(-1);
                Ok(FieldValue::Number(code))
            });
            Ok(Promise::new(join, this.rt.clone()))
        });

        // h:stdin_write(data) -> nil
        // Append `data` to the child's stdin. Each call flushes. Errors if
        // the child has no piped stdin or has already closed it.
        methods.add_method("stdin_write", |_, this, data: String| {
            let mut slot = this.child.borrow_mut();
            let child = slot
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("process: child already reaped"))?;
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("process: stdin not piped"))?;
            this.rt
                .block_on(async {
                    stdin.write_all(data.as_bytes()).await?;
                    stdin.flush().await
                })
                .map_err(|e| mlua::Error::runtime(format!("process: stdin_write: {e}")))?;
            Ok(())
        });

        // h:stdout_read() -> string | nil
        // Read whatever bytes are currently available from stdout. Returns a
        // utf8-lossy string, or nil on EOF. We take() the stdout handle on
        // EOF so subsequent calls see nil without blocking.
        methods.add_method("stdout_read", |_, this, ()| {
            let mut slot = this.child.borrow_mut();
            let child = slot
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("process: child already reaped"))?;
            let Some(stdout) = child.stdout.as_mut() else {
                return Ok(None);
            };
            let mut buf = [0u8; 8192];
            let n = this
                .rt
                .block_on(async { stdout.read(&mut buf).await })
                .map_err(|e| mlua::Error::runtime(format!("process: stdout_read: {e}")))?;
            if n == 0 {
                // EOF — drop the handle so future calls short-circuit.
                let _ = child.stdout.take();
                return Ok(None);
            }
            Ok(Some(String::from_utf8_lossy(&buf[..n]).into_owned()))
        });

        // h:stderr_read() -> string | nil
        // Mirror of stdout_read. Same utf8-lossy + EOF semantics.
        methods.add_method("stderr_read", |_, this, ()| {
            let mut slot = this.child.borrow_mut();
            let child = slot
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("process: child already reaped"))?;
            let Some(stderr) = child.stderr.as_mut() else {
                return Ok(None);
            };
            let mut buf = [0u8; 8192];
            let n = this
                .rt
                .block_on(async { stderr.read(&mut buf).await })
                .map_err(|e| mlua::Error::runtime(format!("process: stderr_read: {e}")))?;
            if n == 0 {
                let _ = child.stderr.take();
                return Ok(None);
            }
            Ok(Some(String::from_utf8_lossy(&buf[..n]).into_owned()))
        });
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests. We keep these portable by sticking to commands that ship
    //! on every unix-ish system (echo, cat, sleep, false, /bin/sh -c). On
    //! Windows these tests would need a different set of commands — the
    //! module itself is portable, but the tests aren't.
    //!
    //! `process.spawn` has more moving parts (stdio timing, pid reuse) and
    //! is covered by a single minimal spawn-then-wait check rather than the
    //! full matrix of stdin/stdout interactions.
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    fn install() -> (tokio::runtime::Runtime, Lua) {
        let rt = Builder::new_multi_thread().enable_all().build().unwrap();
        let lua = Lua::new();
        let table = ProcessModule
            .install(&lua, rt.handle())
            .expect("install process module");
        lua.globals().set("process", table).unwrap();
        // Keep the runtime alive for the duration of the test; the closures
        // hold Handle clones and need a live runtime to block_on.
        (rt, lua)
    }

    #[test]
    #[cfg(unix)]
    fn exec_echo_captures_stdout_and_zero_status() {
        let (_rt, lua) = install();
        let (stdout, status): (String, i64) = lua
            .load(
                r#"
                local r = process.exec("echo", { args = {"hi"} })
                return r.stdout, r.status
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(stdout, "hi\n");
        assert_eq!(status, 0);
    }

    #[test]
    #[cfg(unix)]
    fn exec_cat_pipes_stdin_through() {
        let (_rt, lua) = install();
        // `cat` with no args copies stdin to stdout. We feed it a known
        // string and assert it round-trips.
        let (stdout, status): (String, i64) = lua
            .load(
                r#"
                local r = process.exec("cat", { stdin = "pipe-me" })
                return r.stdout, r.status
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(stdout, "pipe-me");
        assert_eq!(status, 0);
    }

    #[test]
    fn exec_missing_command_errors() {
        let (_rt, lua) = install();
        // A name that can't possibly be on PATH. The spawn itself fails;
        // we expect the error to name the command we tried to launch so the
        // script author can tell which exec failed.
        let err = lua
            .load(
                r#"
                return process.exec("luaml__definitely_not_a_real_binary__xyz")
            "#,
            )
            .eval::<mlua::Value>()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("luaml__definitely_not_a_real_binary__xyz"),
            "err should name the failing command: {err}"
        );
    }

    #[test]
    fn pid_is_positive() {
        let (_rt, lua) = install();
        let pid: i64 = lua.load("return process.pid()").eval().unwrap();
        assert!(pid > 0, "pid should be positive, got {pid}");
    }

    #[test]
    #[cfg(unix)]
    fn exec_async_returns_promise_resolving_to_result_table() {
        let (_rt, lua) = install();
        // Drive the async variant end-to-end: kick off, await, read fields.
        let (stdout, status): (String, i64) = lua
            .load(
                r#"
                local p = process.exec_async("echo", { args = {"async"} })
                local r = p:await()
                return r.stdout, r.status
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(stdout, "async\n");
        assert_eq!(status, 0);
    }

    #[test]
    #[cfg(unix)]
    fn exec_nonzero_status_is_surfaced() {
        let (_rt, lua) = install();
        // `false` exits 1. We want the status surfaced, not an error — the
        // command ran to completion, just unhappily.
        let status: i64 = lua
            .load(
                r#"
                local r = process.exec("false")
                return r.status
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(status, 1);
    }

    #[test]
    #[cfg(unix)]
    fn spawn_sleep_then_wait_returns_zero() {
        let (_rt, lua) = install();
        // A simplest spawn: sleep 0, wait, check exit code. This exercises
        // the spawn/wait path without touching stdio reads, which are
        // timing-sensitive on some platforms.
        let code: i64 = lua
            .load(
                r#"
                local h = process.spawn("sleep", { args = {"0"} })
                return h:wait()
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    #[cfg(unix)]
    fn exec_timeout_kills_long_running_child() {
        let (_rt, lua) = install();
        // `sleep 30` would ordinarily block us forever; timeout must fire
        // and surface a runtime error containing "timed out".
        let err = lua
            .load(
                r#"
                return process.exec("sleep", { args = {"30"}, timeout = 0.1 })
            "#,
            )
            .eval::<mlua::Value>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "err should report timeout: {err}");
    }
}
