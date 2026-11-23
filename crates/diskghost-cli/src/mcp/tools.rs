//! The tool surface: an exact allowlist, each tool a thin wrapper over core.
//! Every argument is untrusted model input — validated here, before
//! anything is opened or spawned.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use diskghost_core::remote::{parse_ssh_url, AgentMode};
use diskghost_core::{
    disk_space, index_dupes, index_remote, index_scan, is_cow_fs, is_rotational, make_plan,
    validate_globs, volume_uuid, Index, IndexDupesReport, IndexReportOptions, LocalMount,
    LocationStatus, Options, Policy, Progress, Role,
};
use serde_json::{json, Map, Value};

use super::jobs::State;
use super::{Ctx, ToolError};

type R = Result<Value, ToolError>;

/// Default and maximum page size of the paged outputs.
const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 500;
/// `dupes_report` ignores files below this unless told otherwise — the
/// CLI's `--min-mb 1`.
const DEFAULT_MIN_SIZE: u64 = 1024 * 1024;

fn def(
    name: &str,
    title: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
    idempotent: bool,
) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
        "annotations": {
            "title": title,
            "readOnlyHint": read_only,
            "destructiveHint": false,
            "idempotentHint": idempotent,
            "openWorldHint": false,
        },
    })
}

/// `tools/list`: the allowlist, in a fixed order.
pub(crate) fn definitions() -> Vec<Value> {
    let walk = json!({
        "exclude": {"type": "array", "items": {"type": "string"},
            "description": "Glob patterns to skip (matched on path, component or name), e.g. [\"node_modules\", \"*.tmp\"]. Local folders only."},
        "one_file_system": {"type": "boolean",
            "description": "Stay on the filesystem of the scan root. Local folders only."},
    });
    let agent = json!({"type": "string", "enum": ["auto", "never"],
        "description": "ssh:// only. auto (default): copy this program once to ~/.cache/diskghost/ on the host (mv -n, never overwrites, not a user file) and let it list and hash there; falls back to the host's shell tools and says why in agent_note. never: shell tools only, nothing is uploaded."});
    let mut scan_props = walk.as_object().cloned().unwrap_or_default();
    scan_props.insert("agent".into(), agent.clone());
    let mut add_props = scan_props.clone();
    add_props.insert("path".into(), json!({"type": "string",
        "description": "An absolute local folder, or ssh://host//absolute/path (note the double slash; a relative remote path is refused)."}));
    add_props.insert("role".into(), json!({"type": "string", "enum": ["normal", "master", "scratch"],
        "description": "Role of the volume in later copy policies: master = the copy to keep, scratch = copies here are expendable."}));
    let mut start_props = scan_props;
    start_props.insert("location".into(), json!({"type": "string",
        "description": "uuid of an indexed location (list_locations): rescan every folder indexed on it."}));
    start_props.insert("path".into(), json!({"type": "string",
        "description": "Alternatively one folder (or ssh:// URL) inside an already indexed location."}));
    let page = |what: &str| json!({"type": "integer", "minimum": 0, "description": what});
    vec![
        def(
            "list_drives",
            "List drives",
            "List this machine's real local filesystems (mount point, type, source device, total and free bytes, media hdd/ssd where ssd means not rotational, copy-on-write, filesystem uuid and whether the index already holds it) and the remembered ssh locations of the index. Read-only; asks no remote host.",
            json!({}), &[], true, true,
        ),
        def(
            "list_locations",
            "List indexed locations",
            "List every location in the index: uuid, host, role, root, scanned folders, files, bytes, last scan, and whether it is online now. An ssh location is checked over ssh (up to 10 s per unreachable host); pass probe_remote=false to skip that. Offline locations keep their rows and still count in duplicate reports. Read-only.",
            json!({"probe_remote": {"type": "boolean", "description": "Check ssh locations over ssh (default true). false leaves their online as null."}}),
            &[], true, true,
        ),
        def(
            "add_location",
            "Add a location and scan it",
            "Index a new folder: an absolute local path or ssh://host//abs/path. Validates the argument, then starts a background scan job and returns its job_id immediately; poll job_status. Only the index is written, never the scanned files. A scan killed mid-way (server stopped) records nothing for a local folder: rerun it. For ssh with agent=auto this program is copied once to ~/.cache/diskghost/ on the host.",
            Value::Object(add_props), &["path"], false, false,
        ),
        def(
            "set_location_role",
            "Set a location's role",
            "Set the role of an indexed location (master = keep, scratch = expendable, normal). Changes the index only.",
            json!({
                "location": {"type": "string", "description": "uuid from list_locations."},
                "role": {"type": "string", "enum": ["normal", "master", "scratch"]},
            }),
            &["location", "role"], false, true,
        ),
        def(
            "start_scan",
            "Rescan indexed folders",
            "Rescan an already indexed location (every folder indexed on it) or one folder inside it. Unchanged files are not re-read. Returns a job_id; poll job_status. Only the index is written.",
            Value::Object(start_props), &[], false, true,
        ),
        def(
            "job_status",
            "Job status",
            "State (queued, running, done, failed, cancelled, interrupted), phase, live counters, elapsed time and — when finished — the result or the error of a background job — of this or of another diskghost process on the same index. Read-only.",
            json!({"job_id": {"type": "string"}}), &["job_id"], true, true,
        ),
        def(
            "list_jobs",
            "List jobs",
            "Every background job every diskghost process on this index recorded — this server's and other MCP servers' alike; `interrupted` means the job's writer died mid-way. Read-only.",
            json!({}), &[], true, true,
        ),
        def(
            "cancel_job",
            "Cancel a job",
            "Ask a queued or running job to stop — only a job of this server; another process's job answers foreign_job. A cancelled local scan keeps what it already committed; nothing in the scanned folders is touched.",
            json!({"job_id": {"type": "string"}}), &["job_id"], false, true,
        ),
        def(
            "dupes_report",
            "Duplicate report",
            "Duplicate groups across every indexed location, offline ones included, biggest reclaimable first: copies (location, path, available, probable, shared inodes, suggested keep/reclaimable), unique_copies, reclaimable and actionable bytes, a copy-on-write warning, locations, single-copy counts per location, hash stats and warnings. Missing hashes are computed once and stored in the index (a writer job): the answer is inline when ready within ~20 s, else a job_id to poll. Reports counts only for single-copy files, never their paths. Changes no file.",
            json!({
                "min_size_bytes": page("Ignore files smaller than this (default 1048576)."),
                "limit": json!({"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "Groups per page (default 50, max 500)."}),
                "offset": page("Groups to skip."),
                "location": {"type": "string", "description": "Only groups with a copy on this location uuid."},
            }),
            &[], false, true,
        ),
        def(
            "plan_dedup",
            "Plan duplicate clean-up",
            "Decide which copies to keep and which are redundant under a policy, and write the plan as a JSON file in the fixed plans directory (never a path you choose, never overwritten). Nothing is applied: the answer carries the literal next step for the owner to run by hand (diskghost apply <plan> --dry-run). The policy expresses both the agent's decision and the user's stated preferences: min_copies, distinct devices, and per-location roles (master = keep, scratch = expendable). Per-file overrides are not supported — express a wish as a master rule, or edit the plan JSON. A writer job: inline within ~20 s, else a job_id.",
            json!({
                "min_copies": {"type": "integer", "minimum": 1,
                    "description": "Copies of each content that must remain (default 2)."},
                "distinct": {"type": "string", "enum": ["physical_device", "none"],
                    "description": "physical_device (default): remaining copies must sit on different physical devices; none: any copies count."},
                "locations": {"type": "array", "description": "Role overrides for this plan.",
                    "items": {"type": "object",
                        "properties": {
                            "match": {"type": "string", "description": "A location uuid, an absolute local path, or host:/path (e.g. ssh://box:/data)."},
                            "role": {"type": "string", "enum": ["normal", "master", "scratch"]},
                        },
                        "required": ["match", "role"], "additionalProperties": false}},
                "groups_limit": json!({"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "Groups returned inline (default 50, max 500); the file holds all."}),
                "groups_offset": page("Groups to skip."),
            }),
            &[], false, false,
        ),
    ]
}

/// Run a tool; `None` when there is no such tool.
pub(crate) fn call(ctx: &Arc<Ctx>, name: &str, args: &Value) -> Option<R> {
    Some(match name {
        "list_drives" => list_drives(ctx, args),
        "list_locations" => list_locations(ctx, args),
        "add_location" => add_location(ctx, args),
        "set_location_role" => set_location_role(ctx, args),
        "start_scan" => start_scan(ctx, args),
        "job_status" => job_status(ctx, args),
        "list_jobs" => list_jobs(ctx, args),
        "cancel_job" => cancel_job(ctx, args),
        "dupes_report" => dupes_report(ctx, args),
        "plan_dedup" => plan_dedup(ctx, args),
        _ => return None,
    })
}

// ------------------------------------------------------------ arguments

/// The arguments object, refusing any key the tool does not declare — a
/// stray `output` or `path` argument fails loudly instead of being ignored.
fn args_of(v: &Value, allowed: &[&str]) -> Result<Map<String, Value>, ToolError> {
    let m = match v {
        Value::Null => Map::new(),
        Value::Object(m) => m.clone(),
        _ => return Err(ToolError::invalid("arguments must be a JSON object")),
    };
    for k in m.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(ToolError::invalid(format!(
                "unknown argument {k:?}; this tool takes: {}",
                if allowed.is_empty() {
                    "no arguments".to_string()
                } else {
                    allowed.join(", ")
                }
            )));
        }
    }
    Ok(m)
}

fn opt_str<'a>(m: &'a Map<String, Value>, k: &str) -> Result<Option<&'a str>, ToolError> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(ToolError::invalid(format!("{k} must be a string"))),
    }
}

