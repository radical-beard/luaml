use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use luaml::api::{ApiError, ApiHandler};
use luaml::types::FieldValue;

use crate::protocol::{ApiCallParams, Request, Response};

/// Shared read/write halves of a connection stream.
pub struct StreamPair {
    pub reader: BufReader<Box<dyn Read + Send>>,
    pub writer: BufWriter<Box<dyn Write + Send>>,
}

/// ApiHandler that sends JSON-RPC `api_call` requests to the consumer
/// and blocks until the response arrives.
///
/// Used during `dispatch` — Lua execution is synchronous, so when a script
/// calls `client.save("file.txt")`, this handler sends the call over the wire,
/// reads the response, and returns the result to Lua.
pub struct RemoteApiHandler {
    stream: Mutex<StreamPair>,
    next_id: AtomicU64,
}

impl RemoteApiHandler {
    pub fn new(stream: StreamPair) -> Self {
        Self {
            stream: Mutex::new(stream),
            // Start api_call IDs at a high range to avoid collisions
            // with consumer request IDs.
            next_id: AtomicU64::new(1_000_000),
        }
    }

    /// Lock the stream and return a mutable reference.
    /// Only valid because the connection is single-threaded.
    pub fn stream(&self) -> std::sync::MutexGuard<'_, StreamPair> {
        self.stream.lock().unwrap()
    }
}

impl ApiHandler for RemoteApiHandler {
    fn call(
        &self,
        namespace: &str,
        method: &str,
        args: Vec<FieldValue>,
    ) -> Result<FieldValue, ApiError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let params = ApiCallParams {
            namespace: namespace.to_string(),
            method: method.to_string(),
            args,
        };

        let request = Request::new(
            "api_call",
            serde_json::to_value(&params).map_err(|e| ApiError::new(e.to_string()))?,
            id,
        );

        let mut stream = self.stream.lock().unwrap();

        // Write request as newline-delimited JSON.
        serde_json::to_writer(&mut stream.writer, &request)
            .map_err(|e| ApiError::new(format!("failed to write api_call: {e}")))?;
        stream
            .writer
            .write_all(b"\n")
            .map_err(|e| ApiError::new(format!("failed to write newline: {e}")))?;
        stream
            .writer
            .flush()
            .map_err(|e| ApiError::new(format!("failed to flush: {e}")))?;

        // Read response (blocking).
        let mut line = String::new();
        stream
            .reader
            .read_line(&mut line)
            .map_err(|e| ApiError::new(format!("failed to read api_call response: {e}")))?;

        if line.is_empty() {
            return Err(ApiError::new("connection closed during api_call"));
        }

        let response: Response = serde_json::from_str(&line)
            .map_err(|e| ApiError::new(format!("invalid api_call response: {e}")))?;

        if response.id != id {
            return Err(ApiError::new(format!(
                "api_call response id mismatch: expected {id}, got {}",
                response.id
            )));
        }

        if let Some(err) = response.error {
            return Err(ApiError::new(err.message));
        }

        match response.result {
            Some(json) => serde_json::from_value(json)
                .map_err(|e| ApiError::new(format!("invalid api_call result: {e}"))),
            None => Ok(FieldValue::Null),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Create a StreamPair from a reader string and a writable buffer.
    /// Returns (handler, write_buffer_ref) where write_buffer_ref can be
    /// read after the handler writes to it.
    fn make_handler(
        reader_data: &str,
    ) -> (RemoteApiHandler, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let reader: Box<dyn Read + Send> =
            Box::new(Cursor::new(reader_data.to_string().into_bytes()));
        let write_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer: Box<dyn Write + Send> = Box::new(SharedWriter(write_buf.clone()));
        let pair = StreamPair {
            reader: std::io::BufReader::new(reader),
            writer: std::io::BufWriter::new(writer),
        };
        (RemoteApiHandler::new(pair), write_buf)
    }

    /// A Write impl that writes to a shared Vec<u8>.
    struct SharedWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn ok_response(id: u64, result: serde_json::Value) -> String {
        let resp = Response::ok(id, result);
        let mut s = serde_json::to_string(&resp).unwrap();
        s.push('\n');
        s
    }

    fn err_response(id: u64, code: i64, msg: &str) -> String {
        let resp = Response::err(id, code, msg);
        let mut s = serde_json::to_string(&resp).unwrap();
        s.push('\n');
        s
    }

    #[test]
    fn api_call_sends_correct_request() {
        let response_line = ok_response(1_000_000, serde_json::json!({"String": "ok"}));
        let (handler, write_buf) = make_handler(&response_line);

        let result = handler.call("client", "save", vec![FieldValue::String("f.txt".into())]);
        assert!(result.is_ok());

        let written = write_buf.lock().unwrap();
        let written_str = String::from_utf8(written.clone()).unwrap();
        let req: crate::protocol::Request = serde_json::from_str(written_str.trim()).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "api_call");
        assert_eq!(req.id, 1_000_000);
    }

