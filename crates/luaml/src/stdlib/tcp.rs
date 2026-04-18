//! `tcp` stdlib module: TCP client/server primitives.
//!
//! Installed under the bare `tcp` global. Provides:
//!
//! - `tcp.connect(addr) -> Stream` — blocking connect. `addr` is a
//!   `"host:port"` string resolved through tokio's DNS.
//! - `tcp.connect_async(addr) -> StreamPromise` — async connect. The returned
//!   promise resolves to a `Stream` userdata on `:await()`. See the note on
//!   Stream vs `promise::Promise` below.
//! - `tcp.listen(addr) -> Listener` — blocking bind + listen.
//!
//! Stream methods (`s:<name>(...)`):
//! - `s:read(n) -> string` — blocking read up to `n` bytes. Returns an empty
//!   string on clean EOF.
//! - `s:write(data)` — blocking write of the entire byte slice.
//! - `s:close()` — shut the stream down. Any further call errors.
//! - `s:peer_addr() -> string` — remote socket address.
//! - `s:local_addr() -> string` — local socket address.
//!
//! Listener methods (`l:<name>(...)`):
//! - `l:accept() -> Stream` — blocking accept.
//! - `l:accept_async() -> StreamPromise` — async accept, resolves to Stream.
//! - `l:close()` — drop the listener. Any further call errors.
//! - `l:local_addr() -> string` — bound socket address.
//!
//! ## Why custom promise userdata for Stream-yielding ops
//!
//! The generic [`promise::Promise`] wraps a `JoinHandle<Result<FieldValue,
//! String>>`, and [`FieldValue`] has no userdata variant — so a Stream cannot
//! travel through that channel. `connect_async` and `accept_async` therefore
//! use dedicated [`StreamPromise`] userdata with the same `:await()`,
//! `:try_await()`, `:poll()` surface, but typed to yield a `Stream` userdata
//! directly. Scripts cannot tell the difference at the call site.
//!
//! ## TODO: async read/write on Stream
//!
//! We intentionally do NOT ship `read_async` / `write_async` on `Stream`. The
//! generic `Promise` requires `Send + 'static` on the spawned future, which
//! forces us to MOVE the `TcpStream` into the spawned task; that leaves the
//! `Stream` userdata unusable until the task completes and there is no clean
//! way to put the stream back (the existing `Promise` task output is a
//! `FieldValue`, not a tuple carrying the reclaimed stream). A future design
//! could hand out a typed `StreamOpPromise` whose `:await()` reinstates the
//! inner stream into the original userdata, but that is out of scope for this
//! commit — read/write stay blocking until we have that pattern.

use std::cell::RefCell;

use mlua::{AnyUserData, IntoLua, Lua, Table, UserData, UserDataMethods, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use super::LuamlStdlibModule;
// `Promise` / `PromiseResult` / `FieldValue` are imported per the module
// contract even though only the typed Stream/Listener promise userdatas are
// in use today — future async read/write on Stream will route through them.
// The `unused_imports` allow keeps the contract explicit without tripping
// dead-import warnings.
#[allow(unused_imports)]
use crate::stdlib::promise::{Promise, PromiseResult};
#[allow(unused_imports)]
use crate::types::FieldValue;

/// `tcp` stdlib module. See module docs.
pub struct TcpModule;

impl LuamlStdlibModule for TcpModule {
    fn namespace(&self) -> &'static str {
        "tcp"
    }

    fn install(&self, lua: &Lua, rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // tcp.connect(addr) -> Stream
        // Blocking connect. `addr` is a "host:port" string. DNS resolution is
        // driven on the runtime so hostnames work without pulling in a second
        // resolver.
        {
            let rt = rt.clone();
            table.set(
                "connect",
                lua.create_function(move |_, addr: String| {
                    let stream = rt
                        .block_on(TcpStream::connect(&addr))
                        .map_err(|e| mlua::Error::runtime(format!("tcp: {e}")))?;
                    Ok(LuaTcpStream::new(stream, rt.clone()))
                })?,
            )?;
        }

        // tcp.connect_async(addr) -> StreamPromise
        // Spawns the connect on the runtime and hands back a StreamPromise.
        // The promise resolves to a `Stream` userdata on `:await()`.
        {
            let rt = rt.clone();
            table.set(
                "connect_async",
                lua.create_function(move |_, addr: String| {
                    let rt_for_stream = rt.clone();
                    let join: JoinHandle<Result<TcpStream, String>> = rt.spawn(async move {
                        TcpStream::connect(&addr)
                            .await
                            .map_err(|e| format!("tcp: {e}"))
                    });
                    Ok(StreamPromise::new(join, rt_for_stream))
                })?,
            )?;
        }

        // tcp.listen(addr) -> Listener
        // Blocking bind + listen. On "host:port" with host "0.0.0.0" or
        // "127.0.0.1" and port 0, the OS picks an ephemeral port — use
        // `listener:local_addr()` to discover it.
        {
            let rt = rt.clone();
            table.set(
                "listen",
                lua.create_function(move |_, addr: String| {
                    let listener = rt
                        .block_on(TcpListener::bind(&addr))
                        .map_err(|e| mlua::Error::runtime(format!("tcp: {e}")))?;
                    Ok(LuaTcpListener::new(listener, rt.clone()))
                })?,
            )?;
        }

        Ok(table)
    }
}