fn req_str<'a>(m: &'a Map<String, Value>, k: &str) -> Result<&'a str, ToolError> {
    opt_str(m, k)?.ok_or_else(|| ToolError::invalid(format!("{k} is required")))
}

fn opt_u64(m: &Map<String, Value>, k: &str) -> Result<Option<u64>, ToolError> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::invalid(format!("{k} must be a non-negative integer"))),
    }
}

fn opt_bool(m: &Map<String, Value>, k: &str) -> Result<Option<bool>, ToolError> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ToolError::invalid(format!("{k} must be a boolean"))),
    }
}

fn opt_strs(m: &Map<String, Value>, k: &str) -> Result<Vec<String>, ToolError> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| ToolError::invalid(format!("{k} must be an array of strings")))
            })
            .collect(),
        Some(_) => Err(ToolError::invalid(format!(
            "{k} must be an array of strings"
        ))),
    }
}

fn page_args(
    m: &Map<String, Value>,
    limit_key: &str,
    offset_key: &str,
) -> Result<(usize, usize), ToolError> {
    let limit = opt_u64(m, limit_key)?.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ToolError::invalid(format!(
            "{limit_key} must be between 1 and {MAX_LIMIT}"
        )));
    }
    let offset = opt_u64(m, offset_key)?.unwrap_or(0);
    Ok((
        usize::try_from(limit).unwrap_or(usize::MAX),
        usize::try_from(offset).unwrap_or(usize::MAX),
    ))
}

