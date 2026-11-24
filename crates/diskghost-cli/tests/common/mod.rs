//! Shared harness for the MCP integration tests: a sandboxed `diskghost mcp`
//! child and a line-oriented JSON-RPC client.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub fn bin() -> String {
    std::env::var("DISKGHOST_E2E_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_diskghost").to_string())
}

/// The only tools the server may ever offer. Adding one must fail a test.
pub const ALLOWLIST: [&str; 15] = [
    "add_location",
    "cancel_job",
    "diff_since",
    "dupes_report",
    "find_files",
    "forget_location",
    "index_status",
    "job_status",
    "list_drives",
    "list_jobs",
    "list_locations",
    "list_snapshots",
    "plan_dedup",
    "set_location_role",
    "start_scan",
];

/// A throwaway directory unique to this test process and label.
pub fn scratch(label: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("diskghost-mcp-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The sandbox one server lives in.
pub struct Sandbox {
    pub root: PathBuf,
    pub db: PathBuf,
    pub state: PathBuf,
    /// A fake `ssh` first on PATH: records every call it gets.
    pub ssh_marker: PathBuf,
}

impl Sandbox {
    pub fn new(label: &str) -> Sandbox {
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

    /// A server with a fake `ssh` first on PATH and HOME inside the sandbox.
    pub fn spawn(&self, extra_env: &[(&str, &str)]) -> Mcp {
        self.spawn_as(false, extra_env)
    }

    /// A server that keeps the real HOME and PATH (real ssh, real known
    /// hosts) — only the index (`--db`), XDG data and state stay in the
    /// sandbox. For the end-to-end run against a real host.
    pub fn spawn_real(&self, extra_env: &[(&str, &str)]) -> Mcp {
        self.spawn_as(true, extra_env)
    }

    fn spawn_as(&self, real: bool, extra_env: &[(&str, &str)]) -> Mcp {
        let mut cmd = Command::new(bin());
        cmd.arg("mcp")
            .arg("--db")
            .arg(&self.db)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", &self.state)
            .env_remove("DISKGHOST_MCP_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if !real {
            let path = format!(
                "{}:{}",
                self.root.join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            );
            cmd.env("HOME", self.root.join("home")).env("PATH", path);
        }
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
    pub fn fixture(&self, name: &str) -> PathBuf {
        let t = self.root.join(name);
        std::fs::create_dir_all(t.join("a")).unwrap();
        std::fs::create_dir_all(t.join("b")).unwrap();
        let big = vec![7u8; 64 * 1024];
        std::fs::write(t.join("a/one.bin"), &big).unwrap();
        std::fs::write(t.join("b/two.bin"), &big).unwrap();
        std::fs::write(t.join("b/three.bin"), &big).unwrap();
        std::fs::write(t.join("a/unique.bin"), vec![9u8; 70 * 1024]).unwrap();
        // Written an hour ago, as files usually are: a rescan right after
        // the first scan must find them unchanged, not racy (written within
        // the index's 2 s trust window of the scan) and re-read.
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for f in ["a/one.bin", "b/two.bin", "b/three.bin", "a/unique.bin"] {
            std::fs::File::options()
                .write(true)
                .open(t.join(f))
                .unwrap()
                .set_modified(hour_ago)
                .unwrap();
        }
        t
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A running server and the pipe to talk to it.
pub struct Mcp {
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
    pub fn send(&mut self, v: &Value) {
        let mut line = serde_json::to_string(v).unwrap();
        assert!(!line.contains('\n'));
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    /// The next frame the server wrote; stdout must carry JSON frames only.
    pub fn recv(&mut self) -> Value {
        match self.rx.recv_timeout(Duration::from_secs(30)) {
            Ok(l) => serde_json::from_str(&l)
                .unwrap_or_else(|e| panic!("stdout line is not a JSON frame ({e}): {l:?}")),
            Err(RecvTimeoutError::Timeout) => panic!("server silent for 30 s"),
            Err(RecvTimeoutError::Disconnected) => panic!("server closed stdout"),
        }
    }

    /// Send a request, return the response with its id (skipping
    /// notifications the server may interleave).
    pub fn request(&mut self, method: &str, params: Value) -> Value {
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

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub fn handshake(&mut self, version: &str) -> Value {
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

    /// Kill the server hard (SIGKILL): it cannot clean up — the way a dead
    /// job writer looks to the shared registry.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// `tools/call`: the result object.
    pub fn call(&mut self, name: &str, args: Value) -> Value {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        assert!(r.get("error").is_none(), "{name}: protocol error {r}");
        r["result"].clone()
    }

    /// A call that must succeed: its structured content.
    pub fn ok(&mut self, name: &str, args: Value) -> Value {
        let r = self.call(name, args);
        assert_ne!(r["isError"], json!(true), "{name} failed: {r}");
        assert_eq!(
            serde_json::from_str::<Value>(r["content"][0]["text"].as_str().unwrap()).unwrap(),
            r["structuredContent"],
            "{name}: text content must mirror structuredContent"
        );
        r["structuredContent"].clone()
    }

    pub fn wait_job(&mut self, id: &str, want: &str) -> Value {
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