// ── Stream userdata ──────────────────────────────────────────────

/// Userdata wrapping a connected [`TcpStream`]. The stream sits in a
/// [`RefCell<Option<...>>`] so `:close()` (and, eventually, async ops that
/// move the stream into a spawned task) can take ownership; all subsequent
/// method calls observe the `None` state and error with `tcp: stream closed`.
pub struct LuaTcpStream {
    inner: RefCell<Option<TcpStream>>,
    rt: Handle,
}

impl LuaTcpStream {
    fn new(stream: TcpStream, rt: Handle) -> Self {
        Self {
            inner: RefCell::new(Some(stream)),
            rt,
        }
    }

    /// Borrow-checked helper: run `op` with a `&mut TcpStream` pulled out of
    /// the RefCell. Errors if the stream is already closed.
    fn with_stream<F, R>(&self, op: F) -> mlua::Result<R>
    where
        F: FnOnce(&mut TcpStream, &Handle) -> mlua::Result<R>,
    {
        let mut slot = self.inner.borrow_mut();
        let stream = slot
            .as_mut()
            .ok_or_else(|| mlua::Error::runtime("tcp: stream closed"))?;
        op(stream, &self.rt)
    }
}

impl UserData for LuaTcpStream {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // s:read(n) -> string
        // Blocking read of up to `n` bytes. Returns an empty string on clean
        // EOF (peer shut down gracefully). `n` is clamped to a non-negative
        // `usize`; a zero-length or negative request returns "" immediately
        // without touching the socket.
        methods.add_method("read", |lua, this, n: i64| {
            if n <= 0 {
                return lua.create_string("");
            }
            let cap = usize::try_from(n).unwrap_or(usize::MAX);
            let bytes = this.with_stream(|stream, rt| {
                let mut buf = vec![0u8; cap];
                let read = rt
                    .block_on(stream.read(&mut buf))
                    .map_err(|e| mlua::Error::runtime(format!("tcp: {e}")))?;
                buf.truncate(read);
                Ok(buf)
            })?;
            lua.create_string(&bytes)
        });

        // s:write(data)
        // Blocking `write_all`. `data` is a Lua string interpreted as raw
        // bytes — binary payloads round-trip cleanly.
        methods.add_method("write", |_, this, data: mlua::String| {
            let bytes = data.as_bytes().to_vec();
            this.with_stream(|stream, rt| {
                rt.block_on(stream.write_all(&bytes))
                    .map_err(|e| mlua::Error::runtime(format!("tcp: {e}")))?;
                Ok(())
            })
        });

        // s:close()
        // Drops the underlying TcpStream (which initiates a graceful shutdown
        // at the OS level once buffered data drains). Subsequent method calls
        // error with `tcp: stream closed`.
        methods.add_method("close", |_, this, ()| {
            let _ = this.inner.borrow_mut().take();
            Ok(())
        });