fn role_arg(s: &str) -> Result<Role, ToolError> {
    match s {
        "normal" => Ok(Role::Normal),
        "master" => Ok(Role::Master),
        "scratch" => Ok(Role::Scratch),
        other => Err(ToolError::invalid(format!(
            "role {other:?}: expected normal, master or scratch"
        ))),
    }
}

fn agent_arg(m: &Map<String, Value>) -> Result<AgentMode, ToolError> {
    match opt_str(m, "agent")? {
        None | Some("auto") => Ok(AgentMode::Auto),
        Some("never") => Ok(AgentMode::Never),
        Some(other) => Err(ToolError::invalid(format!(
            "agent {other:?}: expected auto or never"
        ))),
    }
}

/// Where a scan goes.
#[derive(Clone)]
enum Target {
    Local(PathBuf),
    Remote { url: String, host: String },
}

impl Target {
    fn label(&self) -> String {
        match self {
            Target::Local(p) => p.display().to_string(),
            Target::Remote { url, .. } => url.clone(),
        }
    }
}

/// A folder argument: an absolute existing local directory, or an ssh URL
/// that survives [`parse_ssh_url`] (host charset check included) — checked
/// before any process is spawned.
fn parse_target(s: &str) -> Result<Target, ToolError> {
    if s.is_empty() || s.chars().any(char::is_control) {
        return Err(ToolError::invalid(
            "path must be non-empty and free of control characters",
        ));
    }
    if s.starts_with("ssh://") {
        let (host, path) = parse_ssh_url(s).map_err(ToolError::invalid)?;
        // The path half is model input as well. Only `ssh://host//abs/path`
        // is accepted: a relative path (`-delete`, `-fprintf…`) could be
        // read as an option by the remote `find`.
        if !path.starts_with('/') {
            return Err(ToolError::invalid(format!(
                "{s:?}: the remote path must be absolute — write ssh://{host}//absolute/path"
            )));
        }
        if !diskghost_core::remote::shell_safe(&path) {
            return Err(ToolError::invalid(format!(
                "{s:?}: a backslash in a remote path cannot be passed safely to the remote shell"
            )));
        }
        return Ok(Target::Remote {
            url: s.to_string(),
            host,
        });
    }
    let p = Path::new(s);
    if !p.is_absolute() {
        return Err(ToolError::invalid(format!(
            "{s:?} is not an absolute path (the server's working directory is not yours)"
        )));
    }
    if !p.is_dir() {
        return Err(ToolError::new(
            "not_a_directory",
            format!("{s:?} is not a directory on this machine"),
        ));
    }
    Ok(Target::Local(p.to_path_buf()))
}

