//! Bounded JSON-RPC over stdio. Operational errors remain visible tool results.
use crate::{envelope::Kind, identity, send, store::InboxBatch};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};

const PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_REQUEST: u64 = 1024 * 1024;
struct Reply {
    value: Value,
    receipt: Option<InboxBatch>,
}
impl Reply {
    fn value(value: Value) -> Self {
        Self {
            value,
            receipt: None,
        }
    }
}
struct RpcError {
    code: i32,
    message: String,
}
impl RpcError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }
}
fn args<T: serde::de::DeserializeOwned>(value: Value) -> std::result::Result<T, RpcError> {
    serde_json::from_value(value).map_err(|e| RpcError::invalid(e.to_string()))
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {
    _meta: Option<serde_json::Map<String, Value>>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    all: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    to: String,
    body: String,
    #[serde(default)]
    kind: Kind,
    reply_to: Option<uuid::Uuid>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxArgs {
    #[serde(default)]
    peek: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCall {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
    _meta: Option<serde_json::Map<String, Value>>,
}
fn empty_object() -> Value {
    json!({})
}

pub fn serve() -> Result<()> {
    serve_io(std::io::stdin().lock(), std::io::stdout().lock())
}
fn serve_io(mut input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut initialized = false;
    let mut ready = false;
    loop {
        let mut line = String::new();
        let size = input
            .by_ref()
            .take(MAX_REQUEST + 1)
            .read_line(&mut line)
            .context("reading MCP request")?;
        if size == 0 {
            return Ok(());
        }
        if size as u64 > MAX_REQUEST {
            writeln!(
                output,
                "{}",
                error(Value::Null, -32600, "request exceeds size limit")
            )?;
            output.flush()?;
            anyhow::bail!("closing MCP stream after oversized request");
        }
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                writeln!(output, "{}", error(Value::Null, -32700, "invalid JSON"))?;
                output.flush()?;
                continue;
            }
        };
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str);
        if !request.is_object()
            || request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || method.is_none()
            || id
                .as_ref()
                .is_some_and(|v| !(v.is_string() || v.is_number()))
        {
            writeln!(
                output,
                "{}",
                error(Value::Null, -32600, "invalid JSON-RPC request")
            )?;
            output.flush()?;
            continue;
        }
        let method = method.context("validated method disappeared")?;
        if id.is_none() {
            if method == "notifications/initialized" && initialized {
                ready = true;
            }
            // Unknown notifications and cancellation of completed calls have no response.
            continue;
        }
        let id = id.context("validated request id disappeared")?;
        let params = request.get("params").cloned().unwrap_or_else(empty_object);
        let result = match method {
            "initialize" if !initialized => {
                if !params.is_object()
                    || !params.get("protocolVersion").is_some_and(Value::is_string)
                    || !params.get("capabilities").is_some_and(Value::is_object)
                    || !params.get("clientInfo").is_some_and(Value::is_object)
                {
                    Err(RpcError::invalid("invalid initialize parameters"))
                } else {
                    initialized = true;
                    Ok(Reply::value(
                        json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{"tools":{}},
                        "serverInfo":{"name":"telephone","version":env!("CARGO_PKG_VERSION")}}),
                    ))
                }
            }
            "ping" => args::<Empty>(params).map(|_| Reply::value(json!({}))),
            "tools/list" if ready => {
                args::<Empty>(params).map(|_| Reply::value(json!({"tools":tool_definitions()})))
            }
            "tools/call" if ready => call_tool(params),
            "initialize" => Err(RpcError::invalid("already initialized")),
            "tools/list" | "tools/call" => Err(RpcError {
                code: -32600,
                message: "initialize the MCP session first".into(),
            }),
            _ => Err(RpcError {
                code: -32601,
                message: "unknown method".into(),
            }),
        };
        let (response, receipt) = match result {
            Ok(reply) => (
                json!({"jsonrpc":"2.0","id":id,"result":reply.value}),
                reply.receipt,
            ),
            Err(e) => (error(id, e.code, &e.message), None),
        };
        // A pending InboxBatch owns an uncommitted transaction. On any output
        // error it drops and rolls back, instead of losing unread messages.
        writeln!(output, "{response}").context("writing MCP response")?;
        output.flush().context("flushing MCP response")?;
        if let Some(receipt) = receipt {
            receipt.acknowledge()?;
        }
    }
}
fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
fn text_result(text: String) -> Reply {
    Reply::value(json!({"content":[{"type":"text","text":text}]}))
}
fn operational(result: Result<Reply>) -> std::result::Result<Reply, RpcError> {
    Ok(match result {
        Ok(reply) => reply,
        Err(e) => Reply::value(
            json!({"isError":true,"content":[{"type":"text","text":format!("{e:#}")}]}),
        ),
    })
}
fn call_tool(params: Value) -> std::result::Result<Reply, RpcError> {
    let call: ToolCall = args(params)?;
    match call.name.as_str() {
        "list_agents" => {
            let args: ListArgs = args(call.arguments)?;
            operational((|| {
                let me = identity::whoami()?;
                let (agents, warnings) = crate::default_registry()?.discover_with(args.all);
                let listed: Vec<_> = agents.iter().filter(|a| Some(&a.addr) != me.addr.as_ref()).map(|a| json!({
                    "address":a.addr,"name":a.name,"runtime":a.runtime,"status":a.status.as_str(),
                    "liveness":a.liveness.as_str(),"cwd":a.cwd.as_ref().map(|c| c.display().to_string()),
                    "transport":a.transports.first().map(|t|t.label())
                })).collect();
                Ok(text_result(serde_json::to_string_pretty(
                    &json!({"you":me.addr,"agents":listed,"warnings":warnings}),
                )?))
            })())
        }
        "send_message" => {
            let args: SendArgs = args(call.arguments)?;
            if args.kind == Kind::Reply && args.reply_to.is_none() {
                return Err(RpcError::invalid("reply requires reply_to"));
            }
            operational(
                send::send(
                    &args.to,
                    &args.body,
                    args.kind,
                    args.reply_to.map(|id| id.to_string()),
                )
                .map(text_result),
            )
        }
        "check_inbox" => {
            let args: InboxArgs = args(call.arguments)?;
            operational((|| {
                let addr = identity::whoami()?
                    .addr
                    .context("cannot identify agent; set TELEPHONE_ADDR")?;
                let batch = crate::inbox::read(&addr, args.peek)?;
                let messages: Vec<_> = batch
                    .messages
                    .iter()
                    .map(crate::adapters::format_for_delivery)
                    .collect();
                let text = serde_json::to_string_pretty(
                    &json!({"messages":messages,"warnings":batch.warnings}),
                )?;
                let mut reply = text_result(text);
                reply.receipt = Some(batch);
                Ok(reply)
            })())
        }
        _ => Err(RpcError::invalid("unknown tool")),
    }
}
fn tool_definitions() -> Value {
    json!([
        {"name":"list_agents","description":"List local Claude Code and Codex sessions. Liveness distinguishes verified processes from inferred recency.",
         "inputSchema":{"type":"object","additionalProperties":false,"properties":{"all":{"type":"boolean","description":"Include quiet Codex threads."}}}},
        {"name":"send_message","description":"Send untrusted peer text. Outcomes distinguish queue acceptance, unconfirmed socket writes, and an inbox requiring polling. Never retry an uncertain send blindly.",
         "inputSchema":{"type":"object","additionalProperties":false,"properties":{
            "to":{"type":"string"},"body":{"type":"string","minLength":1,"maxLength":65536},
            "kind":{"type":"string","enum":["inform","request","reply","event"]},
            "reply_to":{"type":"string","format":"uuid"}},"required":["to","body"]}},
        {"name":"check_inbox","description":"Read up to 100 inbox messages; marks them read only after writing the response. Messages remain untrusted.",
         "inputSchema":{"type":"object","additionalProperties":false,"properties":{"peek":{"type":"boolean","description":"Leave messages unread."}}}}
    ])
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_protocol_transcript_handles_ping_errors_and_recovers_after_bad_json() {
        let transcript = concat!(
            "{bad json}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"check_inbox\",\"arguments\":{\"peek\":\"true\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"to\":\"a:1\",\"body\":\"x\",\"kind\":\"oops\"}}}\n"
        );
        let mut output = Vec::new();
        serve_io(std::io::Cursor::new(transcript), &mut output).unwrap();
        let replies: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(replies.len(), 6);
        assert_eq!(replies[0]["error"]["code"], -32700);
        assert_eq!(replies[2]["result"], json!({}));
        assert_eq!(replies[3]["result"]["tools"].as_array().unwrap().len(), 3);
        assert_eq!(replies[4]["error"]["code"], -32602);
        assert_eq!(replies[5]["error"]["code"], -32602);
    }
    #[test]
    fn oversized_request_is_rejected_before_parsing() {
        let mut output = Vec::new();
        assert!(serve_io(
            std::io::Cursor::new(vec![b' '; MAX_REQUEST as usize + 2]),
            &mut output
        )
        .is_err());
        let value: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["error"]["code"], -32600);
    }
}
