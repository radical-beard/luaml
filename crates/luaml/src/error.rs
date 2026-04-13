use std::fmt;

#[derive(Debug)]
pub struct PatternParseError {
    pub message: String,
    pub input: String,
}

impl fmt::Display for PatternParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pattern parse error: {} (input: {:?})",
            self.message, self.input
        )
    }
}

impl std::error::Error for PatternParseError {}

pub fn parse_err(message: impl Into<String>, input: impl Into<String>) -> PatternParseError {
    PatternParseError {
        message: message.into(),
        input: input.into(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LuamlError {
    #[error("parse error: {message} (in {source_name})")]
    Parse {
        message: String,
        source_name: String,
    },

    #[error("pattern error: {0}")]
    Pattern(#[from] PatternParseError),

    #[error("guard error: {0}")]
    Guard(String),

    #[error("lua error: {0}")]
    Lua(#[from] mlua::Error),

    #[error("api error: {namespace}.{method}: {message}")]
    Api {
        namespace: String,
        method: String,
        message: String,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Display formats ────────────────────────────────────────────

    #[test]
    fn pattern_parse_error_display() {
        let err = PatternParseError {
            message: "bad".into(),
            input: "xyz".into(),
        };
        let s = err.to_string();
        assert!(s.contains("pattern parse error: bad"), "{s}");
        assert!(s.contains("xyz"), "{s}");
    }

    #[test]
    fn luaml_error_parse_display() {
        let err = LuamlError::Parse {
            message: "msg".into(),
            source_name: "file.luaml".into(),
        };
        let s = err.to_string();
        assert!(s.contains("parse error: msg"), "{s}");
        assert!(s.contains("file.luaml"), "{s}");
    }

    #[test]
    fn luaml_error_pattern_display() {
        let err = LuamlError::Pattern(parse_err("msg", "input"));
        assert!(err.to_string().contains("pattern error"), "{}", err);
    }

    #[test]
    fn luaml_error_guard_display() {
        let err = LuamlError::Guard("msg".into());
        assert_eq!(err.to_string(), "guard error: msg");
    }

    #[test]
    fn luaml_error_api_display() {
        let err = LuamlError::Api {
            namespace: "ns".into(),
            method: "m".into(),
            message: "err".into(),
        };
        assert_eq!(err.to_string(), "api error: ns.m: err");
    }

    #[test]
    fn luaml_error_io_display() {
        let err = LuamlError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert!(err.to_string().contains("io error"), "{}", err);
    }

    // ── From conversions ───────────────────────────────────────────

    #[test]
    fn from_pattern_parse_error() {
        let pe = parse_err("msg", "input");
        let err: LuamlError = pe.into();
        assert!(matches!(err, LuamlError::Pattern(_)));
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "fail");
        let err: LuamlError = io_err.into();
        assert!(matches!(err, LuamlError::Io(_)));
    }

    #[test]
    fn from_mlua_error() {
        let lua_err = mlua::Error::RuntimeError("boom".into());
        let err: LuamlError = lua_err.into();
        assert!(matches!(err, LuamlError::Lua(_)));
    }

    #[test]
    fn parse_err_helper() {
        let err = parse_err("bad syntax", "abc");
        assert_eq!(err.message, "bad syntax");
        assert_eq!(err.input, "abc");
    }
}