fn walk_options(m: &Map<String, Value>) -> Result<Options, ToolError> {
    let exclude = opt_strs(m, "exclude")?;
    let bad = validate_globs(&exclude);
    if !bad.is_empty() {
        return Err(ToolError::invalid(format!(
            "invalid exclude glob(s): {}",
            bad.join(", ")
        )));
    }
    Ok(Options {
        exclude,
        same_file_system: opt_bool(m, "one_file_system")?.unwrap_or(false),
        skip_remote: true,
        ..Options::default()
    })
}

// ---------------------------------------------------------------- index

fn open_index(db: &Path, busy: std::time::Duration) -> Result<Index, ToolError> {
    Index::open_with_busy_timeout(db, busy).map_err(|e| ToolError::from_io(db, &e))
}

/// Before a write job is promised: fail with `index_busy` now if another
/// process holds the write lock. While a job of this server holds the
/// turn, the new one simply queues behind it.
fn preflight_writer(ctx: &Ctx) -> Result<(), ToolError> {
    if ctx.jobs.writer_active().is_some() {
        return Ok(());
    }
    let idx = open_index(&ctx.db, ctx.busy)?;
    idx.probe_write_lock()
        .map_err(|e| ToolError::from_io(&ctx.db, &e))
}

fn role_name(r: Role) -> Value {
    serde_json::to_value(r).unwrap_or(Value::Null)
}

fn unix(secs: i64) -> u64 {
    u64::try_from(secs).unwrap_or(0)
}

fn location_json(l: &LocationStatus, online: Value) -> Value {
    json!({
        "uuid": l.uuid,
        "host": l.host,
        "role": role_name(l.role),
        "root": l.root,
        "roots": l.roots,
        "online": online,
        "files": l.files,
        "bytes": l.bytes,
        "last_seen_unix": unix(l.last_seen),
    })
}

fn sorted_locations(mut v: Vec<LocationStatus>) -> Vec<LocationStatus> {
    v.sort_by(|a, b| (&a.host, &a.root, &a.uuid).cmp(&(&b.host, &b.root, &b.uuid)));
    v
}

// ------------------------------------------------------------- read tools

#[cfg(unix)]
fn mounts() -> Vec<LocalMount> {
    diskghost_core::local_mounts()
}

#[cfg(not(unix))]
fn mounts() -> Vec<LocalMount> {
    Vec::new()
}

fn list_drives(ctx: &Arc<Ctx>, args: &Value) -> R {
    args_of(args, &[])?;
    let mut warnings = Vec::new();
    let status = match open_index(&ctx.db, ctx.busy).and_then(|i| {
        i.status_without_remote_probe()
            .map_err(|e| ToolError::from_io(&ctx.db, &e))
    }) {
        Ok(s) => Some(sorted_locations(s)),
        Err(e) => {
            warnings.push(format!(
                "index not readable, indexed state unknown: {}",
                e.message
            ));
            None
        }
    };
    let known: HashMap<&str, &LocationStatus> = status
        .iter()
        .flatten()
        .map(|l| (l.uuid.as_str(), l))
        .collect();
    let mut ms = mounts();
    ms.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    let drives: Vec<Value> = ms
        .iter()
        .map(|m| {
            let (total, free) = disk_space(&m.mount_point);
            let uuid = volume_uuid(&m.mount_point).ok();
            let indexed = match (&status, &uuid) {
                (Some(_), Some(u)) => json!(known.contains_key(u.as_str())),
                _ => Value::Null,
            };
            json!({
                "mount_point": m.mount_point,
                "fstype": m.fstype,
                "source": m.source,
                "total_bytes": total,
                "free_bytes": free,
                "media": if is_rotational(&m.mount_point) { "hdd" } else { "ssd" },
                "cow": is_cow_fs(&m.mount_point),
                "uuid": uuid,
                "indexed": indexed,
            })
        })
        .collect();
    let remembered: Vec<Value> = status
        .iter()
        .flatten()
        .filter(|l| diskghost_core::remote::is_remote_host(&l.host))
        .map(|l| {
            json!({
                "uuid": l.uuid, "host": l.host, "role": role_name(l.role),
                "root": l.root, "roots": l.roots, "files": l.files, "bytes": l.bytes,
                "last_seen_unix": unix(l.last_seen),
            })
        })
        .collect();
    Ok(json!({
        "db": ctx.db,
        "drives": drives,
        "remembered_ssh": remembered,
        "warnings": warnings,
    }))
}

fn list_locations(ctx: &Arc<Ctx>, args: &Value) -> R {
    let m = args_of(args, &["probe_remote"])?;
    let probe = opt_bool(&m, "probe_remote")?.unwrap_or(true);
    let idx = open_index(&ctx.db, ctx.busy)?;
    let status = if probe {
        idx.status()
    } else {
        idx.status_without_remote_probe()
    }
    .map_err(|e| ToolError::from_io(&ctx.db, &e))?;
    let locations: Vec<Value> = sorted_locations(status)
        .iter()
        .map(|l| {
            let remote = diskghost_core::remote::is_remote_host(&l.host);
            let online = if remote && !probe {
                Value::Null
            } else {
                json!(l.online)
            };
            location_json(l, online)
        })
        .collect();
    Ok(json!({"db": ctx.db, "locations": locations}))
}

