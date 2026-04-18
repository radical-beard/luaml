//! `crypto` stdlib module: synchronous cryptographic primitives.
//!
//! Exposes hashing, HMAC, signing, random bytes, and AEAD encryption on the
//! bare `crypto` namespace. Every function is synchronous — none return a
//! [`Promise`][crate::stdlib::promise::Promise]. All byte I/O flows through
//! Lua strings: Lua strings in Lua 5.4 are 8-bit clean, so raw cipher/hash
//! output is returned as a Lua string rather than a hex string. Callers that
//! need hex encoding should layer their own encoder on top.
//!
//! ## Functions
//!
//! | Method | Shape |
//! |--------|-------|
//! | `crypto.random_bytes(n)` | `n -> string` |
//! | `crypto.hash(alg, data)` | `sha256 / sha512 / sha3-256 / blake3` |
//! | `crypto.hmac(alg, key, data)` | `hmac-sha256 / hmac-sha512` |
//! | `crypto.sign(alg, key, data)` | `ed25519 (32-byte seed) / hmac-sha256` |
//! | `crypto.verify(alg, key, data, sig)` | same algs as `sign`, returns bool |
//! | `crypto.keypair(alg)` | `ed25519` only — `{public, private}`, both 32 B |
//! | `crypto.aead_encrypt(alg, key, nonce, data, aad?)` | `chacha20poly1305` |
//! | `crypto.aead_decrypt(alg, key, nonce, ciphertext, aad?)` | inverse |
//!
//! ## Algorithm selection
//!
//! Algorithm selection is string-based and strict: unknown algorithm names
//! surface `mlua::Error::runtime("crypto: unknown algorithm {}")` rather than
//! silently falling back. This matches the `json` and `fs` modules' posture
//! of rejecting ambiguous input over guessing intent.
//!
//! ## Tag placement (AEAD)
//!
//! `aead_encrypt` returns `ciphertext || tag` in a single Lua string — the
//! default shape emitted by `chacha20poly1305`'s `Aead::encrypt`. `aead_decrypt`
//! accepts that same concatenated form and verifies the tag internally. The
//! 16-byte tag is *not* returned separately — callers must pass the whole
//! ciphertext back.
//!
//! ## Key length validation
//!
//! Fixed-width key material (32-byte chacha20poly1305 keys, 12-byte nonces,
//! 32-byte ed25519 seeds/public keys, 64-byte ed25519 signatures) is
//! validated with explicit length checks before calling into the RustCrypto
//! constructors. The underlying APIs would error anyway, but explicit checks
//! let us surface a more legible error string.
//
// NEW DEPS:
// sha2 = "0.10"
// sha3 = "0.10"
// blake3 = "1"
// hmac = "0.12"
// ed25519-dalek = "2"
// chacha20poly1305 = "0.10"
// rand = "0.8"

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key as ChaChaKey, Nonce as ChaChaNonce,
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hmac::{Hmac, Mac};
use mlua::{Lua, Table, Value};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest as Sha2Digest, Sha256, Sha512};
use sha3::Sha3_256;
use tokio::runtime::Handle;

use super::LuamlStdlibModule;

/// Stateless stdlib module installer for the `crypto` namespace.
pub struct CryptoModule;

