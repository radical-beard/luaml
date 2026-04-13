use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// JSON-RPC 2.0 request (sent by either side).
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: JsonValue,
    pub id: u64,
}

/// JSON-RPC 2.0 response (sent by either side).
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
    pub id: u64,
}

/// JSON-RPC error object.
#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

// Standard JSON-RPC error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const LUAML_ERROR: i64 = -32000;

impl Request {
    pub fn new(method: impl Into<String>, params: JsonValue, id: u64) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
            id,
        }
    }
}

impl Response {
    pub fn ok(id: u64, result: JsonValue) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            result: Some(result),
            error: None,
            id,
        }
    }

    pub fn err(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
            id,
        }
    }
}

/// Parameters for the `register` method.
#[derive(Debug, Deserialize)]
pub struct RegisterParams {
    pub source_path: String,
    pub text: String,
}

/// Parameters for the `register_api` method.
/// Pattern is a map of field names to FieldValues (literals only).
#[derive(Debug, Deserialize)]
pub struct RegisterApiParams {
    pub namespace: String,
    #[serde(default)]
    pub pattern: serde_json::Map<String, JsonValue>,
}

/// Parameters for the `dispatch` method.
#[derive(Debug, Deserialize)]
pub struct DispatchParams {
    pub event: serde_json::Map<String, JsonValue>,
}

/// Parameters for the `api_call` callback (service → consumer).
#[derive(Debug, Serialize)]
pub struct ApiCallParams {
    pub namespace: String,
    pub method: String,
    pub args: Vec<luaml::types::FieldValue>,
}

/// Single match in a dispatch result.
#[derive(Debug, Serialize)]
pub struct DispatchMatch {
    pub script_path: String,
    pub bindings: luaml::types::FieldBindings,
}

/// Result of the `dispatch` method.
#[derive(Debug, Serialize)]
pub struct DispatchResult {
    pub matches: Vec<DispatchMatch>,
}

/// Single result from a subset query.
#[derive(Debug, Serialize)]
pub struct SubsetQueryMatch {
    pub script_path: String,
    pub clause_index: usize,
    pub annotations: Vec<(String, String)>,
    pub field_annotations: std::collections::BTreeMap<String, Vec<(String, String)>>,
}

