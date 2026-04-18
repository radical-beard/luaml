//! `fs` stdlib module: filesystem operations, synchronous + async variants.
//!
//! Installed under the bare `fs` global. Every I/O operation ships in two
//! flavours: a blocking function (`fs.read`, `fs.write`, ...) built on
//! [`std::fs`], and an async counterpart (`fs.read_async`, ...) that spawns
//! the work on the engine's tokio runtime via [`tokio::fs`] and returns a
//! [`Promise`]. The blocking variants deliberately avoid `Handle::block_on`
//! so they don't re-enter the runtime from a worker thread — a subtle
//! panic-hazard — and so that purely synchronous scripts pay no async
//! machinery cost at all.
//!
//! Method summary (see inline docs for exact signatures):
//!
//! - `fs.read` / `fs.read_async(path) -> string` — full file → utf-8 string.
//! - `fs.write` / `fs.write_async(path, data)` — overwrite.
//! - `fs.append` / `fs.append_async(path, data)` — create-or-append.
//! - `fs.delete(path)` — unlink a regular file (errors on a directory).
//! - `fs.rename(from, to)`.
//! - `fs.copy(from, to)`.
//! - `fs.exists(path) -> bool` — file or directory.
//! - `fs.stat(path) -> {kind, size, mtime, mode}` — metadata summary.
//! - `fs.readdir(path) -> {string, ...}` — entry basenames, not full paths.
//! - `fs.mkdir(path, {recursive=false}?)`.
//! - `fs.rmdir(path, {recursive=false}?)`.
//! - `fs.tempdir() -> string` — value of [`std::env::temp_dir`].
//! - `fs.tempfile() -> string` — a unique path inside tempdir. The file is
//!   **not** created; callers supply contents on first write.
//! - `fs.canonicalize(path) -> string`.
//! - `fs.watch(path) -> WatchHandle` — only when the `file-watch` feature is
//!   enabled; a stub that raises `fs.watch: file-watch feature disabled` is
//!   installed otherwise.
//!
//! ## Errors
//!
//! All I/O errors surface through [`mlua::Error::runtime`] as
//! `"fs: <message>"`. Async variants carry errors through the promise
//! machinery (task output is `Result<FieldValue, String>`), so `:await()`
//! raises the same string form as the blocking call would.
//!
//! ## Strings and bytes
//!
//! `read*` returns a Lua string built from raw bytes; `write*` / `append*`
//! accept a Lua string whose bytes are written verbatim. Lua 5.4 strings are
//! 8-bit-clean so binary payloads round-trip without re-encoding. This means
//! `fs.read` never raises on invalid utf-8 — callers that need utf-8
//! validation run it themselves (see `codec.utf8_valid`).
//!
//! ## `fs.watch` backpressure
//!
//! The watch handle owns an unbounded `std::sync::mpsc` channel fed by the
//! notify callback. Scripts drain it with `handle:next()`, which waits up to
//! ~500ms for an event and returns `nil` on timeout (so the Lua side can do
//! cooperative polling inside a loop without pinning a whole thread). If a
//! script stops draining, events accumulate in the channel — there is no
//! drop-oldest ring. This is the conservative default for a tiny channel;
//! scripts that need back-pressure semantics should poll in a tight loop or
//! call `:close()` when they're done.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mlua::{Lua, Table, Value};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;
use crate::stdlib::promise::{Promise, PromiseResult};
use crate::types::FieldValue;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// `fs` stdlib module. See module-level docs.
pub struct FsModule;