fn job_status(ctx: &Arc<Ctx>, args: &Value) -> R {
    let m = args_of(args, &["job_id"])?;
    let id = req_str(&m, "job_id")?;
    ctx.jobs
        .get(id)
        .ok_or_else(|| ToolError::new("unknown_job", format!("no job {id:?} in this server")))
}

fn list_jobs(ctx: &Arc<Ctx>, args: &Value) -> R {
    args_of(args, &[])?;
    let jobs: Vec<Value> = ctx
        .jobs
        .list()
        .into_iter()
        .map(|mut j| {
            if let Some(o) = j.as_object_mut() {
                if o.remove("result").is_some() {
                    o.insert("has_result".into(), json!(true));
                }
            }
            j
        })
        .collect();
    Ok(json!({"jobs": jobs}))
}

fn cancel_job(ctx: &Arc<Ctx>, args: &Value) -> R {
    let m = args_of(args, &["job_id"])?;
    let id = req_str(&m, "job_id")?;
    let live = ctx.jobs.cancel(id).ok_or_else(|| {
        if ctx.jobs.foreign(id) {
            ToolError::new(
                "foreign_job",
                format!(
                    "job {id:?} is owned by another diskghost process; \
                     only the server that started it can cancel it"
                ),
            )
        } else {
            ToolError::new("unknown_job", format!("no job {id:?} in this server"))
        }
    })?;
    let state = ctx
        .jobs
        .get(id)
        .map(|j| j["state"].clone())
        .unwrap_or(Value::Null);
    Ok(json!({"job_id": id, "cancel_requested": live, "state": state}))
}

// ------------------------------------------------------------ write tools

fn set_location_role(ctx: &Arc<Ctx>, args: &Value) -> R {
    let m = args_of(args, &["location", "role"])?;
    let uuid = req_str(&m, "location")?;
    let role = role_arg(req_str(&m, "role")?)?;
    if let Some(job) = ctx.jobs.writer_active() {
        return Err(ToolError {
            code: "index_busy",
            message: format!(
                "{job} of this server is writing the index right now; its transaction would \
                 hold this change until it ends — retry afterwards"
            ),
            db: Some(ctx.db.display().to_string()),
        });
    }
    let idx = open_index(&ctx.db, ctx.busy)?;
    match idx.set_role(uuid, role) {
        Ok(()) => Ok(json!({"location": uuid, "role": role_name(role)})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(ToolError::new("unknown_location", e.to_string()))
        }
        Err(e) => Err(ToolError::from_io(&ctx.db, &e)),
    }
}

/// One scan the job performs.
struct ScanSpec {
    target: Target,
    role: Option<Role>,
}

/// The job body of a scan: each spec in turn, one summary each.
fn scan_work(
    db: PathBuf,
    busy: std::time::Duration,
    specs: Vec<ScanSpec>,
    opts: Options,
    agent: AgentMode,
) -> Box<dyn FnOnce(&Progress) -> R + Send> {
    Box::new(move |p| {
        let idx = open_index(&db, busy)?;
        let mut summaries = Vec::new();
        for spec in &specs {
            if p.cancelled() {
                break;
            }
            let scanned = match &spec.target {
                Target::Local(path) => index_scan(&idx, path, &opts, p)
                    .and_then(|s| idx.configure(&s.uuid, spec.role, "local").map(|()| s)),
                Target::Remote { url, host } => index_remote(&idx, url, agent, p).and_then(|s| {
                    idx.configure(&s.uuid, spec.role, &format!("ssh://{host}"))
                        .map(|()| s)
                }),
            };
            let s = scanned.map_err(|e| ToolError::from_io(&db, &e))?;
            summaries.push(serde_json::to_value(&s).unwrap_or(Value::Null));
        }
        Ok(json!({"summaries": summaries}))
    })
}

fn upload_notes(target: &Target, agent: AgentMode) -> Vec<String> {
    match (target, agent) {
        (Target::Remote { host, .. }, AgentMode::Auto) => vec![format!(
            "agent=auto: on a Linux x86_64 host this program is copied once to \
             ~/.cache/diskghost/ on {host} (mv -n, never overwrites; not a user file) and runs there \
             as a read-only agent; pass agent=\"never\" to forbid the upload. The result's \
             transport/agent_note say what actually ran."
        )],
        _ => Vec::new(),
    }
}

