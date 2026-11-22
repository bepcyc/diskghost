//! The copy policy a plan is made under: how many copies of each content
//! must stay, on how many physical devices, and which locations are
//! `master` (never touched) or `scratch` (emptied first).
//!
//! ```toml
//! [policy]
//! min_copies = 2
//! distinct = "physical_device"   # or "none"
//!
//! [[location]]
//! match = "nas:/archive"         # uuid, host:/path prefix, or /path prefix
//! role = "master"
//!
//! [[location]]
//! match = "/mnt/usb3"
//! role = "scratch"
//! ```
//!
//! A location's role: the best-matching `[[location]]` entry, else the role
//! stored in the index, else `normal`. An exact uuid beats any path; among
//! paths the longest (most components) wins; on a tie the earlier entry.
//! Paths match whole components (`/mnt/usb3` never matches `/mnt/usb30`)
//! against the location's root (its mountpoint).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::index::Role;

/// Whether kept copies must sit on different physical devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Distinct {
    /// Kept copies count once per filesystem (volume uuid, bind mounts and
    /// subvolumes folded into their filesystem).
    PhysicalDevice,
    /// Any copies count.
    None,
}

/// One `[[location]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationRule {
    #[serde(rename = "match")]
    pub matches: String,
    pub role: Role,
}

/// A parsed policy file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub min_copies: u64,
    pub distinct: Distinct,
    #[serde(default)]
    pub locations: Vec<LocationRule>,
}

impl Default for Policy {
    /// Without a policy file: two copies on two physical devices — the
    /// strictest reading, so a plan made without a file keeps more.
    fn default() -> Policy {
        Policy {
            min_copies: 2,
            distinct: Distinct::PhysicalDevice,
            locations: Vec::new(),
        }
    }
}

impl Policy {
    /// Parse a policy file's text. Errors name the offending field.
    pub fn parse(_text: &str) -> Result<Policy, String> {
        Err("policy parsing not implemented".into())
    }