impl LuamlStdlibModule for CryptoModule {
    fn namespace(&self) -> &'static str {
        "crypto"
    }

    fn install(&self, lua: &Lua, _rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        // crypto.random_bytes(n) -> string
        //
        // Fills `n` bytes from `OsRng`, the OS-backed CSPRNG. Negative or
        // excessively large `n` (> 16 MiB) is rejected — the cap exists to
        // refuse obviously-wrong arguments before allocating; cryptographic
        // callers want at most a few hundred bytes.
        table.set(
            "random_bytes",
            lua.create_function(|lua, n: i64| -> mlua::Result<mlua::String> {
                if n < 0 {
                    return Err(mlua::Error::runtime("crypto.random_bytes: n must be >= 0"));
                }
                if n > 16 * 1024 * 1024 {
                    return Err(mlua::Error::runtime(
                        "crypto.random_bytes: n exceeds 16 MiB cap",
                    ));
                }
                let mut buf = vec![0u8; n as usize];
                OsRng.fill_bytes(&mut buf);
                lua.create_string(&buf)
            })?,
        )?;

        // crypto.hash(alg, data) -> string (raw digest bytes)
        table.set(
            "hash",
            lua.create_function(
                |lua, (alg, data): (String, mlua::String)| -> mlua::Result<mlua::String> {
                    let data = data.as_bytes();
                    let digest: Vec<u8> = match alg.as_str() {
                        "sha256" => Sha256::digest(&*data).to_vec(),
                        "sha512" => Sha512::digest(&*data).to_vec(),
                        "sha3-256" => Sha3_256::digest(&*data).to_vec(),
                        "blake3" => blake3::hash(&data).as_bytes().to_vec(),
                        other => {
                            return Err(mlua::Error::runtime(format!(
                                "crypto: unknown algorithm {other}"
                            )));
                        }
                    };
                    lua.create_string(&digest)
                },
            )?,
        )?;

        // crypto.hmac(alg, key, data) -> string (raw MAC bytes)
        table.set(
            "hmac",
            lua.create_function(
                |lua,
                 (alg, key, data): (String, mlua::String, mlua::String)|
                 -> mlua::Result<mlua::String> {
                    let key = key.as_bytes();
                    let data = data.as_bytes();
                    let mac = compute_hmac(alg.as_str(), &key, &data)?;
                    lua.create_string(&mac)
                },
            )?,
        )?;

        // crypto.sign(alg, key, data) -> string (raw signature bytes)
        //
        // For "ed25519", `key` is the 32-byte seed (private component) and
        // the returned signature is 64 bytes. For "hmac-sha256", this is an
        // alias for `crypto.hmac("hmac-sha256", ...)`.
        table.set(
            "sign",
            lua.create_function(
                |lua,
                 (alg, key, data): (String, mlua::String, mlua::String)|
                 -> mlua::Result<mlua::String> {
                    let key = key.as_bytes();
                    let data = data.as_bytes();
                    let sig = match alg.as_str() {
                        "ed25519" => {
                            let seed = ed25519_seed(&key)?;
                            let signing_key = SigningKey::from_bytes(&seed);
                            signing_key.sign(&data).to_bytes().to_vec()
                        }
                        "hmac-sha256" => compute_hmac("hmac-sha256", &key, &data)?,
                        other => {
                            return Err(mlua::Error::runtime(format!(
                                "crypto: unknown algorithm {other}"
                            )));
                        }
                    };
                    lua.create_string(&sig)
                },
            )?,
        )?;

        // crypto.verify(alg, key, data, sig) -> bool
        //
        // For "ed25519" `key` is the 32-byte PUBLIC key. For "hmac-sha256",
        // `key` is the shared secret and verification is constant-time via
        // `CtOutput`-compared re-computation.
        table.set(
            "verify",
            lua.create_function(
                |_,
                 (alg, key, data, sig): (
                    String,
                    mlua::String,
                    mlua::String,
                    mlua::String,
                )|
                 -> mlua::Result<bool> {
                    let key = key.as_bytes();
                    let data = data.as_bytes();
                    let sig = sig.as_bytes();
                    match alg.as_str() {
                        "ed25519" => {
                            let pk_bytes = ed25519_public(&key)?;
                            let verifying = match VerifyingKey::from_bytes(&pk_bytes) {
                                Ok(v) => v,
                                Err(_) => return Ok(false),
                            };
                            if sig.len() != 64 {
                                return Ok(false);
                            }
                            let mut sig_arr = [0u8; 64];
                            sig_arr.copy_from_slice(&sig);
                            let signature = Signature::from_bytes(&sig_arr);
                            Ok(verifying.verify(&data, &signature).is_ok())
                        }
                        "hmac-sha256" => {
                            // Prefer the library's constant-time `verify_slice`
                            // over comparing bytes ourselves — it sidesteps the
                            // timing-leak pitfall of naive `==`. UFCS is used
                            // because both `hmac::Mac` and `aead::KeyInit` are
                            // in scope in this file and both expose
                            // `new_from_slice` on the generic Hmac type.
                            let mut mac =
                                <Hmac<Sha256> as Mac>::new_from_slice(&key).map_err(|e| {
                                    mlua::Error::runtime(format!(
                                        "crypto.verify: invalid key: {e}"
                                    ))
                                })?;
                            mac.update(&data);
                            Ok(mac.verify_slice(&sig).is_ok())
                        }
                        other => Err(mlua::Error::runtime(format!(
                            "crypto: unknown algorithm {other}"
                        ))),
                    }
                },
            )?,
        )?;

        // crypto.keypair(alg) -> {public = string, private = string}
        //
        // Only "ed25519" is defined. `private` is the 32-byte seed — pass it
        // back to `crypto.sign("ed25519", private, data)`. `public` is the
        // 32-byte compressed Edwards point; pass to `crypto.verify(...)`.
        table.set(
            "keypair",
            lua.create_function(|lua, alg: String| -> mlua::Result<Table> {
                match alg.as_str() {
                    "ed25519" => {
                        // Generate a fresh 32-byte seed with the same OS CSPRNG
                        // `random_bytes` uses, then derive the SigningKey from
                        // it. This avoids pulling in ed25519-dalek's optional
                        // `rand_core` feature and keeps the dependency surface
                        // aligned with `rand = "0.8"`.
                        let mut seed = [0u8; 32];
                        OsRng.fill_bytes(&mut seed);
                        let signing_key = SigningKey::from_bytes(&seed);
                        let verifying_key = signing_key.verifying_key();
                        let out = lua.create_table()?;
                        out.set("public", lua.create_string(verifying_key.to_bytes())?)?;
                        out.set("private", lua.create_string(seed)?)?;
                        Ok(out)
                    }
                    other => Err(mlua::Error::runtime(format!(
                        "crypto: unknown algorithm {other}"
                    ))),
                }
            })?,
        )?;

        // crypto.aead_encrypt(alg, key, nonce, data, aad?) -> string
        //
        // Only "chacha20poly1305". Returns `ciphertext || tag` (tag = 16 B at
        // end). Key MUST be 32 B, nonce MUST be 12 B — enforced before calling
        // into the cipher so we can surface a precise error string. `aad` is
        // typed as `Value` rather than `Option<String>` so both an omitted
        // arg and an explicit `nil` are accepted uniformly; any non-nil value
        // must be a Lua string.
        table.set(
            "aead_encrypt",
            lua.create_function(
                |lua,
                 (alg, key, nonce, data, aad): (
                    String,
                    mlua::String,
                    mlua::String,
                    mlua::String,
                    Value,
                )|
                 -> mlua::Result<mlua::String> {
                    match alg.as_str() {
                        "chacha20poly1305" => {
                            let key = key.as_bytes();
                            let nonce = nonce.as_bytes();
                            let data = data.as_bytes();
                            let aad_owned = extract_optional_bytes("aad", aad)?;
                            check_len("key", &key, 32)?;
                            check_len("nonce", &nonce, 12)?;
                            let cipher = ChaCha20Poly1305::new(ChaChaKey::from_slice(&key));
                            let nonce_ga = ChaChaNonce::from_slice(&nonce);
                            let ct = match &aad_owned {
                                Some(aad) => cipher.encrypt(
                                    nonce_ga,
                                    Payload {
                                        msg: &data,
                                        aad: aad.as_slice(),
                                    },
                                ),
                                None => cipher.encrypt(nonce_ga, &*data),
                            }
                            .map_err(|e| {
                                mlua::Error::runtime(format!("crypto.aead_encrypt: {e}"))
                            })?;
                            lua.create_string(&ct)
                        }
                        other => Err(mlua::Error::runtime(format!(
                            "crypto: unknown algorithm {other}"
                        ))),
                    }
                },
            )?,
        )?;

        // crypto.aead_decrypt(alg, key, nonce, ciphertext, aad?) -> string
        //
        // Verifies the tag and returns plaintext on success. Tag mismatch,
        // wrong key, or wrong nonce surface as `mlua::Error::runtime`. `aad`
        // is typed as `Value` (see `aead_encrypt` for rationale).
        table.set(
            "aead_decrypt",
            lua.create_function(
                |lua,
                 (alg, key, nonce, ciphertext, aad): (
                    String,
                    mlua::String,
                    mlua::String,
                    mlua::String,
                    Value,
                )|
                 -> mlua::Result<mlua::String> {
                    match alg.as_str() {
                        "chacha20poly1305" => {
                            let key = key.as_bytes();
                            let nonce = nonce.as_bytes();
                            let ct = ciphertext.as_bytes();
                            let aad_owned = extract_optional_bytes("aad", aad)?;
                            check_len("key", &key, 32)?;
                            check_len("nonce", &nonce, 12)?;
                            let cipher = ChaCha20Poly1305::new(ChaChaKey::from_slice(&key));
                            let nonce_ga = ChaChaNonce::from_slice(&nonce);
                            let pt = match &aad_owned {
                                Some(aad) => cipher.decrypt(
                                    nonce_ga,
                                    Payload {
                                        msg: &ct,
                                        aad: aad.as_slice(),
                                    },
                                ),
                                None => cipher.decrypt(nonce_ga, &*ct),
                            }
                            .map_err(|e| {
                                mlua::Error::runtime(format!("crypto.aead_decrypt: {e}"))
                            })?;
                            lua.create_string(&pt)
                        }
                        other => Err(mlua::Error::runtime(format!(
                            "crypto: unknown algorithm {other}"
                        ))),
                    }
                },
            )?,
        )?;

        Ok(table)
    }
}