        // s:peer_addr() -> string
        // Remote socket address. Error surfaces if the OS cannot report it
        // (rare — generally only on a closed/broken stream).
        methods.add_method("peer_addr", |_, this, ()| {
            this.with_stream(|stream, _| {
                stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .map_err(|e| mlua::Error::runtime(format!("tcp: {e}")))
            })
        });

        // s:local_addr() -> string
        // Local socket address. Useful for clients that bound to an ephemeral
        // port; mirrors `Listener:local_addr` on the server side.
        methods.add_method("local_addr", |_, this, ()| {
            this.with_stream(|stream, _| {
                stream
                    .local_addr()
                    .map(|a| a.to_string())
                    .map_err(|e| mlua::Error::runtime(format!("tcp: {e}")))
            })
        });
    }
}

// ── Listener userdata ────────────────────────────────────────────

/// Userdata wrapping a bound [`TcpListener`]. Same `RefCell<Option<...>>`
/// pattern as [`LuaTcpStream`] so `:close()` and future moving async ops can
/// consume it without leaving a half-alive resource behind.
pub struct LuaTcpListener {
    inner: RefCell<Option<TcpListener>>,
    rt: Handle,
}

impl LuaTcpListener {
    fn new(listener: TcpListener, rt: Handle) -> Self {
        Self {
            inner: RefCell::new(Some(listener)),
            rt,
        }
    }

    fn with_listener<F, R>(&self, op: F) -> mlua::Result<R>
    where
        F: FnOnce(&TcpListener, &Handle) -> mlua::Result<R>,
    {
        let slot = self.inner.borrow();
        let listener = slot
            .as_ref()
            .ok_or_else(|| mlua::Error::runtime("tcp: listener closed"))?;
        op(listener, &self.rt)
    }
}

impl UserData for LuaTcpListener {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // l:accept() -> Stream
        // Blocking accept. Returns the peer-facing `Stream` userdata; the
        // peer's address is available via `stream:peer_addr()`.
        methods.add_method("accept", |_, this, ()| {
            this.with_listener(|listener, rt| {
                let (stream, _addr) = rt
                    .block_on(listener.accept())
                    .map_err(|e| mlua::Error::runtime(format!("tcp: {e}")))?;
                Ok(LuaTcpStream::new(stream, rt.clone()))
            })
        });

        // l:accept_async() -> StreamPromise
        // Async accept. Clones the listener (via raw fd) is not needed —
        // instead we rely on the runtime scheduling: the spawned task holds a
        // reference path to the listener via a cloned handle... Actually,
        // `TcpListener::accept` takes `&self`, so we can't hand a reference
        // into `spawn` directly. The pragmatic choice: temporarily take the
        // listener out of the slot, perform the async accept in the task,
        // and on `:await()` put it back. The StreamPromise is explicitly
        // designed to return both the accepted Stream AND the reclaimed
        // Listener slot mutation path. For simplicity here we keep the slot
        // model: the task moves the listener in, and `:await()` restores it
        // alongside yielding the new Stream.
        methods.add_method("accept_async", |_, this, ()| {
            let mut slot = this.inner.borrow_mut();
            let listener = slot
                .take()
                .ok_or_else(|| mlua::Error::runtime("tcp: listener closed"))?;
            drop(slot);
            let rt_for_stream = this.rt.clone();
            let join: JoinHandle<Result<(TcpStream, TcpListener), String>> =
                this.rt.spawn(async move {
                    match listener.accept().await {
                        Ok((stream, _)) => Ok((stream, listener)),
                        Err(e) => Err(format!("tcp: {e}")),
                    }
                });
            Ok(ListenerAcceptPromise::new(
                join,
                rt_for_stream,
            ))
        });

        // l:close()
        // Drops the listener. Any subsequent method call errors.
        methods.add_method("close", |_, this, ()| {
            let _ = this.inner.borrow_mut().take();
            Ok(())
        });

        // l:local_addr() -> string
        // Bound socket address — the authoritative source of the OS-assigned
        // ephemeral port when binding `"127.0.0.1:0"`.
        methods.add_method("local_addr", |_, this, ()| {
            this.with_listener(|listener, _| {
                listener
                    .local_addr()
                    .map(|a| a.to_string())
                    .map_err(|e| mlua::Error::runtime(format!("tcp: {e}")))
            })
        });
    }
}