    #[test]
    fn api_call_receives_ok_response() {
        let response_line = ok_response(1_000_000, serde_json::json!({"String": "done"}));
        let (handler, _) = make_handler(&response_line);

        let result = handler.call("ns", "method", vec![]).unwrap();
        assert_eq!(result, FieldValue::String("done".into()));
    }

    #[test]
    fn api_call_receives_error_response() {
        let response_line = err_response(1_000_000, -32000, "something failed");
        let (handler, _) = make_handler(&response_line);

        let err = handler.call("ns", "method", vec![]).unwrap_err();
        assert!(err.to_string().contains("something failed"));
    }

    #[test]
    fn api_call_receives_null_result() {
        // Response with result: null
        let response_line = ok_response(1_000_000, serde_json::json!("Null"));
        let (handler, _) = make_handler(&response_line);

        let result = handler.call("ns", "method", vec![]).unwrap();
        assert_eq!(result, FieldValue::Null);
    }

    #[test]
    fn api_call_receives_complex_result() {
        let complex = serde_json::json!({"Map": {"key": {"String": "val"}, "num": {"Number": 42}}});
        let response_line = ok_response(1_000_000, complex);
        let (handler, _) = make_handler(&response_line);

        let result = handler.call("ns", "method", vec![]).unwrap();
        if let FieldValue::Map(m) = result {
            assert_eq!(m.get("key"), Some(&FieldValue::String("val".into())));
            assert_eq!(m.get("num"), Some(&FieldValue::Number(42)));
        } else {
            panic!("expected Map, got {:?}", result);
        }
    }

    #[test]
    fn api_call_connection_closed() {
        // Empty reader — simulates closed connection
        let (handler, _) = make_handler("");

        let err = handler.call("ns", "method", vec![]).unwrap_err();
        assert!(
            err.to_string()
                .contains("connection closed during api_call")
        );
    }

    #[test]
    fn api_call_response_id_mismatch() {
        // Respond with wrong ID
        let response_line = ok_response(999, serde_json::json!({"String": "ok"}));
        let (handler, _) = make_handler(&response_line);

        let err = handler.call("ns", "method", vec![]).unwrap_err();
        assert!(err.to_string().contains("id mismatch"));
    }

    #[test]
    fn api_call_malformed_json_response() {
        let (handler, _) = make_handler("not json at all\n");

        let err = handler.call("ns", "method", vec![]).unwrap_err();
        assert!(err.to_string().contains("invalid api_call response"));
    }

    #[test]
    fn api_call_sequential_calls() {
        // Three responses for three sequential calls
        let lines = format!(
            "{}{}{}",
            ok_response(1_000_000, serde_json::json!({"Number": 1})),
            ok_response(1_000_001, serde_json::json!({"Number": 2})),
            ok_response(1_000_002, serde_json::json!({"Number": 3})),
        );
        let (handler, _) = make_handler(&lines);

        assert_eq!(
            handler.call("ns", "a", vec![]).unwrap(),
            FieldValue::Number(1)
        );
        assert_eq!(
            handler.call("ns", "b", vec![]).unwrap(),
            FieldValue::Number(2)
        );
        assert_eq!(
            handler.call("ns", "c", vec![]).unwrap(),
            FieldValue::Number(3)
        );
    }

    #[test]
    fn api_call_increments_ids() {
        let lines = format!(
            "{}{}",
            ok_response(1_000_000, serde_json::json!({"Number": 1})),
            ok_response(1_000_001, serde_json::json!({"Number": 2})),
        );
        let (handler, write_buf) = make_handler(&lines);

        handler.call("ns", "first", vec![]).unwrap();
        handler.call("ns", "second", vec![]).unwrap();

        let written = write_buf.lock().unwrap();
        let written_str = String::from_utf8(written.clone()).unwrap();
        let lines: Vec<&str> = written_str.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);

        let req1: crate::protocol::Request = serde_json::from_str(lines[0]).unwrap();
        let req2: crate::protocol::Request = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(req1.id, 1_000_000);
        assert_eq!(req2.id, 1_000_001);
    }

    #[test]
    fn api_call_with_various_arg_types() {
        let response_line = ok_response(1_000_000, serde_json::json!("Null"));
        let (handler, write_buf) = make_handler(&response_line);

        let args = vec![
            FieldValue::Enum("status".into()),
            FieldValue::String("hello".into()),
            FieldValue::Number(42),
            FieldValue::Bool(true),
            FieldValue::Null,
        ];

        handler.call("ns", "method", args).unwrap();

        let written = write_buf.lock().unwrap();
        let written_str = String::from_utf8(written.clone()).unwrap();
        let req: crate::protocol::Request = serde_json::from_str(written_str.trim()).unwrap();

        // Verify args are in the params
        let params = req.params;
        let args_json = params.get("args").expect("should have args");
        assert!(args_json.is_array());
        assert_eq!(args_json.as_array().unwrap().len(), 5);
    }
}
