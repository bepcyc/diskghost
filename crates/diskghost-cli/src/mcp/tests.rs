//! Unit tests of the MCP module's own helpers.

use super::tools::{create_plan_file, plans_dir, utc_stamp};

fn scratch(label: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("diskghost-mcp-unit-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn utc_stamp_is_the_compact_iso_time() {
    assert_eq!(utc_stamp(0), "19700101T000000Z");
    assert_eq!(utc_stamp(1_700_000_000), "20231114T221320Z");
    assert_eq!(utc_stamp(951_782_400), "20000229T000000Z"); // leap day
}

#[test]
fn a_taken_plan_name_is_never_overwritten() {
    let d = scratch("create-new");
    let dir = d.join("plans");
    let mut seq = [1u32, 1, 2].into_iter();
    let mut rand = || seq.next().unwrap();
    let first = create_plan_file(&dir, "first\n", "20260101T000000Z", &mut rand).unwrap();
    assert!(first.ends_with("plan-20260101T000000Z-00000001.json"));
    // The next draw collides with `first`; the file must survive untouched
    // and the new plan land under the following suffix.
    let second = create_plan_file(&dir, "second\n", "20260101T000000Z", &mut rand).unwrap();
    assert!(second.ends_with("plan-20260101T000000Z-00000002.json"));
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "first\n");
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "second\n");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_earlier_applys_journal_blocks_the_plan_name() {
    let d = scratch("journal");
    let dir = d.join("plans");
    std::fs::create_dir_all(&dir).unwrap();
    let name = dir.join("plan-20260101T000000Z-0000000a.json");
    std::fs::write(diskghost_core::journal_path(&name), "{}\n").unwrap();
    let mut rand = || 0xa_u32;
    let err = create_plan_file(&dir, "x\n", "20260101T000000Z", &mut rand).unwrap_err();
    assert!(err.contains("earlier apply"), "{err}");
    assert!(!name.exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn plans_dir_is_under_the_state_home() {
    // Only checks the shape; the integration tests pin XDG_STATE_HOME.
    if let Some(p) = plans_dir() {
        assert!(p.ends_with("diskghost/plans"), "{p:?}");
    }
}

#[test]
fn an_sshfs_root_is_a_structured_error_with_its_url() {
    use diskghost_core::sshfs::SshfsRoot;
    let abs = SshfsRoot {
        path: "/mnt/nas/films".into(),
        source: "nas:/srv/media".into(),
        url: Some("ssh://nas//srv/media/films".into()),
        why: None,
    };
    let j = super::ToolError::sshfs(&abs).to_json();
    assert_eq!(j["code"], "sshfs_root");
    assert_eq!(j["url"], "ssh://nas//srv/media/films");
    let msg = j["message"].as_str().unwrap();
    assert!(
        msg.contains("allow_sshfs=true") && !msg.contains("--allow-sshfs"),
        "{msg}"
    );
    assert!(!msg.contains("relative to the remote home"), "{msg}");

    // A home-relative URL: add_location takes only absolute remote paths.
    let rel = SshfsRoot {
        url: Some("ssh://nas/media".into()),
        ..abs.clone()
    };
    let msg = super::ToolError::sshfs(&rel).message;
    assert!(msg.contains("relative to the remote home"), "{msg}");

    // No URL (IPv6): no url field, the hint is in the message.
    let v6 = SshfsRoot {
        url: None,
        why: Some("add a Host alias".into()),
        ..abs.clone()
    };
    let j = super::ToolError::sshfs(&v6).to_json();
    assert!(j.get("url").is_none());
    assert!(j["message"].as_str().unwrap().contains("add a Host alias"));

    // The same error out of a scan job is classified the same way.
    let io = std::io::Error::new(std::io::ErrorKind::InvalidInput, abs);
    let e = super::ToolError::from_io(std::path::Path::new("/db"), &io);
    assert_eq!(
        (e.code, e.url.as_deref()),
        ("sshfs_root", Some("ssh://nas//srv/media/films"))
    );
}
