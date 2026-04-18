// NEW DEP: url = "2"
//
//! `url` stdlib module: URL parsing, construction, and percent-encoding.
//!
//! All functions are synchronous — URL handling is pure string/parse work
//! with no blocking I/O, so the promise machinery doesn't apply. The
//! [`Handle`] passed to [`LuamlStdlibModule::install`] is ignored.
//!
//! Methods installed under the `url` global:
//! - `url.parse(s) -> { scheme, host, port, path, query, fragment, username, password }`
//!   — fields missing from the input surface as `nil`.
//! - `url.format({ ... }) -> string` — construct a URL string from parts.
//!   Requires at minimum a `scheme` and a `host`.
//! - `url.encode(s) -> string` — percent-encode a single component.
//! - `url.decode(s) -> string` — percent-decode a string (UTF-8 lossy).
//! - `url.encode_query(t) -> string` — build an `a=1&b=2` query body from a
//!   table; keys and values are percent-encoded per `application/x-www-form-urlencoded`.
//! - `url.decode_query(s) -> { ... }` — parse an `a=1&b=2` query body into a
//!   table; repeated keys keep the last-seen value (standard form-parsing
//!   convention).
//!
//! ## Percent-encoding set
//!
//! `url.encode` / `url.decode` implement the equivalent of the
//! `percent_encoding::NON_ALPHANUMERIC` set — every byte outside `[A-Za-z0-9]`
//! is escaped, including `-`, `_`, `.`, `~`, `/`, `?`, `#`, `&`, `=`, etc.
//! This is the safest general-purpose **COMPONENT** encoder (stricter than a
//! PATH-only or QUERY-only set): scripts embed the encoded output in any URL
//! position (path segment, query value, fragment), so we pick the most
//! conservative set and let the decoder recover the original bytes. Over-
//! escaping is always safe; decoding is symmetric.
//!
//! We deliberately do **not** reach for `url::form_urlencoded::byte_serialize`
//! here because form-urlencoded turns spaces into `+` rather than `%20` —
//! that's the right contract for HTTP form bodies (which
//! `url.encode_query` / `url.decode_query` do use) but wrong for a
//! general-purpose component encoder. Rather than add a separate
//! `percent-encoding` dep on top of `url`, the encoder/decoder here is a
//! small hand-rolled helper — see [`encode_component`] / [`decode_component`].
//!
//! Parse / format both go through `url::Url`, so they inherit the RFC 3986
//! / WHATWG URL behaviour implemented there (IDNA hosts, default ports per
//! scheme, etc.). `format` validates the minimum required pair (`scheme` +
//! `host`) before constructing, and surfaces any constructor error via
//! `mlua::Error::runtime`.

use std::fmt::Write as _;

use mlua::{Lua, Table, Value};
use tokio::runtime::Handle;
use url::Url;
use url::form_urlencoded;

use super::LuamlStdlibModule;

/// Component-safe percent-encoder: escapes every byte outside `[A-Za-z0-9]`,
/// matching the behaviour of `percent_encoding::NON_ALPHANUMERIC`. Kept
/// private and small so we don't need a second dep alongside `url`.
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b;
        if c.is_ascii_alphanumeric() {
            out.push(c as char);
        } else {
            // write! into a String never fails.
            let _ = write!(out, "%{c:02X}");
        }
    }
    out
}

/// Percent-decoder. Decodes `%HH` triplets; bytes that don't start a valid
/// triplet are passed through literally. Output is utf8-lossy, matching the
/// `percent_decode_str(...).decode_utf8_lossy()` contract: invalid utf8
/// sequences become U+FFFD rather than surfacing as errors, which is the
/// right tradeoff for script callers.
fn decode_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Zero-sized module handle. Registration happens in `mod.rs` under the
/// `stdlib-url` feature flag; this type only exists to implement the trait.
pub struct UrlModule;

