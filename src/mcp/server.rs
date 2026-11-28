//! JSON-RPC MCP session negotiation and bounded stdio dispatch.

use crate::error::Error;
use crate::mcp::coordinator::{AccountContext, ColabCoordinator};
use crate::mcp::tools::{
    call_tool_json, err_from, list_resources_json, list_tools_json, pin_tool_context,
    read_resource_json, tool_allowed, validate_arguments, HtmlLoader, APP_MIME, APP_URI,
    SERVER_NAME, SERVER_VERSION,
};
use crate::util::redact_secrets;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{self, BufRead, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const UI_EXTENSION: &str = "io.modelcontextprotocol/ui";
const MAX_BATCH_ITEMS: usize = 64;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DEFERRED_RESPONSES: usize = 4 * MAX_BATCH_ITEMS;

#[derive(Default)]
pub struct Connection {
    protocol: Option<&'static str>,
    initialized: bool,
    ui: bool,
}

struct Request {
    id: Value,
    method: String,
    params: Value,
    ui: bool,
    entered: Instant,
    context: Option<Result<Box<AccountContext>, Error>>,
}

enum Dispatch {
    Reply(Value),
    Execute(Request),
    Ignore,
}

fn response(id: &Value, result: Value) -> Value {
    let mut reply = json!({ "jsonrpc": "2.0", "id": id });
    reply["result"] = result;
    reply
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": redact_secrets(message, &[]) } })
}

fn invalid_request() -> Value {
    error(&Value::Null, -32600, "Invalid Request")
}

impl Connection {
    fn accepts_batch(&self, arr: &[Value]) -> bool {
        self.protocol == Some("2025-03-26")
            && (1..=MAX_BATCH_ITEMS).contains(&arr.len())
            && !arr
                .iter()
                .any(|msg| msg.get("method") == Some(&json!("initialize")))
    }

    fn prepare(&mut self, msg: &Value, entered: Instant) -> Dispatch {
        let Some(object) = msg.as_object() else {
            return Dispatch::Reply(invalid_request());
        };
        let id = object.get("id");
        let good_id = id.is_none_or(|v| v.is_string() || v.is_i64() || v.is_u64());
        let fields_valid =
            msg["jsonrpc"] == "2.0" && object.get("params").is_none_or(Value::is_object);
        if !fields_valid {
            return Dispatch::Reply(invalid_request());
        }
        let Some(method) = msg["method"].as_str() else {
            let valid_error = msg["error"].is_object()
                && msg["error"]["code"].as_i64().is_some()
                && msg["error"]["message"].is_string();
            if id.is_some()
                && (good_id || (valid_error && id.is_some_and(Value::is_null)))
                && !object.contains_key("method")
                && !object.contains_key("params")
                && (object.contains_key("result") ^ object.contains_key("error"))
                && (object.contains_key("result") || valid_error)
            {
                return Dispatch::Ignore;
            }
            return Dispatch::Reply(invalid_request());
        };
        if !good_id
            || method.is_empty()
            || object.contains_key("result")
            || object.contains_key("error")
        {
            return Dispatch::Reply(invalid_request());
        }
        let Some(id) = id else {
            if method == "notifications/initialized" && self.protocol.is_some() {
                self.initialized = true;
            }
            return Dispatch::Ignore;
        };
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        if method == "initialize" {
            if self.protocol.is_some() {
                return Dispatch::Reply(error(id, -32600, "Connection already negotiated"));
            }
            let valid_info = params["clientInfo"].is_object()
                && params["clientInfo"]["name"].is_string()
                && params["clientInfo"]["version"].is_string();
            let Some(requested) = params["protocolVersion"].as_str().filter(|version| {
                !version.is_empty() && params["capabilities"].is_object() && valid_info
            }) else {
                return Dispatch::Reply(error(
                    id,
                    -32602,
                    "initialize requires protocolVersion, capabilities and clientInfo",
                ));
            };
            let mut ui = false;
            if let Some(extensions) = params["capabilities"].get("extensions") {
                let Some(extensions) = extensions.as_object() else {
                    return Dispatch::Reply(error(
                        id,
                        -32602,
                        "initialize extensions must be an object",
                    ));
                };
                if let Some(extension) = extensions.get(UI_EXTENSION) {
                    let Some(types) = extension["mimeTypes"]
                        .as_array()
                        .filter(|types| types.iter().all(Value::is_string))
                    else {
                        return Dispatch::Reply(error(
                            id,
                            -32602,
                            "initialize UI mimeTypes must be an array of strings",
                        ));
                    };
                    ui = types.iter().any(|mime| mime == APP_MIME);
                }
            }
            let negotiated = SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .find(|v| *v == requested)
                .unwrap_or(LATEST_PROTOCOL_VERSION);
            self.protocol = Some(negotiated);
            self.ui = ui;
            let mut capabilities = json!({
                "tools": { "listChanged": false },
                "resources": { "subscribe": false, "listChanged": false },
            });
            if self.ui {
                capabilities["extensions"] = json!({ UI_EXTENSION: { "mimeTypes": [APP_MIME] } });
            }
            return Dispatch::Reply(response(
                id,
                json!({
                    "protocolVersion": negotiated, "capabilities": capabilities,
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                }),
            ));
        }
        if method == "ping" {
            return Dispatch::Reply(response(id, json!({})));
        }
        if !matches!(
            method,
            "tools/list" | "tools/call" | "resources/list" | "resources/read" | "prompts/list"
        ) {
            return Dispatch::Reply(error(id, -32601, "Method not found"));
        }
        if !self.initialized {
            return Dispatch::Reply(error(id, -32000, "Connection is not initialized"));
        }
        if method == "tools/call" {
            let name = params["name"].as_str().unwrap_or_default();
            if !tool_allowed(name, self.ui) {
                return Dispatch::Reply(error(id, -32602, "Unknown or unavailable tool"));
            }
            if let Some(args) = params.get("arguments") {
                if let Err(detail) = validate_arguments(name, args) {
                    return Dispatch::Reply(error(id, -32602, &detail));
                }
            } else if let Err(detail) = validate_arguments(name, &json!({})) {
                return Dispatch::Reply(error(id, -32602, &detail));
            }
        }
        if method == "resources/read" {
            match params["uri"].as_str() {
                None => return Dispatch::Reply(error(id, -32602, "resources/read requires a URI")),
                Some(uri) if uri != APP_URI => {
                    return Dispatch::Reply(error(id, -32002, "Resource not found"))
                }
                _ => {}
            }
        }
        Dispatch::Execute(Request {
            id: id.clone(),
            method: method.into(),
            params,
            ui: self.ui,
            entered,
            context: None,
        })
    }
}

impl Request {
    fn fast(&self) -> bool {
        if self.method != "tools/call" {
            return true;
        }
        let name = self.params["name"].as_str().unwrap();
        let args = &self.params["arguments"];
        let action = args["action"].as_str().unwrap_or_default();
        matches!(
            (name, action),
            ("colab_panel", _)
                | ("colab_session_action", "cancel_provision")
                | ("colab_tunnel_action", "list")
                | ("colab_deploy_action", "cancel")
                | ("colab_app_plugins", "cancel_run" | "list")
                | ("colab_app_settings", "get")
                | ("colab_app_accounts", "list" | "preview_switch")
        ) || matches!(name, "colab_status" | "colab_app_poll") && args["force"] != true
    }

    fn emergency(&self) -> bool {
        self.method == "tools/call"
            && self.params["name"] == "colab_session_action"
            && matches!(
                self.params["arguments"]["action"].as_str(),
                Some("stop" | "reap_orphan")
            )
    }

    fn pin_account(mut self, coord: &ColabCoordinator) -> Self {
        if self.method != "tools/call" {
            return self;
        }
        let tool = self.params["name"].as_str().unwrap();
        let args = &self.params["arguments"];
        self.context = pin_tool_context(tool, args, coord).map(|result| result.map(Box::new));
        self
    }

    fn run(self, coord: &Arc<ColabCoordinator>, html: &HtmlLoader) -> Value {
        let result = match self.method.as_str() {
            "tools/list" => list_tools_json(self.ui),
            "resources/list" => list_resources_json(),
            "resources/read" => {
                match read_resource_json(
                    self.params["uri"].as_str().unwrap(),
                    html,
                    coord,
                    self.ui,
                    self.entered,
                ) {
                    Ok(result) => result,
                    Err(message) => return error(&self.id, -32603, &message),
                }
            }
            "prompts/list" => json!({ "prompts": [] }),
            "tools/call" => {
                let name = self.params["name"].as_str().unwrap();
                let empty = json!({});
                let args = self.params.get("arguments").unwrap_or(&empty);
                let context = match self.context.transpose() {
                    Ok(context) => context.map(|ctx| *ctx),
                    Err(error) => return response(&self.id, err_from(&error)),
                };
                let mut result = call_tool_json(name, args, coord, self.entered, context);
                if !matches!(name, "colab_panel" | "colab_status" | "colab_app_poll")
                    || result["isError"] == true
                {
                    coord.redact_result(&mut result, !name.starts_with("colab_app_"));
                }
                result
            }
            _ => unreachable!(),
        };
        response(&self.id, result)
    }
}

pub fn handle_rpc(
    msg: &Value,
    connection: &mut Connection,
    coord: &Arc<ColabCoordinator>,
    html: &HtmlLoader,
) -> Option<Value> {
    let entered = Instant::now();
    if msg
        .as_array()
        .is_some_and(|batch| !connection.accepts_batch(batch))
    {
        return Some(invalid_request());
    }
    let mut run = |msg| match connection.prepare(msg, entered) {
        Dispatch::Reply(reply) => Some(reply),
        Dispatch::Execute(req) => Some(req.pin_account(coord).run(coord, html)),
        Dispatch::Ignore => None,
    };
    if let Some(batch) = msg.as_array() {
        let mut replies = Batch::new(batch.len());
        let mut frame = None;
        for (index, msg) in batch.iter().enumerate() {
            frame = replies.record(index, run(msg));
        }
        frame.map(|bytes| serde_json::from_slice(&bytes).expect("batch contains JSON-RPC replies"))
    } else {
        run(msg)
    }
}

type Output = Arc<dyn Fn(Vec<u8>, bool) + Send + Sync>;
type Job = Box<dyn FnOnce() + Send>;

struct WorkQueues {
    slow: SyncSender<Job>,
    emergency: SyncSender<Job>,
    deferred: Arc<Mutex<DeferredResponses>>,
}

#[derive(Default)]
struct DeferredResponses {
    ordinary: VecDeque<Vec<u8>>,
    emergency: VecDeque<Vec<u8>>,
}

impl DeferredResponses {
    fn defer(&mut self, frame: Vec<u8>, emergency: bool) {
        let queue = if emergency {
            &mut self.emergency
        } else {
            &mut self.ordinary
        };
        // Keep separate budgets so a stalled reader cannot exhaust the space
        // reserved for responses to stops. Bound both frames and encoded bytes.
        if queue.len() < MAX_DEFERRED_RESPONSES
            && queue.iter().map(Vec::len).sum::<usize>() + frame.len() <= 4 * MAX_RESPONSE_BYTES
        {
            queue.push_back(frame);
        }
    }
}

fn next_response(
    incoming: &Receiver<Vec<u8>>,
    deferred: &Mutex<DeferredResponses>,
) -> Option<Vec<u8>> {
    loop {
        if let Some(frame) = deferred.lock().unwrap().emergency.pop_front() {
            return Some(frame);
        }
        let disconnected = match incoming.try_recv() {
            Ok(frame) => return Some(frame),
            Err(error) => error == TryRecvError::Disconnected,
        };
        {
            let mut deferred = deferred.lock().unwrap();
            if let Some(frame) = deferred
                .emergency
                .pop_front()
                .or_else(|| deferred.ordinary.pop_front())
            {
                return Some(frame);
            }
        }
        if disconnected {
            return None;
        }
        if let Ok(frame) = incoming.recv_timeout(Duration::from_millis(25)) {
            return Some(frame);
        }
    }
}

struct Batch {
    replies: Vec<Option<Vec<u8>>>,
    remaining: usize,
    bytes: usize,
    count: usize,
    exceeded: bool,
}

impl Batch {
    fn new(count: usize) -> Self {
        Self {
            replies: vec![None; count],
            remaining: count,
            bytes: 2,
            count: 0,
            exceeded: false,
        }
    }

    fn record(&mut self, index: usize, reply: Option<Value>) -> Option<Vec<u8>> {
        if !self.exceeded {
            if let Some(reply) = reply {
                let bytes = encode(&reply);
                self.bytes += bytes.len() + usize::from(self.count != 0);
                self.count += 1;
                if self.bytes > MAX_RESPONSE_BYTES {
                    self.exceeded = true;
                    self.replies.clear();
                } else {
                    self.replies[index] = Some(bytes);
                }
            }
        }
        self.remaining -= 1;
        if self.remaining != 0 {
            return None;
        }
        if self.exceeded {
            return Some(encode(&error(
                &Value::Null,
                -32003,
                "Batch response exceeds 4 MiB",
            )));
        }
        if self.count == 0 {
            return None;
        }
        let mut frame = Vec::with_capacity(self.bytes);
        frame.push(b'[');
        for reply in std::mem::take(&mut self.replies).into_iter().flatten() {
            if frame.len() > 1 {
                frame.push(b',');
            }
            frame.extend_from_slice(&reply);
        }
        frame.push(b']');
        Some(frame)
    }
}

fn encode(reply: &Value) -> Vec<u8> {
    serde_json::to_vec(reply).expect("JSON-RPC Value serializes")
}

fn dispatch(
    msg: Value,
    entered: Instant,
    conn: &mut Connection,
    coord: &Arc<ColabCoordinator>,
    html: &Arc<HtmlLoader>,
    queues: &WorkQueues,
    output: &Output,
) {
    let ready = if let Some(batch) = msg.as_array() {
        if !conn.accepts_batch(batch) {
            output(encode(&invalid_request()), false);
            return;
        }
        batch
            .iter()
            .map(|msg| conn.prepare(msg, entered))
            .filter(|task| !matches!(task, Dispatch::Ignore))
            .collect::<Vec<_>>()
    } else {
        vec![conn.prepare(&msg, entered)]
    };
    if ready.is_empty() {
        return;
    }
    let batch = msg
        .is_array()
        .then(|| Arc::new(Mutex::new(Batch::new(ready.len()))));
    let priority = ready
        .iter()
        .any(|item| matches!(item, Dispatch::Execute(req) if req.emergency()));
    for (index, item) in ready.into_iter().enumerate() {
        let batch = batch.clone();
        let output = output.clone();
        let deliver: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(move |reply| {
            if let Some(batch) = &batch {
                let frame = batch.lock().unwrap().record(index, Some(reply));
                if let Some(frame) = frame {
                    output(frame, priority);
                }
            } else {
                let bytes = encode(&reply);
                if bytes.len() > MAX_RESPONSE_BYTES {
                    output(
                        encode(&error(&Value::Null, -32003, "Response exceeds 4 MiB")),
                        priority,
                    );
                } else {
                    output(bytes, priority);
                }
            }
        });
        match item {
            Dispatch::Reply(reply) => deliver(reply),
            Dispatch::Ignore => {}
            Dispatch::Execute(req) if req.fast() => {
                deliver(req.pin_account(coord).run(coord, html))
            }
            Dispatch::Execute(req) => {
                let req = req.pin_account(coord);
                let id = req.id.clone();
                let coord = coord.clone();
                let html = html.clone();
                let worker_output = deliver.clone();
                let emergency = req.emergency();
                if !emergency && !queues.deferred.lock().unwrap().ordinary.is_empty() {
                    deliver(error(
                        &id,
                        -32001,
                        "Client output is congested; retry later",
                    ));
                    continue;
                }
                let queue = if emergency {
                    &queues.emergency
                } else {
                    &queues.slow
                };
                if queue
                    .try_send(Box::new(move || worker_output(req.run(&coord, &html))))
                    .is_err()
                {
                    deliver(error(
                        &id,
                        -32001,
                        if emergency {
                            "Stop request queue is full; retry immediately"
                        } else {
                            "Slow request queue is full; retry later"
                        },
                    ));
                }
            }
        }
    }
}

fn start_workers(
    name: &str,
    count: usize,
    capacity: usize,
    workers: &mut Vec<thread::JoinHandle<()>>,
) -> io::Result<SyncSender<Job>> {
    let (send, receive) = mpsc::sync_channel::<Job>(capacity);
    let receive = Arc::new(Mutex::new(receive));
    for n in 0..count {
        let receive = receive.clone();
        workers.push(
            thread::Builder::new()
                .name(format!("colab-{name}-{n}"))
                .spawn(move || loop {
                    let job = receive.lock().unwrap().recv();
                    match job {
                        Ok(job) => job(),
                        Err(_) => break,
                    }
                })?,
        );
    }
    Ok(send)
}

pub fn serve_transport<R, W>(
    mut input: R,
    output: W,
    coord: Arc<ColabCoordinator>,
    html: HtmlLoader,
) -> io::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    const MAX_FRAME: u64 = 4 * 1024 * 1024;
    let html = Arc::new(html);
    let (response_tx, response_rx) = mpsc::sync_channel::<Vec<u8>>(32);
    let deferred = Arc::new(Mutex::new(DeferredResponses::default()));
    let writer_deferred = deferred.clone();
    let sink_deferred = deferred.clone();
    let write_error = Arc::new(Mutex::new(None));
    let writer_error = write_error.clone();
    let writer = thread::Builder::new()
        .name("colab-stdout".into())
        .spawn(move || {
            let mut writer = BufWriter::new(output);
            while let Some(frame) = next_response(&response_rx, &writer_deferred) {
                if writer_error.lock().unwrap().is_some() {
                    break;
                }
                if let Err(error) = writer
                    .write_all(&frame)
                    .and_then(|()| writer.write_all(b"\n"))
                    .and_then(|()| writer.flush())
                {
                    writer_error.lock().unwrap().get_or_insert(error);
                    break;
                }
            }
        })?;
    let sink_error = write_error.clone();
    let sink: Output = Arc::new(move |frame, emergency| {
        let mut deferred = sink_deferred.lock().unwrap();
        if !deferred.ordinary.is_empty() || (emergency && !deferred.emergency.is_empty()) {
            deferred.defer(frame, emergency);
            return;
        }
        match response_tx.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(frame)) => deferred.defer(frame, emergency),
            Err(TrySendError::Disconnected(_)) => {
                sink_error.lock().unwrap().get_or_insert_with(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "stdout writer disconnected")
                });
            }
        }
    });
    let mut workers = Vec::new();
    let queues = WorkQueues {
        slow: start_workers("rpc", 4, 16, &mut workers)?,
        emergency: start_workers("stop", 2, MAX_BATCH_ITEMS, &mut workers)?,
        deferred,
    };

    let (messages, next) = mpsc::sync_channel(32);
    let reader =
        thread::Builder::new()
            .name("colab-stdin".into())
            .spawn(move || -> io::Result<()> {
                loop {
                    let mut line = Vec::new();
                    let read = input
                        .by_ref()
                        .take(MAX_FRAME + 1)
                        .read_until(b'\n', &mut line)?;
                    if read == 0 {
                        return Ok(());
                    }
                    if read as u64 > MAX_FRAME {
                        // Discard the remainder of this frame without allocating it.
                        if line.last() != Some(&b'\n') {
                            input.skip_until(b'\n')?;
                        }
                        line.clear();
                        line.push(b'!');
                    }
                    if line.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    if messages.send((line, Instant::now())).is_err() {
                        return Ok(());
                    }
                }
            })?;
    let mut conn = Connection::default();
    loop {
        if write_error.lock().unwrap().is_some() {
            // The writer encountered a permanent transport error.
            break;
        }
        match next.recv_timeout(Duration::from_millis(50)) {
            Ok((bytes, entered)) => match serde_json::from_slice(&bytes) {
                Ok(msg) => dispatch(msg, entered, &mut conn, &coord, &html, &queues, &sink),
                Err(_) => sink(encode(&error(&Value::Null, -32700, "Parse error")), false),
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(queues);
    for worker in workers {
        let _ = worker.join();
    }
    drop(sink);
    if let Some(error) = write_error.lock().unwrap().as_ref() {
        return Err(io::Error::new(error.kind(), error.to_string()));
    }
    writer
        .join()
        .map_err(|_| io::Error::other("stdout writer panicked"))?;
    if let Some(error) = write_error.lock().unwrap().take() {
        return Err(error);
    }
    reader
        .join()
        .map_err(|_| io::Error::other("stdin reader panicked"))?
}

pub fn serve_stdio(
    data_dir: PathBuf,
    panel_html: Option<PathBuf>,
    force_mock: Option<bool>,
) -> io::Result<()> {
    let coord = ColabCoordinator::new(data_dir, force_mock);
    let html = HtmlLoader::new(panel_html);
    let _ = html.load();
    let (stop, shutdown) = mpsc::channel();
    let guard = {
        let coord = coord.clone();
        thread::Builder::new()
            .name("colab-guard".into())
            .spawn(move || {
                while shutdown
                    .recv_timeout(Duration::from_secs(1))
                    .is_err_and(|e| e == mpsc::RecvTimeoutError::Timeout)
                {
                    coord.guard_tick();
                }
            })?
    };
    let result = serve_transport(
        io::BufReader::new(io::stdin()),
        io::stdout(),
        coord.clone(),
        html,
    );
    let _ = stop.send(());
    coord.shutdown();
    let _ = guard.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Host;
    use crate::util::PrivateDir;

    #[test]
    fn queued_stop_keeps_the_original_account_and_endpoint() {
        use crate::accounts::{AccountProfile, ADC_ACCOUNT_ID};
        use crate::settings::AuthMode;
        use crate::util::now_secs;

        let dir = PrivateDir::new(&std::env::temp_dir(), "queued-stop").unwrap();
        let coord = ColabCoordinator::with_host(dir.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        coord
            .add_account(AccountProfile {
                id: "second".into(),
                label: "Second".into(),
                email: None,
                auth_mode: AuthMode::Adc,
                added_at: now_secs(),
            })
            .unwrap();
        let html = Arc::new(HtmlLoader::new(None));
        let (slow, _slow_jobs) = mpsc::sync_channel::<Job>(1);
        let (emergency, worker) = mpsc::sync_channel::<Job>(1);
        let queues = WorkQueues {
            slow,
            emergency,
            deferred: Arc::new(Mutex::new(DeferredResponses::default())),
        };
        let (out, replies) = mpsc::channel();
        let output: Output = Arc::new(move |reply, _| {
            out.send(serde_json::from_slice::<Value>(&reply).unwrap())
                .unwrap();
        });
        let mut conn = Connection {
            protocol: Some("2025-11-25"),
            initialized: true,
            ui: false,
        };
        dispatch(
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"colab_session_action","arguments":{"action":"stop","session":"c-g4-train"}}
            }),
            Instant::now(),
            &mut conn,
            &coord,
            &html,
            &queues,
            &output,
        );
        coord.switch_account("second").unwrap();
        worker.try_recv().unwrap()();
        let reply = replies.try_recv().unwrap();
        assert_eq!(reply["result"]["structuredContent"]["stopped"], true);
        coord.switch_account(ADC_ACCOUNT_ID).unwrap();
        coord.refresh(true).unwrap();
        assert!(!coord.snapshot_json(None)["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["display_name"] == "c-g4-train"));
        coord.shutdown();
    }

    #[test]
    fn saturated_slow_queue_cannot_block_fast_or_mixed_batch() {
        let dir = PrivateDir::new(&std::env::temp_dir(), "dispatch").unwrap();
        let coord = ColabCoordinator::with_host(dir.path().to_owned(), Some(true), Host::denied());
        let html = Arc::new(HtmlLoader::new(None));
        let (slow, _workers) = mpsc::sync_channel::<Job>(1);
        let (emergency, stop_worker) = mpsc::sync_channel::<Job>(1);
        slow.try_send(Box::new(|| {})).unwrap();
        let queues = WorkQueues {
            slow,
            emergency,
            deferred: Arc::new(Mutex::new(DeferredResponses::default())),
        };
        let (out, replies) = mpsc::channel();
        let output: Output = Arc::new(move |reply, _| {
            out.send(serde_json::from_slice::<Value>(&reply).unwrap())
                .unwrap();
        });
        let mut conn = Connection {
            protocol: Some("2025-03-26"),
            initialized: true,
            ui: true,
        };
        dispatch(
            json!([
                {"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"colab_app_settings","arguments":{"action":"doctor"}}},
                {"jsonrpc":"2.0","id":2,"method":"ping"},
                {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"colab_app_poll"}},
                {"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"colab_deploy_action","arguments":{"action":"cancel","run_id":"missing"}}},
                {"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":APP_URI}},
                {"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"colab_session_action","arguments":{"action":"stop","session":"c-g4-train"}}}
            ]),
            Instant::now(),
            &mut conn,
            &coord,
            &html,
            &queues,
            &output,
        );
        stop_worker.try_recv().unwrap()();
        let reply = replies.try_recv().unwrap();
        assert_eq!(reply[0]["error"]["code"], -32001);
        assert_eq!(reply[1]["result"], json!({}));
        assert_eq!(reply[2]["result"]["isError"], false);
        assert_eq!(reply[3]["result"]["structuredContent"]["cancelled"], false);
        assert!(reply[4]["result"]["contents"][0]["text"].as_str().is_some());
        assert_eq!(reply[5]["result"]["structuredContent"]["stopped"], true);
        coord.shutdown();
    }

    #[test]
    fn a_blocked_stdout_writer_does_not_delay_a_billing_stop() {
        struct PausedWriter {
            pause: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
            captured: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for PausedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if let Some((entered, release)) = self.pause.take() {
                    entered.send(()).unwrap();
                    release.recv().unwrap();
                }
                self.captured.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let dir = PrivateDir::new(&std::env::temp_dir(), "paused-writer").unwrap();
        let coord = ColabCoordinator::with_host(dir.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        let mut frames = Vec::new();
        let requests = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"0"}
            }}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        ].into_iter().chain((10..60).map(|id| json!({"jsonrpc":"2.0","id":id,"method":"ping"})))
            .chain([json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"colab_session_action","arguments":{"action":"stop","session":"c-g4-train"}
            }})]);
        for msg in requests {
            frames.extend(encode(&msg));
            frames.push(b'\n');
        }
        let (entered, writing) = mpsc::channel();
        let (release, paused) = mpsc::channel();
        let runner = coord.clone();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer = PausedWriter {
            pause: Some((entered, paused)),
            captured: captured.clone(),
        };
        let transport = thread::spawn(move || {
            serve_transport(
                io::Cursor::new(frames),
                writer,
                runner,
                HtmlLoader::new(None),
            )
        });
        writing.recv_timeout(Duration::from_secs(2)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let confirmed = loop {
            let snap = coord.snapshot_json(None);
            if snap["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["display_name"] != "c-g4-train")
            {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(10));
        };
        release.send(()).unwrap();
        transport.join().unwrap().unwrap();
        assert!(confirmed, "stop had to wait for stdout");
        let mut ids = captured
            .lock()
            .unwrap()
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_slice::<Value>(line).unwrap()["id"]
                    .as_u64()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2].into_iter().chain(10..60).collect::<Vec<_>>());
        coord.shutdown();
    }
}
