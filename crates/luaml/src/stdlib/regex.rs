// NEW DEP: regex = "1"
//! `regex` stdlib module.
//!
//! Exposes Rust-style regular expressions (the `regex` crate) — NOT Lua's
//! built-in pattern language. Scripts that need lookarounds, backreferences,
//! or POSIX classes should reach for another engine; this module deliberately
//! stays within the linear-time guarantees of `regex`.
//!
//! Surface:
//! - `regex.compile(pat) -> Regex` — module-level constructor. Returns a
//!   userdata that owns a compiled `regex::Regex`. Compile failures raise
//!   `mlua::Error::runtime`.
//! - `regex.escape(s) -> string` — `regex::escape` passthrough.
//!
//! Methods on the `Regex` userdata:
//! - `r:match(s)` — first match, as `{start, end, captures}` (nil if no match).
//! - `r:match_all(s)` — list of all matches (empty if none).
//! - `r:replace(s, replacement)` / `r:replace_all(s, replacement)` — substitution.
//! - `r:split(s)` — split on every match.
//! - `r:is_match(s)` — fastest boolean check.
//!
//! ## Offsets are BYTE indices, not CHAR indices.
//!
//! The underlying `regex` crate operates on `&str` as a UTF-8 byte slice. Match
//! `start`/`end` (and `end` is exclusive) are byte offsets into the subject
//! string. For ASCII input this is indistinguishable from character offsets;
//! for multi-byte UTF-8 it is not. Scripts that need character positions must
//! walk `string.sub` or a `utf8.*` helper themselves.
//!
//! This judgment mirrors `regex::Match::start`/`end` and sidesteps an O(n)
//! rescan on every call to convert to codepoints. The alternative — returning
//! char indices — would both slow every call and silently disagree with
//! anything else that indexes the string as bytes (e.g. `string.sub` in Lua
//! 5.4). Callers asked for Rust-style regex; Rust-style offsets follow.

use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;

/// Stateless marker type implementing [`LuamlStdlibModule`]; all regex state
/// lives on the `LuaRegex` userdata returned from `regex.compile`.
pub struct RegexModule;

impl LuamlStdlibModule for RegexModule {
    fn namespace(&self) -> &'static str {
        "regex"
    }

    fn install(&self, lua: &Lua, _rt: &Handle) -> mlua::Result<Table> {
        let table = lua.create_table()?;

        table.set(
            "compile",
            lua.create_function(|_, pat: String| -> mlua::Result<LuaRegex> {
                regex::Regex::new(&pat)
                    .map(LuaRegex)
                    .map_err(|e| mlua::Error::runtime(format!("regex compile error: {e}")))
            })?,
        )?;

        table.set(
            "escape",
            lua.create_function(|_, s: String| -> mlua::Result<String> {
                Ok(regex::escape(&s))
            })?,
        )?;

        Ok(table)
    }
}

/// UserData wrapper around a compiled `regex::Regex`. Owns nothing else — the
/// subject string is passed fresh on every call.
pub struct LuaRegex(regex::Regex);

/// Build a `{start, end, captures}` table from a `regex::Captures`. `start`
/// and `end` are byte offsets into the subject (see module docs). `captures`
/// is a 1-indexed Lua list where entry 1 is the full match, entries 2..N are
/// the user capture groups, and unmatched optional groups appear as `nil`.
fn captures_table<'a>(lua: &'a Lua, caps: &regex::Captures<'_>) -> mlua::Result<Table> {
    let whole = caps.get(0).expect("capture 0 is the full match");
    let t = lua.create_table()?;
    t.set("start", whole.start())?;
    t.set("end", whole.end())?;

    let list = lua.create_table()?;
    // Lua is 1-indexed. Group 0 (full match) lands at index 1.
    for (i, group) in caps.iter().enumerate() {
        match group {
            Some(m) => list.set(i + 1, m.as_str())?,
            None => list.set(i + 1, Value::Nil)?,
        }
    }
    t.set("captures", list)?;
    Ok(t)
}

impl UserData for LuaRegex {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // r:match(s) — first match or nil. Returns `{start, end, captures}`.
        methods.add_method("match", |lua, this, s: String| -> mlua::Result<Value> {
            match this.0.captures(&s) {
                Some(caps) => {
                    let t = captures_table(lua, &caps)?;
                    Ok(Value::Table(t))
                }
                None => Ok(Value::Nil),
            }
        });

        // r:match_all(s) — every non-overlapping match as a list of tables.
        // Empty list rather than nil when there are zero matches, so callers
        // can treat the result uniformly with `#result` / `ipairs`.
        methods.add_method("match_all", |lua, this, s: String| -> mlua::Result<Table> {
            let out = lua.create_table()?;
            for (i, caps) in this.0.captures_iter(&s).enumerate() {
                let t = captures_table(lua, &caps)?;
                out.set(i + 1, t)?;
            }
            Ok(out)
        });

        // r:replace(s, replacement) — replaces the FIRST match only.
        // Replacement string honors `$0`, `$1`, ... as per `regex::Regex::replace`.
        methods.add_method(
            "replace",
            |_, this, (s, rep): (String, String)| -> mlua::Result<String> {
                Ok(this.0.replace(&s, rep.as_str()).into_owned())
            },
        );

