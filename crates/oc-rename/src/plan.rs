//! Planning (new names plus every problem, so Rename stays disabled until
//! there are none), safe two-phase execution, and undo.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hash, Hasher};

use oc_vfs::{ListOptions, VPath};

use crate::mask::{NameError, Renamer};
use crate::rule::{Item, Rule};

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

#[cfg(windows)]
const WINDOWS_BAD: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

#[cfg(windows)]
fn windows_reserved(stem: &str) -> bool {
    let s = stem.to_lowercase();
    matches!(s.as_str(), "con" | "prn" | "aux" | "nul")
        || ((s.starts_with("com") || s.starts_with("lpt")) && s.len() == 4 && matches!(s.as_bytes()[3], b'1'..=b'9'))
}

/// Why `name` can't be a file name, or None if it can.
pub fn name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() || name == "." || name == ".." {
        return Some("empty name");
    }
    if name.contains(['/', '\0']) {
        return Some("contains “/”");
    }
    #[cfg(windows)]
    {
        if name.contains(WINDOWS_BAD) {
            return Some("contains a character Windows doesn't allow");
        }
        if name.ends_with([' ', '.']) {
            return Some("ends with a space or dot");
        }
        if windows_reserved(name.split('.').next().unwrap_or("")) {
            return Some("reserved name on Windows");
        }
    }
    if name.len() > 255 {
        return Some("name too long");
    }
    None
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// One item's outcome.
#[derive(Clone, Debug)]
pub struct Change {
    pub item: Item,
    pub new: String,
    /// Blocks the batch.
    pub error: String,
    /// Left unchanged, e.g. "skipped: no artist".
    pub note: String,
    /// Waiting for metadata.
    pub pending: bool,
}

impl Change {
    pub fn new(item: Item, new: impl Into<String>) -> Self {
        Change { item, new: new.into(), error: String::new(), note: String::new(), pending: false }
    }

    fn unchanged(item: &Item) -> Self {
        Change::new(item.clone(), item.name.clone())
    }

    pub fn changed(&self) -> bool {
        self.new != self.item.name
    }

    pub fn ok(&self) -> bool {
        self.error.is_empty()
    }
}

/// New names for a batch plus everything that would go wrong.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub changes: Vec<Change>,
    /// The rule itself is broken (then every change is "unchanged").
    pub mask_error: String,
}

impl Plan {
    /// Changes that will rename something.
    pub fn to_rename(&self) -> impl Iterator<Item = &Change> {
        self.changes.iter().filter(|c| c.changed() && c.ok())
    }

    pub fn problems(&self) -> impl Iterator<Item = &Change> {
        self.changes.iter().filter(|c| !c.ok())
    }

    /// How many items still wait for metadata.
    pub fn pending(&self) -> usize {
        self.changes.iter().filter(|c| c.pending).count()
    }

    /// Items left alone for want of a metadata value.
    pub fn skipped(&self) -> impl Iterator<Item = &Change> {
        self.changes.iter().filter(|c| !c.note.is_empty() && !c.pending)
    }

    pub fn can_run(&self) -> bool {
        self.mask_error.is_empty() && self.problems().next().is_none() && self.pending() == 0 && self.to_rename().next().is_some()
    }
}

/// Whether names in this folder are case-insensitive (typical for macOS and Windows disks).
fn folds_case(location: &VPath) -> bool {
    cfg!(any(target_os = "macos", windows)) && location.is_local()
}

/// How names compare in a folder: folded where the disk ignores case.
fn name_key(location: &VPath, name: &str) -> String {
    if folds_case(location) { caseless::default_case_fold_str(name) } else { name.to_string() }
}

type Key = (Option<VPath>, String);

fn key(location: &VPath, name: &str) -> Key {
    (location.parent(), name_key(location, name))
}

/// [`plan_with`] asking the file system whether names are taken.
pub fn plan(items: &[Item], rule: &Rule) -> Plan {
    plan_with(&Renamer::new(rule), items, &|location: &VPath| location.exists())
}