impl LuamlStdlibModule for UrlModule {
    fn namespace(&self) -> &'static str {
        "url"
    }

    fn install(&self, lua: &Lua, _rt: &Handle) -> mlua::Result<Table> {
        let t = lua.create_table()?;

        // url.parse(s) -> { scheme, host, port, path, query, fragment,
        //                   username, password }
        // Missing fields are nil. `path` is always present (an empty URL
        // still has a path component, though it may be "/"). `username` is
        // only surfaced when non-empty, matching intuition ("no userinfo" →
        // nil rather than ""). `password` is surfaced only when present.
        t.set(
            "parse",
            lua.create_function(|lua, s: String| {
                let parsed = Url::parse(&s)
                    .map_err(|e| mlua::Error::runtime(format!("url.parse: {e}")))?;
                let out = lua.create_table()?;

                // Scheme is always present on a parsed URL.
                out.set("scheme", parsed.scheme().to_string())?;

                // Host: None for scheme-only / opaque URLs (e.g. `mailto:`).
                if let Some(host) = parsed.host_str() {
                    out.set("host", host.to_string())?;
                }

                // Port: `port_or_known_default` would fill in 80/443 from the
                // scheme; we want the explicit port only. Scripts can derive
                // defaults themselves if they want them.
                if let Some(port) = parsed.port() {
                    out.set("port", port as i64)?;
                }

                // Path is always a string — at minimum "" for cannot-be-base
                // URLs, or "/" for standard ones. Keep it unconditional so
                // callers don't have to nil-check.
                out.set("path", parsed.path().to_string())?;

                if let Some(q) = parsed.query() {
                    out.set("query", q.to_string())?;
                }
                if let Some(f) = parsed.fragment() {
                    out.set("fragment", f.to_string())?;
                }

                // userinfo: `url::Url::username` returns "" when absent, so
                // we gate on non-empty rather than surfacing a spurious "".
                let u = parsed.username();
                if !u.is_empty() {
                    out.set("username", u.to_string())?;
                }
                if let Some(p) = parsed.password() {
                    out.set("password", p.to_string())?;
                }

                Ok(out)
            })?,
        )?;

        // url.format({ scheme = ..., host = ..., ... }) -> string
        // Minimum requirements: `scheme` and `host` must be present and
        // non-empty. Everything else is optional. We construct a minimal
        // `scheme://host/` base and then layer port / path / query /
        // fragment / userinfo on top via the typed `url::Url` setters —
        // that way we let the crate handle encoding and scheme-specific
        // rules rather than string-concatenating ourselves.
        t.set(
            "format",
            lua.create_function(|_, parts: Table| {
                let scheme: Option<String> = parts.get("scheme")?;
                let host: Option<String> = parts.get("host")?;
                let scheme = scheme
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| mlua::Error::runtime("url.format: missing 'scheme'"))?;
                let host = host
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| mlua::Error::runtime("url.format: missing 'host'"))?;

                let base = format!("{scheme}://{host}/");
                let mut url = Url::parse(&base).map_err(|e| {
                    mlua::Error::runtime(format!("url.format: invalid scheme/host: {e}"))
                })?;

                // Port: Lua numbers come through as f64; we accept i64 too.
                if let Some(port) = parts.get::<Option<i64>>("port")? {
                    let p: u16 = u16::try_from(port).map_err(|_| {
                        mlua::Error::runtime(format!("url.format: port {port} out of range"))
                    })?;
                    url.set_port(Some(p))
                        .map_err(|_| mlua::Error::runtime("url.format: scheme forbids a port"))?;
                }

                // Path: overwrite wholesale. Empty is fine; `url` will
                // normalise to "/" for http-family schemes.
                if let Some(path) = parts.get::<Option<String>>("path")? {
                    url.set_path(&path);
                }

                // Query: stored verbatim; caller owns encoding. Explicitly
                // nil/empty clears it (set_query(Some("")) sets "?", so we
                // map "" → None).
                if let Some(query) = parts.get::<Option<String>>("query")? {
                    if query.is_empty() {
                        url.set_query(None);
                    } else {
                        url.set_query(Some(&query));
                    }
                }

                if let Some(fragment) = parts.get::<Option<String>>("fragment")? {
                    if fragment.is_empty() {
                        url.set_fragment(None);
                    } else {
                        url.set_fragment(Some(&fragment));
                    }
                }

                // Userinfo: `set_username` / `set_password` return Err only
                // for cannot-be-base URLs, which we've already rejected by
                // constructing from `scheme://host/`. We still surface the
                // error to be defensive against future url-crate changes.
                if let Some(u) = parts.get::<Option<String>>("username")? {
                    if !u.is_empty() {
                        url.set_username(&u).map_err(|_| {
                            mlua::Error::runtime("url.format: cannot set username on this URL")
                        })?;
                    }
                }
                if let Some(p) = parts.get::<Option<String>>("password")? {
                    url.set_password(Some(&p)).map_err(|_| {
                        mlua::Error::runtime("url.format: cannot set password on this URL")
                    })?;
                }

                Ok(url.to_string())
            })?,
        )?;

        // url.encode(s) -> string
        // Percent-encode using a NON_ALPHANUMERIC-equivalent set. This is
        // the safest-for-any-position encoder — see module docs.
        t.set(
            "encode",
            lua.create_function(|_, s: String| Ok(encode_component(&s)))?,
        )?;

        // url.decode(s) -> string
        // UTF-8 lossy decode; invalid UTF-8 sequences in the encoded
        // payload are replaced with U+FFFD rather than surfaced as an
        // error. Strict validation isn't worth the friction for script
        // consumers; if they care, they can re-encode and compare.
        t.set(
            "decode",
            lua.create_function(|_, s: String| Ok(decode_component(&s)))?,
        )?;

        // url.encode_query({ k = v, ... }) -> string
        // application/x-www-form-urlencoded serialiser (spaces → "+", etc.).
        // Lua-table iteration order is unspecified, so tests must not assume
        // a particular key ordering. Non-string keys/values are stringified
        // by the type signature (mlua converts numbers/bools at the boundary).
        t.set(
            "encode_query",
            lua.create_function(|_, t: Table| {
                let mut ser = form_urlencoded::Serializer::new(String::new());
                // pairs() ordering is insertion-dependent in Lua 5.4 for
                // string keys, which is fine: callers who need a stable
                // serialisation should pre-sort externally.
                for pair in t.pairs::<String, String>() {
                    let (k, v) = pair?;
                    ser.append_pair(&k, &v);
                }
                Ok(ser.finish())
            })?,
        )?;

        // url.decode_query(s) -> { k = v, ... }
        // Parses application/x-www-form-urlencoded. Repeated keys keep the
        // last value (standard form-parsing convention); scripts that need
        // multi-valued keys should either encode distinct names or reach
        // for a lower-level parser.
        t.set(
            "decode_query",
            lua.create_function(|lua, s: String| {
                let out = lua.create_table()?;
                for (k, v) in form_urlencoded::parse(s.as_bytes()) {
                    out.set(k.into_owned(), Value::String(lua.create_string(v.as_ref())?))?;
                }
                Ok(out)
            })?,
        )?;

        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests: one per surface method plus a round-trip.
    //!
    //! These tests do not touch the filesystem or network — URL handling is
    //! pure string math — so no mutex or isolation is needed. They run in
    //! parallel with the rest of the suite.
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    fn install() -> Lua {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let lua = Lua::new();
        let table = UrlModule
            .install(&lua, rt.handle())
            .expect("install url module");
        lua.globals().set("url", table).unwrap();
        drop(rt);
        lua
    }

    #[test]
    fn parse_full_url_extracts_all_parts() {
        let lua = install();
        let (scheme, host, port, path, query, fragment, username, password): (
            String,
            String,
            i64,
            String,
            String,
            String,
            String,
            String,
        ) = lua
            .load(
                r#"
                local u = url.parse("https://alice:sekret@example.com:8443/a/b?x=1&y=2#frag")
                return u.scheme, u.host, u.port, u.path, u.query, u.fragment, u.username, u.password
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(scheme, "https");
        assert_eq!(host, "example.com");
        assert_eq!(port, 8443);
        assert_eq!(path, "/a/b");
        assert_eq!(query, "x=1&y=2");
        assert_eq!(fragment, "frag");
        assert_eq!(username, "alice");
        assert_eq!(password, "sekret");
    }

    #[test]
    fn parse_minimal_url_leaves_missing_fields_nil() {
        let lua = install();
        // `https://example.com/` — no port, query, fragment, userinfo.
        let (has_port, has_query, has_frag, has_user): (bool, bool, bool, bool) = lua
            .load(
                r#"
                local u = url.parse("https://example.com/")
                return u.port ~= nil, u.query ~= nil, u.fragment ~= nil, u.username ~= nil
                "#,
            )
            .eval()
            .unwrap();
        assert!(!has_port);
        assert!(!has_query);
        assert!(!has_frag);
        assert!(!has_user);
    }

    #[test]
    fn format_round_trip_preserves_core_fields() {
        let lua = install();
        // Build a URL, parse it back, check we got the same parts.
        let (scheme, host, port, path, query): (String, String, i64, String, String) = lua
            .load(
                r#"
                local s = url.format({
                    scheme = "http",
                    host = "example.org",
                    port = 8080,
                    path = "/x/y",
                    query = "a=1",
                })
                local u = url.parse(s)
                return u.scheme, u.host, u.port, u.path, u.query
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(scheme, "http");
        assert_eq!(host, "example.org");
        assert_eq!(port, 8080);
        assert_eq!(path, "/x/y");
        assert_eq!(query, "a=1");
    }

    #[test]
    fn format_requires_scheme_and_host() {
        let lua = install();
        let err = lua
            .load(r#"return url.format({ host = "example.com" })"#)
            .eval::<String>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("scheme"), "err should mention scheme: {err}");

        let err = lua
            .load(r#"return url.format({ scheme = "https" })"#)
            .eval::<String>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("host"), "err should mention host: {err}");
    }

    #[test]
    fn encode_escapes_space_and_special_chars() {
        let lua = install();
        let out: String = lua.load(r#"return url.encode("hello world")"#).eval().unwrap();
        assert_eq!(out, "hello%20world");

        // NON_ALPHANUMERIC escapes `/` and `?` too — important guarantee
        // that this is component-safe, not path-safe.
        let slashed: String = lua.load(r#"return url.encode("a/b?c")"#).eval().unwrap();
        assert_eq!(slashed, "a%2Fb%3Fc");
    }

    #[test]
    fn encode_decode_round_trip() {
        let lua = install();
        let out: String = lua
            .load(
                r#"return url.decode(url.encode("hello world & friends/?#"))"#,
            )
            .eval()
            .unwrap();
        assert_eq!(out, "hello world & friends/?#");
    }

    #[test]
    fn encode_query_two_keys_round_trip() {
        let lua = install();
        // Lua-table iteration order for string keys is insertion-ordered in
        // 5.4 mlua, but we still round-trip rather than pinning the exact
        // serialisation string to keep the test robust.
        let (a, b): (String, String) = lua
            .load(
                r#"
                local s = url.encode_query({ a = "1 plus 2", b = "x&y" })
                local t = url.decode_query(s)
                return t.a, t.b
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(a, "1 plus 2");
        assert_eq!(b, "x&y");
    }

    #[test]
    fn decode_query_parses_raw_pairs() {
        let lua = install();
        let (a, b, c): (String, String, String) = lua
            .load(
                r#"
                local t = url.decode_query("a=hello%20world&b=x%26y&c=plain")
                return t.a, t.b, t.c
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(a, "hello world");
        assert_eq!(b, "x&y");
        assert_eq!(c, "plain");
    }
}
