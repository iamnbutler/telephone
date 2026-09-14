//! One tool worker, bounded queues, and a nonblocking protocol loop. Cancellation
//! never detaches work or retries a send. An already-running operation finishes
//! under its existing deadlines; the protocol loop can still answer ping.
use super::{
    args, call_tool, empty_object, error, io::Nonblocking, tool_definitions, Empty, Reply,
    RpcError, MAX_REQUEST, PROTOCOL_VERSION,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::{self, Write},
    os::fd::BorrowedFd,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const IO_DEADLINE: Duration = Duration::from_secs(5);
const MAX_RESPONSES: usize = 32;
const MAX_OUTPUT: usize = 64 * 1024 * 1024;

struct Frame {
    bytes: Vec<u8>,
    offset: usize,
    since: Instant,
    id: Value,
    tool: bool,
    receipt: Option<crate::store::InboxBatch>,
}

// Cap serialization itself, not just the resulting allocation. Oversized tool
// output is an error; dropping its receipt leaves the inbox unread.
struct Limited(Vec<u8>);
impl Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_OUTPUT.saturating_sub(self.0.len() + 1) {
            return Err(io::Error::other("MCP response exceeds 64 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Frame {
    fn new(id: Value, result: std::result::Result<Reply, RpcError>, tool: bool) -> Result<Self> {
        let (value, receipt) = match result {
            Ok(reply) => (
                json!({"jsonrpc":"2.0","id":id,"result":reply.value}),
                reply.receipt,
            ),
            Err(e) => (error(id.clone(), e.code, &e.message), None),
        };
        let mut bytes = Limited(Vec::new());
        serde_json::to_writer(&mut bytes, &value).context("serializing bounded MCP response")?;
        bytes.0.push(b'\n');
        Ok(Self {
            bytes: bytes.0,
            offset: 0,
            since: Instant::now(),
            id,
            tool,
            receipt,
        })
    }
}

struct Worker {
    id: Value,
    cancelled: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<Option<Frame>>>>,
}
impl Worker {
    fn start(id: Value, params: Value, inbox_root: PathBuf) -> Result<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let request_id = id.clone();
        let handle = thread::Builder::new()
            .name("mcp-tool".into())
            .spawn(move || {
                if flag.load(Ordering::Acquire) {
                    return Ok(None);
                }
                // A send that passes this admission point may have side effects.
                // Finish journaling it even when cancellation arrives in the meantime.
                let result = call_tool(params, &inbox_root);
                if flag.load(Ordering::Acquire) {
                    return Ok(None);
                }
                let frame = Frame::new(request_id, result, true)?;
                if flag.load(Ordering::Acquire) {
                    return Ok(None);
                }
                Ok(Some(frame))
            })
            .context("starting MCP tool worker")?;
        Ok(Self {
            id,
            cancelled,
            handle: Some(handle),
        })
    }
    fn finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
    fn join(&mut self) -> Result<Option<Frame>> {
        match self.handle.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| anyhow!("MCP tool worker panicked; delivery may be uncertain"))?,
            None => Ok(None),
        }
    }
    fn stop(mut self) -> Result<()> {
        self.cancelled.store(true, Ordering::Release);
        // Never detach a worker holding a receipt or owning a subprocess guard.
        // Native calls retain their own deadlines. A stuck filesystem syscall
        // remains outside that guarantee, as it is for discovery in the CLI.
        drop(self.join()?);
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Err(error) = self.join() {
            crate::warn(format!("MCP worker cleanup failed: {error:#}"));
        }
    }
}

#[derive(Deserialize)]
struct Cancellation {
    #[serde(rename = "requestId")]
    request_id: Value,
    #[serde(rename = "reason")]
    _reason: Option<String>,
}

