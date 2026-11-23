//! End-to-end: a scripted stdio client drives `diskghost mcp` through EVERY
//! tool against a scratch index (`--db`, never the owner's default one).
//!
//! * Local part: always runs — a tempdir fixture with known duplicates.
//! * ssh part: runs when `DISKGHOST_MCP_E2E_SSH=<host>` names a reachable
//!   host (a real ssh location). A fixture `/tmp/dg-mcp-e2e-<pid>` is created
//!   there and removed again by a guard, whatever happens.
//!
//! The "untouched" proof: before and after the whole run — `plan_dedup`
//! included — a manifest (type, path, size, mtime, inode, sha256) of the
//! local fixture and of the remote fixture is taken with the same script
//! and must be byte-identical. The only new path a run may create is the
//! agent upload in the remote `~/.cache/diskghost/`, listed at the end.
//!
//! Run (release binary, so the agent that gets uploaded is the real size):
//!   DISKGHOST_E2E_BIN=target/release/diskghost DISKGHOST_MCP_E2E_SSH=AIAssistant \
//!   cargo test -p diskghost-cli --test mcp_e2e -- --nocapture

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

/// Every entry of a directory: type, path, size, mtime (ns), inode and, for
/// files, the sha256 — the same script runs locally and over ssh.
const MANIFEST: &str = r#"cd "$1" && find . -printf '%y\t%p\t%s\t%T@\t%i\n' | LC_ALL=C sort | while IFS="$(printf '\t')" read -r y p s t i; do
  if [ "$y" = f ]; then h=$(sha256sum -- "$p" | cut -d' ' -f1); else h=-; fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$y" "$p" "$s" "$t" "$i" "$h"
done"#;

fn local_manifest(dir: &Path) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(MANIFEST)
        .arg("sh")
        .arg(dir)
        .output()
        .expect("manifest script");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A host reached with the real ssh (BatchMode: no prompts).
struct Remote {
    host: String,
    dir: String,
}

impl Remote {
    fn sh(&self, script: &str, args: &[&str]) -> String {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ClearAllForwardings=yes",
        ])
        .arg(&self.host)
        .arg("sh -s --")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("ssh");
        {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "ssh {}: {}",
            self.host,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn manifest(&self) -> String {
        self.sh(MANIFEST, &[&self.dir])
    }

    fn cache_listing(&self) -> String {
        self.sh(
            "ls -la --time-style=+%F_%T \"$HOME/.cache/diskghost\" 2>&1 | sed 1d",
            &[],
        )
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        // Only ever the directory this run created.
        if self.dir.starts_with("/tmp/dg-mcp-e2e-") && !self.dir.contains("..") {
            let _ = self.sh("rm -rf -- \"$1\"", &[&self.dir]);
        }
    }
}

