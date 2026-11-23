//! `diskghost mcp` — an MCP (Model Context Protocol) server on stdio, so an AI
//! agent can see the machine's drives, choose what to index (local folders
//! and `ssh://host//path` remotes), run scans as background jobs, find
//! duplicates across every indexed volume and write a dedup *plan*.
//!
//! # Read-only by construction
//!
//! No tool can move, delete, rename or write a user file: carrying a plan
//! out stays a manual `diskghost apply` step. The only things this server
//! ever writes are (a) the index database (hashes and rows, exactly as
//! `index add` and `dupes --index` do), (b) plan files, created with
//! `create_new` in one fixed directory, and (c) on a remote host, the
//! existing agent upload confined to `~/.cache/diskghost/` (opt out with
//! `agent: "never"`). A source-level test fails the build when this module
//! names anything that could do more.
//!
//! # Protocol
//!
//! Hand-rolled JSON-RPC 2.0 over newline-delimited stdio — stdout carries
//! protocol frames only, diagnostics go to the file named by
//! `DISKGHOST_MCP_LOG` (`-` = stderr). Versions: `2025-06-18` and
//! `2025-11-25` through `initialize`; `2026-07-28` statelessly (every request
//! names its version in `_meta`; `server/discover` replaces `initialize`).
//!
//! # Jobs
//!
//! Scans, `dupes_report` and `plan_dedup` write the index, and the index
//! takes one writer at a time: they run as jobs, one writer per server,
//! the rest queued. When *another process* holds the index lock the tool
//! answers `index_busy` (naming the database) instead of waiting forever.

mod jobs;
mod tools;

#[cfg(test)]
mod tests;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};

use jobs::Jobs;

/// Protocol versions negotiated through `initialize`, oldest first.
pub(crate) const LEGACY_VERSIONS: [&str; 2] = ["2025-06-18", "2025-11-25"];
/// The stateless version: no `initialize`, `server/discover` instead.
pub(crate) const MODERN_VERSION: &str = "2026-07-28";
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
/// How long a client may cache `tools/list` / `server/discover` (2026-07-28).
const TTL_MS: u64 = 300_000;

const INSTRUCTIONS: &str = "Diskghost MCP server: find duplicate files across drives and ssh \
hosts, and plan their clean-up. READ-ONLY: no tool moves, deletes, renames or writes a user \
file; a plan is a JSON document under the state directory that the owner may run by hand with \
`diskghost apply <plan> --dry-run`. Workflow: list_drives / list_locations, add_location (or \
start_scan) returns a job_id, poll job_status until done, dupes_report, then plan_dedup with \
the policy the user stated. Scans, dupes_report and plan_dedup write the index, so only one runs \
at a time (others queue); index_busy means another diskghost process holds the index lock. A \
local scan killed mid-way records nothing: rerun it.";

/// A tool's failure as the agent sees it (`isError` result, or a failed job).
#[derive(Debug, Clone)]
pub(crate) struct ToolError {
    pub code: &'static str,
    pub message: String,
    /// The index database involved, for `index_busy` and index failures.
    pub db: Option<String>,
}

