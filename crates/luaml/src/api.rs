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

/// Caller-facing declaration of an API namespace binding.
///
/// Binds a namespace to a handler, scoped by pattern match against clause
/// execution policy. When the engine executes a clause, it checks each
/// binding's pattern against the clause's execution policy fields; if the
/// pattern matches, the namespace is injected into the Lua environment as a
/// table with proxy functions.
pub struct ApiBindingSpec {
    pub namespace: String,
    pub pattern: Vec<(String, Pattern)>,
    pub handler: Arc<dyn ApiHandler>,
}

/// Opaque handle returned by [`crate::LuamlEngine::register_api`]. Use it with
/// [`crate::LuamlEngine::unregister_api`] / [`crate::LuamlEngine::replace_api`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApiBindingId(pub(crate) u64);

/// Internal storage: pairs an id with its spec so handlers can be removed or
/// hot-swapped. Crate-private — consumers only see the id and the spec.
pub(crate) struct ApiBindingEntry {
    pub(crate) id: ApiBindingId,
    pub(crate) spec: ApiBindingSpec,
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
    fn api_binding_spec_with_empty_pattern() {
        struct DummyHandler;
        impl ApiHandler for DummyHandler {
            fn call(&self, _: &str, _: &str, _: Vec<FieldValue>) -> Result<FieldValue, ApiError> {
                Ok(FieldValue::Null)
            }
        }

        let spec = ApiBindingSpec {
            namespace: "test".into(),
            pattern: vec![],
            handler: Arc::new(DummyHandler),
        };
        assert_eq!(spec.namespace, "test");
        assert!(spec.pattern.is_empty());
    }

    #[test]
    fn api_binding_spec_with_pattern() {
        struct DummyHandler;
        impl ApiHandler for DummyHandler {
            fn call(&self, _: &str, _: &str, _: Vec<FieldValue>) -> Result<FieldValue, ApiError> {
                Ok(FieldValue::Null)
            }
        }

        let spec = ApiBindingSpec {
            namespace: "client".into(),
            pattern: vec![("surface".into(), Pattern::Enum("tui".into()))],
            handler: Arc::new(DummyHandler),
        };
        assert_eq!(spec.namespace, "client");
        assert_eq!(spec.pattern.len(), 1);
    }
}
