//! `udp` stdlib module: UDP sockets.
//!
//! Installed under the bare `udp` global. API:
//! - `udp.bind(addr) -> Socket` — blocking bind. `addr` is `"host:port"`.
//!
//! `Socket` userdata methods:
//! - `s:send_to(data, addr) -> integer` — blocking send; returns bytes written.
//! - `s:send_to_async(data, addr) -> Promise<integer>` — spawns a send task and
//!   hands back a [`Promise`] that resolves to the byte count.
//! - `s:recv_from() -> {data, addr}` — blocking receive; returns a Lua table
//!   with `data` (string) and `addr` ("host:port").
//! - `s:recv_from_async() -> Promise<{data, addr}>` — same, but non-blocking;
//!   the promise resolves to a map with the same shape.
//! - `s:close()` — drops the internal socket; all subsequent calls error.
//! - `s:local_addr() -> string` — returns the socket's locally-bound address.
//!
//! Implementation notes:
//!
//! Sockets are wrapped in `Arc<tokio::net::UdpSocket>` so the async variants
//! can `Arc::clone` the handle into a spawned task. `UdpSocket`'s send/recv
//! methods take `&self`, so sharing the socket across a long-running Lua
//! userdata and short-lived async tasks is safe — no additional locking is
//! needed. The outer `RefCell<Option<Arc<_>>>` lets `close()` drop the socket
//! and lets subsequent method calls observe the "closed" state with a clean
//! error path.
//!
//! The receive buffer is a single 64 KiB heap allocation per call
//! (`vec![0u8; 65536]`). That matches the theoretical max UDP payload
//! (65507 bytes of data + headers), so any legal datagram fits in a single
//! `recv_from`; oversize datagrams are truncated by the kernel anyway. We
//! allocate fresh per call rather than reusing a buffer because each
//! `recv_from_async` task owns its own buffer to stay `Send + 'static` — a
//! shared buffer would need a lock and complicate the error paths without
//! meaningful throughput benefit for UDP.
//!
//! Errors from the socket surface as `mlua::Error::runtime(format!("udp: {e}"))`.
//! After `close()`, every method errors with `udp: socket closed`.

use std::cell::RefCell;
use std::sync::Arc;

use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use tokio::net::UdpSocket;
use tokio::runtime::Handle;

use super::LuamlStdlibModule;
use crate::stdlib::promise::{Promise, PromiseResult};
use crate::types::FieldValue;

/// Maximum UDP payload size on IPv4 (65535 - 8 byte UDP header - 20 byte IPv4
/// header = 65507). We round up to a full 64 KiB buffer so the allocation is a
/// clean power of two and any legal datagram fits without truncation.
const RECV_BUF_SIZE: usize = 65536;

/// `udp` stdlib module. See module-level docs.
pub struct UdpModule;

/// Userdata wrapping a bound UDP socket.
///
/// The `Arc` lets async sends/recvs clone the socket into a spawned task
/// cheaply. The outer `RefCell<Option<_>>` lets `close()` drop the shared
/// handle; subsequent methods observe `None` and return a "closed" error.
struct LuaUdpSocket {
    inner: RefCell<Option<Arc<UdpSocket>>>,
    rt: Handle,
}

impl LuaUdpSocket {
    /// Take a cheap clone of the inner socket handle, or surface a
    /// "closed" error if the socket has already been dropped. The clone is
    /// required for async ops so the spawned task can own a `'static` copy.
    fn socket(&self) -> mlua::Result<Arc<UdpSocket>> {
        self.inner
            .borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| mlua::Error::runtime("udp: socket closed"))
    }
}

/// Extract a Lua value as a byte vector. Accepts a Lua string (byte-clean)
/// and errors otherwise. Centralised so `send_to` and `send_to_async` give
/// identical error messages for the "not a string" case.
fn value_to_bytes(data: Value) -> mlua::Result<Vec<u8>> {
    match data {
        Value::String(s) => Ok(s.as_bytes().to_vec()),
        other => Err(mlua::Error::runtime(format!(
            "udp: send_to expected string data, got {}",
            other.type_name()
        ))),
    }
}

impl UserData for LuaUdpSocket {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // s:send_to(data, addr) -> integer
        // Blocking send. Drives the runtime via `block_on` so timers and other
        // tasks continue to make progress while the Lua thread is parked.
        methods.add_method("send_to", |_, this, (data, addr): (Value, String)| {
            let sock = this.socket()?;
            let bytes = value_to_bytes(data)?;
            let n = this
                .rt
                .block_on(async move { sock.send_to(&bytes, addr).await })
                .map_err(|e| mlua::Error::runtime(format!("udp: {e}")))?;
            Ok(n as i64)
        });