impl ToolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> ToolError {
        ToolError {
            code,
            message: message.into(),
            db: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> ToolError {
        ToolError::new("invalid_argument", message)
    }

    /// An I/O error from the index or a scan, classified: another writer
    /// holding the lock is `index_busy`, never a generic failure.
    pub fn from_io(db: &std::path::Path, e: &std::io::Error) -> ToolError {
        let db_s = db.display().to_string();
        if diskghost_core::is_busy(e) {
            return ToolError {
                code: "index_busy",
                message: format!(
                    "the index {db_s} is locked by another process (for example a running \
                     `diskghost index add`); nothing was changed, retry when it has finished"
                ),
                db: Some(db_s),
            };
        }
        if e.kind() == std::io::ErrorKind::Interrupted {
            return ToolError::new("cancelled", "cancelled");
        }
        ToolError {
            code: "index_error",
            message: e.to_string(),
            db: Some(db_s),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("code".into(), json!(self.code));
        m.insert("message".into(), json!(self.message));
        if let Some(db) = &self.db {
            m.insert("db".into(), json!(db));
        }
        Value::Object(m)
    }
}

/// Fixed server settings and the job manager, shared by every request.
pub(crate) struct Ctx {
    pub db: PathBuf,
    /// How long an index statement waits for another process's lock.
    pub busy: Duration,
    /// How long `dupes_report` / `plan_dedup` wait before answering with a
    /// job handle instead of the result.
    pub inline_wait: Duration,
    /// Test hook (`DISKGHOST_MCP_JOB_HOLD_MS`): a writer job idles this long
    /// inside its turn. 0 in production.
    pub hold: Duration,
    pub jobs: Arc<Jobs>,
}

/// A JSON-RPC error object.
struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// Protocol diagnostics: one line per frame, to a file (or stderr).
struct Log(Option<Mutex<Box<dyn Write + Send>>>);

impl Log {
    fn from_env() -> Log {
        let sink: Option<Box<dyn Write + Send>> = match std::env::var_os("DISKGHOST_MCP_LOG") {
            None => None,
            Some(v) if v.is_empty() => None,
            Some(v) if v == "-" => Some(Box::new(std::io::stderr())),
            Some(v) => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(v)
                .ok()
                .map(|f| Box::new(f) as Box<dyn Write + Send>),
        };
        Log(sink.map(Mutex::new))
    }

    fn line(&self, msg: &str) {
        if let Some(w) = &self.0 {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            if let Ok(mut w) = w.lock() {
                let _ = writeln!(w, "{ms} pid={} {msg}", std::process::id());
                let _ = w.flush();
            }
        }
    }
}

/// What the connection has negotiated so far.
struct Session {
    /// The version `initialize` settled on.
    negotiated: Mutex<Option<String>>,
    log: Log,
}

/// Write one frame: a single line, atomically.
fn send(v: &Value) {
    let mut line = serde_json::to_string(v).unwrap_or_else(|_| "null".into());
    line.push('\n');
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

fn millis_env(name: &str, default: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}

/// `diskghost mcp [--db FILE]`: serve until stdin closes.
pub(crate) fn run(db: Option<PathBuf>) -> ExitCode {
    let Some(db) = db.or_else(crate::default_index_db) else {
        eprintln!("error: no --db given and no data directory ($XDG_DATA_HOME / $HOME) known");
        return ExitCode::FAILURE;
    };
    let ctx = Arc::new(Ctx {
        db,
        busy: millis_env("DISKGHOST_MCP_BUSY_MS", 10_000),
        inline_wait: millis_env("DISKGHOST_MCP_INLINE_WAIT_MS", 20_000),
        hold: millis_env("DISKGHOST_MCP_JOB_HOLD_MS", 0),
        jobs: Arc::new(Jobs::default()),
    });
    let session = Arc::new(Session {
        negotiated: Mutex::new(None),
        log: Log::from_env(),
    });
    session.log.line(&format!(
        "start diskghost {} db={}",
        env!("CARGO_PKG_VERSION"),
        ctx.db.display()
    ));
    let inflight = Arc::new(AtomicUsize::new(0));

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                send(&error_frame(
                    &Value::Null,
                    &RpcError::new(-32700, format!("parse error: {e}")),
                ));
                continue;
            }
        };
        let Some(obj) = msg.as_object() else {
            send(&error_frame(
                &Value::Null,
                &RpcError::new(-32600, "a request is a single JSON object (no batches)"),
            ));
            continue;
        };
        let Some(method) = obj
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            // A response to something we never asked for: ignore.
            continue;
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = obj.get("id").cloned() else {
            session.log.line(&format!("recv notification {method}"));
            continue; // notifications get no answer; unknown ones are ignored
        };
        if method == "tools/call" {
            // A call may wait on a job: never on the read loop.
            let (ctx, session, inflight) = (ctx.clone(), session.clone(), inflight.clone());
            inflight.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                respond(&ctx, &session, &id, &method, &params);
                inflight.fetch_sub(1, Ordering::SeqCst);
            });
        } else {
            respond(&ctx, &session, &id, &method, &params);
        }
    }

    // stdin closed: stop the jobs (a local scan's single transaction rolls
    // back), give answers in flight a moment, leave.
    session.log.line("stdin closed");
    ctx.jobs.cancel_all();
    let t = std::time::Instant::now();
    while (inflight.load(Ordering::SeqCst) > 0 || !ctx.jobs.all_idle())
        && t.elapsed() < Duration::from_secs(5)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    ExitCode::SUCCESS
}

fn error_frame(id: &Value, e: &RpcError) -> Value {
    let mut err = json!({"code": e.code, "message": e.message});
    if let Some(d) = &e.data {
        err["data"] = d.clone();
    }
    json!({"jsonrpc": "2.0", "id": id, "error": err})
}

