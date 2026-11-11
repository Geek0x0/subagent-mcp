//! JSON-RPC 2.0 frames for the MCP stdio transport.
//!
//! Mirrors mcp-go v0.44.0 `server/request_handler.go`: one JSON object per
//! line, a missing or `null` `id` is a notification (never answered), and a
//! non-null `id` with a `method` is a request. Request IDs keep their raw JSON
//! value for the response echo while [`canonical_request_id`] (Go
//! `canonicalRequestID`) supplies the cancellation key.

use serde::Deserialize;
use serde_json::Value;

pub use crate::util::json::canonical_request_id;

/// Invalid JSON was received.
pub const PARSE_ERROR: i32 = -32700;
/// The JSON sent is not a valid request object.
pub const INVALID_REQUEST: i32 = -32600;
/// The method does not exist or is not available.
pub const METHOD_NOT_FOUND: i32 = -32601;
/// Invalid method parameters.
pub const INVALID_PARAMS: i32 = -32602;
/// Internal JSON-RPC error.
pub const INTERNAL_ERROR: i32 = -32603;

/// A JSON-RPC request ID.
///
/// `raw` is the value exactly as it appeared on the wire and is echoed back in
/// the response. `canonical` is the Go `canonicalRequestID` key used to match
/// `notifications/cancelled` to the in-flight call; it is `None` for values Go
/// cannot key (`null` never reaches this type because it is a notification).
#[derive(Debug, Clone, PartialEq)]
pub struct RequestId {
    pub raw: Value,
    pub canonical: Option<String>,
}

impl RequestId {
    pub fn new(raw: Value) -> Self {
        let canonical = canonical_request_id(&raw);
        Self { raw, canonical }
    }
}

/// Serializes as the raw value so responses echo the client's ID unchanged.
impl serde::Serialize for RequestId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(&self.raw, serializer)
    }
}

/// A request that expects a response.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    pub params: Option<Value>,
}

/// A notification that must not be answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Option<Value>,
}

/// A response to a request this server sent (for example an elicitation).
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: RequestId,
    pub result: Option<Value>,
    pub error: Option<RpcError>,
}

/// A JSON-RPC error object carried by a [`Response`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RpcError {
    pub code: i32,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

/// One decoded JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

impl Frame {
    /// The ID this frame carries, if any.
    ///
    /// Notifications never carry an ID and must not be answered.
    pub fn request_id(&self) -> Option<&RequestId> {
        match self {
            Frame::Request(request) => Some(&request.id),
            Frame::Notification(_) => None,
            Frame::Response(response) => Some(&response.id),
        }
    }
}

/// A frame-level rejection that the server turns into a JSON-RPC error
/// response. `code` is one of the `-327xx` constants above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

impl JsonRpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "json-rpc error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for JsonRpcError {}

