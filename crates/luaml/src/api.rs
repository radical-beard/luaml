use std::sync::Arc;

use crate::pattern::Pattern;
use crate::types::FieldValue;

/// Error from an API call.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub message: String,
}

impl ApiError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Consumer-provided handler for API calls from Lua scripts.
///
/// Library mode: consumer implements this directly.
/// Service mode: RemoteApiHandler wraps calls in JSON-RPC.
pub trait ApiHandler: Send + Sync {
    /// Call a function in a namespace.
    fn call(
        &self,
        namespace: &str,
        method: &str,
        args: Vec<FieldValue>,
    ) -> Result<FieldValue, ApiError>;
}

/// Binds a namespace to a handler, scoped by pattern match against clause execution policy.
///
/// When the engine executes a clause, it checks each ApiBinding's pattern against the
/// clause's execution policy fields. If the pattern matches, the namespace is injected
/// into the Lua environment as a table with proxy functions.
pub struct ApiBinding {
    pub namespace: String,
    pub pattern: Vec<(String, Pattern)>,
    pub handler: Arc<dyn ApiHandler>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_display() {
        let err = ApiError::new("broken");
        assert_eq!(err.to_string(), "broken");
    }

    #[test]
    fn api_error_debug() {
        let err = ApiError::new("broken");
        let debug = format!("{err:?}");
        assert!(debug.contains("broken"), "{debug}");
    }

    #[test]
    fn api_binding_with_empty_pattern() {
        struct DummyHandler;
        impl ApiHandler for DummyHandler {
            fn call(&self, _: &str, _: &str, _: Vec<FieldValue>) -> Result<FieldValue, ApiError> {
                Ok(FieldValue::Null)
            }
        }

        let binding = ApiBinding {
            namespace: "test".into(),
            pattern: vec![],
            handler: Arc::new(DummyHandler),
        };
        assert_eq!(binding.namespace, "test");
        assert!(binding.pattern.is_empty());
    }

    #[test]
    fn api_binding_with_pattern() {
        struct DummyHandler;
        impl ApiHandler for DummyHandler {
            fn call(&self, _: &str, _: &str, _: Vec<FieldValue>) -> Result<FieldValue, ApiError> {
                Ok(FieldValue::Null)
            }
        }

        let binding = ApiBinding {
            namespace: "client".into(),
            pattern: vec![("surface".into(), Pattern::Enum("tui".into()))],
            handler: Arc::new(DummyHandler),
        };
        assert_eq!(binding.namespace, "client");
        assert_eq!(binding.pattern.len(), 1);
    }
}
