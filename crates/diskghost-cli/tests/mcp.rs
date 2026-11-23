//! `diskghost mcp`: the MCP server (stdio, JSON-RPC 2.0) — protocol
//! handshake, the exact read-only tool allowlist, hostile-input rejection,
//! the fixed plans directory, and the one-writer-per-index rule.
//!
//! Every server here runs against a scratch index (`--db`) with HOME,
//! XDG_DATA_HOME and XDG_STATE_HOME pointed into a throwaway directory:
//! no test ever opens the owner's index or writes outside its tempdir.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_diskghost")
}

/// The only tools the server may ever offer. Adding one must fail a test.
const ALLOWLIST: [&str; 10] = [
    "add_location",
    "cancel_job",
    "dupes_report",
    "job_status",
    "list_drives",
    "list_jobs",
    "list_locations",
    "plan_dedup",
    "set_location_role",
    "start_scan",
];

/// A throwaway directory unique to this test process and label.
fn scratch(label: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("diskghost-mcp-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The sandbox one server lives in.
struct Sandbox {
    root: PathBuf,
    db: PathBuf,
    state: PathBuf,
    /// A fake `ssh` first on PATH: records every call it gets.
    ssh_marker: PathBuf,
}

impl Sandbox {
    fn new(label: &str) -> Sandbox {
        let root = scratch(label);
        let bindir = root.join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let ssh_marker = root.join("ssh-was-called");
        let fake = bindir.join("ssh");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\nexit 255\n",
                ssh_marker.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Sandbox {
            db: root.join("scratch-index.db"),
            state: root.join("state"),
            ssh_marker,
            root,
        }
    }

    fn spawn(&self, extra_env: &[(&str, &str)]) -> Mcp {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(bin());
        cmd.arg("mcp")
            .arg("--db")
            .arg(&self.db)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", &self.state)
            .env("PATH", path)
            .env_remove("DISKGHOST_MCP_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn diskghost mcp");
        let stdin = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Mcp {
            child,
            stdin,
            rx,
            next_id: 1,
        }
    }

    /// A folder of files with known duplicates: `a/one.bin` == `b/two.bin`
    /// (3 copies in all with `b/three.bin`), plus a unique file.
    fn fixture(&self, name: &str) -> PathBuf {
        let t = self.root.join(name);
        std::fs::create_dir_all(t.join("a")).unwrap();
        std::fs::create_dir_all(t.join("b")).unwrap();
        let big = vec![7u8; 64 * 1024];
        std::fs::write(t.join("a/one.bin"), &big).unwrap();
        std::fs::write(t.join("b/two.bin"), &big).unwrap();
        std::fs::write(t.join("b/three.bin"), &big).unwrap();
        std::fs::write(t.join("a/unique.bin"), vec![9u8; 70 * 1024]).unwrap();
        t
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A running server and the pipe to talk to it.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    next_id: u64,
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Mcp {
    fn send(&mut self, v: &Value) {
        let mut line = serde_json::to_string(v).unwrap();
        assert!(!line.contains('\n'));
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    /// The next frame the server wrote; stdout must carry JSON frames only.
    fn recv(&mut self) -> Value {
        match self.rx.recv_timeout(Duration::from_secs(30)) {
            Ok(l) => serde_json::from_str(&l)
                .unwrap_or_else(|e| panic!("stdout line is not a JSON frame ({e}): {l:?}")),
            Err(RecvTimeoutError::Timeout) => panic!("server silent for 30 s"),
            Err(RecvTimeoutError::Disconnected) => panic!("server closed stdout"),
        }
    }

    /// Send a request, return the response with its id (skipping
    /// notifications the server may interleave).
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let m = self.recv();
            if m.get("id") == Some(&json!(id)) {
                assert_eq!(m["jsonrpc"], "2.0");
                return m;
            }
            assert!(m.get("method").is_some(), "stray frame: {m}");
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn handshake(&mut self, version: &str) -> Value {
        let r = self.request(
            "initialize",
            json!({
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": {"name": "mcp-test", "version": "0"}
            }),
        );
        self.notify("notifications/initialized", json!({}));
        r
    }

    /// `tools/call`: the result object.
    fn call(&mut self, name: &str, args: Value) -> Value {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        assert!(r.get("error").is_none(), "{name}: protocol error {r}");
        r["result"].clone()
    }

    /// A call that must succeed: its structured content.
    fn ok(&mut self, name: &str, args: Value) -> Value {
        let r = self.call(name, args);
        assert_ne!(r["isError"], json!(true), "{name} failed: {r}");
        assert_eq!(
            serde_json::from_str::<Value>(r["content"][0]["text"].as_str().unwrap()).unwrap(),
            r["structuredContent"],
            "{name}: text content must mirror structuredContent"
        );
        r["structuredContent"].clone()
    }

    fn wait_job(&mut self, id: &str, want: &str) -> Value {
        let t = Instant::now();
        loop {
            let s = self.ok("job_status", json!({"job_id": id}));
            if s["state"] == want {
                return s;
            }
            let state = s["state"].as_str().unwrap().to_string();
            assert!(
                !matches!(state.as_str(), "done" | "failed" | "cancelled") || state == want,
                "job {id} ended {state}, wanted {want}: {s}"
            );
            assert!(t.elapsed() < Duration::from_secs(60), "job {id} stuck: {s}");
            std::thread::sleep(Duration::from_millis(40));
        }
    }
}

// ---------------------------------------------------------------- protocol

#[test]
fn mcp_handshake_2025_06_18() {
    let sb = Sandbox::new("hs-0618");
    let mut m = sb.spawn(&[]);
    let r = m.handshake("2025-06-18");
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    assert!(r["result"]["capabilities"]["tools"].is_object());
    assert_eq!(r["result"]["serverInfo"]["name"], "diskghost");
    assert_eq!(
        r["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    // ping answers an empty result.
    let p = m.request("ping", json!({}));
    assert_eq!(p["result"], json!({}));
}

#[test]
fn mcp_handshake_2025_11_25() {
    let sb = Sandbox::new("hs-1125");
    let mut m = sb.spawn(&[]);
    let r = m.handshake("2025-11-25");
    assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
    assert!(r["result"]["capabilities"]["tools"].is_object());
}

#[test]
fn mcp_unsupported_client_version_gets_the_newest_supported() {
    let sb = Sandbox::new("hs-future");
    let mut m = sb.spawn(&[]);
    let r = m.handshake("2099-01-01");
    assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
}

#[test]
fn mcp_unknown_method_is_an_error_and_unknown_notifications_are_ignored() {
    let sb = Sandbox::new("unknown");
    let mut m = sb.spawn(&[]);
    m.handshake("2025-06-18");
    m.notify("notifications/definitely-not-real", json!({"x": 1}));
    let r = m.request("resources/subscribe", json!({}));
    assert_eq!(r["error"]["code"], -32601, "{r}");
    // The ignored notification produced no frame: the next frame is this ping's.
    let p = m.request("ping", json!({}));
    assert_eq!(p["result"], json!({}));
}

// 2026-07-28: no `initialize`; every request names its version in `_meta`,
// `server/discover` says what the server speaks.
#[test]
fn mcp_2026_07_28_is_stateless_with_server_discover() {
    let sb = Sandbox::new("modern");
    let mut m = sb.spawn(&[]);
    let meta = json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}});
    // No handshake at all.
    let d = m.request("server/discover", meta.clone());
    let r = &d["result"];
    assert_eq!(r["resultType"], "complete");
    assert_eq!(
        r["supportedVersions"],
        json!(["2025-06-18", "2025-11-25", "2026-07-28"])
    );
    assert!(r["capabilities"]["tools"].is_object());
    assert_eq!(
        r["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "diskghost"
    );
    assert!(r["ttlMs"].as_u64().is_some() && r["cacheScope"].is_string());
    // tools/list carries resultType + ttlMs + cacheScope on the result.
    let l = m.request("tools/list", meta.clone());
    assert_eq!(l["result"]["resultType"], "complete");
    assert!(l["result"]["ttlMs"].as_u64().is_some());
    assert!(l["result"]["cacheScope"].is_string());
    assert_eq!(
        l["result"]["tools"].as_array().unwrap().len(),
        ALLOWLIST.len()
    );
    // tools/call too.
    let c = m.request(
        "tools/call",
        json!({"name": "list_jobs", "arguments": {}, "_meta": meta["_meta"].clone()}),
    );
    assert_eq!(c["result"]["resultType"], "complete");
    // An unknown version is refused with the supported list.
    let bad = m.request(
        "tools/list",
        json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2099-01-01"}}),
    );
    assert_eq!(bad["error"]["code"], -32022, "{bad}");
    assert_eq!(bad["error"]["data"]["requested"], "2099-01-01");
    assert!(bad["error"]["data"]["supported"].is_array());
}

// --------------------------------------------------- the read-only surface

#[test]
fn mcp_tool_allowlist_is_exact() {
    let sb = Sandbox::new("allow");
    let mut m = sb.spawn(&[]);
    m.handshake("2025-06-18");
    let r = m.request("tools/list", json!({}));
    let tools = r["result"]["tools"].as_array().expect("tools array");
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(names, ALLOWLIST, "the tool list must equal the allowlist");

    let read_only = ["job_status", "list_drives", "list_jobs", "list_locations"];
    for t in tools {
        let name = t["name"].as_str().unwrap();
        assert_eq!(t["inputSchema"]["type"], "object", "{name}");
        assert!(t["description"].as_str().is_some_and(|d| d.len() > 20));
        let a = &t["annotations"];
        assert_eq!(
            a["readOnlyHint"],
            json!(read_only.contains(&name)),
            "{name}: readOnlyHint must be honest"
        );
        assert_eq!(a["destructiveHint"], json!(false), "{name}");
        // No caller-chosen output / cache path anywhere in any schema.
        let props = t["inputSchema"]["properties"].as_object().cloned();
        for key in props.iter().flat_map(|p| p.keys()) {
            assert!(
                !matches!(
                    key.as_str(),
                    "output"
                        | "output_path"
                        | "out"
                        | "file"
                        | "cache"
                        | "cache_file"
                        | "plan_path"
                ),
                "{name} takes a caller path: {key}"
            );
        }
    }
    // plan_dedup in particular: the policy, never a destination.
    let plan = tools.iter().find(|t| t["name"] == "plan_dedup").unwrap();
    let keys: Vec<&String> = plan["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert!(keys.iter().any(|k| *k == "min_copies"), "{keys:?}");
}

/// Strip `//` comments and string/char literals from Rust source, so the
/// scan below sees code only (a doc line may *say* "apply"; a call may not).
fn code_only(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '"' {
            i += 1;
            while i < b.len() && b[i] != '"' {
                if b[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push_str("\"\"");
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn words(src: &str) -> Vec<String> {
    src.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn mcp_module_has_no_mutating_symbols() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no mcp module sources in {}",
        dir.display()
    );
    let forbidden = [
        "apply",
        "undo",
        "reclaim",
        "remove_path",
        "RemoveMode",
        "ReclaimAction",
        "forget",
        "rename",
        "remove_file",
        "remove_dir",
        "remove_dir_all",
        "trash",
        "hard_link",
        "symlink",
        "set_permissions",
        "set_modified",
        "truncate",
    ];
    let mut hits = Vec::new();
    for f in &files {
        let name = f.file_name().unwrap().to_string_lossy();
        if name == "tests.rs" {
            continue; // the module's own tests may name what they forbid
        }
        let code = code_only(&std::fs::read_to_string(f).unwrap());
        for w in words(&code) {
            if forbidden.contains(&w.as_str()) {
                hits.push(format!("{name}: {w}"));
            }
        }
        // `fs::write` / `File::create` would be a free-form file write; only
        // `create_new` on the plans directory is allowed.
        for needle in ["fs::write", "File::create(", "fs::remove", "fs::rename"] {
            if code.contains(needle) {
                hits.push(format!("{name}: {needle}"));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "mutating symbols in the mcp module: {hits:?}"
    );
}

// ------------------------------------------------------ hostile arguments

#[test]
fn mcp_hostile_ssh_urls_are_rejected_before_any_process_is_spawned() {
    let sb = Sandbox::new("hostile");
    let mut m = sb.spawn(&[]);
    m.handshake("2025-06-18");
    for url in [
        "ssh://-oProxyCommand=x//p",
        "ssh://a b//p",
        "ssh://host;rm//p",
        "ssh://$(id)//p",
        "ssh://host`id`//p",
    ] {
        for tool in ["add_location", "start_scan"] {
            let r = m.call(tool, json!({"path": url}));
            assert_eq!(r["isError"], json!(true), "{tool} {url}: {r}");
            assert_eq!(
                r["structuredContent"]["error"]["code"], "invalid_argument",
                "{tool} {url}: {r}"
            );
        }
    }
    // A relative local path is ambiguous for a server whose cwd is unknown.
    let r = m.call("add_location", json!({"path": "relative/dir"}));
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(
        !sb.ssh_marker.exists(),
        "ssh was spawned: {}",
        std::fs::read_to_string(&sb.ssh_marker).unwrap_or_default()
    );
}

// ------------------------------------------------------------- plans dir

#[test]
fn mcp_plan_goes_to_the_fixed_plans_dir_and_never_overwrites() {
    let sb = Sandbox::new("plans");
    let tree = sb.fixture("tree");
    let mut m = sb.spawn(&[]);
    m.handshake("2025-06-18");
    let add = m.ok("add_location", json!({"path": tree.to_str().unwrap()}));
    m.wait_job(add["job_id"].as_str().unwrap(), "done");

    let policy = json!({"min_copies": 1, "distinct": "none"});
    let p1 = m.ok("plan_dedup", policy.clone());
    let path1 = PathBuf::from(p1["plan_path"].as_str().expect("plan_path"));
    let plans = sb.state.join("diskghost/plans");
    assert_eq!(path1.parent().unwrap(), plans, "{path1:?}");
    let fname = path1.file_name().unwrap().to_string_lossy().to_string();
    let ok_name = fname.starts_with("plan-")
        && fname.ends_with(".json")
        && fname.len() == "plan-20260101T000000Z-0123abcd.json".len()
        && fname[5..13].chars().all(|c| c.is_ascii_digit())
        && fname[13..14] == *"T"
        && fname[20..21] == *"Z";
    assert!(ok_name, "unexpected plan file name {fname}");
    assert_eq!(
        p1["next_step"],
        format!("diskghost apply {} --dry-run", path1.display())
    );
    let plan: Value = serde_json::from_str(&std::fs::read_to_string(&path1).unwrap()).unwrap();
    assert_eq!(plan["version"], 1);
    assert!(plan["summary"]["actions"].as_u64().unwrap() >= 2, "{plan}");
    assert_eq!(p1["summary"], plan["summary"]);

    // A second plan is a new file; the first is byte-identical afterwards.
    let before = std::fs::read(&path1).unwrap();
    let p2 = m.ok("plan_dedup", policy);
    let path2 = PathBuf::from(p2["plan_path"].as_str().unwrap());
    assert_ne!(path1, path2);
    assert_eq!(std::fs::read(&path1).unwrap(), before);
    let mut listed: Vec<_> = std::fs::read_dir(&plans)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    listed.sort();
    assert_eq!(listed.len(), 2, "{listed:?}");

    // The fixture is untouched (no quarantine folder, same files).
    let mut names: Vec<_> = std::fs::read_dir(&tree)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["a", "b"]);
}

// ----------------------------------------------- one writer, honest busy

#[test]
fn mcp_writer_jobs_are_serialized_and_queued() {
    let sb = Sandbox::new("serial");
    let t1 = sb.fixture("one");
    let t2 = sb.fixture("two");
    // The test-only hold keeps the first writer inside its turn long
    // enough to observe the second one queued behind it.
    let mut m = sb.spawn(&[("DISKGHOST_MCP_JOB_HOLD_MS", "1500")]);
    m.handshake("2025-06-18");
    let a = m.ok("add_location", json!({"path": t1.to_str().unwrap()}));
    let b = m.ok("add_location", json!({"path": t2.to_str().unwrap()}));
    let (ja, jb) = (a["job_id"].as_str().unwrap(), b["job_id"].as_str().unwrap());
    assert_ne!(ja, jb);
    let t = Instant::now();
    let mut saw_queue = false;
    loop {
        let sa = m.ok("job_status", json!({"job_id": ja}));
        let sb_ = m.ok("job_status", json!({"job_id": jb}));
        let running = |s: &Value| s["state"] == "running";
        assert!(
            !(running(&sa) && running(&sb_)),
            "two writer jobs running at once: {sa} / {sb_}"
        );
        if running(&sa) && sb_["state"] == "queued" {
            saw_queue = true;
        }
        if sa["state"] == "done" && sb_["state"] == "done" {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(60), "stuck: {sa} / {sb_}");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(
        saw_queue,
        "never saw the second job queued behind the first"
    );
    let jobs = m.ok("list_jobs", json!({}));
    assert_eq!(jobs["jobs"].as_array().unwrap().len(), 2);
}

#[test]
fn mcp_cancel_job_ends_cancelled() {
    let sb = Sandbox::new("cancel");
    let t1 = sb.fixture("one");
    let t2 = sb.fixture("two");
    let mut m = sb.spawn(&[("DISKGHOST_MCP_JOB_HOLD_MS", "30000")]);
    m.handshake("2025-06-18");
    let a = m.ok("add_location", json!({"path": t1.to_str().unwrap()}));
    let b = m.ok("add_location", json!({"path": t2.to_str().unwrap()}));
    let (ja, jb) = (a["job_id"].as_str().unwrap(), b["job_id"].as_str().unwrap());
    // Cancelling the queued one first, then the running one.
    let c = m.ok("cancel_job", json!({"job_id": jb}));
    assert_eq!(c["cancel_requested"], json!(true));
    m.wait_job(jb, "cancelled");
    m.ok("cancel_job", json!({"job_id": ja}));
    m.wait_job(ja, "cancelled");
    let r = m.call("cancel_job", json!({"job_id": "job-nope"}));
    assert_eq!(r["isError"], json!(true));
}

#[test]
fn mcp_index_busy_is_honest_when_another_process_holds_the_lock() {
    let sb = Sandbox::new("busy");
    let tree = sb.fixture("tree");
    let mut m = sb.spawn(&[("DISKGHOST_MCP_BUSY_MS", "300")]);
    m.handshake("2025-06-18");
    // Seed the scratch index first, through the server itself.
    let add = m.ok("add_location", json!({"path": tree.to_str().unwrap()}));
    m.wait_job(add["job_id"].as_str().unwrap(), "done");

    // Another process (as `diskghost index add` is for #1366) takes the
    // write lock and keeps it.
    let other = rusqlite::Connection::open(&sb.db).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();

    let started = Instant::now();
    for (tool, args) in [
        ("add_location", json!({"path": tree.to_str().unwrap()})),
        ("dupes_report", json!({"min_size_bytes": 1})),
        ("plan_dedup", json!({"min_copies": 1, "distinct": "none"})),
    ] {
        let r = m.call(tool, args);
        assert_eq!(r["isError"], json!(true), "{tool} must not succeed: {r}");
        let e = &r["structuredContent"]["error"];
        assert_eq!(e["code"], "index_busy", "{tool}: {r}");
        assert_eq!(e["db"], sb.db.to_str().unwrap(), "{tool}: {r}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "busy must be bounded, took {:?}",
        started.elapsed()
    );
    // Reads are not blocked by the other writer.
    let locs = m.ok("list_locations", json!({}));
    assert_eq!(locs["locations"].as_array().unwrap().len(), 1);

    // Lock released → the same call works.
    other.execute_batch("ROLLBACK").unwrap();
    drop(other);
    let again = m.ok("add_location", json!({"path": tree.to_str().unwrap()}));
    m.wait_job(again["job_id"].as_str().unwrap(), "done");
}
