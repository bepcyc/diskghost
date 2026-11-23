//! `plan`: which copies of each duplicate content to keep under a copy
//! policy, and which to quarantine — written as a JSON file a human can read
//! and edit before `apply` acts on it.
//!
//! # Rules
//!
//! For each group of confirmed copies (same size and whole-file hash; one
//! copy = one inode, all its names):
//!
//! - every copy on a `master` location is kept and never in `actions`;
//! - further copies are kept until `min(min_copies, unique_copies)` is
//!   reached — with `distinct = "physical_device"` counting one per
//!   filesystem — preferring a new device, then master > normal > scratch,
//!   then a reachable copy, then path order;
//! - when the policy cannot be met (fewer copies, or fewer devices, than it
//!   asks for) the group is `under_protected` and gets **no** actions: every
//!   copy stays;
//! - every other copy is quarantined — scratch first — only when all its
//!   names are on reachable, non-master locations; otherwise it stays
//!   (`why` says which rule kept it);
//! - a copy on a remote (ssh) location is marked `remote: true`. Among
//!   equal candidates a local copy is kept first (it can be verified and
//!   reached). A remote copy the policy does not need becomes an action
//!   with `remote: true` — listed so the user sees it, but `apply` never
//!   acts on a remote host and records it `remote_skipped`. Remote actions
//!   are counted in `summary.remote_actions`, not in `actions` or
//!   `quarantine_bytes`;
//! - a group of remote MD5 / SHA-1 digests is planned like any other
//!   (`algo` says which); all its copies are remote.
//! - copies only *probably* alike (offline, never hashed whole) are neither
//!   kept nor acted on: they do not count.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::index::{Index, Probe, Role};
use crate::index_report::{abs, compute_groups, filesystem_of, role_rank, Draft, Groups};
use crate::policy::{Distinct, Policy};
use crate::remote::hex;
use crate::Progress;

/// Plan file format version `apply` understands.
pub const PLAN_VERSION: u32 = 1;

/// A plan file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    /// When it was made, `YYYY-MM-DD HH:MM UTC`.
    pub created: String,
    /// The same instant, unix seconds.
    pub created_unix: u64,
    /// The policy it was made under (roles resolved with it again at apply).
    pub policy: Policy,
    /// Every location of the index as the plan saw it.
    pub locations: Vec<PlanLocation>,
    pub summary: PlanSummary,
    pub groups: Vec<PlanGroup>,
    /// Problems that did not stop the plan.
    pub warnings: Vec<String>,
}

/// A location as the plan saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanLocation {
    pub uuid: String,
    pub host: String,
    pub root: PathBuf,
    /// The role stored in the index.
    pub stored_role: Role,
    /// The role used: policy file > stored role.
    pub role: Role,
    pub available: bool,
}

/// Totals over the plan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub groups: u64,
    pub actions: u64,
    /// Sum of the sizes of the files `actions` would move (an upper bound
    /// on a copy-on-write filesystem).
    pub quarantine_bytes: u64,
    pub under_protected_groups: u64,
    /// Actions on remote locations: listed, never applied (`apply` records
    /// them `remote_skipped`); not in `actions` / `quarantine_bytes`.
    #[serde(default)]
    pub remote_actions: u64,
}

/// One content and what happens to its copies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanGroup {
    /// Whole-file digest, hex (`algo`).
    pub hash: String,
    /// `blake3`, or `md5` / `sha1` for a group of remote files.
    #[serde(default = "blake3_name")]
    pub algo: String,
    pub size: u64,
    /// Confirmed physical copies, offline included.
    pub unique_copies: u64,
    /// Physical devices among the kept copies.
    pub kept_devices: u64,
    /// The policy cannot be met: every copy stays.
    pub under_protected: bool,
    /// Copies that stay, and why.
    pub kept: Vec<KeptCopy>,
    /// What `apply` does, in order.
    pub actions: Vec<Action>,
}

