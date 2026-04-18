//! `codec` stdlib module: synchronous encoding conversions.
//!
//! Exposes the following Lua functions on the bare `codec` namespace:
//!
//!   - `codec.base64_encode(data, {url_safe=false, padded=true}?) -> string`
//!   - `codec.base64_decode(s) -> string`
//!   - `codec.hex_encode(data) -> string`
//!   - `codec.hex_decode(s) -> string`
//!   - `codec.utf8_valid(data) -> bool`
//!   - `codec.utf8_encode(data) -> string`
//!   - `codec.utf8_decode(s) -> {chars...}`
//!   - `codec.string_to_bytes(s) -> {...}`
//!   - `codec.bytes_to_string({...}) -> string`
//!
//! All functions are synchronous (no `Promise`). Errors are surfaced as
//! `mlua::Error::runtime`.
//!
//! ## "Bytes" in Lua
//!
//! Lua has no dedicated byte-buffer type. The codec module treats a Lua
//! **string** as the canonical byte container: strings in Lua 5.4 are
//! 8-bit-clean byte sequences that may contain any `\x00..=\xff` byte, not
//! just valid UTF-8. Every `data` argument documented as "bytes" accepts a
//! Lua string whose bytes are used directly — no encoding is presumed. Every
//! result documented as returning "raw bytes" returns a Lua string built from
//! the underlying bytes via `Lua::create_string(&[u8])`.
//!
//! The `string_to_bytes` / `bytes_to_string` helpers bridge to the alternate
//! representation — a 1-indexed array table of integers `0..=255` — for
//! callers that need to manipulate individual bytes as numbers. `utf8_encode`
//! accepts either representation; `utf8_decode` returns an array of Unicode
//! codepoints (integers), which is distinct from a byte array because each
//! element may exceed 255.

// NEW DEP: base64 = "0.22"
// NEW DEP: hex = "0.4"

use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::prelude::*;
use mlua::{Lua, Table, Value};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;

/// Stateless stdlib module installer for the `codec` namespace.
pub struct CodecModule;