fn job_started(ctx: &Arc<Ctx>, id: &str, target: &str, notes: &[String]) -> Value {
    let state = ctx.jobs.get(id).map_or(Value::Null, |j| j["state"].clone());
    json!({"job_id": id, "kind": "scan", "state": state, "target": target, "notes": notes})
}

fn add_location(ctx: &Arc<Ctx>, args: &Value) -> R {
    let m = args_of(
        args,
        &["path", "role", "agent", "exclude", "one_file_system"],
    )?;
    let target = parse_target(req_str(&m, "path")?)?;
    let role = opt_str(&m, "role")?.map(role_arg).transpose()?;
    let agent = agent_arg(&m)?;
    let opts = walk_options(&m)?;
    if matches!(target, Target::Remote { .. })
        && (m.contains_key("exclude") || m.contains_key("one_file_system"))
    {
        return Err(ToolError::invalid(
            "exclude and one_file_system apply to local folders only: the host lists its own files",
        ));
    }
    preflight_writer(ctx)?;
    let label = target.label();
    let notes = upload_notes(&target, agent);
    let id = ctx.jobs.submit(
        "scan",
        label.clone(),
        ctx.hold,
        scan_work(
            ctx.db.clone(),
            ctx.busy,
            vec![ScanSpec { target, role }],
            opts,
            agent,
        ),
    );
    Ok(job_started(ctx, &id, &label, &notes))
}

fn start_scan(ctx: &Arc<Ctx>, args: &Value) -> R {
    let m = args_of(
        args,
        &["location", "path", "agent", "exclude", "one_file_system"],
    )?;
    let agent = agent_arg(&m)?;
    let opts = walk_options(&m)?;
    let by_uuid = opt_str(&m, "location")?;
    let by_path = opt_str(&m, "path")?;
    if by_uuid.is_some() == by_path.is_some() {
        return Err(ToolError::invalid("give exactly one of location or path"));
    }
    let single = by_path.map(parse_target).transpose()?;
    let idx = open_index(&ctx.db, ctx.busy)?;
    let status = idx
        .status_without_remote_probe()
        .map_err(|e| ToolError::from_io(&ctx.db, &e))?;
    drop(idx);
    let mut specs = Vec::new();
    match (by_uuid, &single) {
        (Some(uuid), _) => {
            let l = status.iter().find(|l| l.uuid == uuid).ok_or_else(|| {
                ToolError::new(
                    "unknown_location",
                    format!("no location {uuid:?}; see list_locations"),
                )
            })?;
            let remote = diskghost_core::remote::is_remote_host(&l.host);
            for root in &l.roots {
                let target = if remote {
                    let url = format!("{}/{}", l.host, root.display());
                    parse_target(&url)?
                } else {
                    Target::Local(root.clone())
                };
                specs.push(ScanSpec { target, role: None });
            }
            if specs.is_empty() {
                return Err(ToolError::new(
                    "unknown_location",
                    format!("location {uuid} has no recorded scan folder; use add_location"),
                ));
            }
        }
        (None, Some(t)) => {
            // "Inside a location" is judged lexically, so a `..` component
            // (`/indexed/../elsewhere`) must not pass it; a remote folder,
            // like a local one, must lie under that location's root.
            let inside = |p: &Path, root: &Path| {
                !p.components().any(|c| c == std::path::Component::ParentDir) && p.starts_with(root)
            };
            let known = status.iter().any(|l| match t {
                Target::Local(p) => {
                    !diskghost_core::remote::is_remote_host(&l.host) && inside(p, &l.root)
                }
                Target::Remote { url, host } => {
                    l.host == format!("ssh://{host}")
                        && parse_ssh_url(url)
                            .is_ok_and(|(_, path)| inside(Path::new(&path), &l.root))
                }
            });
            if !known {
                return Err(ToolError::new(
                    "unknown_location",
                    "that folder is not inside an indexed location; use add_location",
                ));
            }
            specs.push(ScanSpec {
                target: t.clone(),
                role: None,
            });
        }
        (None, None) => unreachable!("one of location or path is set"),
    }
    for s in &specs {
        if let Target::Local(p) = &s.target {
            if !p.is_dir() {
                return Err(ToolError::new(
                    "location_offline",
                    format!("{} is not reachable now (unplugged?)", p.display()),
                ));
            }
        }
    }
    preflight_writer(ctx)?;
    let label = specs
        .iter()
        .map(|s| s.target.label())
        .collect::<Vec<_>>()
        .join(", ");
    let notes: Vec<String> = specs
        .first()
        .map(|s| upload_notes(&s.target, agent))
        .unwrap_or_default();
    let id = ctx.jobs.submit(
        "scan",
        label.clone(),
        ctx.hold,
        scan_work(ctx.db.clone(), ctx.busy, specs, opts, agent),
    );
    Ok(job_started(ctx, &id, &label, &notes))
}

