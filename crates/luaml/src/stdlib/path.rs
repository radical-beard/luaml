//! `path` stdlib module: platform-aware path operations.
//!
//! All functions are synchronous — path handling is pure string/OS math with
//! no blocking I/O, so the promise machinery doesn't apply. The module owns
//! only `std::path` and `std::env` — no new dependencies.
//!
//! Semantic notes:
//! - `normalize` is lexical: it collapses `.` and resolves `..` against prior
//!   components where possible, but never touches the filesystem (so it
//!   cannot resolve symlinks or know if a `..` crossed a symlinked directory
//!   boundary). For an absolute path, leading `..` beyond the root is
//!   discarded. For a relative path, leading `..` components that can't be
//!   cancelled are preserved.
//! - `absolute` prefers `std::fs::canonicalize` (which resolves symlinks and
//!   requires the path to exist); if canonicalization fails (e.g. the path
//!   doesn't exist) it falls back to joining the path onto
//!   `std::env::current_dir()` and then lexically normalizing. If `current_dir`
//!   itself fails and the input path is relative, we surface a runtime error.
//!   For an already-absolute non-existent path, we just return its
//!   lexically-normalized form.

use std::path::{Component, Path, PathBuf};

use mlua::{Lua, Table};
use tokio::runtime::Handle;

use super::LuamlStdlibModule;

/// Zero-sized module handle. Registration is done by `mod.rs` under the
/// `stdlib-path` feature flag; this type only exists to implement the trait.
pub struct PathModule;

impl LuamlStdlibModule for PathModule {
    fn namespace(&self) -> &'static str {
        "path"
    }

    fn install(&self, lua: &Lua, _rt: &Handle) -> mlua::Result<Table> {
        let t = lua.create_table()?;

        // path.join(a, b, ...) — variadic join. Later absolute paths replace
        // the accumulated buffer, matching `PathBuf::push` semantics.
        t.set(
            "join",
            lua.create_function(|_, args: mlua::Variadic<String>| {
                if args.is_empty() {
                    return Ok(String::new());
                }
                let mut buf = PathBuf::from(args[0].clone());
                for part in args.iter().skip(1) {
                    buf.push(part);
                }
                Ok(path_to_string(&buf))
            })?,
        )?;

        // path.parent(p) — parent directory, empty string if none.
        t.set(
            "parent",
            lua.create_function(|_, p: String| {
                Ok(Path::new(&p)
                    .parent()
                    .map(path_to_string)
                    .unwrap_or_default())
            })?,
        )?;

        // path.basename(p) — final component (file name), empty if none.
        t.set(
            "basename",
            lua.create_function(|_, p: String| {
                Ok(Path::new(&p)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default())
            })?,
        )?;

        // path.extension(p) — extension without the leading dot.
        t.set(
            "extension",
            lua.create_function(|_, p: String| {
                Ok(Path::new(&p)
                    .extension()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default())
            })?,
        )?;

        // path.stem(p) — file name without its extension.
        t.set(
            "stem",
            lua.create_function(|_, p: String| {
                Ok(Path::new(&p)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default())
            })?,
        )?;

        // path.normalize(p) — lexical normalization. See module docs.
        t.set(
            "normalize",
            lua.create_function(|_, p: String| Ok(path_to_string(&normalize_lexical(Path::new(&p)))))?,
        )?;

        // path.absolute(p) — canonicalize if possible, otherwise
        // cwd-join + normalize. See module docs.
        t.set(
            "absolute",
            lua.create_function(|_, p: String| {
                let path = Path::new(&p);
                if let Ok(canon) = std::fs::canonicalize(path) {
                    return Ok(path_to_string(&canon));
                }
                if path.is_absolute() {
                    return Ok(path_to_string(&normalize_lexical(path)));
                }
                let cwd = std::env::current_dir()
                    .map_err(|e| mlua::Error::runtime(format!("cannot resolve cwd: {e}")))?;
                Ok(path_to_string(&normalize_lexical(&cwd.join(path))))
            })?,
        )?;

        // path.relative(from, to) — produce `to` expressed relative to `from`
        // using lexical reasoning only. Both sides are normalized first.
        // Emits `..` components where needed. If either side is relative and
        // the other is absolute (or vice versa), we return `to` unchanged —
        // mixing frames has no well-defined answer without a cwd, and we
        // already expose `path.absolute` for the caller to anchor first.
        t.set(
            "relative",
            lua.create_function(|_, (from, to): (String, String)| {
                let from_n = normalize_lexical(Path::new(&from));
                let to_n = normalize_lexical(Path::new(&to));
                if from_n.is_absolute() != to_n.is_absolute() {
                    return Ok(path_to_string(&to_n));
                }
                Ok(path_to_string(&relative_lexical(&from_n, &to_n)))
            })?,
        )?;

        // path.is_absolute(p) — platform-aware absoluteness check.
        t.set(
            "is_absolute",
            lua.create_function(|_, p: String| Ok(Path::new(&p).is_absolute()))?,
        )?;

        // path.components(p) — table (array) of string components. Root and
        // prefix components are surfaced as their display form; `.` and `..`
        // are surfaced verbatim so scripts can reason about them.
        t.set(
            "components",
            lua.create_function(|lua, p: String| {
                let table = lua.create_table()?;
                for (i, comp) in Path::new(&p).components().enumerate() {
                    let s = match comp {
                        Component::Prefix(p) => p.as_os_str().to_string_lossy().into_owned(),
                        Component::RootDir => std::path::MAIN_SEPARATOR.to_string(),
                        Component::CurDir => ".".to_string(),
                        Component::ParentDir => "..".to_string(),
                        Component::Normal(s) => s.to_string_lossy().into_owned(),
                    };
                    table.set(i + 1, s)?;
                }
                Ok(table)
            })?,
        )?;

        Ok(t)
    }
}