        // r:replace_all(s, replacement) — replaces every match.
        methods.add_method(
            "replace_all",
            |_, this, (s, rep): (String, String)| -> mlua::Result<String> {
                Ok(this.0.replace_all(&s, rep.as_str()).into_owned())
            },
        );

        // r:split(s) — splits the subject on every match. The match text
        // itself is discarded; adjacent matches or leading/trailing matches
        // produce empty strings, matching `regex::Regex::split` semantics.
        methods.add_method("split", |lua, this, s: String| -> mlua::Result<Table> {
            let out = lua.create_table()?;
            for (i, piece) in this.0.split(&s).enumerate() {
                out.set(i + 1, piece)?;
            }
            Ok(out)
        });

        // r:is_match(s) — cheapest path when only presence matters; does not
        // allocate captures.
        methods.add_method("is_match", |_, this, s: String| -> mlua::Result<bool> {
            Ok(this.0.is_match(&s))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    fn install_regex(lua: &Lua) {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let table = RegexModule
            .install(lua, &rt.handle().clone())
            .expect("install regex");
        lua.globals().set("regex", table).unwrap();
    }

    #[test]
    fn compile_and_match_returns_captures_with_byte_offsets() {
        let lua = Lua::new();
        install_regex(&lua);
        // Word at offset 6 ("world" in "hello world").
        let script = r#"
            local r = regex.compile("\\w+")
            local m = r:match("hello world")
            return m.start, m["end"], m.captures[1]
        "#;
        let (start, end_, cap): (i64, i64, String) = lua.load(script).eval().unwrap();
        assert_eq!(start, 0);
        assert_eq!(end_, 5);
        assert_eq!(cap, "hello");
    }

    #[test]
    fn match_returns_nil_when_no_match() {
        let lua = Lua::new();
        install_regex(&lua);
        let script = r#"
            local r = regex.compile("^xyz$")
            return r:match("abc")
        "#;
        let v: Value = lua.load(script).eval().unwrap();
        assert!(matches!(v, Value::Nil));
    }

    #[test]
    fn match_all_returns_every_occurrence() {
        let lua = Lua::new();
        install_regex(&lua);
        // Three words separated by spaces: "one two three".
        let script = r#"
            local r = regex.compile("\\w+")
            local ms = r:match_all("one two three")
            return #ms, ms[1].captures[1], ms[2].captures[1], ms[3].captures[1]
        "#;
        let (n, a, b, c): (i64, String, String, String) = lua.load(script).eval().unwrap();
        assert_eq!(n, 3);
        assert_eq!(a, "one");
        assert_eq!(b, "two");
        assert_eq!(c, "three");
    }

    #[test]
    fn replace_all_substitutes_every_match() {
        let lua = Lua::new();
        install_regex(&lua);
        let script = r#"
            local r = regex.compile("a")
            return r:replace_all("banana", "o")
        "#;
        let out: String = lua.load(script).eval().unwrap();
        assert_eq!(out, "bonono");
    }

    #[test]
    fn replace_substitutes_only_first_match() {
        let lua = Lua::new();
        install_regex(&lua);
        let script = r#"
            local r = regex.compile("a")
            return r:replace("banana", "o")
        "#;
        let out: String = lua.load(script).eval().unwrap();
        assert_eq!(out, "bonana");
    }

    #[test]
    fn split_on_comma() {
        let lua = Lua::new();
        install_regex(&lua);
        let script = r#"
            local r = regex.compile(",")
            local parts = r:split("a,b,c")
            return parts[1], parts[2], parts[3]
        "#;
        let (a, b, c): (String, String, String) = lua.load(script).eval().unwrap();
        assert_eq!(a, "a");
        assert_eq!(b, "b");
        assert_eq!(c, "c");
    }

    #[test]
    fn is_match_true_and_false() {
        let lua = Lua::new();
        install_regex(&lua);
        let script = r#"
            local r = regex.compile("^\\d+$")
            return r:is_match("123"), r:is_match("12a")
        "#;
        let (yes, no): (bool, bool) = lua.load(script).eval().unwrap();
        assert!(yes);
        assert!(!no);
    }

    #[test]
    fn escape_escapes_regex_metacharacters() {
        let lua = Lua::new();
        install_regex(&lua);
        // "1+2" -> "1\+2": the + must be backslash-escaped. In the returned
        // string we expect a literal backslash followed by '+'.
        let out: String = lua.load(r#"return regex.escape("1+2")"#).eval().unwrap();
        assert_eq!(out, "1\\+2");
    }

    #[test]
    fn compile_surfaces_error_as_runtime_error() {
        let lua = Lua::new();
        install_regex(&lua);
        // Trailing `[` is an unclosed character class.
        let err = lua
            .load(r#"return regex.compile("[")"#)
            .eval::<Value>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("regex compile error"), "got: {err}");
    }

    #[test]
    fn unmatched_optional_capture_is_nil() {
        let lua = Lua::new();
        install_regex(&lua);
        // Two alternatives, each with a capture group; only one matches per run.
        // When the second alt matches, the first group is absent.
        let script = r#"
            local r = regex.compile("(foo)|(bar)")
            local m = r:match("bar")
            return m.captures[1], m.captures[2], m.captures[3]
        "#;
        let (whole, g1, g2): (String, Value, String) = lua.load(script).eval().unwrap();
        assert_eq!(whole, "bar");
        assert!(matches!(g1, Value::Nil), "unmatched group should be nil");
        assert_eq!(g2, "bar");
    }
}