/// New names for `items` plus everything that would go wrong. `exists` checks
/// whether a name is already taken (see [`existing_names`] for servers).
pub fn plan_with(renamer: &Renamer, items: &[Item], exists: &dyn Fn(&VPath) -> bool) -> Plan {
    let mut changes: Vec<Change> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        match renamer.new_name(item, index) {
            Ok(new) => changes.push(Change::new(item.clone(), new)),
            Err(NameError::MetadataPending) => {
                changes.push(Change { note: "reading metadata…".into(), pending: true, ..Change::unchanged(item) })
            }
            Err(NameError::MissingMetadata(why)) => {
                changes.push(Change { note: format!("skipped: {why}"), ..Change::unchanged(item) })
            }
            Err(NameError::Mask(e)) => {
                return Plan {
                    changes: items.iter().map(Change::unchanged).collect(),
                    mask_error: format!("{} (at position {})", e.message, e.position + 1),
                };
            }
            Err(e @ NameError::Search(_)) => {
                return Plan { changes: items.iter().map(Change::unchanged).collect(), mask_error: e.to_string() };
            }
        }
    }

    check(changes, exists)
}

/// One item to one new name, checked like a batch (invalid name, name taken;
/// a change of letter case only is fine): F2's in-place rename.
pub fn plan_one(item: &Item, new: &str, exists: &dyn Fn(&VPath) -> bool) -> Plan {
    check(vec![Change::new(item.clone(), new)], exists)
}

/// Mark the changes that can't be made.
fn check(mut changes: Vec<Change>, exists: &dyn Fn(&VPath) -> bool) -> Plan {
    // Names that are freed by this batch (unchanged items keep theirs).
    let old_names: HashSet<Key> =
        changes.iter().filter(|c| c.changed()).map(|c| key(&c.item.location, &c.item.name)).collect();
    let renamed_dirs: HashSet<VPath> =
        changes.iter().filter(|c| c.changed() && c.item.is_dir).map(|c| c.item.location.clone()).collect();
    let mut seen: HashMap<Key, usize> = HashMap::new();
    for i in 0..changes.len() {
        let change = &changes[i];
        if !change.changed() {
            continue;
        }
        if let Some(problem) = name_problem(&change.new) {
            changes[i].error = problem.into();
            continue;
        }
        let mut outer = if renamed_dirs.is_empty() { None } else { change.item.location.parent() };
        while let Some(o) = &outer {
            if renamed_dirs.contains(o) {
                break;
            }
            outer = o.parent();
        }
        if let Some(o) = outer {
            changes[i].error = format!("inside {}, which is renamed too (rename separately)", o.name());
            continue;
        }
        let k = key(&change.item.location, &change.new);
        if let Some(&first) = seen.get(&k) {
            changes[i].error = format!("same new name as {}", changes[first].item.name);
            if changes[first].error.is_empty() {
                changes[first].error = format!("same new name as {}", changes[i].item.name);
            }
            continue;
        }
        let same_file = key(&change.item.location, &change.item.name) == k; // case-only change
        let taken = !same_file
            && !old_names.contains(&k)
            && k.0.as_ref().is_some_and(|folder| exists(&folder.child(&[change.new.as_str()])));
        seen.insert(k, i);
        if taken {
            changes[i].error = "a file with this name already exists".into();
        }
    }
    Plan { changes, mask_error: String::new() }
}

// ---------------------------------------------------------------------------
// Executing
// ---------------------------------------------------------------------------

/// One finished rename, for undo.
#[derive(Clone, Debug, PartialEq)]
pub struct Renamed {
    pub folder: VPath,
    pub new_name: String,
    pub old_name: String,
}

/// A finished batch: what was renamed (in order) and what failed.
#[derive(Clone, Debug, Default)]
pub struct Done {
    pub renames: Vec<Renamed>,
    pub errors: Vec<String>,
}