/// Poll a job to a final state, returning every distinct (state, phase,
/// files) sample seen on the way — the progression to quote.
fn follow(m: &mut Mcp, id: &str) -> (Value, Vec<String>) {
    let t = Instant::now();
    let mut seen: Vec<String> = Vec::new();
    loop {
        let s = m.ok("job_status", json!({"job_id": id}));
        let line = format!(
            "state={} phase={} files={} bytes={}",
            s["state"].as_str().unwrap(),
            s["phase"].as_str().unwrap(),
            s["files"],
            s["bytes"]
        );
        if seen.last() != Some(&line) {
            seen.push(line);
        }
        if matches!(s["state"].as_str(), Some("done" | "failed" | "cancelled")) {
            return (s, seen);
        }
        assert!(
            t.elapsed() < Duration::from_secs(300),
            "job {id} stuck: {s}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap()
}

#[test]
fn mcp_e2e_every_tool_leaves_the_files_untouched() {
    let sb = Sandbox::new("e2e");
    let local = sb.fixture("local-fixture");
    let ssh_host = std::env::var("DISKGHOST_MCP_E2E_SSH")
        .ok()
        .filter(|h| !h.is_empty());

    // The remote fixture: the same 64 KiB content as the local duplicates
    // (cross-host copies) plus a duplicate pair of its own.
    let remote = ssh_host.as_ref().map(|h| {
        let r = Remote {
            host: h.clone(),
            dir: format!("/tmp/dg-mcp-e2e-{}", std::process::id()),
        };
        r.sh(
            r#"set -e
mkdir -p "$1/x" "$1/y"
head -c 65536 /dev/zero | tr '\0' '\007' > "$1/x/copy-of-local.bin"
head -c 65536 /dev/zero | tr '\0' '\007' > "$1/y/another-copy.bin"
head -c 90000 /dev/zero | tr '\0' '\021' > "$1/x/remote-only-1.bin"
head -c 90000 /dev/zero | tr '\0' '\021' > "$1/y/remote-only-2.bin"
head -c 5000 /dev/zero | tr '\0' '\033' > "$1/y/unique.bin""#,
            &[&r.dir],
        );
        r
    });

    // ---- manifests BEFORE
    let local_before = local_manifest(&local);
    let remote_before = remote.as_ref().map(Remote::manifest);
    let cache_before = remote.as_ref().map(Remote::cache_listing);
    if let Some(c) = &cache_before {
        println!("--- remote ~/.cache/diskghost BEFORE:\n{c}");
    }

    let mut m = sb.spawn_real(&[]);
    let init = m.handshake("2025-11-25");
    println!(
        "--- initialize -> protocolVersion {}",
        init["result"]["protocolVersion"]
    );

    // ---- list_drives / list_locations
    let drives = m.ok("list_drives", json!({}));
    let list = drives["drives"].as_array().expect("drives");
    assert!(!list.is_empty(), "no local drives listed: {drives}");
    for d in list {
        assert!(d["mount_point"].is_string() && d["fstype"].is_string());
        assert!(d["total_bytes"].is_u64() && d["free_bytes"].is_u64());
        assert!(matches!(d["media"].as_str(), Some("ssd" | "hdd")), "{d}");
        assert!(d["cow"].is_boolean());
    }
    println!(
        "--- list_drives: {} drive(s); first: {}",
        list.len(),
        pretty(&list[0])
    );
    let empty = m.ok("list_locations", json!({}));
    assert_eq!(empty["locations"], json!([]));

    // ---- add_location (local): job_status progression
    let add = m.ok(
        "add_location",
        json!({"path": local.to_str().unwrap(), "role": "normal"}),
    );
    println!("--- add_location(local) -> {}", pretty(&add));
    let (done, prog) = follow(&mut m, add["job_id"].as_str().unwrap());
    assert_eq!(done["state"], "done", "{done}");
    println!("--- job_status progression (local scan): {prog:#?}");
    let s0 = &done["result"]["summaries"][0];
    assert_eq!(s0["seen"], 4, "{done}");
    assert_eq!(s0["new"], 4);
    let local_uuid = s0["uuid"].as_str().unwrap().to_string();

    // ---- add_location (ssh): transport / agent_note verbatim
    let mut remote_uuid = None;
    // Whether the remote digests are BLAKE3 (agent tier, or b3sum on the
    // host): only then can they group with local files.
    let mut remote_blake3 = false;
    if let Some(r) = &remote {
        let url = format!("ssh://{}/{}", r.host, r.dir); // `host//abs/path`
        let add = m.ok("add_location", json!({"path": url, "role": "scratch"}));
        println!("--- add_location(ssh) -> {}", pretty(&add));
        assert!(add["notes"][0]
            .as_str()
            .unwrap()
            .contains("~/.cache/diskghost/"));
        let (done, prog) = follow(&mut m, add["job_id"].as_str().unwrap());
        println!("--- job_status progression (ssh scan): {prog:#?}");
        assert_eq!(done["state"], "done", "{done}");
        let s = &done["result"]["summaries"][0];
        println!(
            "--- ssh scan summary: transport={} agent_note={} algo={} seen={} hashed={}",
            s["transport"], s["agent_note"], s["algo"], s["seen"], s["hashed"]
        );
        assert_eq!(s["seen"], 5, "{done}");
        assert!(matches!(s["transport"].as_str(), Some("agent" | "shell")));
        remote_blake3 = s["algo"] == "blake3";
        remote_uuid = Some(s["uuid"].as_str().unwrap().to_string());
    }

    // ---- list_locations / set_location_role
    let locs = m.ok("list_locations", json!({}));
    println!("--- list_locations: {}", pretty(&locs));
    assert_eq!(
        locs["locations"].as_array().unwrap().len(),
        if remote.is_some() { 2 } else { 1 }
    );
    let r = m.ok(
        "set_location_role",
        json!({"location": local_uuid, "role": "master"}),
    );
    assert_eq!(r["role"], "master");
    let locs = m.ok("list_locations", json!({"probe_remote": false}));
    let me = locs["locations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["uuid"] == json!(local_uuid))
        .unwrap();
    assert_eq!(me["role"], "master");
    let bad = m.call(
        "set_location_role",
        json!({"location": "no-such", "role": "master"}),
    );
    assert_eq!(
        bad["structuredContent"]["error"]["code"],
        "unknown_location"
    );

    // ---- start_scan: by location, by path (unchanged files are not re-read)
    let ss = m.ok("start_scan", json!({"location": local_uuid}));
    let (done, _) = follow(&mut m, ss["job_id"].as_str().unwrap());
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["result"]["summaries"][0]["unchanged"], 4, "{done}");
    let ss = m.ok(
        "start_scan",
        json!({"path": local.join("a").to_str().unwrap(), "exclude": ["*.tmp"]}),
    );
    let (done, _) = follow(&mut m, ss["job_id"].as_str().unwrap());
    assert_eq!(done["state"], "done", "{done}");
    let nope = m.call("start_scan", json!({"path": "/definitely/not/indexed"}));
    assert_eq!(nope["isError"], json!(true));

    // ---- dupes_report: groups across both hosts; byte-identical re-runs
    let args = json!({"min_size_bytes": 1});
    let first = m.ok("dupes_report", args.clone());
    let second = m.call("dupes_report", args.clone());
    let third = m.call("dupes_report", args.clone());
    assert_eq!(
        second["content"][0]["text"], third["content"][0]["text"],
        "same index state must give byte-identical output"
    );
    let _ = first;
    let rep = &second["structuredContent"];
    println!("--- dupes_report: {}", pretty(rep));
    let groups = rep["groups"].as_array().unwrap();
    assert!(!groups.is_empty());
    let reclaim: Vec<u64> = groups
        .iter()
        .map(|g| g["reclaimable_bytes"].as_u64().unwrap())
        .collect();
    assert!(
        reclaim.windows(2).all(|w| w[0] >= w[1]),
        "not sorted: {reclaim:?}"
    );
    let local_group = groups
        .iter()
        .find(|g| g["size"] == 65536)
        .expect("64 KiB group");
    let want = if remote_blake3 { 5 } else { 3 };
    assert_eq!(local_group["unique_copies"], want, "{local_group}");
    if remote_blake3 {
        let hosts: std::collections::BTreeSet<_> = local_group["copies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["location_uuid"].as_str().unwrap())
            .collect();
        assert_eq!(
            hosts.len(),
            2,
            "copies must span both locations: {local_group}"
        );
    } else if remote.is_some() {
        // Honest: MD5 from a host without b3sum never groups with BLAKE3.
        assert!(rep["unverifiable_total"].as_u64().unwrap() >= 1, "{rep}");
    }
    // Paging and the location filter.
    let page = m.ok(
        "dupes_report",
        json!({"min_size_bytes": 1, "limit": 1, "offset": 0}),
    );
    assert_eq!(page["groups"].as_array().unwrap().len(), 1);
    assert_eq!(page["total_groups"], rep["total_groups"]);
    let filtered = m.ok(
        "dupes_report",
        json!({"min_size_bytes": 1, "location": local_uuid}),
    );
    assert!(filtered["total_groups"].as_u64().unwrap() >= 1);

    // ---- plan_dedup: the user's stated preference as a policy
    let mut rules = vec![json!({"match": local_uuid, "role": "master"})];
    if let Some(u) = &remote_uuid {
        rules.push(json!({"match": u, "role": "scratch"}));
    }
    let plan = m.ok(
        "plan_dedup",
        json!({"min_copies": 1, "distinct": "none", "locations": rules, "groups_limit": 10}),
    );
    println!("--- plan_dedup: {}", pretty(&plan));
    let plan_path = PathBuf::from(plan["plan_path"].as_str().unwrap());
    assert!(plan_path.starts_with(sb.state.join("diskghost/plans")));
    assert_eq!(
        plan["next_step"],
        format!("diskghost apply {} --dry-run", plan_path.display())
    );
    assert!(plan_path.is_file());
    println!("--- plan_path: {}", plan_path.display());

    // The plan only reads; applying it stays a manual CLI step. A dry run
    // by hand shows what the owner would be asked to approve.
    let dry = Command::new(bin())
        .args(["apply", "--db"])
        .arg(&sb.db)
        .arg(&plan_path)
        .arg("--dry-run")
        .output()
        .unwrap();
    println!(
        "--- manual `diskghost apply --dry-run` (by the test, not by the MCP): {}",
        String::from_utf8_lossy(&dry.stdout)
            .lines()
            .last()
            .unwrap_or("")
    );

    // ---- cancel_job on a real running remote listing (or a queued job)
    if let Some(r) = &remote {
        let url = format!("ssh://{}//usr", r.host); // read-only listing of a big tree
        let a = m.ok(
            "add_location",
            json!({"path": url, "role": "scratch", "agent": "auto"}),
        );
        let id = a["job_id"].as_str().unwrap().to_string();
        let t = Instant::now();
        loop {
            let s = m.ok("job_status", json!({"job_id": id}));
            if s["state"] == "running" && s["files"].as_u64().unwrap_or(0) > 0 {
                println!("--- before cancel: {}", s);
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(120), "never started: {s}");
            std::thread::sleep(Duration::from_millis(50));
        }
        let cancelled_at = Instant::now();
        let c = m.ok("cancel_job", json!({"job_id": id}));
        println!("--- cancel_job -> {c}");
        assert_eq!(c["cancel_requested"], json!(true));
        let (fin, _) = follow(&mut m, &id);
        println!(
            "--- after cancel ({} ms later): state={} files={}",
            cancelled_at.elapsed().as_millis(),
            fin["state"],
            fin["files"]
        );
        assert_eq!(fin["state"], "cancelled", "{fin}");
        // That location is not part of the fixture: forget nothing (no such
        // tool) — drop the scratch DB with the sandbox at the end.
    }
    let jobs = m.ok("list_jobs", json!({}));
    assert!(jobs["jobs"].as_array().unwrap().len() >= 3);

    // ---- manifests AFTER: identical
    drop(m);
    let local_after = local_manifest(&local);
    assert_eq!(local_before, local_after, "local fixture changed");
    println!(
        "--- local manifest diff: (empty)  [{} entries]",
        local_before.lines().count()
    );
    if let Some(r) = &remote {
        let after = r.manifest();
        assert_eq!(
            remote_before.as_deref(),
            Some(after.as_str()),
            "remote fixture changed"
        );
        println!(
            "--- remote manifest diff: (empty)  [{} entries]",
            after.lines().count()
        );
        let cache_after = r.cache_listing();
        println!("--- remote ~/.cache/diskghost AFTER:\n{cache_after}");
        let before = cache_before.unwrap();
        // The directory's own line changes with its mtime; only entries count.
        let new: Vec<&str> = cache_after
            .lines()
            .filter(|l| !l.ends_with(" .") && !before.contains(l))
            .collect();
        println!("--- new in ~/.cache/diskghost: {new:?}");
        assert!(
            new.iter().all(|l| l.contains("diskghost-agent-")),
            "anything but the agent upload appeared: {new:?}"
        );
    }
}