impl LuamlStdlibModule for CodecModule {
    fn namespace(&self) -> &'static str {
        "codec"
    }

    fn install(&self, lua: &Lua, _rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // codec.base64_encode(data, {url_safe=false, padded=true}?) -> string
        let base64_encode_fn =
            lua.create_function(|_, (data, opts): (mlua::String, Option<Table>)| {
                let (url_safe, padded) = match opts {
                    Some(t) => {
                        let url_safe = t.get::<Option<bool>>("url_safe")?.unwrap_or(false);
                        let padded = t.get::<Option<bool>>("padded")?.unwrap_or(true);
                        (url_safe, padded)
                    }
                    None => (false, true),
                };
                let bytes = data.as_bytes();
                let out = match (url_safe, padded) {
                    (false, true) => BASE64_STANDARD.encode(&bytes),
                    (false, false) => BASE64_STANDARD_NO_PAD.encode(&bytes),
                    (true, true) => BASE64_URL_SAFE.encode(&bytes),
                    (true, false) => BASE64_URL_SAFE_NO_PAD.encode(&bytes),
                };
                Ok(out)
            })?;
        table.set("base64_encode", base64_encode_fn)?;

        // codec.base64_decode(s) -> string (raw bytes)
        // Autodetect: try standard first, fall back to url-safe. Both alphabets
        // are paired with padding-indifferent configs so padded and unpadded
        // inputs decode without explicit hints from the caller.
        let base64_decode_fn = lua.create_function(|lua, s: String| {
            let std_indifferent = GeneralPurpose::new(
                &alphabet::STANDARD,
                GeneralPurposeConfig::new()
                    .with_decode_padding_mode(DecodePaddingMode::Indifferent),
            );
            let url_indifferent = GeneralPurpose::new(
                &alphabet::URL_SAFE,
                GeneralPurposeConfig::new()
                    .with_decode_padding_mode(DecodePaddingMode::Indifferent),
            );
            let bytes = match std_indifferent.decode(s.as_bytes()) {
                Ok(b) => b,
                Err(_) => url_indifferent
                    .decode(s.as_bytes())
                    .map_err(|e| mlua::Error::runtime(format!("base64 decode error: {e}")))?,
            };
            lua.create_string(&bytes).map(Value::String)
        })?;
        table.set("base64_decode", base64_decode_fn)?;

        // codec.hex_encode(data) -> string (lowercase)
        let hex_encode_fn = lua.create_function(|_, data: mlua::String| {
            let bytes = data.as_bytes();
            Ok(hex::encode(&bytes))
        })?;
        table.set("hex_encode", hex_encode_fn)?;

        // codec.hex_decode(s) -> string (raw bytes). Accepts upper or lower case.
        // `hex::decode` is already case-insensitive, so no normalization needed.
        let hex_decode_fn = lua.create_function(|lua, s: String| {
            let bytes = hex::decode(&s)
                .map_err(|e| mlua::Error::runtime(format!("hex decode error: {e}")))?;
            lua.create_string(&bytes).map(Value::String)
        })?;
        table.set("hex_decode", hex_decode_fn)?;

        // codec.utf8_valid(data) -> bool
        let utf8_valid_fn = lua.create_function(|_, data: mlua::String| {
            let bytes = data.as_bytes();
            Ok(std::str::from_utf8(&bytes).is_ok())
        })?;
        table.set("utf8_valid", utf8_valid_fn)?;

        // codec.utf8_encode(data) -> string. Accepts either:
        //   - a Lua string that is already valid UTF-8 (returned as-is), OR
        //   - a 1-indexed table of integers `0..=255` whose bytes form valid
        //     UTF-8 (assembled and returned as a Lua string).
        // Any other shape, any out-of-range entry, or any invalid UTF-8
        // payload errors out.
        let utf8_encode_fn = lua.create_function(|lua, data: Value| match data {
            Value::String(s) => {
                let bytes = s.as_bytes();
                std::str::from_utf8(&bytes)
                    .map_err(|e| mlua::Error::runtime(format!("invalid UTF-8: {e}")))?;
                Ok(s)
            }
            Value::Table(t) => {
                let len = t.raw_len();
                let mut bytes = Vec::with_capacity(len);
                for i in 1..=(len as i64) {
                    let n: i64 = t.get(i)?;
                    if !(0..=255).contains(&n) {
                        return Err(mlua::Error::runtime(format!(
                            "byte value out of range at index {i}: {n}"
                        )));
                    }
                    bytes.push(n as u8);
                }
                std::str::from_utf8(&bytes)
                    .map_err(|e| mlua::Error::runtime(format!("invalid UTF-8: {e}")))?;
                lua.create_string(&bytes)
            }
            other => Err(mlua::Error::runtime(format!(
                "utf8_encode expects string or table, got {}",
                other.type_name()
            ))),
        })?;
        table.set("utf8_encode", utf8_encode_fn)?;

        // codec.utf8_decode(s) -> {codepoints...}. Errors on invalid UTF-8.
        let utf8_decode_fn = lua.create_function(|lua, s: mlua::String| {
            let bytes = s.as_bytes();
            let text = std::str::from_utf8(&bytes)
                .map_err(|e| mlua::Error::runtime(format!("invalid UTF-8: {e}")))?;
            let out = lua.create_table()?;
            for (i, ch) in text.chars().enumerate() {
                out.set(i as i64 + 1, ch as u32 as i64)?;
            }
            Ok(out)
        })?;
        table.set("utf8_decode", utf8_decode_fn)?;

        // codec.string_to_bytes(s) -> {...}
        let string_to_bytes_fn = lua.create_function(|lua, s: mlua::String| {
            let bytes = s.as_bytes();
            let out = lua.create_table()?;
            for (i, b) in bytes.iter().enumerate() {
                out.set(i as i64 + 1, *b as i64)?;
            }
            Ok(out)
        })?;
        table.set("string_to_bytes", string_to_bytes_fn)?;

        // codec.bytes_to_string({...}) -> string. Errors on out-of-range.
        let bytes_to_string_fn = lua.create_function(|lua, t: Table| {
            let len = t.raw_len();
            let mut bytes = Vec::with_capacity(len);
            for i in 1..=(len as i64) {
                let n: i64 = t.get(i)?;
                if !(0..=255).contains(&n) {
                    return Err(mlua::Error::runtime(format!(
                        "byte value out of range at index {i}: {n}"
                    )));
                }
                bytes.push(n as u8);
            }
            lua.create_string(&bytes).map(Value::String)
        })?;
        table.set("bytes_to_string", bytes_to_string_fn)?;

        Ok(table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    fn install() -> (tokio::runtime::Runtime, Lua) {
        let rt = Builder::new_multi_thread().enable_all().build().unwrap();
        let lua = Lua::new();
        let table = CodecModule
            .install(&lua, &rt.handle().clone())
            .expect("install codec module");
        lua.globals().set("codec", table).expect("set codec global");
        (rt, lua)
    }

    #[test]
    fn base64_round_trip() {
        let (_rt, lua) = install();
        let ok: bool = lua
            .load(
                r#"
                local s = "hello, world!"
                local enc = codec.base64_encode(s)
                local dec = codec.base64_decode(enc)
                return dec == s
                "#,
            )
            .eval()
            .expect("base64 round trip");
        assert!(ok);
    }

    #[test]
    fn hex_round_trip() {
        let (_rt, lua) = install();
        let (enc, round_trips): (String, bool) = lua
            .load(
                r#"
                local s = "hi!"
                local enc = codec.hex_encode(s)
                local dec = codec.hex_decode(enc)
                return enc, dec == s
                "#,
            )
            .eval()
            .expect("hex round trip");
        assert_eq!(enc, "686921");
        assert!(round_trips);
    }

    #[test]
    fn utf8_valid_returns_true_and_false() {
        let (_rt, lua) = install();
        let good: bool = lua
            .load(r#"return codec.utf8_valid("café")"#)
            .eval()
            .expect("valid utf8");
        assert!(good);

        // 0xFF is never a valid UTF-8 byte on its own.
        let bad: bool = lua
            .load(r#"return codec.utf8_valid("\xff\xfe\xfd")"#)
            .eval()
            .expect("invalid utf8");
        assert!(!bad);
    }

    #[test]
    fn string_to_bytes_round_trip() {
        let (_rt, lua) = install();
        let ok: bool = lua
            .load(
                r#"
                local s = "ABZ"
                local b = codec.string_to_bytes(s)
                local back = codec.bytes_to_string(b)
                return b[1] == 65 and b[2] == 66 and b[3] == 90 and back == s
                "#,
            )
            .eval()
            .expect("bytes round trip");
        assert!(ok);
    }

    #[test]
    fn base64_url_safe_option_swaps_alphabet() {
        let (_rt, lua) = install();
        // Bytes 0xFB 0xFF encode to "+/8=" in standard base64 and to "-_8="
        // in url-safe. Verifying we emit `-` and `_` (and do NOT emit `+` or
        // `/`) confirms the option flipped the alphabet.
        let (std_enc, url_enc): (String, String) = lua
            .load(
                r#"
                local raw = codec.bytes_to_string({251, 255})
                local std = codec.base64_encode(raw)
                local url = codec.base64_encode(raw, { url_safe = true })
                return std, url
                "#,
            )
            .eval()
            .expect("base64 url_safe");
        assert!(std_enc.contains('+') || std_enc.contains('/'));
        assert!(!url_enc.contains('+'));
        assert!(!url_enc.contains('/'));
        assert!(url_enc.contains('-') || url_enc.contains('_'));
    }

    #[test]
    fn hex_decode_accepts_uppercase() {
        let (_rt, lua) = install();
        let ok: bool = lua
            .load(
                r#"
                local upper = codec.hex_decode("DEADBEEF")
                local lower = codec.hex_decode("deadbeef")
                return upper == lower
                "#,
            )
            .eval()
            .expect("hex uppercase");
        assert!(ok);
    }

    #[test]
    fn utf8_decode_returns_codepoints() {
        let (_rt, lua) = install();
        // "é" is U+00E9 = 233, outside the byte range — confirms decode
        // returns codepoints, not bytes.
        let (len, first, second): (i64, i64, i64) = lua
            .load(
                r#"
                local cps = codec.utf8_decode("Aé")
                return #cps, cps[1], cps[2]
                "#,
            )
            .eval()
            .expect("utf8_decode");
        assert_eq!(len, 2);
        assert_eq!(first, 65);
        assert_eq!(second, 0xE9);
    }

    #[test]
    fn utf8_encode_accepts_byte_table() {
        let (_rt, lua) = install();
        let s: String = lua
            .load(r#"return codec.utf8_encode({72, 105})"#)
            .eval()
            .expect("utf8_encode table");
        assert_eq!(s, "Hi");
    }

    #[test]
    fn base64_decode_autodetects_url_safe() {
        let (_rt, lua) = install();
        // Encode with url_safe then decode without specifying — the decoder
        // should fall back to the url-safe alphabet after the standard
        // alphabet rejects the `-`/`_` characters.
        let ok: bool = lua
            .load(
                r#"
                local raw = codec.bytes_to_string({251, 255, 252})
                local url = codec.base64_encode(raw, { url_safe = true })
                local dec = codec.base64_decode(url)
                return dec == raw
                "#,
            )
            .eval()
            .expect("autodetect url_safe");
        assert!(ok);
    }

    #[test]
    fn bytes_to_string_rejects_out_of_range() {
        let (_rt, lua) = install();
        let err = lua
            .load(r#"return codec.bytes_to_string({65, 256})"#)
            .eval::<String>()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("out of range"),
            "error should flag out-of-range byte, got {err}"
        );
    }
}
