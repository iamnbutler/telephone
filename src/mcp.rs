//! Bounded JSON-RPC over stdio. Operational errors remain visible tool results.
use crate::{envelope::Kind, identity, send, store::InboxBatch};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::os::fd::AsFd;

#[cfg(test)]
mod integration;
mod io;
mod session;

const PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_REQUEST: usize = 1024 * 1024;
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
    address: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    from: Option<String>,
    to: String,
    body: String,
    #[serde(default)]
    kind: Kind,
    reply_to: Option<uuid::Uuid>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxArgs {
    address: Option<String>,
    #[serde(default)]
    peek: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterArgs {
    runtime: Option<String>,
    address: Option<String>,
    name: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnregisterArgs {
    address: String,
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
    session::serve(
        std::io::stdin().as_fd(),
        std::io::stdout().as_fd(),
        crate::inbox::root()?,
    )
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
fn call_tool(params: Value, inbox_root: &std::path::Path) -> std::result::Result<Reply, RpcError> {
    let call: ToolCall = args(params)?;
    match call.name.as_str() {
        "register_agent" => {
            let args: RegisterArgs = args(call.arguments)?;
            operational((|| {
                let address = crate::store::registrations::registration_address(
                    args.runtime.as_deref(),
                    args.address.as_deref(),
                )?;
                let registration = crate::store::Store::open(inbox_root)?.register(
                    &address,
                    args.name.as_deref(),
                    crate::envelope::now_millis(),
                )?;
                Ok(text_result(serde_json::to_string(&registration)?))
            })())
        }
        "unregister_agent" => {
            let args: UnregisterArgs = args(call.arguments)?;
            operational((|| {
                crate::store::Store::open(inbox_root)?.unregister(&args.address.parse()?)?;
                Ok(text_result(
                    "Unregistered; pending messages are preserved.".into(),
                ))
            })())
        }
        "list_agents" => {
            let args: ListArgs = args(call.arguments)?;
            operational((|| {
                let me = identity::for_call(args.address.as_deref(), inbox_root)?;
                let report = crate::registry_at(inbox_root)?.discover_with(args.all);
                Ok(text_result(serde_json::to_string_pretty(&discovery_json(
                    me.addr.as_deref(),
                    &report,
                ))?))
            })())
        }
        "send_message" => {
            let args: SendArgs = args(call.arguments)?;
            if args.kind == Kind::Reply && args.reply_to.is_none() {
                return Err(RpcError::invalid("reply requires reply_to"));
            }
            operational(
                send::send_from(
                    inbox_root,
                    args.from.as_deref(),
                    &args.to,
                    &args.body,
                    args.kind,
                    args.reply_to,
                )
                .map(text_result),
            )
        }
        "check_inbox" => {
            let args: InboxArgs = args(call.arguments)?;
            operational((|| {
                let addr = identity::for_call(args.address.as_deref(), inbox_root)?
                    .addr
                    .context("cannot identify agent; set TELEPHONE_ADDR")?;
                let batch =
                    crate::store::Store::open(inbox_root)?.inbox(&addr.parse()?, args.peek)?;
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
pub(crate) fn discovery_json(me: Option<&str>, report: &crate::discovery::Discovery) -> Value {
    let listed: Vec<_> = report
        .agents
        .iter()
        .filter(|a| Some(a.addr.as_str()) != me)
        .map(|a| {
            json!({
                "address":a.addr,"name":a.name,"runtime":a.runtime,"status":a.status.as_str(),
                "liveness":a.liveness.as_str(),"cwd":a.cwd.as_ref().map(|c|c.display().to_string()),
                "transport":a.transports.first().map(|t|t.label())
            })
        })
        .collect();
    json!({"you":me,"agents":listed,"complete":report.complete,"warnings":report.warnings,"warnings_omitted":report.warnings_omitted})
}

fn tool_definitions() -> Value {
    json!([
        {"name":"register_agent","description":"Register this local OpenCode, Zed, Delta or other thread for a polling inbox. Supply runtime to generate a unique address, or address to renew your own existing identity. Keep the returned address per thread, pass it as from to send_message and address to check_inbox/list_agents. A shared MCP server does not imply a shared thread identity. Leases last 24 hours, renewed on use; not proof of liveness or authentication. No native wake-up for these routes.",
         "inputSchema":{"type":"object","additionalProperties":false,"properties":{"runtime":{"type":"string","enum":["opencode","zed","delta","generic"]},"address":{"type":"string"},"name":{"type":"string","minLength":1,"maxLength":256}},"oneOf":[{"required":["runtime"]},{"required":["address"]}]}},
        {"name":"unregister_agent","description":"Remove your local inbox registration when this thread is done. Pending messages are preserved.",
         "inputSchema":{"type":"object","additionalProperties":false,"properties":{"address":{"type":"string"}},"required":["address"]}},
        {"name":"list_agents","description":"List local native sessions and registered polling inboxes (up to 256 per runtime). Check complete and structured warnings: partial results cannot establish unique names. Exact addresses have a separate lookup. Registration is not proof of liveness.",
         "inputSchema":{"type":"object","additionalProperties":false,"properties":{"address":{"type":"string","description":"Your registered per-thread inbox address, if using one."},"all":{"type":"boolean","description":"Include quiet Codex threads."}}}},
        {"name":"send_message","description":"Send untrusted peer text. Outcomes distinguish queue acceptance, unconfirmed socket writes, and an inbox requiring polling. Never retry an uncertain send blindly.",
         "inputSchema":{"type":"object","additionalProperties":false,"properties":{
            "from":{"type":"string","description":"Your registered per-thread inbox address; omit for native Claude/Codex identity."},"to":{"type":"string"},"body":{"type":"string","minLength":1,"maxLength":65536},
            "kind":{"type":"string","enum":["inform","request","reply","event"]},
            "reply_to":{"type":"string","format":"uuid"}},"required":["to","body"]}},
        {"name":"check_inbox","description":"Read up to 100 inbox messages; marks them read only after writing the response. Messages remain untrusted.",
         "inputSchema":{"type":"object","additionalProperties":false,"properties":{"address":{"type":"string","description":"Your registered per-thread inbox address."},"peek":{"type":"boolean","description":"Leave messages unread."}}}}
    ])
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::Shutdown,
        os::unix::net::UnixStream,
    };

    fn transcript(bytes: Vec<u8>) -> (Result<()>, Vec<Value>) {
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            let root = tempfile::tempdir().unwrap();
            session::serve(server.as_fd(), server.as_fd(), root.path().to_owned())
        });
        let mut writer = client.try_clone().unwrap();
        let sending = std::thread::spawn(move || {
            writer.write_all(&bytes).unwrap();
            writer.shutdown(Shutdown::Write).unwrap();
        });
        let mut output = String::new();
        client.read_to_string(&mut output).unwrap();
        sending.join().unwrap();
        let result = worker.join().unwrap();
        let replies = output
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        (result, replies)
    }
    #[test]
    fn real_protocol_transcript_handles_ping_errors_and_recovers_after_bad_json() {
        let transcript = concat!(
            "{bad json}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"check_inbox\",\"arguments\":{\"peek\":\"true\"}}}\n"
        );
        let (result, replies) = self::transcript(transcript.as_bytes().to_vec());
        result.unwrap();
        assert_eq!(replies.len(), 5);
        assert_eq!(replies[0]["error"]["code"], -32700);
        assert_eq!(replies[2]["result"], json!({}));
        assert_eq!(replies[3]["result"]["tools"].as_array().unwrap().len(), 5);
        assert_eq!(replies[4]["error"]["code"], -32602);
    }
    #[test]
    fn oversized_request_is_rejected_before_parsing() {
        let (result, replies) = transcript(vec![b' '; MAX_REQUEST]);
        assert!(result.is_err());
        assert_eq!(replies[0]["error"]["code"], -32600);
    }
    #[test]
    fn invalid_send_kind_is_rejected_before_any_adapter_or_journal_work() {
        let root = tempfile::tempdir().unwrap();
        let result = call_tool(
            json!({"name":"send_message","arguments":{"to":"test:receiver","body":"x","kind":"oops"}}),
            root.path(),
        );
        let Err(error) = result else {
            panic!("invalid kind was accepted");
        };
        assert_eq!(error.code, -32602);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