// ------------------------------------------------- reports (writer jobs)

/// Submit `work` as a writer job and wait for it: the result inline when it
/// finishes within the inline wait, otherwise the job handle.
fn inline_or_job(
    ctx: &Arc<Ctx>,
    kind: &'static str,
    target: String,
    work: Box<dyn FnOnce(&Progress) -> R + Send>,
) -> R {
    let id = ctx.jobs.submit(kind, target, ctx.hold, work);
    match ctx.jobs.wait_finished(&id, ctx.inline_wait) {
        Some((State::Done, Some(v), _)) => Ok(v),
        Some((State::Failed, _, Some(e))) => Err(e),
        Some((State::Cancelled, ..)) => Err(ToolError::new("cancelled", "the job was cancelled")),
        Some((s, ..)) => Err(ToolError::new(
            "internal",
            format!("job {id} ended {s:?} without a result"),
        )),
        None => {
            let state = ctx
                .jobs
                .get(&id)
                .map_or(Value::Null, |j| j["state"].clone());
            Ok(json!({
                "job_id": id,
                "state": state,
                "note": "not finished within the inline wait: poll job_status; the result is its `result`",
            }))
        }
    }
}

fn dupes_report(ctx: &Arc<Ctx>, args: &Value) -> R {
    let m = args_of(args, &["min_size_bytes", "limit", "offset", "location"])?;
    let min_size = opt_u64(&m, "min_size_bytes")?.unwrap_or(DEFAULT_MIN_SIZE);
    let (limit, offset) = page_args(&m, "limit", "offset")?;
    let location = opt_str(&m, "location")?.map(str::to_string);
    let (db, busy) = (ctx.db.clone(), ctx.busy);
    inline_or_job(
        ctx,
        "dupes_report",
        "all indexed locations".into(),
        Box::new(move |p| {
            let idx = open_index(&db, busy)?;
            let status = idx
                .status_without_remote_probe()
                .map_err(|e| ToolError::from_io(&db, &e))?;
            if let Some(u) = &location {
                if !status.iter().any(|l| &l.uuid == u) {
                    return Err(ToolError::new(
                        "unknown_location",
                        format!("no location {u:?}; see list_locations"),
                    ));
                }
            }
            let opts = IndexReportOptions {
                min_size,
                singles: true,
                singles_paths: false,
            };
            let report = index_dupes(&idx, &opts, p).map_err(|e| ToolError::from_io(&db, &e))?;
            Ok(dupes_output(
                &report,
                &status,
                location.as_deref(),
                limit,
                offset,
            ))
        }),
    )
}

/// The report as the agent gets it: stable order (reclaimable bytes
/// descending, then hash), one page of groups, no wall-clock fields.
fn dupes_output(
    r: &IndexDupesReport,
    status: &[LocationStatus],
    location: Option<&str>,
    limit: usize,
    offset: usize,
) -> Value {
    let mut groups: Vec<&diskghost_core::IndexGroup> = r
        .groups
        .iter()
        .filter(|g| location.is_none_or(|u| g.copies.iter().any(|c| c.location_uuid == u)))
        .collect();
    groups.sort_by(|a, b| {
        let key = |g: &diskghost_core::IndexGroup| {
            g.hash
                .clone()
                .or_else(|| g.sample.clone())
                .unwrap_or_default()
        };
        (Reverse(a.reclaimable_bytes), key(a), a.size).cmp(&(
            Reverse(b.reclaimable_bytes),
            key(b),
            b.size,
        ))
    });
    let total = groups.len();
    let reclaimable: u64 = groups.iter().map(|g| g.reclaimable_bytes).sum();
    let actionable: u64 = groups.iter().map(|g| g.actionable_bytes).sum();
    let page: Vec<Value> = groups
        .iter()
        .skip(offset)
        .take(limit)
        .map(|g| serde_json::to_value(g).unwrap_or(Value::Null))
        .collect();
    let by_uuid: HashMap<&str, &LocationStatus> =
        status.iter().map(|l| (l.uuid.as_str(), l)).collect();
    let mut locations: Vec<Value> = r
        .locations
        .iter()
        .map(|l| {
            let st = by_uuid.get(l.uuid.as_str());
            json!({
                "uuid": l.uuid,
                "root": l.root,
                "host": st.map(|s| s.host.clone()),
                "role": st.map(|s| role_name(s.role)),
                "available": l.available,
                "rotational": l.rotational,
                "cow": l.cow,
            })
        })
        .collect();
    locations.sort_by(|a, b| a["uuid"].as_str().cmp(&b["uuid"].as_str()));
    let mut singles = r.singles.clone().unwrap_or_default();
    singles.sort_by(|a, b| a.location_uuid.cmp(&b.location_uuid));
    let mut unverifiable = r.unverifiable.clone();
    unverifiable.sort_by(|a, b| (Reverse(a.size), &a.algos).cmp(&(Reverse(b.size), &b.algos)));
    json!({
        "total_groups": total,
        "offset": offset,
        "limit": limit,
        "reclaimable_bytes_total": reclaimable,
        "actionable_bytes_total": actionable,
        "groups": page,
        "locations": locations,
        "unverifiable_total": unverifiable.len(),
        "unverifiable": unverifiable.iter().take(limit).collect::<Vec<_>>(),
        "singles": singles,
        "stats": r.stats,
        "warnings": r.warnings,
    })
}

