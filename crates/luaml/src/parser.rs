use crate::clause::Script;
use crate::error::LuamlError;
use std::path::PathBuf;

/// Parse a .luaml file from source text into a Script.
pub fn parse_luaml(
    _source_path: impl Into<PathBuf>,
    _text: &str,
) -> Result<Script, LuamlError> {
    // TODO: implement
    Err(LuamlError::Parse {
        message: "not yet implemented".to_string(),
        source_name: "parser".to_string(),
    })
}