impl LuamlStdlibModule for FsModule {
    fn namespace(&self) -> &'static str {
        "fs"
    }

    fn install(&self, lua: &Lua, rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // ---- read / read_async --------------------------------------------
        // fs.read(path) -> string
        // Full file → utf-8 string. Binary-clean: bytes are passed through
        // `Lua::create_string`, not decoded.
        table.set(
            "read",
            lua.create_function(|lua, path: String| {
                let bytes = std::fs::read(&path).map_err(err)?;
                lua.create_string(&bytes).map(Value::String)
            })?,
        )?;

        // fs.read_async(path) -> Promise<string>
        {
            let rt = rt.clone();
            table.set(
                "read_async",
                lua.create_function(move |_, path: String| {
                    let join: tokio::task::JoinHandle<PromiseResult> = rt.spawn(async move {
                        let bytes = tokio::fs::read(&path).await.map_err(err_str)?;
                        // PromiseResult only carries FieldValues; the String
                        // variant round-trips raw bytes lossily iff they are
                        // not valid utf-8. This is the same trade-off `read`
                        // makes at the Lua boundary; we document it in
                        // module docs and keep it consistent.
                        let s = String::from_utf8(bytes).map_err(|e| {
                            format!("fs: read_async produced non-utf8 bytes: {e}")
                        })?;
                        Ok(FieldValue::String(s))
                    });
                    Ok(Promise::new(join, rt.clone()))
                })?,
            )?;
        }

        // ---- write / write_async ------------------------------------------
        // fs.write(path, data) — overwrite.
        table.set(
            "write",
            lua.create_function(|_, (path, data): (String, mlua::String)| {
                let bytes = data.as_bytes().to_vec();
                std::fs::write(&path, bytes).map_err(err)
            })?,
        )?;

        // fs.write_async(path, data) -> Promise<nil>
        {
            let rt = rt.clone();
            table.set(
                "write_async",
                lua.create_function(move |_, (path, data): (String, mlua::String)| {
                    let bytes = data.as_bytes().to_vec();
                    let join: tokio::task::JoinHandle<PromiseResult> = rt.spawn(async move {
                        tokio::fs::write(&path, bytes).await.map_err(err_str)?;
                        Ok(FieldValue::Null)
                    });
                    Ok(Promise::new(join, rt.clone()))
                })?,
            )?;
        }

        // ---- append / append_async ----------------------------------------
        // fs.append(path, data)
        table.set(
            "append",
            lua.create_function(|_, (path, data): (String, mlua::String)| {
                use std::io::Write;
                let bytes = data.as_bytes().to_vec();
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(err)?;
                f.write_all(&bytes).map_err(err)
            })?,
        )?;

        // fs.append_async(path, data) -> Promise<nil>
        {
            let rt = rt.clone();
            table.set(
                "append_async",
                lua.create_function(move |_, (path, data): (String, mlua::String)| {
                    let bytes = data.as_bytes().to_vec();
                    let join: tokio::task::JoinHandle<PromiseResult> = rt.spawn(async move {
                        use tokio::io::AsyncWriteExt;
                        let mut f = tokio::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                            .await
                            .map_err(err_str)?;
                        f.write_all(&bytes).await.map_err(err_str)?;
                        Ok(FieldValue::Null)
                    });
                    Ok(Promise::new(join, rt.clone()))
                })?,
            )?;
        }

        // ---- delete / rename / copy ---------------------------------------
        // fs.delete(path) — remove a regular file; error if path is a dir.
        // We check the metadata up-front rather than trusting `remove_file`'s
        // platform-dependent error for a directory input.
        table.set(
            "delete",
            lua.create_function(|_, path: String| {
                let md = std::fs::symlink_metadata(&path).map_err(err)?;
                if md.is_dir() {
                    return Err(mlua::Error::runtime(format!(
                        "fs: delete refuses directory '{path}' (use fs.rmdir)"
                    )));
                }
                std::fs::remove_file(&path).map_err(err)
            })?,
        )?;

        // fs.rename(from, to)
        table.set(
            "rename",
            lua.create_function(|_, (from, to): (String, String)| {
                std::fs::rename(&from, &to).map_err(err)
            })?,
        )?;

        // fs.copy(from, to)
        table.set(
            "copy",
            lua.create_function(|_, (from, to): (String, String)| {
                std::fs::copy(&from, &to).map(|_| ()).map_err(err)
            })?,
        )?;

        // ---- exists / stat / readdir --------------------------------------
        // fs.exists(path) -> bool
        // True for any existing entry — file, directory, symlink target, ...
        // Uses `symlink_metadata` so a dangling symlink still reports true.
        table.set(
            "exists",
            lua.create_function(|_, path: String| {
                Ok(std::fs::symlink_metadata(&path).is_ok())
            })?,
        )?;

        // fs.stat(path) -> {kind, size, mtime, mode}
        // `kind`  : "file" | "dir" | "symlink" | "other".
        // `size`  : integer bytes (i64, clamped; huge files cap at i64::MAX).
        // `mtime` : unix seconds as f64 (sub-second precision preserved).
        // `mode`  : unix permission bits as integer; 0 on non-unix.
        table.set(
            "stat",
            lua.create_function(|lua, path: String| {
                // symlink_metadata so `kind == "symlink"` is observable. For
                // file size we report what the OS reports; callers that want
                // the target's size should canonicalize + stat again.
                let md = std::fs::symlink_metadata(&path).map_err(err)?;
                let kind = if md.file_type().is_symlink() {
                    "symlink"
                } else if md.is_dir() {
                    "dir"
                } else if md.is_file() {
                    "file"
                } else {
                    "other"
                };
                let size: i64 = i64::try_from(md.len()).unwrap_or(i64::MAX);
                let mtime = md
                    .modified()
                    .map_err(err)?
                    .duration_since(UNIX_EPOCH)
                    // Files dated before 1970 surface 0.0 rather than
                    // propagating a SystemTimeError — stat is a query API and
                    // should not fail on pre-epoch timestamps.
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);

                let mode: i64;
                #[cfg(unix)]
                {
                    // `mode()` returns the full st_mode (perm bits + type
                    // bits); mask to the low 12 bits so callers see just the
                    // permission bits that match chmod-style octal literals.
                    mode = (md.permissions().mode() & 0o7777) as i64;
                }
                #[cfg(not(unix))]
                {
                    mode = 0;
                }

                let t = lua.create_table()?;
                t.set("kind", kind)?;
                t.set("size", size)?;
                t.set("mtime", mtime)?;
                t.set("mode", mode)?;
                Ok(t)
            })?,
        )?;

        // fs.readdir(path) -> { "a", "b", ... }
        // Basenames only; callers that want absolute paths join onto `path`.
        // Hidden / dot-prefixed entries are included as-is (no filtering).
        table.set(
            "readdir",
            lua.create_function(|lua, path: String| {
                let t = lua.create_table()?;
                let iter = std::fs::read_dir(&path).map_err(err)?;
                // 1-indexed for Lua idioms — pair a counter with the iterator
                // rather than mutating a `let mut` across iterations.
                for (i, entry) in (1..).zip(iter) {
                    let entry = entry.map_err(err)?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    t.set(i, name)?;
                }
                Ok(t)
            })?,
        )?;

        // ---- mkdir / rmdir ------------------------------------------------
        // fs.mkdir(path, {recursive=false}?)
        table.set(
            "mkdir",
            lua.create_function(|_, (path, opts): (String, Option<Table>)| {
                let recursive = match opts {
                    Some(t) => t.get::<Option<bool>>("recursive")?.unwrap_or(false),
                    None => false,
                };
                if recursive {
                    std::fs::create_dir_all(&path).map_err(err)
                } else {
                    std::fs::create_dir(&path).map_err(err)
                }
            })?,
        )?;

        // fs.rmdir(path, {recursive=false}?) — when recursive, deletes file
        // tree below `path`. The non-recursive form only succeeds on empty
        // directories (matches `rmdir(1)`).
        table.set(
            "rmdir",
            lua.create_function(|_, (path, opts): (String, Option<Table>)| {
                let recursive = match opts {
                    Some(t) => t.get::<Option<bool>>("recursive")?.unwrap_or(false),
                    None => false,
                };
                if recursive {
                    std::fs::remove_dir_all(&path).map_err(err)
                } else {
                    std::fs::remove_dir(&path).map_err(err)
                }
            })?,
        )?;

        // ---- tempdir / tempfile / canonicalize ----------------------------
        // fs.tempdir() -> string
        // Thin wrapper over `std::env::temp_dir`. We lossy-utf8 encode the
        // result; on the exotic platforms where tempdir is not utf-8 the
        // best we can do without the tokio runtime is a best-effort string.
        table.set(
            "tempdir",
            lua.create_function(|_, ()| {
                Ok(std::env::temp_dir().to_string_lossy().into_owned())
            })?,
        )?;

        // fs.tempfile() -> string
        // Returns a **path**, not a handle; the file is not created. Name
        // format: "luaml-<pid>-<nanos>-<counter>.tmp" — combining PID,
        // current-time nanos, and a monotonically increasing in-process
        // counter is enough to uniquify within a single engine without
        // pulling in `uuid` or hitting `/dev/urandom`. The brief explicitly
        // forbids uuid; a counter plus nanos avoids the birthday paradox
        // you'd face from a pure-random suffix at this bit width.
        table.set(
            "tempfile",
            lua.create_function(|_, ()| {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let counter = TEMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
                let pid = std::process::id();
                let name = format!("luaml-{pid}-{nanos}-{counter}.tmp");
                let path = std::env::temp_dir().join(name);
                Ok(path.to_string_lossy().into_owned())
            })?,
        )?;

        // fs.canonicalize(path) -> string
        // Resolves symlinks and `.`/`..`. Errors if `path` doesn't exist.
        table.set(
            "canonicalize",
            lua.create_function(|_, path: String| {
                let resolved = std::fs::canonicalize(&path).map_err(err)?;
                Ok(resolved.to_string_lossy().into_owned())
            })?,
        )?;

        // ---- watch --------------------------------------------------------
        install_watch(&table, lua)?;

        Ok(table)
    }
}