/// A copy that stays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptCopy {
    pub location_uuid: String,
    pub path: PathBuf,
    /// Further names of the same inode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_inodes: Vec<PathBuf>,
    pub role: Role,
    pub available: bool,
    /// On a remote (ssh) location.
    #[serde(default)]
    pub remote: bool,
    pub why: KeepReason,
    /// What the copy looked like (reachable copies only): `apply
    /// --paranoid` compares a target against it byte by byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expect>,
}

/// Why a copy stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepReason {
    /// On a master location.
    Master,
    /// Chosen to meet `min_copies` / `distinct`.
    Policy,
    /// The group is under-protected: nothing in it is touched.
    UnderProtected,
    /// Not reachable now (offline volume) — never touched.
    Offline,
    /// A name of this inode is on an unreachable or master
    /// location: moving the other names would free nothing.
    NamesOutOfReach,
}

/// One step of `apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    /// Unique in the plan; the journal's key.
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ActionKind,
    pub location_uuid: String,
    pub role: Role,
    /// The file to move (absolute; on the remote host when `remote`).
    pub path: PathBuf,
    /// On a remote (ssh) location: `apply` never acts on it and records
    /// `remote_skipped`.
    #[serde(default)]
    pub remote: bool,
    /// What it must still be, or `apply` leaves it alone.
    pub expect: Expect,
}

/// What an action does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// `rename` into `<scan root>/.diskghost-quarantine/` (the longest
    /// folder `index add` was given that holds the file, else the volume's
    /// mountpoint) — same volume, instant, undone by renaming back. Never
    /// an unlink.
    Quarantine,
}

/// A file's identity when the plan was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expect {
    pub size: u64,
    pub mtime_ns: u64,
    pub dev: u64,
    pub ino: u64,
    /// BLAKE3 of the whole file, hex.
    pub full: String,
}

/// Make a plan from the index under `policy`, completing the hashes it
/// needs (and writing them back to the index, as `dupes --index` does).
pub fn make_plan(idx: &Index, policy: &Policy, progress: &Progress) -> io::Result<Plan> {
    plan_with(idx, policy, progress, &Probe::REAL)
}