/// Convert a `Path` to a Lua-friendly `String`. Uses `to_string_lossy` so
/// non-UTF8 paths degrade to replacement characters rather than erroring —
/// a `path` module whose methods occasionally failed on valid OS paths would
/// be worse than one that returns a slightly-mangled string.
fn path_to_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Lexical path normalization. Collapses `.`, resolves `..` against the prior
/// normal component where possible, and preserves leading `..` for relative
/// paths that can't be cancelled. Never touches the filesystem.
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => { /* drop */ }
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {
                    // `..` above root is a no-op on absolute paths.
                }
                Some(Component::ParentDir) | None => {
                    out.push(Component::ParentDir);
                }
                Some(Component::CurDir) => unreachable!("CurDir is dropped above"),
            },
            other => out.push(other),
        }
    }
    if out.is_empty() {
        return PathBuf::from(".");
    }
    let mut buf = PathBuf::new();
    for c in out {
        buf.push(c.as_os_str());
    }
    buf
}

/// Produce a relative path from `from` to `to`. Both inputs are assumed to
/// already be lexically normalized and share the same absoluteness. Walks
/// the longest common prefix, emits `..` for the remaining `from` suffix,
/// then appends the remaining `to` suffix.
fn relative_lexical(from: &Path, to: &Path) -> PathBuf {
    let from_parts: Vec<Component> = from.components().collect();
    let to_parts: Vec<Component> = to.components().collect();

    let mut shared = 0usize;
    while shared < from_parts.len()
        && shared < to_parts.len()
        && from_parts[shared] == to_parts[shared]
    {
        shared += 1;
    }

    let mut out = PathBuf::new();
    for _ in shared..from_parts.len() {
        out.push("..");
    }
    for c in &to_parts[shared..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;
    use tokio::runtime::Builder;

    fn install() -> (tokio::runtime::Runtime, Lua) {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let lua = Lua::new();
        let table = PathModule.install(&lua, rt.handle()).unwrap();
        lua.globals().set("path", table).unwrap();
        (rt, lua)
    }

    #[test]
    fn join_concatenates_components() {
        let (_rt, lua) = install();
        let joined: String = lua
            .load(r#"return path.join("a", "b", "c.txt")"#)
            .eval()
            .unwrap();
        // Use the platform separator for the assertion so this works on both
        // unix and windows without branching.
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(joined, format!("a{sep}b{sep}c.txt"));
    }

    #[test]
    fn basename_stem_extension_split_filename() {
        let (_rt, lua) = install();
        let base: String = lua
            .load(r#"return path.basename("/etc/hosts.conf")"#)
            .eval()
            .unwrap();
        let stem: String = lua
            .load(r#"return path.stem("/etc/hosts.conf")"#)
            .eval()
            .unwrap();
        let ext: String = lua
            .load(r#"return path.extension("/etc/hosts.conf")"#)
            .eval()
            .unwrap();
        assert_eq!(base, "hosts.conf");
        assert_eq!(stem, "hosts");
        assert_eq!(ext, "conf");

        let none: String = lua
            .load(r#"return path.extension("/etc/hosts")"#)
            .eval()
            .unwrap();
        assert_eq!(none, "");
    }

    #[test]
    fn is_absolute_matches_platform() {
        let (_rt, lua) = install();
        // On unix `/foo` is absolute; on windows it isn't (no drive). Mirror
        // `Path::is_absolute` to keep the test portable.
        let expect = Path::new("/foo").is_absolute();
        let got: bool = lua
            .load(r#"return path.is_absolute("/foo")"#)
            .eval()
            .unwrap();
        assert_eq!(got, expect);

        let rel: bool = lua
            .load(r#"return path.is_absolute("foo/bar")"#)
            .eval()
            .unwrap();
        assert!(!rel);
    }

    #[test]
    fn normalize_collapses_dot_and_dotdot() {
        let (_rt, lua) = install();
        let n1: String = lua
            .load(r#"return path.normalize("a/./b/../c")"#)
            .eval()
            .unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(n1, format!("a{sep}c"));

        // Leading `..` on a relative path is preserved.
        let n2: String = lua
            .load(r#"return path.normalize("../a/b")"#)
            .eval()
            .unwrap();
        assert_eq!(n2, format!("..{sep}a{sep}b"));

        // `.` alone normalizes to `.`.
        let n3: String = lua.load(r#"return path.normalize(".")"#).eval().unwrap();
        assert_eq!(n3, ".");
    }

    #[test]
    fn relative_walks_up_and_down() {
        let (_rt, lua) = install();
        let rel: String = lua
            .load(r#"return path.relative("/a/b/c", "/a/x/y")"#)
            .eval()
            .unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        // From /a/b/c to /a/x/y: up twice, then x/y.
        assert_eq!(rel, format!("..{sep}..{sep}x{sep}y"));

        // Same path relative to itself is ".".
        let same: String = lua
            .load(r#"return path.relative("/a/b", "/a/b")"#)
            .eval()
            .unwrap();
        assert_eq!(same, ".");
    }

    #[test]
    fn components_returns_array_table() {
        let (_rt, lua) = install();
        let comps: Vec<String> = lua
            .load(r#"return path.components("/usr/local/bin")"#)
            .eval()
            .unwrap();
        // First component on unix is root "/", then the three segments. On
        // windows there'd be no leading root for this input, so we just check
        // the tail.
        assert!(comps.contains(&"usr".to_string()));
        assert!(comps.contains(&"local".to_string()));
        assert!(comps.contains(&"bin".to_string()));
    }

    #[test]
    fn absolute_produces_absolute_path() {
        let (_rt, lua) = install();
        // A relative path should come back absolute (cwd-anchored).
        let abs: String = lua
            .load(r#"return path.absolute("Cargo.toml")"#)
            .eval()
            .unwrap();
        assert!(
            Path::new(&abs).is_absolute(),
            "expected absolute path, got: {abs}"
        );
    }

    #[test]
    fn parent_of_root_is_empty() {
        let (_rt, lua) = install();
        // On unix `/` has no parent, so empty string. Use Path::new to decide
        // the expected behavior so this stays portable.
        let p = Path::new("/").parent();
        let got: String = lua.load(r#"return path.parent("/")"#).eval().unwrap();
        assert_eq!(got.is_empty(), p.is_none());
    }
}