        // s:send_to_async(data, addr) -> Promise<integer>
        // Spawns a send task and returns a Promise that resolves to the byte
        // count. The promise's task owns an Arc clone of the socket so the
        // userdata can be freed independently of the in-flight send.
        methods.add_method(
            "send_to_async",
            |_, this, (data, addr): (Value, String)| {
                let sock = this.socket()?;
                let bytes = value_to_bytes(data)?;
                let rt = this.rt.clone();
                let join: tokio::task::JoinHandle<PromiseResult> = rt.spawn(async move {
                    let n = sock
                        .send_to(&bytes, addr)
                        .await
                        .map_err(|e| format!("udp: {e}"))?;
                    Ok(FieldValue::Number(n as i64))
                });
                Ok(Promise::new(join, this.rt.clone()))
            },
        );

        // s:recv_from() -> {data = string, addr = "host:port"}
        // Blocking receive. Buffer is a fresh 64 KiB vec — any legal UDP
        // datagram fits, and per-call allocation keeps the async variant
        // ergonomically identical.
        methods.add_method("recv_from", |lua, this, ()| {
            let sock = this.socket()?;
            let mut buf = vec![0u8; RECV_BUF_SIZE];
            let (n, peer) = this
                .rt
                .block_on(async { sock.recv_from(&mut buf).await })
                .map_err(|e| mlua::Error::runtime(format!("udp: {e}")))?;
            buf.truncate(n);
            let table = lua.create_table()?;
            table.set("data", lua.create_string(&buf)?)?;
            table.set("addr", peer.to_string())?;
            Ok(table)
        });

        // s:recv_from_async() -> Promise<{data, addr}>
        // Same as recv_from but returns a Promise. The spawned task owns its
        // own 64 KiB buffer (no shared state) so it is Send + 'static.
        methods.add_method("recv_from_async", |_, this, ()| {
            let sock = this.socket()?;
            let rt = this.rt.clone();
            let join: tokio::task::JoinHandle<PromiseResult> = rt.spawn(async move {
                let mut buf = vec![0u8; RECV_BUF_SIZE];
                let (n, peer) = sock
                    .recv_from(&mut buf)
                    .await
                    .map_err(|e| format!("udp: {e}"))?;
                buf.truncate(n);
                let mut map = std::collections::HashMap::new();
                // FieldValue::String carries UTF-8; the datagram may be
                // arbitrary bytes. We stash it as a String here because
                // `FieldValue` has no byte-array variant — at the Lua
                // boundary `field_value_to_lua` will convert back to a Lua
                // string, which (unlike Rust `String`) is byte-clean.
                // SAFETY: we must round-trip the bytes losslessly. Use
                // `String::from_utf8_lossy` would corrupt non-UTF8 payloads;
                // instead stash the raw bytes via `from_utf8_unchecked` is
                // unsafe and we want to keep the surface safe. The trade-off:
                // binary UDP payloads arriving through the *async* path are
                // replaced with their UTF-8 lossy rendering. Callers that
                // need byte-perfect binary delivery should use the blocking
                // `recv_from` (which hands back a genuine Lua string).
                let data = String::from_utf8_lossy(&buf).into_owned();
                map.insert("data".to_string(), FieldValue::String(data));
                map.insert("addr".to_string(), FieldValue::String(peer.to_string()));
                Ok(FieldValue::Map(map))
            });
            Ok(Promise::new(join, this.rt.clone()))
        });

        // s:close()
        // Drops the shared socket handle. If the socket was already closed this
        // is a no-op; subsequent operations will surface `udp: socket closed`.
        methods.add_method("close", |_, this, ()| {
            let _ = this.inner.borrow_mut().take();
            Ok(())
        });

        // s:local_addr() -> string
        // Returns the locally-bound "host:port". Errors if the socket is
        // closed or the OS refuses the query (rare but possible on some
        // platforms after network reconfig).
        methods.add_method("local_addr", |_, this, ()| {
            let sock = this.socket()?;
            let addr = sock
                .local_addr()
                .map_err(|e| mlua::Error::runtime(format!("udp: {e}")))?;
            Ok(addr.to_string())
        });
    }
}