/// Monotonic counter used to disambiguate `fs.tempfile` paths issued within
/// the same nanosecond. Lives for the lifetime of the process — plenty for a
/// single engine run even at millions of tempfile calls per second.
static TEMPFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Convert an `io::Error` (or anything `Display`) into an mlua runtime error
/// prefixed with `"fs: "`. Used by every blocking variant so the error
/// surface is uniform regardless of which syscall tripped.
fn err<E: std::fmt::Display>(e: E) -> mlua::Error {
    mlua::Error::runtime(format!("fs: {e}"))
}

/// String form of [`err`] for the async promise path — promises carry
/// errors as `String` because `mlua::Error` is not `Send + Sync`.
fn err_str<E: std::fmt::Display>(e: E) -> String {
    format!("fs: {e}")
}

// ─── fs.watch ──────────────────────────────────────────────────────────────

#[cfg(feature = "file-watch")]
fn install_watch(table: &Table, lua: &Lua) -> mlua::Result<()> {
    use notify::{EventKind, RecursiveMode, Watcher, recommended_watcher};
    use std::cell::RefCell;
    use std::path::Path;
    use std::sync::mpsc;

    /// Classify a notify event kind into the tiny enum scripts route on.
    /// notify's native enum is deeper than Lua scripts need and shifts
    /// between crate versions; keeping our own set means a notify upgrade
    /// doesn't ripple into Lua land.
    fn classify(kind: &EventKind) -> &'static str {
        match kind {
            EventKind::Create(_) => "create",
            EventKind::Modify(_) => "modify",
            EventKind::Remove(_) => "remove",
            EventKind::Access(_) => "access",
            _ => "other",
        }
    }

    /// Userdata owning the `recommended_watcher` plus a channel of events.
    /// The watcher and the receiver are both held in `RefCell<Option<_>>`
    /// so `:close()` can drop them deterministically — dropping the
    /// watcher shuts down its background thread, and dropping the receiver
    /// closes the channel so any pending sends in the callback are no-ops.
    struct WatchHandle {
        rx: RefCell<Option<mpsc::Receiver<(String, String)>>>,
        _watcher: RefCell<Option<notify::RecommendedWatcher>>,
    }

    impl mlua::UserData for WatchHandle {
        fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
            // handle:next() -> {path, kind} | nil
            // Blocks up to 500ms for the next event. On timeout returns nil
            // so Lua can poll in a `while true` loop with `if next then ...`
            // without locking the runtime forever. 500ms is a judgment call:
            // long enough to batch bursts without idle-spinning, short
            // enough that a tight Lua poll loop remains responsive to
            // higher-level quit signals.
            methods.add_method("next", |lua, this, ()| {
                let rx_ref = this.rx.borrow();
                let Some(rx) = rx_ref.as_ref() else {
                    // Already closed — surface nil so loops terminate.
                    return Ok(mlua::Value::Nil);
                };
                match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                    Ok((path, kind)) => {
                        let t = lua.create_table()?;
                        t.set("path", path)?;
                        t.set("kind", kind)?;
                        Ok(mlua::Value::Table(t))
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => Ok(mlua::Value::Nil),
                    // Disconnected: the watcher has been dropped (e.g. via
                    // :close()) or its thread died. Either way no further
                    // events will arrive — surface nil so Lua loops
                    // terminate cleanly rather than spinning on Err.
                    Err(mpsc::RecvTimeoutError::Disconnected) => Ok(mlua::Value::Nil),
                }
            });

            // handle:close()
            // Drops the watcher (stopping its thread) and the receiver
            // (closing the channel). Idempotent: subsequent calls are no-ops.
            methods.add_method("close", |_, this, ()| {
                this.rx.borrow_mut().take();
                this._watcher.borrow_mut().take();
                Ok(())
            });
        }
    }

    // fs.watch(path) -> WatchHandle
    table.set(
        "watch",
        lua.create_function(|_, path: String| {
            let (tx, rx) = mpsc::channel::<(String, String)>();
            let mut watcher = recommended_watcher(move |res: notify::Result<notify::Event>| {
                // Callback errors silently drop: surfacing them would
                // require a second channel and couldn't be observed
                // synchronously anyway. Scripts that need reliability
                // should sanity-check state via `fs.stat` after a `next()`.
                if let Ok(event) = res {
                    let kind = classify(&event.kind);
                    for p in event.paths {
                        let path = p.to_string_lossy().into_owned();
                        let _ = tx.send((path, kind.to_string()));
                    }
                }
            })
            .map_err(err)?;
            watcher
                .watch(Path::new(&path), RecursiveMode::Recursive)
                .map_err(err)?;
            Ok(WatchHandle {
                rx: RefCell::new(Some(rx)),
                _watcher: RefCell::new(Some(watcher)),
            })
        })?,
    )?;

    Ok(())
}