/// [`make_plan`] asking `probe` instead of the machine.
pub(crate) fn plan_with(
    idx: &Index,
    policy: &Policy,
    progress: &Progress,
    probe: &Probe,
) -> io::Result<Plan> {
    let c = compute_groups(idx, 1, progress, probe)?;
    let roles: Vec<Role> = c
        .locs
        .iter()
        .map(|l| policy.role_for(&l.uuid, &l.host, &l.root, l.role))
        .collect();
    let mut warnings = c.warnings.clone();
    let named: Vec<(&str, &str, &Path)> = c
        .locs
        .iter()
        .map(|l| (l.uuid.as_str(), l.host.as_str(), l.root.as_path()))
        .collect();
    warnings.extend(policy.unmatched(&named));

    let mut groups: Vec<PlanGroup> = c
        .drafts
        .iter()
        .filter_map(|d| plan_group(&c, &roles, policy, d))
        .collect();
    let local = |g: &PlanGroup| g.actions.iter().filter(|a| !a.remote).count() as u64;
    let freed = |g: &PlanGroup| g.size.saturating_mul(local(g));
    groups.sort_by(|a, b| freed(b).cmp(&freed(a)).then_with(|| a.hash.cmp(&b.hash)));

    let summary = PlanSummary {
        groups: groups.len() as u64,
        actions: groups.iter().map(local).sum(),
        quarantine_bytes: groups.iter().map(freed).sum(),
        under_protected_groups: groups.iter().filter(|g| g.under_protected).count() as u64,
        remote_actions: groups
            .iter()
            .map(|g| g.actions.len() as u64 - local(g))
            .sum(),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Ok(Plan {
        version: PLAN_VERSION,
        created: crate::format_timestamp(now),
        created_unix: now,
        policy: policy.clone(),
        locations: c
            .locs
            .iter()
            .zip(&roles)
            .map(|(l, &role)| PlanLocation {
                uuid: l.uuid.clone(),
                host: l.host.clone(),
                root: l.root.clone(),
                stored_role: l.role,
                role,
                available: l.available,
            })
            .collect(),
        summary,
        groups,
        warnings,
    })
}

fn blake3_name() -> String {
    "blake3".into()
}

/// One confirmed physical copy as the plan weighs it.
struct Cand {
    p: usize,
    /// Best role among the locations of its names.
    role: Role,
    /// Its filesystem: the unit of `distinct = "physical_device"`.
    device: String,
    available: bool,
    /// On a remote (ssh) location.
    remote: bool,
    /// Why it may not be moved, if so.
    pinned: Option<KeepReason>,
    path: PathBuf,
}

fn plan_group(c: &Groups, roles: &[Role], policy: &Policy, d: &Draft) -> Option<PlanGroup> {
    let (hash, algo) = match (&d.full, &d.alt) {
        (Some(f), _) => (blake3::Hash::from(*f).to_hex().to_string(), blake3_name()),
        (None, Some((a, dig))) => (hex(dig), a.as_str().to_string()),
        (None, None) => return None,
    };
    if d.confirmed.len() < 2 {
        return None; // nothing to choose between
    }
    let row_loc = |r: usize| c.rows[r].loc;
    let cands: Vec<Cand> = d
        .confirmed
        .iter()
        .map(|&p| {
            let ph = &c.phys[p];
            let rep = &c.rows[ph.rows[0]];
            let role = ph
                .rows
                .iter()
                .map(|&r| roles[row_loc(r)])
                .min_by_key(|&r| role_rank(r))
                .unwrap_or(Role::Normal);
            let out_of_reach = ph
                .rows
                .iter()
                .any(|&r| !c.locs[row_loc(r)].available || roles[row_loc(r)] == Role::Master);
            let pinned = if role == Role::Master {
                Some(KeepReason::Master)
            } else if !ph.available {
                Some(KeepReason::Offline)
            } else if out_of_reach {
                Some(KeepReason::NamesOutOfReach)
            } else {
                None
            };
            Cand {
                p,
                role,
                device: filesystem_of(&c.locs[rep.loc].uuid).to_string(),
                available: ph.available,
                remote: ph.rows.iter().any(|&r| c.locs[row_loc(r)].remote),
                pinned,
                path: abs(&c.locs[rep.loc].root, &rep.path),
            }
        })
        .collect();

    let unique = cands.len() as u64;
    let target = policy.min_copies.min(unique);
    let by_device = policy.distinct == Distinct::PhysicalDevice;
    let devices = |kept: &[usize]| -> HashSet<&str> {
        kept.iter().map(|&k| cands[k].device.as_str()).collect()
    };
    let met = |kept: &[usize]| {
        let n = if by_device {
            devices(kept).len()
        } else {
            kept.len()
        };
        n as u64 >= target
    };
    // Every master copy stays; then the best copies until the policy is met.
    let mut kept: Vec<usize> = (0..cands.len())
        .filter(|&i| cands[i].role == Role::Master)
        .collect();
    while !met(&kept) {
        let used = devices(&kept);
        let pick = (0..cands.len())
            .filter(|i| !kept.contains(i))
            .filter(|&i| !by_device || !used.contains(cands[i].device.as_str()))
            .min_by_key(|&i| {
                let k = &cands[i];
                (role_rank(k.role), !k.available, k.remote, k.path.clone())
            });
        match pick {
            Some(i) => kept.push(i),
            None => break, // no further device (or copy) exists
        }
    }
    let under_protected = policy.min_copies > unique || !met(&kept);
    let kept_devices = devices(&kept).len() as u64;

    let names = |p: usize| -> Vec<PathBuf> {
        c.phys[p]
            .rows
            .iter()
            .map(|&r| abs(&c.locs[row_loc(r)].root, &c.rows[r].path))
            .collect()
    };
    let expect_of = |r: usize| {
        let row = &c.rows[r];
        Expect {
            size: row.size,
            mtime_ns: row.mtime_ns,
            dev: row.dev,
            ino: row.ino,
            full: hash.clone(),
        }
    };
    let keep = |i: usize, why: KeepReason| {
        let k = &cands[i];
        let rep = c.phys[k.p].rows[0];
        let mut n = names(k.p);
        let path = n.remove(0);
        KeptCopy {
            location_uuid: c.locs[row_loc(rep)].uuid.clone(),
            path,
            shared_inodes: n,
            role: k.role,
            available: k.available,
            remote: k.remote,
            why,
            expect: c.locs[row_loc(rep)].available.then(|| expect_of(rep)),
        }
    };

    let mut out_kept = Vec::new();
    for &i in &kept {
        let why = if cands[i].role == Role::Master {
            KeepReason::Master
        } else {
            KeepReason::Policy
        };
        out_kept.push(keep(i, why));
    }
    let mut rest: Vec<usize> = (0..cands.len()).filter(|i| !kept.contains(i)).collect();
    rest.sort_by(|&a, &b| cands[a].path.cmp(&cands[b].path));
    let mut actions = Vec::new();
    for i in rest {
        let k = &cands[i];
        if under_protected {
            out_kept.push(keep(i, KeepReason::UnderProtected));
        } else if let Some(why) = k.pinned {
            out_kept.push(keep(i, why));
        } else {
            for &r in &c.phys[k.p].rows {
                let l = row_loc(r);
                actions.push(Action {
                    id: String::new(),
                    kind: ActionKind::Quarantine,
                    location_uuid: c.locs[l].uuid.clone(),
                    role: roles[l],
                    path: abs(&c.locs[l].root, &c.rows[r].path),
                    remote: c.locs[l].remote,
                    expect: expect_of(r),
                });
            }
        }
    }
    // Scratch first, then normal; by path within a role.
    actions.sort_by(|a, b| {
        role_rank(b.role)
            .cmp(&role_rank(a.role))
            .then_with(|| a.path.cmp(&b.path))
    });
    for (n, a) in actions.iter_mut().enumerate() {
        a.id = format!("{}-{}", &hash[..16], n + 1);
    }
    Some(PlanGroup {
        algo,
        hash,
        size: d.size,
        unique_copies: unique,
        kept_devices,
        under_protected,
        kept: out_kept,
        actions,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::policy::LocationRule;
    use crate::testkit::*;

    fn policy(min: u64, distinct: Distinct) -> Policy {
        Policy {
            min_copies: min,
            distinct,
            locations: Vec::new(),
        }
    }

    fn plan(e: &Estate, p: &Policy) -> Plan {
        e.with_probe(|pr| plan_with(&e.idx, p, &Progress::default(), pr))
            .unwrap()
    }

    fn set_role(e: &Estate, name: &str, role: Role) {
        e.idx
            .configure(&Estate::uuid(name), Some(role), "local")
            .unwrap();
    }

    /// The one group of the plan.
    fn only(p: &Plan) -> &PlanGroup {
        assert_eq!(p.groups.len(), 1, "{p:#?}");
        &p.groups[0]
    }

    fn acted(g: &PlanGroup) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = g.actions.iter().map(|a| a.path.clone()).collect();
        v.sort();
        v
    }

    fn kept(g: &PlanGroup) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = g.kept.iter().map(|k| k.path.clone()).collect();
        v.sort();
        v
    }

    /// `content(1)` as `f.bin` on each of `names`, each its own volume.
    fn copies_on(label: &str, names: &[&str]) -> Estate {
        let e = Estate::new(label);
        for n in names {
            write(&e.loc(n).join("f.bin"), &content(1));
        }
        for n in names {
            e.add(n);
        }
        e
    }

    #[test]
    fn min_copies_keeps_that_many_and_quarantines_the_rest() {
        let e = copies_on("plan-min", &["A", "B", "C"]);
        let p = plan(&e, &policy(2, Distinct::PhysicalDevice));
        assert_eq!(p.version, PLAN_VERSION);
        let g = only(&p);
        assert_eq!(g.size, BIG as u64);
        assert_eq!(g.hash.len(), 64);
        assert_eq!(g.unique_copies, 3);
        assert!(!g.under_protected);
        assert_eq!(g.kept.len(), 2, "{g:#?}");
        assert_eq!(g.kept_devices, 2);
        assert_eq!(g.actions.len(), 1, "{g:#?}");
        let a = &g.actions[0];
        assert_eq!(a.kind, ActionKind::Quarantine);
        assert_eq!(a.expect.size, BIG as u64);
        assert_eq!(a.expect.full, g.hash);
        assert!(a.expect.ino != 0);
        // Ties go by path: C's copy is the one to go.
        assert_eq!(a.path, e.loc("C").join("f.bin"));
        assert_eq!(a.location_uuid, Estate::uuid("C"));
        assert_eq!(p.summary.actions, 1);
        assert_eq!(p.summary.quarantine_bytes, BIG as u64);

        let p = plan(&e, &policy(1, Distinct::PhysicalDevice));
        assert_eq!(only(&p).actions.len(), 2);
        let p = plan(&e, &policy(3, Distinct::PhysicalDevice));
        assert!(only(&p).actions.is_empty());
        assert!(!only(&p).under_protected);
    }

    #[test]
    fn kept_copies_sit_on_distinct_devices() {
        // A holds two copies (separate inodes) of one volume, B one.
        let e = Estate::new("plan-distinct");
        write(&e.loc("A").join("f1.bin"), &content(1));
        write(&e.loc("A").join("f2.bin"), &content(1));
        write(&e.loc("B").join("f.bin"), &content(1));
        e.add("A");
        e.add("B");
        let p = plan(&e, &policy(2, Distinct::PhysicalDevice));
        let g = only(&p);
        assert!(!g.under_protected);
        assert_eq!(g.kept_devices, 2);
        assert_eq!(
            kept(g),
            vec![e.loc("A").join("f1.bin"), e.loc("B").join("f.bin")]
        );
        assert_eq!(acted(g), vec![e.loc("A").join("f2.bin")]);

        // Without the device rule, any two copies do.
        let p = plan(&e, &policy(2, Distinct::None));
        let g = only(&p);
        assert_eq!(
            kept(g),
            vec![e.loc("A").join("f1.bin"), e.loc("A").join("f2.bin")]
        );
        assert_eq!(acted(g), vec![e.loc("B").join("f.bin")]);
    }

    #[test]
    fn a_policy_that_cannot_be_met_keeps_everything_and_flags_the_group() {
        // Three copies, all on one device: two devices are impossible.
        let e = Estate::new("plan-under");
        for f in ["f1.bin", "f2.bin", "f3.bin"] {
            write(&e.loc("A").join(f), &content(1));
        }
        e.add("A");
        let p = plan(&e, &policy(2, Distinct::PhysicalDevice));
        let g = only(&p);
        assert!(g.under_protected, "{g:#?}");
        assert!(g.actions.is_empty(), "{g:#?}");
        assert_eq!(g.kept.len(), 3);
        assert_eq!(p.summary.under_protected_groups, 1);
        // Fewer copies than min_copies: under-protected too.
        let e = copies_on("plan-under-few", &["A", "B"]);
        let p = plan(&e, &policy(3, Distinct::None));
        let g = only(&p);
        assert!(g.under_protected && g.actions.is_empty(), "{g:#?}");
        // Same device, no device rule: fine.
        let e = Estate::new("plan-under-none");
        for f in ["f1.bin", "f2.bin", "f3.bin"] {
            write(&e.loc("A").join(f), &content(1));
        }
        e.add("A");
        let p = plan(&e, &policy(2, Distinct::None));
        let g = only(&p);
        assert!(!g.under_protected);
        assert_eq!(acted(g), vec![e.loc("A").join("f3.bin")]);
    }

    #[test]
    fn master_copies_are_always_kept_never_quarantined() {
        // M (master) holds two copies of one content, A one.
        let e = Estate::new("plan-master");
        write(&e.loc("M").join("f1.bin"), &content(1));
        write(&e.loc("M").join("f2.bin"), &content(1));
        write(&e.loc("A").join("f.bin"), &content(1));
        e.add("M");
        e.add("A");
        set_role(&e, "M", Role::Master);
        for min in [1, 2] {
            let p = plan(&e, &policy(min, Distinct::None));
            let g = only(&p);
            assert_eq!(acted(g), vec![e.loc("A").join("f.bin")], "min {min}");
            for k in &g.kept {
                assert_eq!(k.why, KeepReason::Master, "{k:?}");
                assert_eq!(k.role, Role::Master);
            }
        }
        // Even when the masters alone exceed min_copies on one device and
        // the other copy is needed for the device rule, no master moves.
        let p = plan(&e, &policy(2, Distinct::PhysicalDevice));
        let g = only(&p);
        assert!(g.actions.is_empty(), "{g:#?}");
        assert!(g.actions.iter().all(|a| a.role != Role::Master));
    }

    #[test]
    fn scratch_copies_go_first_and_the_policy_file_overrides_the_index() {
        let e = copies_on("plan-scratch", &["A", "S"]);
        set_role(&e, "S", Role::Scratch);
        let p = plan(&e, &policy(1, Distinct::PhysicalDevice));
        assert_eq!(acted(only(&p)), vec![e.loc("S").join("f.bin")]);

        // The policy file says A is scratch and S is master: it wins.
        let mut pol = policy(1, Distinct::PhysicalDevice);
        pol.locations = vec![
            LocationRule {
                matches: e.loc("A").to_str().unwrap().into(),
                role: Role::Scratch,
            },
            LocationRule {
                matches: Estate::uuid("S"),
                role: Role::Master,
            },
        ];
        let p = plan(&e, &pol);
        let g = only(&p);
        assert_eq!(acted(g), vec![e.loc("A").join("f.bin")]);
        assert_eq!(g.actions[0].role, Role::Scratch);
        let s = p
            .locations
            .iter()
            .find(|l| l.uuid == Estate::uuid("S"))
            .unwrap();
        assert_eq!((s.stored_role, s.role), (Role::Scratch, Role::Master));

        // Actions are ordered scratch first.
        let e = copies_on("plan-order", &["A", "B", "S"]);
        set_role(&e, "S", Role::Scratch);
        set_role(&e, "A", Role::Master);
        let p = plan(&e, &policy(1, Distinct::PhysicalDevice));
        let g = only(&p);
        let order: Vec<&Path> = g.actions.iter().map(|a| a.path.as_path()).collect();
        assert_eq!(
            order,
            vec![
                e.loc("S").join("f.bin").as_path(),
                e.loc("B").join("f.bin").as_path()
            ]
        );
    }

    #[test]
    fn offline_copies_count_but_are_never_acted_on() {
        let e = copies_on("plan-offline", &["A", "B", "C"]);
        set_role(&e, "C", Role::Scratch);
        // Hash everything while reachable, then unplug C.
        e.report(&Estate::opts());
        e.unplug("C");
        let p = plan(&e, &policy(1, Distinct::PhysicalDevice));
        let g = only(&p);
        assert_eq!(g.unique_copies, 3);
        for a in &g.actions {
            assert_ne!(a.location_uuid, Estate::uuid("C"), "{a:?}");
        }
        let c = g
            .kept
            .iter()
            .find(|k| k.location_uuid == Estate::uuid("C"))
            .expect("offline copy listed as kept");
        assert!(!c.available);
        assert_eq!(c.why, KeepReason::Offline);
        // The reachable copies: one kept (by the policy), one moved.
        assert_eq!(g.actions.len(), 1, "{g:#?}");
        // An offline copy can still be the second device of the policy.
        let p = plan(&e, &policy(2, Distinct::PhysicalDevice));
        let g = only(&p);
        assert!(!g.under_protected);
        assert!(g
            .actions
            .iter()
            .all(|a| a.location_uuid != Estate::uuid("C")));
    }

    #[test]
    fn probable_copies_are_neither_kept_nor_moved() {
        // B is unplugged before its copy was ever hashed whole: only its
        // two ends are known — a probable copy, not a confirmed one.
        let e = copies_on("plan-probable", &["A", "B", "C"]);
        e.unplug("B");
        let p = plan(&e, &policy(1, Distinct::PhysicalDevice));
        let g = only(&p);
        assert_eq!(g.unique_copies, 2, "{g:#?}");
        let b = e.dir.join("unplugged-B").join("f.bin");
        assert!(g.kept.iter().all(|k| k.path != b), "{g:#?}");
        assert!(g.actions.iter().all(|a| a.path != b), "{g:#?}");
    }

    #[test]
    fn every_name_of_a_moved_inode_is_moved_and_a_kept_one_stays_whole() {
        let e = Estate::new("plan-links");
        let s = e.loc("S");
        write(&s.join("f.bin"), &content(1));
        std::fs::hard_link(s.join("f.bin"), s.join("link.bin")).unwrap();
        write(&e.loc("A").join("f.bin"), &content(1));
        e.add("S");
        e.add("A");
        set_role(&e, "S", Role::Scratch);
        let p = plan(&e, &policy(1, Distinct::PhysicalDevice));
        let g = only(&p);
        assert_eq!(g.unique_copies, 2);
        assert_eq!(acted(g), vec![s.join("f.bin"), s.join("link.bin")]);
        let ids: std::collections::HashSet<&str> =
            g.actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "ids are unique");
        assert_eq!(g.actions[0].expect.ino, g.actions[1].expect.ino);

        // S kept instead (A is scratch now): its two names are one kept copy.
        set_role(&e, "S", Role::Normal);
        set_role(&e, "A", Role::Scratch);
        let p = plan(&e, &policy(1, Distinct::PhysicalDevice));
        let g = only(&p);
        assert_eq!(acted(g), vec![e.loc("A").join("f.bin")]);
        assert_eq!(g.kept.len(), 1);
        assert_eq!(g.kept[0].shared_inodes.len(), 1);
    }

    #[test]
    fn kept_reachable_copies_carry_their_identity_for_paranoid_mode() {
        let e = copies_on("plan-expect", &["A", "B"]);
        let p = plan(&e, &policy(1, Distinct::PhysicalDevice));
        let g = only(&p);
        let k = &g.kept[0];
        let x = k.expect.as_ref().expect("reachable kept copy has expect");
        assert_eq!(x.full, g.hash);
        assert_eq!(x.size, BIG as u64);
    }

    #[test]
    fn the_plan_round_trips_through_json_with_self_describing_fields() {
        let e = copies_on("plan-json", &["A", "B"]);
        let p = plan(&e, &policy(1, Distinct::PhysicalDevice));
        let text = serde_json::to_string_pretty(&p).unwrap();
        for field in [
            "\"version\"",
            "\"created\"",
            "\"policy\"",
            "\"min_copies\"",
            "\"distinct\": \"physical_device\"",
            "\"groups\"",
            "\"hash\"",
            "\"kept\"",
            "\"under_protected\"",
            "\"actions\"",
            "\"type\": \"quarantine\"",
            "\"location_uuid\"",
            "\"expect\"",
            "\"mtime_ns\"",
            "\"ino\"",
            "\"full\"",
        ] {
            assert!(text.contains(field), "{field} missing in\n{text}");
        }
        let back: Plan = serde_json::from_str(&text).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn unmatched_policy_entries_warn() {
        let e = copies_on("plan-warn", &["A", "B"]);
        let mut pol = policy(1, Distinct::PhysicalDevice);
        pol.locations = vec![LocationRule {
            matches: "/no/such/mount".into(),
            role: Role::Master,
        }];
        let p = plan(&e, &pol);
        assert!(
            p.warnings.iter().any(|w| w.contains("/no/such/mount")),
            "{:?}",
            p.warnings
        );
    }
}
