use crate::pattern::Pattern;
use crate::types::FieldValue;
use std::sync::Arc;

/// Error from an API call.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub message: String,
}

/// Consumer-provided handler for API calls from Lua scripts.
pub trait ApiHandler: Send + Sync {
    fn call(
        &self,
        namespace: &str,
        method: &str,
        args: Vec<FieldValue>,
    ) -> Result<FieldValue, ApiError>;
}

/// Binds a namespace to a handler, scoped by pattern match against clause execution policy.
pub struct ApiBinding {
    pub namespace: String,
    pub pattern: Vec<(String, Pattern)>,
    pub handler: Arc<dyn ApiHandler>,
}