/// Lower an optional `Value` argument to `Option<Vec<u8>>`. `nil` (including
/// an omitted trailing argument) maps to `None`; any Lua string maps to
/// `Some(bytes)`; anything else is an explicit type error.
fn extract_optional_bytes(name: &str, v: Value) -> mlua::Result<Option<Vec<u8>>> {
    match v {
        Value::Nil => Ok(None),
        Value::String(s) => Ok(Some(s.as_bytes().to_vec())),
        other => Err(mlua::Error::runtime(format!(
            "crypto: {name} must be a string or nil, got {}",
            other.type_name()
        ))),
    }
}

/// Validate that a byte slice has the expected length and produce a legible
/// error otherwise. Pulled out of the AEAD closures so both encrypt and
/// decrypt share the same phrasing.
fn check_len(what: &str, bytes: &[u8], expected: usize) -> mlua::Result<()> {
    if bytes.len() != expected {
        return Err(mlua::Error::runtime(format!(
            "crypto: {what} must be {expected} bytes, got {}",
            bytes.len()
        )));
    }
    Ok(())
}

/// Extract a 32-byte ed25519 seed from the provided key bytes.
fn ed25519_seed(key: &[u8]) -> mlua::Result<[u8; 32]> {
    if key.len() != 32 {
        return Err(mlua::Error::runtime(format!(
            "crypto: ed25519 key must be 32 bytes, got {}",
            key.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(key);
    Ok(out)
}

/// Extract a 32-byte ed25519 public-key compressed point from the provided key
/// bytes. Structurally identical to [`ed25519_seed`] but kept separate so the
/// error message distinguishes seed (private) from public key usage.
fn ed25519_public(key: &[u8]) -> mlua::Result<[u8; 32]> {
    if key.len() != 32 {
        return Err(mlua::Error::runtime(format!(
            "crypto: ed25519 public key must be 32 bytes, got {}",
            key.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(key);
    Ok(out)
}

/// Shared HMAC computation so `sign` and `hmac` share one code path and
/// treat unknown HMAC algorithms identically. UFCS on `Mac::new_from_slice`
/// is required because both `hmac::Mac` and `aead::KeyInit` are in scope and
/// both expose `new_from_slice` on the Hmac wrapper type.
fn compute_hmac(alg: &str, key: &[u8], data: &[u8]) -> mlua::Result<Vec<u8>> {
    match alg {
        "hmac-sha256" => {
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key)
                .map_err(|e| mlua::Error::runtime(format!("crypto.hmac: invalid key: {e}")))?;
            mac.update(data);
            Ok(mac.finalize().into_bytes().to_vec())
        }
        "hmac-sha512" => {
            let mut mac = <Hmac<Sha512> as Mac>::new_from_slice(key)
                .map_err(|e| mlua::Error::runtime(format!("crypto.hmac: invalid key: {e}")))?;
            mac.update(data);
            Ok(mac.finalize().into_bytes().to_vec())
        }
        other => Err(mlua::Error::runtime(format!(
            "crypto: unknown algorithm {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    fn install_crypto(lua: &Lua) {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let table = CryptoModule
            .install(lua, &rt.handle().clone())
            .expect("install crypto");
        lua.globals().set("crypto", table).unwrap();
    }

    #[test]
    fn sha256_of_empty_string_is_known_digest() {
        // SHA-256("") is a widely-tabulated constant; if this test fails the
        // wiring to sha2 is broken, not the algorithm.
        let lua = Lua::new();
        install_crypto(&lua);
        let hex: String = lua
            .load(
                r#"
                local d = crypto.hash("sha256", "")
                local out = {}
                for i = 1, #d do
                    out[i] = string.format("%02x", string.byte(d, i))
                end
                return table.concat(out)
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hmac_sha256_round_trips_via_verify() {
        let lua = Lua::new();
        install_crypto(&lua);
        let ok: bool = lua
            .load(
                r#"
                local key = "shared-secret"
                local msg = "input message"
                local mac = crypto.hmac("hmac-sha256", key, msg)
                return crypto.verify("hmac-sha256", key, msg, mac)
            "#,
            )
            .eval()
            .unwrap();
        assert!(ok, "hmac-sha256 verify must accept self-produced tag");
    }

    #[test]
    fn ed25519_sign_verify_round_trip() {
        let lua = Lua::new();
        install_crypto(&lua);
        let ok: bool = lua
            .load(
                r#"
                local kp = crypto.keypair("ed25519")
                assert(#kp.public == 32, "public key must be 32 bytes")
                assert(#kp.private == 32, "private seed must be 32 bytes")
                local msg = "message under signature"
                local sig = crypto.sign("ed25519", kp.private, msg)
                assert(#sig == 64, "ed25519 signature must be 64 bytes")
                return crypto.verify("ed25519", kp.public, msg, sig)
            "#,
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn random_bytes_returns_requested_length() {
        let lua = Lua::new();
        install_crypto(&lua);
        let len: i64 = lua
            .load("return #crypto.random_bytes(64)")
            .eval()
            .unwrap();
        assert_eq!(len, 64);
        let zero_len: i64 = lua.load("return #crypto.random_bytes(0)").eval().unwrap();
        assert_eq!(zero_len, 0);
    }

    #[test]
    fn aead_chacha20poly1305_round_trip() {
        let lua = Lua::new();
        install_crypto(&lua);
        let recovered: String = lua
            .load(
                r#"
                local key = crypto.random_bytes(32)
                local nonce = crypto.random_bytes(12)
                local aad = "header"
                local pt = "plaintext payload"
                local ct = crypto.aead_encrypt("chacha20poly1305", key, nonce, pt, aad)
                -- ciphertext length = plaintext + 16-byte poly1305 tag.
                assert(#ct == #pt + 16, "aead output missing tag suffix")
                return crypto.aead_decrypt("chacha20poly1305", key, nonce, ct, aad)
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(recovered, "plaintext payload");
    }

    #[test]
    fn verify_rejects_tampered_ed25519_signature() {
        let lua = Lua::new();
        install_crypto(&lua);
        let ok: bool = lua
            .load(
                r#"
                local kp = crypto.keypair("ed25519")
                local msg = "message under signature"
                local sig = crypto.sign("ed25519", kp.private, msg)
                -- Flip the first byte to tamper with the signature.
                local first = string.byte(sig, 1)
                local tampered = string.char((first + 1) % 256) .. string.sub(sig, 2)
                return crypto.verify("ed25519", kp.public, msg, tampered)
            "#,
            )
            .eval()
            .unwrap();
        assert!(!ok, "tampered ed25519 signature must not verify");
    }

    #[test]
    fn unknown_algorithm_errors() {
        let lua = Lua::new();
        install_crypto(&lua);
        let err = lua
            .load(r#"return crypto.hash("md5", "anything")"#)
            .eval::<mlua::String>()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown algorithm"),
            "error should name the unknown alg: {err}"
        );
    }

    #[test]
    fn aead_rejects_wrong_key_length() {
        let lua = Lua::new();
        install_crypto(&lua);
        let err = lua
            .load(
                r#"
                return crypto.aead_encrypt(
                    "chacha20poly1305",
                    "short",
                    crypto.random_bytes(12),
                    "pt"
                )
            "#,
            )
            .eval::<mlua::String>()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("key must be 32 bytes"),
            "expected length error, got: {err}"
        );
    }

    #[test]
    fn blake3_and_sha3_wire_up() {
        // Smoke-test the less-common algorithm routes so a broken match arm
        // surfaces here rather than at a consumer call site.
        let lua = Lua::new();
        install_crypto(&lua);
        let blake_len: i64 = lua
            .load(r#"return #crypto.hash("blake3", "hello")"#)
            .eval()
            .unwrap();
        assert_eq!(blake_len, 32, "blake3 default output is 32 bytes");
        let sha3_len: i64 = lua
            .load(r#"return #crypto.hash("sha3-256", "hello")"#)
            .eval()
            .unwrap();
        assert_eq!(sha3_len, 32, "sha3-256 output is 32 bytes");
    }
}
