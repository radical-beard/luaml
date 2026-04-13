use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use luaml::LuamlEngine;
use luaml::api::ApiBinding;
use luaml::pattern::Pattern;
use luaml::types::FieldValue;

use crate::protocol::*;
use crate::remote_api::{RemoteApiHandler, StreamPair};

/// Handle a single client connection.
///
/// Each connection gets its own LuamlEngine. The connection handler reads
/// JSON-RPC requests, processes them, and writes responses. During `dispatch`,
/// the RemoteApiHandler may send `api_call` requests to the consumer and
/// read responses — all on the same thread.
pub fn handle_connection(stream: TcpStream) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".into());
    eprintln!("connection from {peer}");

    let reader: Box<dyn Read + Send> = Box::new(
        stream
            .try_clone()
            .expect("failed to clone stream for reader"),
    );
    let writer: Box<dyn Write + Send> = Box::new(stream);

    handle_stream(reader, writer);

    eprintln!("connection from {peer} closed");
}

/// Handle a connection given raw read/write halves.
/// Factored out from handle_connection for testability.
pub fn handle_stream(reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>) {
    let stream_pair = StreamPair {
        reader: BufReader::new(reader),
        writer: BufWriter::new(writer),
    };

    let handler = Arc::new(RemoteApiHandler::new(stream_pair));
    let mut engine = LuamlEngine::new().expect("failed to create Lua VM");

    loop {
        // Read next request from the consumer.
        let mut line = String::new();
        {
            let mut stream = handler.stream();
            match stream.reader.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {}
                Err(e) => {
                    eprintln!("read error: {e}");
                    break;
                }
            }
        }
        // Stream lock released here — dispatch can use it for api_calls.

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::err(0, PARSE_ERROR, format!("invalid JSON: {e}"));
                write_response(&handler, &resp);
                continue;
            }
        };

        let response = process_request(&mut engine, &handler, &request);
        write_response(&handler, &response);
    }
}

fn process_request(
    engine: &mut LuamlEngine,
    handler: &Arc<RemoteApiHandler>,
    request: &Request,
) -> Response {
    match request.method.as_str() {
        "register" => handle_register(engine, request),
        "register_api" => handle_register_api(engine, handler, request),
        "dispatch" => handle_dispatch(engine, request),
        "query" => handle_query(engine, request),
        "query_subset" => handle_query_subset(engine, request),
        _ => Response::err(
            request.id,
            METHOD_NOT_FOUND,
            format!("unknown method: {}", request.method),
        ),
    }
}

fn handle_register(engine: &mut LuamlEngine, request: &Request) -> Response {
    let params: RegisterParams = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(request.id, INVALID_PARAMS, e.to_string()),
    };

    match engine.register(&params.source_path, &params.text) {
        Ok(()) => Response::ok(request.id, serde_json::json!({"ok": true})),
        Err(e) => Response::err(request.id, LUAML_ERROR, e.to_string()),
    }
}

fn handle_register_api(
    engine: &mut LuamlEngine,
    handler: &Arc<RemoteApiHandler>,
    request: &Request,
) -> Response {
    let params: RegisterApiParams = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(request.id, INVALID_PARAMS, e.to_string()),
    };

    // Convert JSON pattern map to Vec<(String, Pattern)>.
    let pattern = match json_map_to_pattern(&params.pattern) {
        Ok(p) => p,
        Err(e) => return Response::err(request.id, INVALID_PARAMS, e),
    };

    engine.register_api(ApiBinding {
        namespace: params.namespace,
        pattern,
        handler: handler.clone(),
    });

    Response::ok(request.id, serde_json::json!({"ok": true}))
}

fn handle_dispatch(engine: &LuamlEngine, request: &Request) -> Response {
    let params: DispatchParams = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(request.id, INVALID_PARAMS, e.to_string()),
    };

    // Convert JSON event map to FieldMap.
    let event = match json_map_to_field_map(&params.event) {
        Ok(e) => e,
        Err(e) => return Response::err(request.id, INVALID_PARAMS, e),
    };

    match engine.dispatch(&event) {
        Ok(results) => {
            let matches: Vec<DispatchMatch> = results
                .iter()
                .map(|r| DispatchMatch {
                    script_path: r.script_path.display().to_string(),
                    bindings: r.bindings.clone(),
                })
                .collect();
            let result = DispatchResult { matches };
            Response::ok(
                request.id,
                serde_json::to_value(&result).unwrap_or(serde_json::json!(null)),
            )
        }
        Err(e) => Response::err(request.id, LUAML_ERROR, e.to_string()),
    }
}

