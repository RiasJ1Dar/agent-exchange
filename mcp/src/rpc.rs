use crate::tools::tool_defs;
use crate::Error;
use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2024-11-05";

impl crate::Mcp {
    pub fn handle_rpc(&self, req: &Value) -> Option<Value> {
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = req.get("id")?.clone();

        let result: Result<Value, Error> = match method {
            "initialize" => Ok(initialize_result(req.get("params").unwrap_or(&Value::Null))),
            "ping" => Ok(json!({})),
            "shutdown" => Ok(Value::Null),
            "tools/list" => Ok(json!({ "tools": tool_defs() })),
            "tools/call" => Ok(self.tools_call(req.get("params").unwrap_or(&Value::Null))),
            _ => {
                return Some(rpc_err(&id, -32601, format!("метод не знайдено: {method}")));
            }
        };

        match result {
            Ok(value) => Some(rpc_ok(&id, value)),
            Err(e) => Some(rpc_err(&id, -32603, e.to_string())),
        }
    }
}

pub(crate) fn rpc_ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub(crate) fn rpc_err(id: &Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn initialize_result(params: &Value) -> Value {
    let pv = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": pv,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "exchange-mcp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}
