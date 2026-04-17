use std::path::Path;

use serde::Deserialize;

use crate::error::LuamlError;

/// Declares which scripts belong to an extension and prevents partial loading.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtensionManifest {
    pub name: String,
    /// Script paths relative to the manifest file's directory.
    pub scripts: Vec<String>,
}

/// Parse an extension manifest from TOML text.
pub fn parse_manifest(text: &str, manifest_path: &Path) -> Result<ExtensionManifest, LuamlError> {
    toml::from_str(text).map_err(|e| LuamlError::Parse {
        message: format!("invalid extension manifest: {e}"),
        source_name: manifest_path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_valid_manifest() {
        let toml = r#"
name = "openai-codex"
scripts = ["openai-codex.luaml"]
"#;
        let manifest = parse_manifest(toml, &PathBuf::from("test.extension.toml")).unwrap();
        assert_eq!(manifest.name, "openai-codex");
        assert_eq!(manifest.scripts, vec!["openai-codex.luaml"]);
    }

    #[test]
    fn parse_manifest_multiple_scripts() {
        let toml = r#"
name = "my-ext"
scripts = ["a.luaml", "b.luaml", "sub/c.luaml"]
"#;
        let manifest = parse_manifest(toml, &PathBuf::from("test.extension.toml")).unwrap();
        assert_eq!(manifest.name, "my-ext");
        assert_eq!(manifest.scripts.len(), 3);
    }

    #[test]
    fn parse_invalid_manifest() {
        let result = parse_manifest("not valid toml {{{", &PathBuf::from("bad.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn parse_missing_name() {
        let result = parse_manifest(r#"scripts = ["a.luaml"]"#, &PathBuf::from("test.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn parse_missing_scripts() {
        let result = parse_manifest(r#"name = "test""#, &PathBuf::from("test.toml"));
        assert!(result.is_err());
    }
}