fn handle_query(engine: &LuamlEngine, request: &Request) -> Response {
    let params: DispatchParams = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(request.id, INVALID_PARAMS, e.to_string()),
    };

    let event = match json_map_to_field_map(&params.event) {
        Ok(e) => e,
        Err(e) => return Response::err(request.id, INVALID_PARAMS, e),
    };

    let matches: Vec<DispatchMatch> = engine
        .query(&event)
        .iter()
        .map(|m| DispatchMatch {
            script_path: m.script.source_path.display().to_string(),
            bindings: m.bindings.clone(),
        })
        .collect();

    let result = DispatchResult { matches };
    Response::ok(
        request.id,
        serde_json::to_value(&result).unwrap_or(serde_json::json!(null)),
    )
}

fn handle_query_subset(engine: &LuamlEngine, request: &Request) -> Response {
    let params: DispatchParams = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(request.id, INVALID_PARAMS, e.to_string()),
    };

    let event = match json_map_to_field_map(&params.event) {
        Ok(e) => e,
        Err(e) => return Response::err(request.id, INVALID_PARAMS, e),
    };

    let matches: Vec<SubsetQueryMatch> = engine
        .query_subset(&event)
        .iter()
        .map(|r| SubsetQueryMatch {
            script_path: r.script.source_path.display().to_string(),
            clause_index: r.clause_index,
            annotations: r.clause.annotations.clone(),
            field_annotations: r.clause.field_annotations.clone(),
        })
        .collect();

    let result = SubsetQueryResult { matches };
    Response::ok(
        request.id,
        serde_json::to_value(&result).unwrap_or(serde_json::json!(null)),
    )
}

/// Write a JSON-RPC response as newline-delimited JSON.
fn write_response(handler: &RemoteApiHandler, response: &Response) {
    let mut stream = handler.stream();
    if let Err(e) = serde_json::to_writer(&mut stream.writer, response) {
        eprintln!("write error: {e}");
        return;
    }
    let _ = stream.writer.write_all(b"\n");
    let _ = stream.writer.flush();
}