#[derive(Debug, Deserialize)]
struct RawFrame {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

/// Parses one newline-delimited JSON-RPC frame.
///
/// A missing or `null` `id` yields [`Frame::Notification`], matching mcp-go;
/// the caller must not write a response for it (a `tools/call` notification is
/// still scheduled through the worker path). A non-null `id` with a `method`
/// is a request, and one with `result` or `error` instead of a method is a
/// response. Frame-level failures carry a [`PARSE_ERROR`] or
/// [`INVALID_REQUEST`] code and the mcp-go message text.
pub fn parse_frame(line: &str) -> Result<Frame, JsonRpcError> {
    let value: Value = serde_json::from_str(line).map_err(|_| parse_error())?;
    if value.is_null() {
        // Go's encoding/json decodes `null` into a struct as a no-op, leaving
        // the version empty; mcp-go then reports an invalid version.
        return Err(invalid_version());
    }
    if !value.is_object() {
        // Arrays and scalars cannot decode into mcp-go's message struct.
        return Err(parse_error());
    }
    let RawFrame {
        jsonrpc,
        id,
        method,
        params,
        result,
        error,
    } = serde_json::from_value(value).map_err(|_| parse_error())?;
    if jsonrpc.as_deref() != Some("2.0") {
        return Err(invalid_version());
    }

    let Some(raw_id) = id else {
        return Ok(Frame::Notification(Notification {
            method: method.unwrap_or_default(),
            params,
        }));
    };
    if !matches!(&raw_id, Value::String(_) | Value::Number(_)) {
        return Err(JsonRpcError::new(
            INVALID_REQUEST,
            format!("invalid request id: {raw_id}"),
        ));
    }
    let id = RequestId::new(raw_id);

    Ok(match method {
        Some(method) => Frame::Request(Request { id, method, params }),
        None if result.is_some() || error.is_some() => {
            Frame::Response(Response { id, result, error })
        }
        // mcp-go dispatches an id-only object as a request with an empty
        // method, producing `Method  not found` (-32601); keep that shape so
        // the server's dispatch stays byte-compatible.
        None => Frame::Request(Request {
            id,
            method: String::new(),
            params,
        }),
    })
}

fn parse_error() -> JsonRpcError {
    JsonRpcError::new(PARSE_ERROR, "Failed to parse message")
}

fn invalid_version() -> JsonRpcError {
    JsonRpcError::new(INVALID_REQUEST, "Invalid JSON-RPC version")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_request(line: &str) -> Request {
        match parse_frame(line).expect("frame should parse") {
            Frame::Request(request) => request,
            other => panic!("expected request, got {other:?}"),
        }
    }

    fn parse_notification(line: &str) -> Notification {
        match parse_frame(line).expect("frame should parse") {
            Frame::Notification(notification) => notification,
            other => panic!("expected notification, got {other:?}"),
        }
    }

    fn parse_response(line: &str) -> Response {
        match parse_frame(line).expect("frame should parse") {
            Frame::Response(response) => response,
            other => panic!("expected response, got {other:?}"),
        }
    }

    fn parse_error(line: &str) -> JsonRpcError {
        match parse_frame(line) {
            Ok(frame) => panic!("expected error, got {frame:?}"),
            Err(err) => err,
        }
    }

    #[test]
    fn brief_cases_parse_requests_and_null_notification() {
        assert!(matches!(
            parse_frame(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap(),
            Frame::Request(_)
        ));
        assert!(matches!(
            parse_frame(r#"{"jsonrpc":"2.0","id":1.5,"method":"x"}"#).unwrap(),
            Frame::Request(_)
        ));
        assert!(matches!(
            parse_frame(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).unwrap(),
            Frame::Notification(_)
        ));
        assert_eq!(canonical_request_id(&json!(1.0)), Some("int64:1".into()));
    }

    #[test]
    fn numeric_ids_use_go_float64_grouping() {
        let cases = [
            (r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#, "int64:1"),
            (r#"{"jsonrpc":"2.0","id":1.0,"method":"x"}"#, "int64:1"),
            (r#"{"jsonrpc":"2.0","id":1e0,"method":"x"}"#, "int64:1"),
            (r#"{"jsonrpc":"2.0","id":1.5,"method":"x"}"#, "float64:1.5"),
            (r#"{"jsonrpc":"2.0","id":-3,"method":"x"}"#, "int64:-3"),
            (r#"{"jsonrpc":"2.0","id":0,"method":"x"}"#, "int64:0"),
        ];
        for (line, want) in cases {
            let request = parse_request(line);
            assert_eq!(request.id.canonical.as_deref(), Some(want), "line={line}");
        }
    }

    #[test]
    fn string_ids_are_canonicalized_and_echoed() {
        let request = parse_request(r#"{"jsonrpc":"2.0","id":"req-7","method":"tools/list"}"#);
        assert_eq!(request.id.raw, json!("req-7"));
        assert_eq!(request.id.canonical.as_deref(), Some("string:req-7"));
        assert_eq!(
            serde_json::to_string(&request.id).unwrap(),
            "\"req-7\"",
            "response echo must keep the raw string id"
        );
    }

    #[test]
    fn notifications_without_ids_are_not_requests() {
        let notification =
            parse_notification(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!(notification.method, "notifications/initialized");
        assert!(notification.params.is_none());
    }

    #[test]
    fn null_id_is_a_notification_that_must_not_be_answered() {
        let frame = parse_frame(
            r#"{"jsonrpc":"2.0","id":null,"method":"tools/call","params":{"name":"start"}}"#,
        )
        .unwrap();
        assert!(matches!(frame, Frame::Notification(_)));
        assert!(
            frame.request_id().is_none(),
            "id:null must not produce a response id"
        );
    }

    #[test]
    fn response_without_an_id_is_a_notification_like_go() {
        // mcp-go checks `id == nil` before looking at `result`, so a result
        // frame without an id is handled as a notification, not a response.
        assert!(matches!(
            parse_frame(r#"{"jsonrpc":"2.0","result":{}}"#).unwrap(),
            Frame::Notification(_)
        ));
    }

    #[test]
    fn tools_call_notification_keeps_its_params_for_the_worker_path() {
        // Task 6 owns the dispatch half: this notification still occupies a
        // worker slot even though no response is written.
        let notification = parse_notification(
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"start","arguments":{"prompt":"hi"}}}"#,
        );
        assert_eq!(notification.method, "tools/call");
        assert_eq!(
            notification.params,
            Some(json!({"name":"start","arguments":{"prompt":"hi"}}))
        );
    }

    #[test]
    fn request_params_are_preserved() {
        let request = parse_request(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"reply","arguments":{"text":"hi"}}}"#,
        );
        assert_eq!(request.method, "tools/call");
        assert_eq!(
            request.params,
            Some(json!({"name":"reply","arguments":{"text":"hi"}}))
        );
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        for line in [
            "",
            "   ",
            "{",
            r#"{"jsonrpc":"2.0""#,
            "not json",
            r#"{"jsonrpc":"2.0","method":1}"#,
        ] {
            let err = parse_error(line);
            assert_eq!(err.code, PARSE_ERROR, "line={line}");
            assert_eq!(err.message, "Failed to parse message", "line={line}");
        }
    }

    #[test]
    fn missing_or_wrong_jsonrpc_version_is_invalid_request() {
        for line in [
            r#"{"id":1,"method":"initialize"}"#,
            r#"{"jsonrpc":"1.0","id":1,"method":"initialize"}"#,
            r#"{"jsonrpc":null,"id":1,"method":"initialize"}"#,
            "null",
        ] {
            let err = parse_error(line);
            assert_eq!(err.code, INVALID_REQUEST, "line={line}");
            assert_eq!(err.message, "Invalid JSON-RPC version", "line={line}");
        }
    }

    #[test]
    fn non_object_frames_are_parse_errors() {
        for line in ["[]", "[{}]", r#""hello""#, "42", "true"] {
            let err = parse_error(line);
            assert_eq!(err.code, PARSE_ERROR, "line={line}");
        }
    }

    #[test]
    fn response_frames_carry_result_or_error() {
        let response = parse_response(r#"{"jsonrpc":"2.0","id":7,"result":{"action":"accept"}}"#);
        assert_eq!(response.id.canonical.as_deref(), Some("int64:7"));
        assert_eq!(response.result, Some(json!({"action":"accept"})));
        assert!(response.error.is_none());

        let response = parse_response(
            r#"{"jsonrpc":"2.0","id":"elicit-1","error":{"code":-32601,"message":"Method not found","data":{"detail":"x"}}}"#,
        );
        assert_eq!(response.id.canonical.as_deref(), Some("string:elicit-1"));
        assert!(response.result.is_none());
        let error = response.error.expect("error should parse");
        assert_eq!(error.code, -32601);
        assert_eq!(error.message, "Method not found");
        assert_eq!(error.data, Some(json!({"detail":"x"})));
    }

    #[test]
    fn response_echo_preserves_the_raw_numeric_id() {
        let response = parse_response(r#"{"jsonrpc":"2.0","id":1.0,"result":{}}"#);
        assert_eq!(
            serde_json::to_string(&response.id).unwrap(),
            "1.0",
            "raw 1.0 must stay 1.0 in the response echo"
        );
        assert_eq!(response.id.canonical.as_deref(), Some("int64:1"));

        let response = parse_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert_eq!(serde_json::to_string(&response.id).unwrap(), "1");

        let response = parse_response(r#"{"jsonrpc":"2.0","id":9007199254740993,"result":{}}"#);
        assert_eq!(
            serde_json::to_string(&response.id).unwrap(),
            "9007199254740993",
            "raw integer spelling must be echoed unchanged"
        );
        assert_eq!(
            response.id.canonical.as_deref(),
            Some("int64:9007199254740992")
        );
    }

    #[test]
    fn canonicalization_uses_go_float64_precision() {
        // Go decodes JSON numbers as float64, so 2^53+1 loses its low bit even
        // though the raw id is echoed unchanged.
        let request = parse_request(r#"{"jsonrpc":"2.0","id":9007199254740993,"method":"x"}"#);
        assert_eq!(
            request.id.canonical.as_deref(),
            Some("int64:9007199254740992")
        );
    }

    #[test]
    fn out_of_range_numbers_use_the_float64_branch() {
        // Observed from Go 1.27 on amd64: int64(1e300) is
        // implementation-defined and lands on MinInt64, so the integral check
        // fails and FormatFloat('f', -1, 64) prints the positional decimal
        // expansion.
        let request = parse_request(r#"{"jsonrpc":"2.0","id":1e300,"method":"x"}"#);
        assert_eq!(
            request.id.canonical,
            Some(format!("float64:1{}", "0".repeat(300)))
        );

        // Just past MaxInt64 the same branch applies.
        let request = parse_request(r#"{"jsonrpc":"2.0","id":1e19,"method":"x"}"#);
        assert_eq!(
            request.id.canonical.as_deref(),
            Some("float64:10000000000000000000")
        );

        // MinInt64 is exactly representable, so it stays an integer key.
        let request = parse_request(r#"{"jsonrpc":"2.0","id":-9223372036854775809,"method":"x"}"#);
        assert_eq!(
            request.id.canonical.as_deref(),
            Some("int64:-9223372036854775808")
        );
    }

    #[test]
    fn out_of_range_exponents_are_parse_errors() {
        // Go's encoding/json rejects numbers that overflow float64.
        let err = parse_error(r#"{"jsonrpc":"2.0","id":1e400,"method":"x"}"#);
        assert_eq!(err.code, PARSE_ERROR);
        assert_eq!(err.message, "Failed to parse message");
    }

    #[test]
    fn id_only_frame_is_a_request_with_an_empty_method() {
        // mcp-go's dispatcher answers `Method  not found` (-32601) for this
        // shape, so the parser keeps it a request instead of rejecting it.
        let request = parse_request(r#"{"jsonrpc":"2.0","id":3}"#);
        assert_eq!(request.method, "");
        assert_eq!(request.id.canonical.as_deref(), Some("int64:3"));
    }

    #[test]
    fn non_scalar_ids_are_rejected() {
        for line in [
            r#"{"jsonrpc":"2.0","id":true,"method":"x"}"#,
            r#"{"jsonrpc":"2.0","id":{"a":1},"method":"x"}"#,
            r#"{"jsonrpc":"2.0","id":[1],"method":"x"}"#,
        ] {
            let err = parse_error(line);
            assert_eq!(err.code, INVALID_REQUEST, "line={line}");
            assert!(
                err.message.starts_with("invalid request id: "),
                "message={}",
                err.message
            );
        }
    }

    #[test]
    fn json_rpc_error_display_names_code_and_message() {
        let err = JsonRpcError::new(PARSE_ERROR, "Failed to parse message");
        assert_eq!(
            err.to_string(),
            "json-rpc error -32700: Failed to parse message"
        );
    }
}
