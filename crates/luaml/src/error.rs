use std::fmt;

#[derive(Debug)]
pub struct PatternParseError {
    pub message: String,
    pub input: String,
}

impl fmt::Display for PatternParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pattern parse error: {} (input: {:?})", self.message, self.input)
    }
}

impl std::error::Error for PatternParseError {}

pub fn parse_err(message: &str, input: &str) -> PatternParseError {
    PatternParseError {
        message: message.to_string(),
        input: input.to_string(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LuamlError {
    #[error("parse error: {message} (in {source_name})")]
    Parse { message: String, source_name: String },

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