/// Convert a JSON map of FieldValues to a pattern list for API binding.
/// Only literal FieldValues are valid in API binding patterns.
fn json_map_to_pattern(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<(String, Pattern)>, String> {
    let mut patterns = Vec::new();
    for (key, value) in map {
        let field_value: FieldValue = serde_json::from_value(value.clone())
            .map_err(|e| format!("invalid pattern value for '{key}': {e}"))?;
        let pattern = field_value_to_pattern(&field_value).ok_or_else(|| {
            format!("pattern value for '{key}' must be a literal (Enum, String, Number, or Bool)")
        })?;
        patterns.push((key.clone(), pattern));
    }
    Ok(patterns)
}

/// Convert a literal FieldValue to a Pattern.
fn field_value_to_pattern(value: &FieldValue) -> Option<Pattern> {
    match value {
        FieldValue::Enum(s) => Some(Pattern::Enum(s.clone())),
        FieldValue::String(s) => Some(Pattern::StringLiteral(s.clone())),
        FieldValue::Number(n) => Some(Pattern::NumberLiteral(*n)),
        FieldValue::Bool(b) => Some(Pattern::BoolLiteral(*b)),
        _ => None,
    }
}

/// Convert a JSON map to a FieldMap.
fn json_map_to_field_map(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<luaml::types::FieldMap, String> {
    let mut field_map = luaml::types::FieldMap::new();
    for (key, value) in map {
        let field_value: FieldValue = serde_json::from_value(value.clone())
            .map_err(|e| format!("invalid field value for '{key}': {e}"))?;
        field_map.insert(key.clone(), field_value);
    }
    Ok(field_map)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── json_map_to_pattern ──────────────────────────────────────────

    #[test]
    fn json_map_to_pattern_enum() {
        let mut map = serde_json::Map::new();
        map.insert("type".into(), serde_json::json!({"Enum": "input"}));
        let result = json_map_to_pattern(&map).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "type");
        match &result[0].1 {
            Pattern::Enum(s) => assert_eq!(s, "input"),
            other => panic!("expected Enum, got {:?}", other),
        }
    }

    #[test]
    fn json_map_to_pattern_string() {
        let mut map = serde_json::Map::new();
        map.insert("key".into(), serde_json::json!({"String": "q"}));
        let result = json_map_to_pattern(&map).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "key");
        match &result[0].1 {
            Pattern::StringLiteral(s) => assert_eq!(s, "q"),
            other => panic!("expected StringLiteral, got {:?}", other),
        }
    }

    #[test]
    fn json_map_to_pattern_number() {
        let mut map = serde_json::Map::new();
        map.insert("depth".into(), serde_json::json!({"Number": 5}));
        let result = json_map_to_pattern(&map).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "depth");
        match &result[0].1 {
            Pattern::NumberLiteral(n) => assert_eq!(*n, 5),
            other => panic!("expected NumberLiteral, got {:?}", other),
        }
    }

    #[test]
    fn json_map_to_pattern_bool() {
        let mut map = serde_json::Map::new();
        map.insert("active".into(), serde_json::json!({"Bool": true}));
        let result = json_map_to_pattern(&map).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "active");
        match &result[0].1 {
            Pattern::BoolLiteral(b) => assert!(*b),
            other => panic!("expected BoolLiteral, got {:?}", other),
        }
    }

    #[test]
    fn json_map_to_pattern_null_rejected() {
        let mut map = serde_json::Map::new();
        map.insert("x".into(), serde_json::json!("Null"));
        let result = json_map_to_pattern(&map);
        assert!(result.is_err());
    }

    #[test]
    fn json_map_to_pattern_list_rejected() {
        let mut map = serde_json::Map::new();
        map.insert("x".into(), serde_json::json!({"List": []}));
        let result = json_map_to_pattern(&map);
        assert!(result.is_err());
    }

    #[test]
    fn json_map_to_pattern_map_rejected() {
        let mut map = serde_json::Map::new();
        map.insert("x".into(), serde_json::json!({"Map": {}}));
        let result = json_map_to_pattern(&map);
        assert!(result.is_err());
    }

    #[test]
    fn json_map_to_pattern_float_rejected() {
        let mut map = serde_json::Map::new();
        map.insert("x".into(), serde_json::json!({"Float": 3.14}));
        let result = json_map_to_pattern(&map);
        assert!(result.is_err());
    }

    #[test]
    fn json_map_to_pattern_empty_map() {
        let map = serde_json::Map::new();
        let result = json_map_to_pattern(&map).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn json_map_to_pattern_invalid_json_value() {
        let mut map = serde_json::Map::new();
        map.insert("x".into(), serde_json::json!(42));
        let result = json_map_to_pattern(&map);
        assert!(result.is_err());
    }

    // ── field_value_to_pattern ───────────────────────────────────────

    #[test]
    fn field_value_to_pattern_enum() {
        let fv = FieldValue::Enum("tui".into());
        match field_value_to_pattern(&fv) {
            Some(Pattern::Enum(s)) => assert_eq!(s, "tui"),
            other => panic!("expected Some(Enum), got {:?}", other),
        }
    }

    #[test]
    fn field_value_to_pattern_string() {
        let fv = FieldValue::String("q".into());
        match field_value_to_pattern(&fv) {
            Some(Pattern::StringLiteral(s)) => assert_eq!(s, "q"),
            other => panic!("expected Some(StringLiteral), got {:?}", other),
        }
    }

    #[test]
    fn field_value_to_pattern_number() {
        let fv = FieldValue::Number(42);
        match field_value_to_pattern(&fv) {
            Some(Pattern::NumberLiteral(n)) => assert_eq!(n, 42),
            other => panic!("expected Some(NumberLiteral), got {:?}", other),
        }
    }

    #[test]
    fn field_value_to_pattern_bool() {
        let fv = FieldValue::Bool(true);
        match field_value_to_pattern(&fv) {
            Some(Pattern::BoolLiteral(b)) => assert!(b),
            other => panic!("expected Some(BoolLiteral), got {:?}", other),
        }
    }

    #[test]
    fn field_value_to_pattern_null() {
        let fv = FieldValue::Null;
        assert!(field_value_to_pattern(&fv).is_none());
    }

    #[test]
    fn field_value_to_pattern_float() {
        let fv = FieldValue::Float(3.14);
        assert!(field_value_to_pattern(&fv).is_none());
    }

    #[test]
    fn field_value_to_pattern_list() {
        let fv = FieldValue::List(vec![]);
        assert!(field_value_to_pattern(&fv).is_none());
    }

    #[test]
    fn field_value_to_pattern_map() {
        let fv = FieldValue::Map(luaml::types::FieldMap::new());
        assert!(field_value_to_pattern(&fv).is_none());
    }

    // ── json_map_to_field_map ────────────────────────────────────────

    #[test]
    fn json_map_to_field_map_all_types() {
        let mut map = serde_json::Map::new();
        map.insert("mode".into(), serde_json::json!({"Enum": "direct"}));
        map.insert("name".into(), serde_json::json!({"String": "alice"}));
        map.insert("count".into(), serde_json::json!({"Number": 7}));
        let result = json_map_to_field_map(&map).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result.get("mode"), Some(&FieldValue::Enum("direct".into())));
        assert_eq!(
            result.get("name"),
            Some(&FieldValue::String("alice".into()))
        );
        assert_eq!(result.get("count"), Some(&FieldValue::Number(7)));
    }

    #[test]
    fn json_map_to_field_map_empty() {
        let map = serde_json::Map::new();
        let result = json_map_to_field_map(&map).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn json_map_to_field_map_invalid_value() {
        let mut map = serde_json::Map::new();
        map.insert("x".into(), serde_json::json!("not_a_field_value"));
        let result = json_map_to_field_map(&map);
        assert!(result.is_err());
    }
}