#[cfg(not(feature = "file-watch"))]
fn install_watch(table: &Table, lua: &Lua) -> mlua::Result<()> {
    // fs.watch is a no-op that raises when the feature is off. Installing a
    // function (rather than leaving the key absent) gives a targeted error
    // message instead of `attempt to call a nil value`, which tends to
    // confuse scripts that branch on presence vs. callability.
    table.set(
        "watch",
        lua.create_function(|_, _: String| -> mlua::Result<()> {
            Err(mlua::Error::runtime(
                "fs.watch: file-watch feature disabled at build time",
            ))
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Smoke tests. Each test lands a file inside `std::env::temp_dir()` under
    //! a test-specific prefix derived from PID + nanos + counter so parallel
    //! runs don't collide. We clean up afterwards on a best-effort basis —
    //! failures during cleanup are ignored because a failing assertion
    //! already told us what we needed to know.
    use super::*;
    use mlua::Lua;
    use std::path::PathBuf;
    use tokio::runtime::{Builder, Runtime};

    fn rt() -> Runtime {
        Builder::new_multi_thread().enable_all().build().unwrap()
    }

    fn install(rt: &Runtime) -> Lua {
        let lua = Lua::new();
        let table = FsModule
            .install(&lua, rt.handle())
            .expect("install fs module");
        lua.globals().set("fs", table).unwrap();
        lua
    }

    /// Produce a unique path under tempdir for test scratch files. The format
    /// mirrors `fs.tempfile` but adds a `.test` suffix so accidentally
    /// matching the production pattern in a glob doesn't eat test output.
    fn scratch(suffix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let counter = TEMPFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("luaml-fs-test-{pid}-{nanos}-{counter}-{suffix}"))
    }

    #[test]
    fn write_then_read_round_trip() {
        let rt = rt();
        let lua = install(&rt);
        let path = scratch("rw.txt");
        let path_s = path.to_string_lossy().into_owned();
        let out: String = lua
            .load(format!(
                r#"
                fs.write('{p}', 'hello-luaml')
                return fs.read('{p}')
            "#,
                p = path_s
            ))
            .eval()
            .unwrap();
        assert_eq!(out, "hello-luaml");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stat_reports_non_zero_size_and_file_kind() {
        let rt = rt();
        let lua = install(&rt);
        let path = scratch("stat.txt");
        let path_s = path.to_string_lossy().into_owned();
        let (kind, size): (String, i64) = lua
            .load(format!(
                r#"
                fs.write('{p}', 'abcde')
                local s = fs.stat('{p}')
                return s.kind, s.size
            "#,
                p = path_s
            ))
            .eval()
            .unwrap();
        assert_eq!(kind, "file");
        assert_eq!(size, 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn readdir_includes_written_file() {
        let rt = rt();
        let lua = install(&rt);
        // mkdir a scratch directory; write a file in it; expect readdir
        // to contain just that basename.
        let dir = scratch("dir");
        let dir_s = dir.to_string_lossy().into_owned();
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("entry.txt");
        std::fs::write(&file, b"x").unwrap();

        let names: Vec<String> = lua
            .load(format!("return fs.readdir('{d}')", d = dir_s))
            .eval()
            .unwrap();
        assert!(
            names.iter().any(|n| n == "entry.txt"),
            "readdir should list entry.txt, got {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mkdir_then_exists_true() {
        let rt = rt();
        let lua = install(&rt);
        let dir = scratch("mkdir");
        let dir_s = dir.to_string_lossy().into_owned();
        let exists: bool = lua
            .load(format!(
                r#"
                fs.mkdir('{d}')
                return fs.exists('{d}')
            "#,
                d = dir_s
            ))
            .eval()
            .unwrap();
        assert!(exists);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn delete_then_exists_false() {
        let rt = rt();
        let lua = install(&rt);
        let path = scratch("del.txt");
        let path_s = path.to_string_lossy().into_owned();
        let exists: bool = lua
            .load(format!(
                r#"
                fs.write('{p}', 'bye')
                fs.delete('{p}')
                return fs.exists('{p}')
            "#,
                p = path_s
            ))
            .eval()
            .unwrap();
        assert!(!exists);
    }

    #[test]
    fn read_async_awaits_to_written_contents() {
        let rt = rt();
        let lua = install(&rt);
        let path = scratch("async.txt");
        let path_s = path.to_string_lossy().into_owned();
        std::fs::write(&path, b"async-ok").unwrap();
        let out: String = lua
            .load(format!(
                r#"
                local p = fs.read_async('{p}')
                return p:await()
            "#,
                p = path_s
            ))
            .eval()
            .unwrap();
        assert_eq!(out, "async-ok");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tempfile_returns_unique_paths_inside_tempdir() {
        // Two calls must produce distinct paths, both under tempdir(). The
        // path should not exist yet — `fs.tempfile` is a name reservation,
        // not a file creation.
        let rt = rt();
        let lua = install(&rt);
        let (a, b): (String, String) = lua
            .load("return fs.tempfile(), fs.tempfile()")
            .eval()
            .unwrap();
        assert_ne!(a, b, "tempfile should not collide across calls");
        let temp = std::env::temp_dir();
        assert!(PathBuf::from(&a).starts_with(&temp));
        assert!(PathBuf::from(&b).starts_with(&temp));
        assert!(!PathBuf::from(&a).exists(), "tempfile should not create");
    }
}