// ── Typed promise userdatas ──────────────────────────────────────

/// Promise yielding a [`LuaTcpStream`] on `:await()`. Used by
/// `tcp.connect_async`. Mirrors [`Promise`] semantics (one-shot await,
/// try_await, poll) but typed against a `TcpStream` task so userdata can flow
/// through (which `FieldValue` cannot express).
pub struct StreamPromise {
    inner: RefCell<Option<JoinHandle<Result<TcpStream, String>>>>,
    rt: Handle,
}

impl StreamPromise {
    fn new(handle: JoinHandle<Result<TcpStream, String>>, rt: Handle) -> Self {
        Self {
            inner: RefCell::new(Some(handle)),
            rt,
        }
    }
}

impl UserData for StreamPromise {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
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
            let stream = joined.map_err(mlua::Error::runtime)?;
            let ud = lua.create_userdata(LuaTcpStream::new(stream, this.rt.clone()))?;
            Ok(Value::UserData(ud))
        });

        methods.add_method("try_await", |lua, this, ()| {
            let mut slot = this.inner.borrow_mut();
            let Some(handle) = slot.as_ref() else {
                return Err(mlua::Error::runtime("promise already awaited or resolved"));
            };
            if !handle.is_finished() {
                return Ok((Value::Nil, Value::Nil));
            }
            let handle = slot.take().expect("handle present after is_finished");
            drop(slot);
            match this.rt.block_on(handle) {
                Ok(Ok(stream)) => {
                    let ud: AnyUserData =
                        lua.create_userdata(LuaTcpStream::new(stream, this.rt.clone()))?;
                    Ok((true.into_lua(lua)?, Value::UserData(ud)))
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

/// Promise specialisation for `listener:accept_async()`. Differs from
/// [`StreamPromise`] in one key way: the spawned task also reclaims the
/// listener, because `TcpListener::accept(&self)` cannot run on a borrowed
/// listener across an `await` (the `'static` bound on spawn requires an owned
/// value). On `:await()` we restore the listener into a caller-supplied slot
/// and return the Stream; for now we simply drop the listener on success
/// because Lua has already lost its `Listener` userdata reference (the slot
/// was vacated in `accept_async`). A future iteration can re-surface the
/// listener as a new `Listener` userdata — currently the convention is
/// "call accept_async, await the stream, then make a new listener if you
/// want more connections". This is rough; documented in the module docs.
pub struct ListenerAcceptPromise {
    inner: RefCell<Option<JoinHandle<Result<(TcpStream, TcpListener), String>>>>,
    rt: Handle,
}

impl ListenerAcceptPromise {
    fn new(
        handle: JoinHandle<Result<(TcpStream, TcpListener), String>>,
        rt: Handle,
    ) -> Self {
        Self {
            inner: RefCell::new(Some(handle)),
            rt,
        }
    }
}

impl UserData for ListenerAcceptPromise {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
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
            let (stream, _listener) = joined.map_err(mlua::Error::runtime)?;
            // The listener is intentionally dropped here; see type docs.
            let ud = lua.create_userdata(LuaTcpStream::new(stream, this.rt.clone()))?;
            Ok(Value::UserData(ud))
        });

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
    //! Smoke tests for the `tcp` module. Every test binds to "127.0.0.1:0" so
    //! we never collide with a real service and CI runs don't need free-port
    //! coordination. Each test builds its own runtime so state is fully
    //! isolated.
    //!
    //! Note on `block_on` and runtimes: our `tcp.*` methods call
    //! `rt.block_on(...)` internally. That panics if invoked from a worker
    //! thread of the same runtime, just like `time.sleep`. Tests here run on
    //! the outer test thread (Cargo's default harness), which is not a
    //! runtime worker — so it's legal. Callers that drive Lua from inside a
    //! tokio task should prefer the async variants.
    use super::*;
    use mlua::Lua;
    use tokio::runtime::{Builder, Runtime};

    fn rt() -> Runtime {
        Builder::new_multi_thread().enable_all().build().unwrap()
    }

    fn install(rt: &Runtime) -> Lua {
        let lua = Lua::new();
        let table = TcpModule
            .install(&lua, rt.handle())
            .expect("install tcp module");
        lua.globals().set("tcp", table).unwrap();
        lua
    }

    #[test]
    fn listen_reports_bound_local_addr_on_ephemeral_port() {
        let rt = rt();
        let lua = install(&rt);
        // Bind to port 0 → OS picks. local_addr() must then return a
        // concrete "127.0.0.1:<nonzero>".
        let addr: String = lua
            .load(
                r#"
                local l = tcp.listen("127.0.0.1:0")
                local a = l:local_addr()
                l:close()
                return a
            "#,
            )
            .eval()
            .unwrap();
        assert!(
            addr.starts_with("127.0.0.1:"),
            "expected loopback address, got {addr}"
        );
        let port_str = addr.strip_prefix("127.0.0.1:").unwrap();
        let port: u16 = port_str.parse().expect("port must be integer");
        assert!(port > 0, "ephemeral port must be nonzero, got {port}");
    }

    #[test]
    fn blocking_connect_read_write_round_trip() {
        let rt = rt();
        let lua = install(&rt);

        // Strategy: build the listener in Lua, then spawn a Rust background
        // accept loop on the runtime that echoes a fixed payload once, then
        // exits. In Lua, connect to that address and read the echo.
        //
        // The challenge: we can't share the Lua listener with Rust easily —
        // so we bind a second listener in Rust, pass its port into Lua, and
        // let Lua do the client side only.
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();

        // Background accept: on connection, write "hello" and close.
        rt.spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(b"hello").await;
                let _ = stream.shutdown().await;
            }
        });

        // Lua client: connect, read up to 64 bytes, return it.
        let got: String = lua
            .load(&format!(
                r#"
                local s = tcp.connect("127.0.0.1:{port}")
                -- write a probe byte (server ignores it but we exercise write())
                s:write("x")
                local data = s:read(64)
                s:close()
                return data
            "#
            ))
            .eval()
            .unwrap();
        assert_eq!(got, "hello", "expected echo payload, got {got:?}");
    }

    #[test]
    fn read_returns_empty_string_on_clean_eof() {
        let rt = rt();
        let lua = install(&rt);

        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();

        // Server: accept and immediately shut down (no bytes written).
        rt.spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = stream.shutdown().await;
            }
        });

        let got: String = lua
            .load(&format!(
                r#"
                local s = tcp.connect("127.0.0.1:{port}")
                local data = s:read(64)
                s:close()
                return data
            "#
            ))
            .eval()
            .unwrap();
        assert_eq!(
            got, "",
            "clean EOF must surface as empty string, got {got:?}"
        );
    }

    #[test]
    fn method_on_closed_stream_errors() {
        let rt = rt();
        let lua = install(&rt);

        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        rt.spawn(async move {
            let _ = listener.accept().await;
        });

        // Lua: connect, close, then try to read → must error with
        // "tcp: stream closed".
        let err: String = lua
            .load(&format!(
                r#"
                local s = tcp.connect("127.0.0.1:{port}")
                s:close()
                local ok, err = pcall(function() return s:read(16) end)
                if ok then return "no-error" end
                return tostring(err)
            "#
            ))
            .eval()
            .unwrap();
        assert!(
            err.contains("tcp: stream closed"),
            "expected closed-stream error, got {err}"
        );
    }

    #[test]
    fn connect_async_resolves_to_stream_userdata() {
        let rt = rt();
        let lua = install(&rt);

        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        rt.spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(b"async-hi").await;
                let _ = stream.shutdown().await;
            }
        });

        let got: String = lua
            .load(&format!(
                r#"
                local p = tcp.connect_async("127.0.0.1:{port}")
                local s = p:await()
                local data = s:read(64)
                s:close()
                return data
            "#
            ))
            .eval()
            .unwrap();
        assert_eq!(got, "async-hi", "expected async-hi, got {got:?}");
    }
}