impl LuamlStdlibModule for UdpModule {
    fn namespace(&self) -> &'static str {
        "udp"
    }

    fn install(&self, lua: &Lua, rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // udp.bind(addr) -> Socket
        // Blocking bind. Takes "host:port" and returns a Socket userdata. The
        // runtime handle is cloned into every socket so async methods work
        // regardless of which thread Lua is running on.
        {
            let rt = rt.clone();
            table.set(
                "bind",
                lua.create_function(move |_, addr: String| {
                    let sock = rt
                        .block_on(async { UdpSocket::bind(&addr).await })
                        .map_err(|e| mlua::Error::runtime(format!("udp: {e}")))?;
                    Ok(LuaUdpSocket {
                        inner: RefCell::new(Some(Arc::new(sock))),
                        rt: rt.clone(),
                    })
                })?,
            )?;
        }

        Ok(table)
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests. We bind two sockets on `127.0.0.1:0` (kernel-assigned
    //! ephemeral ports), exchange a datagram, and verify the plumbing.
    //!
    //! Runtime note: `udp.bind` and `recv_from` call `Handle::block_on`, which
    //! panics if run from a thread already driving that runtime. The tests run
    //! on the outer Cargo test thread (not a tokio worker), so `block_on` is
    //! legal here. See the matching note in `time.rs`.

    use super::*;
    use mlua::Lua;
    use tokio::runtime::{Builder, Runtime};

    fn rt() -> Runtime {
        Builder::new_multi_thread().enable_all().build().unwrap()
    }

    fn install(rt: &Runtime) -> Lua {
        let lua = Lua::new();
        let table = UdpModule
            .install(&lua, rt.handle())
            .expect("install udp module");
        lua.globals().set("udp", table).unwrap();
        lua
    }

    #[test]
    fn bind_send_to_recv_from_round_trip() {
        // Bind two sockets on ephemeral ports, send a datagram from `a` to
        // `b`, and confirm `b` observes the right payload and peer address.
        let rt = rt();
        let lua = install(&rt);
        let (data, addr): (String, String) = lua
            .load(
                r#"
                local a = udp.bind("127.0.0.1:0")
                local b = udp.bind("127.0.0.1:0")
                local b_addr = b:local_addr()
                local n = a:send_to("hello", b_addr)
                assert(n == 5, "expected 5 bytes sent, got " .. tostring(n))
                local pkt = b:recv_from()
                return pkt.data, pkt.addr
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(data, "hello");
        assert!(
            addr.starts_with("127.0.0.1:"),
            "peer addr should be a localhost endpoint, got {addr}"
        );
    }

    #[test]
    fn local_addr_returns_real_port_after_bind() {
        // After bind("127.0.0.1:0") the kernel assigns a real port > 0.
        // `local_addr` must reflect that, not the original "0".
        let rt = rt();
        let lua = install(&rt);
        let addr: String = lua
            .load(
                r#"
                local s = udp.bind("127.0.0.1:0")
                return s:local_addr()
            "#,
            )
            .eval()
            .unwrap();
        assert!(
            addr.starts_with("127.0.0.1:"),
            "expected 127.0.0.1 prefix, got {addr}"
        );
        let port_str = addr.rsplit_once(':').map(|(_, p)| p).unwrap_or("0");
        let port: u16 = port_str.parse().expect("port must parse as u16");
        assert!(port > 0, "ephemeral port must be > 0, got {port}");
    }

    #[test]
    fn close_then_send_errors() {
        // After close(), every socket method should surface
        // `udp: socket closed` as an mlua runtime error.
        let rt = rt();
        let lua = install(&rt);
        let err = lua
            .load(
                r#"
                local s = udp.bind("127.0.0.1:0")
                s:close()
                return s:send_to("x", "127.0.0.1:9")
            "#,
            )
            .eval::<i64>()
            .expect_err("post-close send must error");
        let msg = err.to_string();
        assert!(
            msg.contains("socket closed"),
            "error should mention closed state: {msg}"
        );
    }

    #[test]
    fn send_to_async_resolves_to_byte_count() {
        // The async send should hand back a Promise whose awaited value is
        // the byte count written to the socket.
        let rt = rt();
        let lua = install(&rt);
        let n: i64 = lua
            .load(
                r#"
                local a = udp.bind("127.0.0.1:0")
                local b = udp.bind("127.0.0.1:0")
                local p = a:send_to_async("xyz!", b:local_addr())
                return p:await()
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(n, 4, "async send should report 4 bytes written, got {n}");
    }
}