fn policy_args(m: &Map<String, Value>) -> Result<Policy, ToolError> {
    let min_copies = match m.get("min_copies") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_i64()
                .ok_or_else(|| ToolError::invalid("min_copies must be an integer"))?,
        ),
    };
    let distinct = opt_str(m, "distinct")?;
    let mut rules = Vec::new();
    match m.get("locations") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) => {
            for r in a {
                let o = r
                    .as_object()
                    .ok_or_else(|| ToolError::invalid("each locations entry is an object"))?;
                let o = args_of(&Value::Object(o.clone()), &["match", "role"])?;
                rules.push((
                    req_str(&o, "match")?.to_string(),
                    req_str(&o, "role")?.to_string(),
                ));
            }
        }
        Some(_) => return Err(ToolError::invalid("locations must be an array")),
    }
    Policy::from_rules(min_copies, distinct, &rules).map_err(ToolError::invalid)
}

fn plan_dedup(ctx: &Arc<Ctx>, args: &Value) -> R {
    let m = args_of(
        args,
        &[
            "min_copies",
            "distinct",
            "locations",
            "groups_limit",
            "groups_offset",
        ],
    )?;
    let policy = policy_args(&m)?;
    let (limit, offset) = page_args(&m, "groups_limit", "groups_offset")?;
    let dir = plans_dir().ok_or_else(|| {
        ToolError::new(
            "no_state_dir",
            "no state directory known ($XDG_STATE_HOME / $HOME unset): cannot place a plan",
        )
    })?;
    let (db, busy) = (ctx.db.clone(), ctx.busy);
    inline_or_job(
        ctx,
        "plan_dedup",
        "all indexed locations".into(),
        Box::new(move |p| {
            let idx = open_index(&db, busy)?;
            let plan = make_plan(&idx, &policy, p).map_err(|e| ToolError::from_io(&db, &e))?;
            if p.cancelled() {
                return Err(ToolError::new("cancelled", "cancelled"));
            }
            let text = serde_json::to_string_pretty(&plan).unwrap_or_default() + "\n";
            let path = create_plan_file(&dir, &text, &utc_stamp(plan.created_unix), &mut random32)
                .map_err(|e| ToolError::new("plan_write_failed", e))?;
            let total = plan.groups.len();
            let groups: Vec<&diskghost_core::plan::PlanGroup> =
                plan.groups.iter().skip(offset).take(limit).collect();
            Ok(json!({
                "plan_path": path,
                "next_step": format!("diskghost apply {} --dry-run", path.display()),
                "note": "Nothing was applied or moved. Review the plan, then the owner runs the next step by hand.",
                "policy": plan.policy,
                "summary": plan.summary,
                "warnings": plan.warnings,
                "total_groups": total,
                "groups_offset": offset,
                "groups_limit": limit,
                "groups": groups,
            }))
        }),
    )
}

// ------------------------------------------------------------ plan files

/// The one directory plans are written to.
pub(crate) fn plans_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))?;
    Some(base.join("diskghost").join("plans"))
}

/// `20260102T030405Z` for unix seconds.
pub(crate) fn utc_stamp(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Days since 1970-01-01 → civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 32 random bits from the OS-seeded hasher (no new dependency).
fn random32() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    (h.finish() & 0xffff_ffff) as u32
}

/// Write `text` to a new `plan-<stamp>-<rand>.json` in `dir`, never over an
/// existing file: `create_new` fails when the name is taken and a new
/// random suffix is tried. An earlier apply's journal beside the name would
/// mix records, so that is refused as the CLI's `plan -o` refuses it.
pub(crate) fn create_plan_file(
    dir: &Path,
    text: &str,
    stamp: &str,
    rand: &mut dyn FnMut() -> u32,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    for _ in 0..32 {
        let path = dir.join(format!("plan-{stamp}-{:08x}.json", rand()));
        crate::check_no_journal(&path)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(text.as_bytes())
                    .and_then(|()| f.sync_all())
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
        }
    }
    Err(format!("no free plan file name in {}", dir.display()))
}