fn respond(ctx: &Arc<Ctx>, session: &Session, id: &Value, method: &str, params: &Value) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch(ctx, session, method, params)
    }))
    .unwrap_or_else(|_| Err(RpcError::new(-32603, "internal error")));
    match r {
        Ok(result) => {
            session.log.line(&format!("send result {method}"));
            send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
        }
        Err(e) => {
            session
                .log
                .line(&format!("send error {method} code={}", e.code));
            send(&error_frame(id, &e));
        }
    }
}

/// The version a request declares in `_meta` (2026-07-28 style), if any.
fn declared_version(params: &Value) -> Option<&str> {
    params.get("_meta")?.get(META_VERSION)?.as_str()
}

fn dispatch(
    ctx: &Arc<Ctx>,
    session: &Session,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    let declared = declared_version(params);
    let session_ver = session.negotiated.lock().ok().and_then(|g| g.clone());
    session.log.line(&format!(
        "recv {method} declared={} session={}{}",
        declared.unwrap_or("-"),
        session_ver.as_deref().unwrap_or("-"),
        if method == "tools/call" {
            format!(
                " tool={}",
                params.get("name").and_then(Value::as_str).unwrap_or("?")
            )
        } else {
            String::new()
        }
    ));
    // A request that names a version we do not speak is refused as such.
    if let Some(v) = declared {
        if v != MODERN_VERSION && !LEGACY_VERSIONS.contains(&v) {
            let mut supported: Vec<&str> = LEGACY_VERSIONS.to_vec();
            supported.push(MODERN_VERSION);
            return Err(RpcError {
                code: -32022,
                message: format!("unsupported protocol version {v:?}"),
                data: Some(json!({"supported": supported, "requested": v})),
            });
        }
    }
    let modern = declared == Some(MODERN_VERSION);
    match method {
        "initialize" => Ok(initialize(session, params)),
        "ping" => Ok(json!({})),
        "server/discover" => Ok(discover()),
        "tools/list" => {
            let mut r = json!({"tools": tools::definitions()});
            if modern {
                r["resultType"] = json!("complete");
                r["ttlMs"] = json!(TTL_MS);
                r["cacheScope"] = json!("public");
            }
            Ok(r)
        }
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::new(-32602, "tools/call needs a tool name"))?;
            let args = params.get("arguments").unwrap_or(&Value::Null);
            let outcome = tools::call(ctx, name, args)
                .ok_or_else(|| RpcError::new(-32602, format!("unknown tool: {name}")))?;
            let mut r = match outcome {
                Ok(v) => tool_result(&v, false),
                Err(e) => tool_result(&json!({"error": e.to_json()}), true),
            };
            if modern {
                r["resultType"] = json!("complete");
            }
            Ok(r)
        }
        other => Err(RpcError::new(-32601, format!("method not found: {other}"))),
    }
}

/// A `tools/call` result: the structured value, mirrored as JSON text for
/// clients that only read `content`.
fn tool_result(v: &Value, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": serde_json::to_string(v).unwrap_or_default()}],
        "structuredContent": v,
        "isError": is_error,
    })
}

fn server_info() -> Value {
    json!({
        "name": "diskghost",
        "title": "Diskghost",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn initialize(session: &Session, params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    // Echo a version we speak; otherwise the newest we do.
    let version = LEGACY_VERSIONS
        .iter()
        .find(|v| **v == asked)
        .copied()
        .unwrap_or(LEGACY_VERSIONS[LEGACY_VERSIONS.len() - 1]);
    let client = params
        .get("clientInfo")
        .map(|c| {
            format!(
                "{}/{}",
                c.get("name").and_then(Value::as_str).unwrap_or("?"),
                c.get("version").and_then(Value::as_str).unwrap_or("?")
            )
        })
        .unwrap_or_default();
    session.log.line(&format!(
        "initialize client={client} requested={asked} negotiated={version}"
    ));
    if let Ok(mut g) = session.negotiated.lock() {
        *g = Some(version.to_string());
    }
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": server_info(),
        "instructions": INSTRUCTIONS,
    })
}

/// `server/discover` (2026-07-28): what the server speaks and offers.
fn discover() -> Value {
    let mut supported: Vec<&str> = LEGACY_VERSIONS.to_vec();
    supported.push(MODERN_VERSION);
    json!({
        "resultType": "complete",
        "supportedVersions": supported,
        "capabilities": {"tools": {"listChanged": false}},
        "_meta": {META_SERVER_INFO: server_info()},
        "instructions": INSTRUCTIONS,
        "ttlMs": TTL_MS,
        "cacheScope": "public",
    })
}