    /// Read and parse a policy file.
    pub fn load(path: &Path) -> Result<Policy, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read policy {}: {e}", path.display()))?;
        Policy::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The role of a location: policy entry > `stored` (the index's role).
    pub fn role_for(&self, _uuid: &str, _host: &str, _root: &Path, stored: Role) -> Role {
        stored
    }

    /// Entries that match none of `locations` (uuid, host, root) — a
    /// `master` entry that silently matches nothing protects nothing.
    pub fn unmatched(&self, _locations: &[(&str, &str, &Path)]) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[policy]
min_copies = 2
distinct = "physical_device"   # copies must sit on distinct volumes

[[location]]
match = "nas:/archive"
role = "master"

[[location]]
match = "/mnt/usb3"
role = "scratch"
"#;

    #[test]
    fn the_documented_example_parses() {
        let p = Policy::parse(EXAMPLE).unwrap();
        assert_eq!(p.min_copies, 2);
        assert_eq!(p.distinct, Distinct::PhysicalDevice);
        assert_eq!(
            p.locations,
            vec![
                LocationRule {
                    matches: "nas:/archive".into(),
                    role: Role::Master
                },
                LocationRule {
                    matches: "/mnt/usb3".into(),
                    role: Role::Scratch
                },
            ]
        );
        let none = Policy::parse("[policy]\nmin_copies = 1\ndistinct = \"none\"\n").unwrap();
        assert_eq!((none.min_copies, none.distinct), (1, Distinct::None));
    }

    #[test]
    fn missing_fields_take_the_strict_defaults() {
        assert_eq!(Policy::parse("").unwrap(), Policy::default());
        let p = Policy::parse("[policy]\nmin_copies = 3\n").unwrap();
        assert_eq!((p.min_copies, p.distinct), (3, Distinct::PhysicalDevice));
    }

    fn err(text: &str) -> String {
        Policy::parse(text).expect_err(text)
    }

    #[test]
    fn an_unknown_distinct_value_is_an_error_naming_the_field() {
        let e = err("[policy]\ndistinct = \"physical\"\n");
        assert!(e.contains("distinct") && e.contains("physical"), "{e}");
    }

    #[test]
    fn errors_name_the_field() {
        let e = err("[policy]\nmin_copies = \"two\"\n");
        assert!(e.contains("min_copies"), "{e}");
        let e = err("[policy]\nmin_copies = 0\n");
        assert!(e.contains("min_copies"), "{e}");
        let e = err("[policy]\nmin_copies = -1\n");
        assert!(e.contains("min_copies"), "{e}");
        let e = err("[policy]\nmin_copy = 2\n");
        assert!(e.contains("min_copy"), "{e}");
        let e = err("[[location]]\nmatch = \"/x\"\nrole = \"boss\"\n");
        assert!(e.contains("role") && e.contains("boss"), "{e}");
        let e = err("[[location]]\nrole = \"master\"\n");
        assert!(e.contains("match"), "{e}");
        let e = err("[[location]]\nmatch = \"\"\nrole = \"master\"\n");
        assert!(e.contains("match"), "{e}");
        let e = err("[[location]]\nmatch = \"relative/path\"\nrole = \"master\"\n");
        assert!(e.contains("match"), "{e}");
        let e = err("[polcy]\nmin_copies = 2\n");
        assert!(e.contains("polcy"), "{e}");
    }

    fn policy(rules: &[(&str, Role)]) -> Policy {
        Policy {
            locations: rules
                .iter()
                .map(|(m, r)| LocationRule {
                    matches: (*m).into(),
                    role: *r,
                })
                .collect(),
            ..Policy::default()
        }
    }

    #[test]
    fn policy_beats_the_stored_role_which_beats_normal() {
        let p = policy(&[("/mnt/usb3", Role::Scratch)]);
        let usb = Path::new("/mnt/usb3");
        assert_eq!(p.role_for("u1", "local", usb, Role::Master), Role::Scratch);
        let other = Path::new("/mnt/other");
        assert_eq!(p.role_for("u2", "local", other, Role::Master), Role::Master);
        assert_eq!(p.role_for("u2", "local", other, Role::Normal), Role::Normal);
    }

    #[test]
    fn paths_match_whole_components_of_the_root() {
        let p = policy(&[("/mnt/usb3", Role::Scratch)]);
        let n = Role::Normal;
        assert_eq!(p.role_for("u", "local", Path::new("/mnt/usb30"), n), n);
        assert_eq!(
            p.role_for("u", "local", Path::new("/mnt/usb3/sub"), n),
            Role::Scratch
        );
        assert_eq!(p.role_for("u", "local", Path::new("/mnt"), n), n);
        // A plain path is this machine's: a remote location never matches.
        assert_eq!(p.role_for("u", "nas", Path::new("/mnt/usb3"), n), n);
    }

    #[test]
    fn uuid_beats_paths_and_the_longest_path_wins() {
        let p = policy(&[
            ("/", Role::Scratch),
            ("/data/archive", Role::Master),
            ("/data", Role::Normal),
            ("1234-ABCD", Role::Scratch),
        ]);
        let n = Role::Normal;
        let archive = Path::new("/data/archive");
        assert_eq!(p.role_for("x", "local", archive, n), Role::Master);
        assert_eq!(p.role_for("1234-ABCD", "local", archive, n), Role::Scratch);
        assert_eq!(
            p.role_for("x", "local", Path::new("/data/b"), Role::Master),
            Role::Normal
        );
        assert_eq!(
            p.role_for("x", "local", Path::new("/home"), n),
            Role::Scratch
        );
        // A uuid with colons (bind mount, dev: fallback) is still a uuid.
        let q = policy(&[("dev:8:1", Role::Master), ("ABCD:/sub", Role::Scratch)]);
        assert_eq!(q.role_for("dev:8:1", "local", archive, n), Role::Master);
        assert_eq!(q.role_for("ABCD:/sub", "local", archive, n), Role::Scratch);
    }

    #[test]
    fn host_path_entries_match_only_that_host() {
        let p = policy(&[("nas:/archive", Role::Master)]);
        let n = Role::Normal;
        let a = Path::new("/archive");
        assert_eq!(p.role_for("u", "nas", a, n), Role::Master);
        assert_eq!(p.role_for("u", "local", a, n), n);
        // `localhost:` and `local:` name this machine.
        let q = policy(&[("localhost:/archive", Role::Master)]);
        assert_eq!(q.role_for("u", "local", a, n), Role::Master);
    }

    #[test]
    fn entries_matching_nothing_are_reported() {
        let p = policy(&[("/mnt/usb3", Role::Scratch), ("/srv", Role::Master)]);
        let locs = [("u", "local", Path::new("/srv"))];
        let un = p.unmatched(&locs);
        assert_eq!(un.len(), 1, "{un:?}");
        assert!(un[0].contains("/mnt/usb3"), "{un:?}");
    }
}
