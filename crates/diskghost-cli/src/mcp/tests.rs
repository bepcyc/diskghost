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