/// Result of the `query_subset` method.
#[derive(Debug, Serialize)]
pub struct SubsetQueryResult {
    pub matches: Vec<SubsetQueryMatch>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_str, json, to_string};
    use std::collections::HashMap;

    // ── Request (4) ──

    #[test]
    fn request_new_creates_valid_json_rpc() {
        let req = Request::new("test", json!(null), 1);
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "test");
        assert_eq!(req.id, 1);
        assert_eq!(req.params, json!(null));
    }

    #[test]
    fn request_serialization_roundtrip() {
        let req = Request::new("doStuff", json!({"a": 1}), 42);
        let json_str = to_string(&req).unwrap();
        let back: Request = from_str(&json_str).unwrap();
        assert_eq!(back.jsonrpc, "2.0");
        assert_eq!(back.method, "doStuff");
        assert_eq!(back.id, 42);
        assert_eq!(back.params, json!({"a": 1}));
    }

    #[test]
    fn request_deserialization_missing_params() {
        let raw = r#"{"jsonrpc":"2.0","method":"test","id":1}"#;
        let req: Request = from_str(raw).unwrap();
        assert_eq!(req.params, JsonValue::Null);
    }

    #[test]
    fn request_deserialization_with_object_params() {
        let raw = r#"{"jsonrpc":"2.0","method":"m","params":{"x":1},"id":2}"#;
        let req: Request = from_str(raw).unwrap();
        assert_eq!(req.params, json!({"x": 1}));
    }

    // ── Response (6) ──

    #[test]
    fn response_ok_serialization() {
        let resp = Response::ok(1, json!("hi"));
        let json_str = to_string(&resp).unwrap();
        let v: serde_json::Value = from_str(&json_str).unwrap();
        assert_eq!(v["result"], json!("hi"));
        assert!(v.get("error").is_none());
    }

    #[test]
    fn response_err_serialization() {
        let resp = Response::err(1, -32600, "bad");
        let json_str = to_string(&resp).unwrap();
        let v: serde_json::Value = from_str(&json_str).unwrap();
        assert_eq!(v["error"]["code"], json!(-32600));
        assert_eq!(v["error"]["message"], json!("bad"));
        assert!(v.get("result").is_none());
    }

    #[test]
    fn response_ok_omits_error_field() {
        let resp = Response::ok(1, json!("hi"));
        let json_str = to_string(&resp).unwrap();
        assert!(!json_str.contains("\"error\""));
    }

    #[test]
    fn response_err_omits_result_field() {
        let resp = Response::err(1, -32600, "bad");
        let json_str = to_string(&resp).unwrap();
        assert!(!json_str.contains("\"result\""));
    }

    #[test]
    fn response_roundtrip_ok() {
        let resp = Response::ok(7, json!({"data": [1, 2, 3]}));
        let json_str = to_string(&resp).unwrap();
        let back: Response = from_str(&json_str).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.result, Some(json!({"data": [1, 2, 3]})));
        assert!(back.error.is_none());
    }

    #[test]
    fn response_roundtrip_err() {
        let resp = Response::err(9, -32700, "parse error");
        let json_str = to_string(&resp).unwrap();
        let back: Response = from_str(&json_str).unwrap();
        assert_eq!(back.id, 9);
        assert!(back.result.is_none());
        let err = back.error.unwrap();
        assert_eq!(err.code, -32700);
        assert_eq!(err.message, "parse error");
    }

    // ── Param structs (6) ──

    #[test]
    fn register_params_deserialization() {
        let raw = r#"{"source_path":"a.luaml","text":"---\n"}"#;
        let p: RegisterParams = from_str(raw).unwrap();
        assert_eq!(p.source_path, "a.luaml");
        assert_eq!(p.text, "---\n");
    }

    #[test]
    fn register_params_missing_field() {
        let raw = r#"{"source_path":"a.luaml"}"#;
        let result = from_str::<RegisterParams>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn register_api_params_deserialization() {
        let raw = r#"{"namespace":"tool","pattern":{"kind":"tool"}}"#;
        let p: RegisterApiParams = from_str(raw).unwrap();
        assert_eq!(p.namespace, "tool");
        assert_eq!(p.pattern.len(), 1);
        assert_eq!(p.pattern["kind"], json!("tool"));
    }

    #[test]
    fn register_api_params_empty_pattern() {
        let raw = r#"{"namespace":"ns","pattern":{}}"#;
        let p: RegisterApiParams = from_str(raw).unwrap();
        assert_eq!(p.namespace, "ns");
        assert!(p.pattern.is_empty());
    }

    #[test]
    fn dispatch_params_deserialization() {
        let raw = r#"{"event":{"type":{"Enum":"input"}}}"#;
        let p: DispatchParams = from_str(raw).unwrap();
        assert_eq!(p.event["type"], json!({"Enum": "input"}));
    }

    #[test]
    fn api_call_params_serialization() {
        let params = ApiCallParams {
            namespace: "tool".into(),
            method: "run".into(),
            args: vec![
                luaml::types::FieldValue::String("hello".into()),
                luaml::types::FieldValue::Number(42),
            ],
        };
        let json_str = to_string(&params).unwrap();
        let v: serde_json::Value = from_str(&json_str).unwrap();
        assert_eq!(v["namespace"], json!("tool"));
        assert_eq!(v["method"], json!("run"));
        assert!(v["args"].is_array());
        assert_eq!(v["args"].as_array().unwrap().len(), 2);
    }

    // ── Other (2) ──

    #[test]
    fn dispatch_result_serialization() {
        let mut bindings = HashMap::new();
        bindings.insert(
            "name".into(),
            luaml::types::FieldValue::String("test".into()),
        );
        let result = DispatchResult {
            matches: vec![DispatchMatch {
                script_path: "scripts/a.luaml".into(),
                bindings,
            }],
        };
        let json_str = to_string(&result).unwrap();
        let v: serde_json::Value = from_str(&json_str).unwrap();
        assert_eq!(v["matches"].as_array().unwrap().len(), 1);
        assert_eq!(v["matches"][0]["script_path"], json!("scripts/a.luaml"));
    }

    #[test]
    fn error_code_constants() {
        assert_eq!(PARSE_ERROR, -32700);
        assert_eq!(METHOD_NOT_FOUND, -32601);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(LUAML_ERROR, -32000);
    }
}