#[derive(Default)]
struct Session {
    inbox_root: PathBuf,
    initialized: bool,
    ready: bool,
    worker: Option<Worker>,
    output: VecDeque<Frame>,
    output_bytes: usize,
}
impl Session {
    fn enqueue(&mut self, frame: Frame) -> Result<()> {
        if self.output.len() >= MAX_RESPONSES
            || frame.bytes.len() > MAX_OUTPUT.saturating_sub(self.output_bytes)
        {
            bail!("MCP output queue limit reached; closing stream without retrying work");
        }
        self.output_bytes += frame.bytes.len();
        self.output.push_back(frame);
        Ok(())
    }
    fn reply(&mut self, id: Value, result: std::result::Result<Reply, RpcError>) -> Result<()> {
        self.enqueue(Frame::new(id, result, false)?)
    }
    fn rpc_error(&mut self, code: i32, message: &str) -> Result<()> {
        self.reply(
            Value::Null,
            Err(RpcError {
                code,
                message: message.into(),
            }),
        )
    }
    fn cancel(&mut self, params: Value) {
        let Ok(params) = serde_json::from_value::<Cancellation>(params) else {
            return;
        };
        if let Some(worker) = &self.worker {
            if worker.id == params.request_id {
                worker.cancelled.store(true, Ordering::Release);
            }
        }
        if let Some(index) = self
            .output
            .iter()
            .position(|frame| frame.tool && frame.id == params.request_id)
        {
            // Once any bytes are on the wire, finish the frame to preserve JSON
            // framing. Still roll back its receipt. A cancellation can race a reply.
            if let Some(frame) = self.output.get_mut(index) {
                drop(frame.receipt.take());
                if frame.offset != 0 {
                    return;
                }
            }
            if let Some(frame) = self.output.remove(index) {
                self.output_bytes -= frame.bytes.len();
            }
        }
    }
    fn request(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let request: Value = match serde_json::from_slice(bytes) {
            Ok(value) => value,
            Err(_) => return self.rpc_error(-32700, "invalid JSON"),
        };
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str);
        if !request.is_object()
            || request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || method.is_none()
            || id
                .as_ref()
                .is_some_and(|id| !(id.is_string() || id.is_number()))
        {
            return self.rpc_error(-32600, "invalid JSON-RPC request");
        }
        let method = method.context("validated MCP method disappeared")?;
        let params = request.get("params").cloned().unwrap_or_else(empty_object);
        let Some(id) = id else {
            match method {
                "notifications/initialized" if self.initialized => self.ready = true,
                "notifications/cancelled" => self.cancel(params),
                _ => {}
            }
            return Ok(());
        };
        if self.worker.as_ref().is_some_and(|worker| worker.id == id)
            || self.output.iter().any(|frame| frame.id == id)
        {
            bail!("duplicate in-flight MCP request ID; closing ambiguous stream");
        }
        let result = match method {
            "initialize" if !self.initialized => {
                if !params.is_object()
                    || !params.get("protocolVersion").is_some_and(Value::is_string)
                    || !params.get("capabilities").is_some_and(Value::is_object)
                    || !params.get("clientInfo").is_some_and(Value::is_object)
                {
                    Err(RpcError::invalid("invalid initialize parameters"))
                } else {
                    self.initialized = true;
                    Ok(Reply::value(
                        json!({"protocolVersion":PROTOCOL_VERSION,"capabilities":{"tools":{}},
                        "serverInfo":{"name":"telephone","version":env!("CARGO_PKG_VERSION")}}),
                    ))
                }
            }
            "ping" => args::<Empty>(params).map(|_| Reply::value(json!({}))),
            "tools/list" if self.ready => {
                args::<Empty>(params).map(|_| Reply::value(json!({"tools":tool_definitions()})))
            }
            "tools/call" if self.ready => {
                if self.worker.is_some() || self.output.iter().any(|frame| frame.tool) {
                    Err(RpcError {
                        code: -32000,
                        message:
                            "another tool call is still in progress; this call was not started"
                                .into(),
                    })
                } else {
                    self.worker = Some(Worker::start(id, params, self.inbox_root.clone())?);
                    return Ok(());
                }
            }
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
        self.reply(id, result)
    }
    fn completed_work(&mut self) -> Result<bool> {
        if !self.worker.as_ref().is_some_and(Worker::finished) {
            return Ok(false);
        }
        if let Some(mut worker) = self.worker.take() {
            let frame = worker.join()?;
            if !worker.cancelled.load(Ordering::Acquire) {
                if let Some(frame) = frame {
                    self.enqueue(frame)?;
                }
            }
        }
        Ok(true)
    }
    fn write(&mut self, output: &Nonblocking<'_>) -> Result<bool> {
        // Check every queued frame, not just the front. Slow trickle reads must
        // not renew the deadline or hold an inbox transaction indefinitely.
        if self
            .output
            .iter()
            .any(|frame| frame.since.elapsed() >= IO_DEADLINE)
        {
            bail!("MCP response exceeded its 5s output deadline; delivery may be uncertain");
        }
        let Some(frame) = self.output.front_mut() else {
            return Ok(false);
        };
        let end = frame.bytes.len().min(frame.offset + 64 * 1024);
        match output.write(&frame.bytes[frame.offset..end]) {
            Ok(0) => bail!("zero-length MCP write; closing stream"),
            Ok(size) => frame.offset += size,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(false)
            }
            Err(e) => return Err(e).context("writing MCP response; delivery may be uncertain"),
        }
        if frame.offset == frame.bytes.len() {
            let frame = self
                .output
                .pop_front()
                .context("completed MCP frame disappeared")?;
            self.output_bytes -= frame.bytes.len();
            // Direct descriptor writes have no userspace flush pending. This is
            // NOT proof the client read it; a crash before commit can repeat it.
            if let Some(receipt) = frame.receipt {
                receipt.acknowledge()?;
            }
        }
        Ok(true)
    }
    fn run(&mut self, input: &Nonblocking<'_>, output: &Nonblocking<'_>) -> Result<()> {
        let mut reader = Reader::default();
        let mut terminal_error = None;
        loop {
            let mut progress = false;
            if !reader.eof {
                match reader.read(input)? {
                    Read::Pending => {}
                    Read::Progress => progress = true,
                    Read::Frame(bytes) => {
                        self.request(&bytes)?;
                        progress = true;
                    }
                    Read::Oversized => {
                        self.rpc_error(-32600, "request exceeds size limit")?;
                        terminal_error =
                            Some(anyhow!("closing MCP stream after oversized request"));
                        reader.eof = true;
                    }
                }
            }
            progress |= self.completed_work()?;
            progress |= self.write(output)?;
            if reader.eof && self.worker.is_none() && self.output.is_empty() {
                return terminal_error.map_or(Ok(()), Err);
            }
            if !progress {
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

#[derive(Default)]
struct Reader {
    bytes: Vec<u8>,
    since: Option<Instant>,
    eof: bool,
}
enum Read {
    Pending,
    Progress,
    Frame(Vec<u8>),
    Oversized,
}
impl Reader {
    fn read(&mut self, input: &Nonblocking<'_>) -> Result<Read> {
        if let Some(end) = self.bytes.iter().position(|&b| b == b'\n') {
            let frame = self.bytes.drain(..=end).collect();
            self.since = (!self.bytes.is_empty()).then(Instant::now);
            return Ok(Read::Frame(frame));
        }
        if self.bytes.len() >= MAX_REQUEST {
            return Ok(Read::Oversized);
        }
        if self
            .since
            .is_some_and(|start| start.elapsed() >= IO_DEADLINE)
        {
            bail!("partial MCP request exceeded its 5s input deadline");
        }
        let mut buffer = [0; 8192];
        let length = buffer.len().min(MAX_REQUEST - self.bytes.len());
        match input.read(&mut buffer[..length]) {
            Ok(0) => {
                self.eof = true;
                if !self.bytes.is_empty() {
                    bail!("MCP input ended midway through a request");
                }
                Ok(Read::Progress)
            }
            Ok(size) => {
                self.since.get_or_insert_with(Instant::now);
                self.bytes.extend_from_slice(&buffer[..size]);
                Ok(Read::Progress)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(Read::Pending)
            }
            Err(e) => Err(e).context("reading MCP request"),
        }
    }
}

pub(super) fn serve(
    input: BorrowedFd<'_>,
    output: BorrowedFd<'_>,
    inbox_root: PathBuf,
) -> Result<()> {
    let mut input = Nonblocking::new(input).context("opening MCP input")?;
    let mut output = match Nonblocking::new(output).context("opening MCP output") {
        Ok(output) => output,
        Err(error) => {
            let mut result = Err(error);
            cleanup(&mut result, input.restore());
            return result;
        }
    };
    let mut session = Session {
        inbox_root,
        ..Session::default()
    };
    let mut result = session.run(&input, &output);
    // Roll back queued inbox transactions before waiting for an admitted tool to
    // finish. All cleanup is attempted and errors retain the original failure.
    session.output.clear();
    if let Some(worker) = session.worker.take() {
        cleanup(&mut result, worker.stop());
    }
    cleanup(&mut result, output.restore());
    cleanup(&mut result, input.restore());
    result
}
fn cleanup(result: &mut Result<()>, cleanup: Result<()>) {
    if let Err(error) = cleanup {
        *result = match std::mem::replace(result, Ok(())) {
            Ok(()) => Err(error),
            Err(original) => Err(original.context(format!("MCP cleanup also failed: {error:#}"))),
        };
    }
}