/// 8 hex digits that differ between runs, so temporary names never collide
/// with leftovers of an interrupted batch.
fn batch_tag() -> String {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    std::time::SystemTime::now().hash(&mut h);
    std::process::id().hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// `src` -> `dst` unless `dst` exists. Local renames (POSIX `rename`)
/// silently replace the target: a file created after planning, or the old
/// name a failed swap partner would go back to, must not be overwritten.
fn rename_no_replace(fs: &dyn oc_vfs::FileSystem, src: &str, dst: &str) -> oc_vfs::Result<()> {
    if fs.exists(dst) {
        return Err(oc_vfs::VfsError::AlreadyExists(fs.display(dst)));
    }
    fs.rename(src, dst)
}

/// Rename everything in `plan` (which should be [`Plan::can_run`]).
///
/// Two phases: first every file moves to a temporary name in its folder,
/// then to its final name — so swaps (a→b, b→a) and case-only changes work
/// and no file is overwritten. A file that fails keeps (or gets back) its
/// old name. Blocking: call it on a worker thread. `on_progress(done, total)`
/// is called after each final rename.
pub fn execute(plan: &Plan, mut on_progress: Option<&mut dyn FnMut(usize, usize)>) -> Done {
    let mut done = Done::default();
    let tag = batch_tag();
    let mut staged: Vec<(VPath, &Change, String)> = Vec::new();
    for (i, change) in plan.to_rename().enumerate() {
        let Some(folder) = change.item.location.parent() else {
            done.errors.push(format!("{}: no folder", change.item.name));
            continue;
        };
        let temp = format!(".orchard-rename-{tag}-{i}");
        match folder.fs.rename(&change.item.location.path, &folder.fs.join(&folder.path, &[&temp])) {
            Ok(()) => staged.push((folder, change, temp)),
            Err(e) => done.errors.push(format!("{}: {e}", change.item.name)),
        }
    }
    let total = staged.len();
    for (n, (folder, change, temp)) in staged.into_iter().enumerate() {
        let fs = &folder.fs;
        let temp_path = fs.join(&folder.path, &[&temp]);
        match rename_no_replace(fs.as_ref(), &temp_path, &fs.join(&folder.path, &[&change.new])) {
            Ok(()) => done.renames.push(Renamed {
                folder,
                new_name: change.new.clone(),
                old_name: change.item.name.clone(),
            }),
            Err(e) => {
                done.errors.push(format!("{}: {e}", change.item.name));
                // The old name may be taken by now (another item of a swap got it).
                if rename_no_replace(fs.as_ref(), &temp_path, &change.item.location.path).is_err() {
                    done.errors.push(format!("{} is now called {temp}", change.item.name));
                }
            }
        }
        if let Some(cb) = on_progress.as_mut() {
            cb(n + 1, total);
        }
    }
    done
}

/// Reverse a finished batch (also two-phase). Returns what the undo did.
pub fn undo(done: &Done) -> Done {
    let changes = done
        .renames
        .iter()
        .map(|r| Change::new(Item::new(r.folder.child(&[&r.new_name]), r.new_name.clone(), false, 0.0), r.old_name.clone()))
        .collect();
    execute(&Plan { changes, mask_error: String::new() }, None)
}

/// old location -> new location for a finished batch.
pub fn renamed_locations(done: &Done) -> HashMap<VPath, VPath> {
    done.renames
        .iter()
        .map(|r| (r.folder.child(&[&r.old_name]), r.folder.child(&[&r.new_name])))
        .collect()
}

/// Whether items at `location` can be renamed at all.
pub fn rename_supported(location: &VPath) -> bool {
    let caps = location.fs.capabilities();
    caps.writable && caps.can_rename
}

// ---------------------------------------------------------------------------
// Existing names
// ---------------------------------------------------------------------------

/// An `exists` check for [`plan_with`] that lists each folder once instead of
/// asking per name (cheap on servers). Folders that can't be listed fall
/// back to asking per name.
pub struct ExistingNames {
    taken: HashMap<VPath, HashSet<String>>,
}

/// List the folders of `items` (blocking: call it on a worker thread).
pub fn existing_names(items: &[Item]) -> ExistingNames {
    let folders: HashSet<VPath> = items.iter().filter_map(|i| i.location.parent()).collect();
    let opts = ListOptions { show_hidden: true, ..ListOptions::default() };
    let mut taken = HashMap::with_capacity(folders.len());
    for folder in folders {
        let Ok(entries) = folder.list_dir(opts) else { continue };
        let names = entries.into_iter().map(|e| name_key(&folder, &e.name)).collect();
        taken.insert(folder, names);
    }
    ExistingNames { taken }
}

impl ExistingNames {
    pub fn exists(&self, location: &VPath) -> bool {
        let names = location.parent().and_then(|folder| self.taken.get(&folder));
        match names {
            Some(names) => names.contains(&name_key(location, &location.name())),
            None => location.exists(),
        }
    }
}
