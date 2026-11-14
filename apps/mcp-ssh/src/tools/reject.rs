//! The operator's log line for a `tools/call` that never reaches a tool body:
//! arguments rmcp's `Parameters<T>` will refuse (unknown field, wrong type,
//! unknown `file` action) or a tool that doesn't exist. Without it the journal is
//! silent while an agent loops on a misspelt param. Names the tool and a fixed
//! reason word only — never a key, a value or serde's message, which quotes the
//! offending input (`invalid type: string "…"`).
use rmcp::model::{CallToolRequestParams, JsonObject, RequestId};
use serde::Deserialize;

use super::args::{BashArgs, FileArgs, JobArgs};

/// A refused call: which tool (a fixed word — never the name the client sent)
/// and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rejection {
    pub tool: &'static str,
    pub reason: &'static str,
}

/// Warn when `request` is one the router will answer without running a tool.
pub(super) fn log_if_rejected(request: &CallToolRequestParams, request_id: &RequestId) {
    if let Some(Rejection { tool, reason }) = rejection(&request.name, request.arguments.as_ref()) {
        tracing::warn!(tool, reason, %request_id, "rejected");
    }
}

/// Why the router will refuse this call, if it will. Deserializes the borrowed
/// arguments exactly as `Parameters<T>` does the owned ones, so the two can't
/// disagree; the copy of the string arguments is dropped at once.
pub(super) fn rejection(name: &str, arguments: Option<&JsonObject>) -> Option<Rejection> {
    let none = JsonObject::new();
    let arguments = arguments.unwrap_or(&none);
    let (tool, reason) = match name {
        "bash" => ("bash", invalid::<BashArgs>(arguments)),
        "job" => ("job", invalid::<JobArgs>(arguments)),
        "file" => ("file", invalid::<FileArgs>(arguments)),
        _ => ("unknown", Some("unknown_tool")),
    };
    reason.map(|reason| Rejection { tool, reason })
}

fn invalid<'de, T: Deserialize<'de>>(arguments: &'de JsonObject) -> Option<&'static str> {
    T::deserialize(arguments)
        .err()
        .map(|e| reason(&e.to_string()))
}

/// serde's error kinds, by the fixed words its messages open with. The message
/// itself is never logged.
fn reason(message: &str) -> &'static str {
    const KINDS: [(&str, &str); 6] = [
        ("unknown field", "unknown_field"),
        ("missing field", "missing_field"),
        ("duplicate field", "duplicate_field"),
        ("invalid type", "invalid_type"),
        ("invalid value", "invalid_value"),
        ("unknown variant", "unknown_variant"),
    ];
    KINDS
        .iter()
        .find(|(opening, _)| message.starts_with(opening))
        .map_or("invalid", |(_, kind)| kind)
}
