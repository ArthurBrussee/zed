//! The little the relay reads out of a JSON-RPC frame: its id, its method and
//! the session it belongs to. Everything else passes through untouched, which
//! is what keeps the relay the same across protocol versions.

use serde_json::Value;

pub const INITIALIZE: &str = "initialize";
pub const SESSION_NEW: &str = "session/new";
pub const SESSION_LOAD: &str = "session/load";
pub const SESSION_RESUME: &str = "session/resume";
pub const SESSION_PROMPT: &str = "session/prompt";
pub const SESSION_CLOSE: &str = "session/close";
pub const FS_READ: &str = "fs/read_text_file";
pub const FS_WRITE: &str = "fs/write_text_file";
pub const TERMINAL_PREFIX: &str = "terminal/";
/// The turn state the relay reports after a reattach, and whenever a turn ends
/// that the attached client did not start. Shaped after ACP v2's
/// `state_update`, which replaces it once Zed speaks v2.
pub const STATE_UPDATE: &str = "_zed/relay/state_update";

pub enum Kind {
    Request { id: Value, method: String },
    Notification { method: String },
    Response { id: Value },
}

pub fn parse(line: &str) -> Option<(Value, Kind)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;
    let method = object.get("method").and_then(Value::as_str);
    let kind = match (object.get("id"), method) {
        (Some(id), Some(method)) => Kind::Request {
            id: id.clone(),
            method: method.to_string(),
        },
        (None, Some(method)) => Kind::Notification {
            method: method.to_string(),
        },
        (Some(id), None) => Kind::Response { id: id.clone() },
        (None, None) => return None,
    };
    Some((value, kind))
}

pub fn session_id(value: &Value) -> Option<&str> {
    value.get("params")?.get("sessionId")?.as_str()
}

pub fn with_id(mut value: Value, id: Value) -> String {
    value["id"] = id;
    value.to_string()
}

pub fn request(id: u64, method: &str, params: Value) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        .to_string()
}

pub fn notification(method: &str, params: Value) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

pub fn result(id: &Value, result: Value) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

pub fn error(id: &Value, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32603, "message": message },
    })
    .to_string()
}

/// A subagent session this update announces, so its frames can be filed
/// under the session that launched it: AIR's `subagent_spawned` names it
/// `subagentSessionId`, the RFD form's `subagent_update` names it `sessionId`.
pub fn announced_child_session(value: &Value) -> Option<&str> {
    let update = value.get("params")?.get("update")?;
    match update.get("sessionUpdate")?.as_str()? {
        "subagent_spawned" | "subagent_state_update" => {
            update.get("subagentSessionId")?.as_str()
        }
        "subagent_update" => update.get("sessionId")?.as_str(),
        _ => None,
    }
}

pub fn session_title(value: &Value) -> Option<&str> {
    let update = value.get("params")?.get("update")?;
    (update.get("sessionUpdate")?.as_str()? == "session_info_update")
        .then(|| update.get("title")?.as_str())
        .flatten()
}
