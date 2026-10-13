//! Batch renaming: TC mask placeholders, search & replace, planning checks,
//! safe execution, undo. Port of `tests/test_rename.py`, plus a differential
//! test against outputs recorded from the Python version.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use chrono::{Local, TimeZone};
use oc_rename::{Case, Item, NameError, Renamer, Rule, existing_names, item_for, plan, plan_with};
use oc_vfs::testing::MemFS;
use oc_vfs::{FileSystem, VPath};

use common::{Flaky, local, names_in};

fn when() -> f64 {
    Local.with_ymd_and_hms(2026, 3, 7, 9, 5, 4).unwrap().timestamp() as f64
}

fn mem_root() -> VPath {
    MemFS::new().root()
}

fn item(name: &str, is_dir: bool) -> Item {
    let location = mem_root().child(&["nowhere", name]);
    Item { parent: "Holiday".into(), grandparent: "Photos".into(), ..Item::new(location, name, is_dir, when()) }
}

fn rule() -> Rule {
    Rule::default()
}

fn new_with(name: &str, index: usize, rule: Rule) -> String {
    oc_rename::new_name(&item(name, false), index, &rule).unwrap()
}

fn new_masked(name: &str, mask: &str) -> String {
    new_with(name, 0, Rule { name_mask: mask.into(), ext_mask: String::new(), ..rule() })
}

#[test]
fn name_mask() {
    let cases = [
        ("[N]", "IMG_20260307_beach"),
        ("[N1-3]", "IMG"),
        ("[N5,8]", "20260307"),
        ("[N14-]", "beach"),
        ("[N-5-]", "beach"),
        ("[N-5,2]", "be"),
        ("[N-5-4]", "be"),
        ("[N5--7]", "20260307"),
        ("[N1]", "I"),
        ("[2-4]", "MG_"), // whole name incl. extension
        ("[P] - [N14-]", "Holiday - beach"),
        ("[G]_[P]", "Photos_Holiday"),
        ("[Y]-[M]-[D] [h].[m].[s]", "2026-03-07 09.05.04"),
        ("[d] [t]", "2026-03-07 09.05.04"),
        ("[U][N14-][n]_[N1-3]", "BEACH_IMG"),
        ("[L][N1-3]", "img"),
        ("[F]my holiday [N14-]", "My Holiday Beach"),
        ("[[]x[]]", "[x]"),
        ("photo", "photo"),
    ];
    for (mask, expected) in cases {
        assert_eq!(new_masked("IMG_20260307_beach.JPG", mask), expected, "mask {mask}");
    }
}

#[test]
fn extension_mask_and_folders() {
    assert_eq!(new_with("a.tar.gz", 0, rule()), "a.tar.gz");
    assert_eq!(new_with("a.JPG", 0, Rule { ext_mask: "[L][E]".into(), ..rule() }), "a.jpg");
    assert_eq!(new_with("a.jpeg", 0, Rule { ext_mask: "[E1-2]".into(), ..rule() }), "a.jp");
    assert_eq!(new_with(".bashrc", 0, rule()), ".bashrc"); // a dot file has no extension
    assert_eq!(new_with("no_ext", 0, Rule { ext_mask: "txt".into(), ..rule() }), "no_ext.txt");
    let folder = item("v1.2", true);
    let r = Rule { name_mask: "[N]-old".into(), ext_mask: "[E]".into(), ..rule() };
    assert_eq!(oc_rename::new_name(&folder, 0, &r).unwrap(), "v1.2-old"); // no ext for dirs
}

#[test]
fn counter() {
    let c = |i, mask: &str, r: Rule| new_with("x.jpg", i, Rule { name_mask: mask.into(), ..r });
    assert_eq!((0..3).map(|i| c(i, "[C]", rule())).collect::<Vec<_>>(), ["1.jpg", "2.jpg", "3.jpg"]);
    assert_eq!((0..3).map(|i| c(i, "[C10+5:3]", rule())).collect::<Vec<_>>(), ["010.jpg", "015.jpg", "020.jpg"]);
    assert_eq!(c(2, "Img[C:4]", Rule { counter_start: 7, ..rule() }), "Img0009.jpg");
    let r = Rule { counter_start: 100, counter_step: 10, counter_digits: 4, ..rule() };
    assert_eq!(c(1, "[C]", r), "0110.jpg");
}

#[test]
fn bad_masks() {
    for mask in ["[N", "[X]", "[N0-3]", "[Cabc]", "a]b", "[N2,]"] {
        let r = Rule { name_mask: mask.into(), ..rule() };
        let err = oc_rename::new_name(&item("a.txt", false), 0, &r).unwrap_err();
        assert!(matches!(err, NameError::Mask(_)), "{mask}: {err:?}");
    }
}

#[test]
fn mask_error_messages_and_positions() {
    let err = |mask: &str| match oc_rename::new_name(&item("a.txt", false), 0, &Rule { name_mask: mask.into(), ..rule() }) {
        Err(NameError::Mask(e)) => (e.message, e.position),
        other => panic!("{mask}: {other:?}"),
    };
    assert_eq!(err("ab[N"), ("“[” is never closed".to_string(), 2));
    assert_eq!(err("äö]"), ("“]” without “[” (write []] for a bracket)".to_string(), 2)); // characters, not bytes
    assert_eq!(err("[N0-3]"), ("Ranges start at 1".to_string(), 0));
    assert_eq!(err("x[N2,]"), ("Missing length in “2,”".to_string(), 1));
    assert_eq!(err("[Nx]"), ("Unknown range “x”".to_string(), 0));
    assert_eq!(err("[Q]"), ("Unknown placeholder “[Q]”".to_string(), 0));
    assert_eq!(err("[C+]"), ("Unknown counter “[C+]” (example: [C10+5:3])".to_string(), 0));
    assert_eq!(err("[=nonsense]"), ("Unknown field “nonsense” (see Insert… for the list)".to_string(), 0));
    assert_eq!(err("[= x]"), ("Unknown placeholder “[= x]” (example: [=artist])".to_string(), 0));
}

#[test]
fn search_and_replace() {
    let sr = |name: &str, search: &str, replace: &str, regex: bool, cs: bool| {
        new_with(name, 0, Rule { search: search.into(), replace: replace.into(), regex, case_sensitive: cs, ..rule() })
    };
    assert_eq!(sr("My File copy.txt", "copy", "", false, false), "My File .txt");
    assert_eq!(sr("a-b_c.txt", "-|_", " ", false, false), "a b c.txt"); // several searches, one replacement
    assert_eq!(sr("a-b_c.txt", "-|_", "+|=", false, false), "a+b=c.txt"); // pairs
    assert_eq!(sr("ABC.txt", "b", "x", false, false), "AxC.txt"); // case-insensitive by default
    assert_eq!(sr("ABC.txt", "b", "x", false, true), "ABC.txt");
    assert_eq!(sr("2026-03-07 trip.jpg", r"(\d+)-(\d+)-(\d+)", "$3.$2.$1", true, false), "07.03.2026 trip.jpg");
    assert_eq!(sr("a.b.c", ".", "_", false, false), "a_b_c"); // plain text: "." is a dot, not "any"
    assert_eq!(sr("aXbxc", "x", "-", true, false), "a-b-c"); // RegEx: case-insensitive too
    assert_eq!(sr("abab.txt", r"(?<=a)b", "B", true, true), "aBaB.txt"); // look-behind works
}

#[test]
fn case_conversion() {
    let c = |name: &str, case| new_with(name, 0, Rule { case, ..rule() });
    assert_eq!(c("hello WORLD.TXT", Case::Lower), "hello world.txt");
    assert_eq!(c("hello WORLD.txt", Case::Upper), "HELLO WORLD.TXT");
    assert_eq!(c("hello WORLD.txt", Case::First), "Hello world.txt");
    assert_eq!(c("hello WORLD-again.txt", Case::Words), "Hello World-Again.Txt");
}

#[test]
fn dates_use_now_without_mtime() {
    let it = Item { mtime: 0.0, ..item("a.txt", false) };
    let r = Rule { name_mask: "[Y]".into(), ext_mask: String::new(), ..rule() };
    let year = oc_rename::new_name(&it, 0, &r).unwrap();
    assert_eq!(year, chrono::Local::now().format("%Y").to_string());
}

#[test]
fn renamer_reuse_matches_free_functions() {
    let r = Rule { name_mask: "[C:3]_[N]".into(), search: "_".into(), replace: "-".into(), ..rule() };
    let renamer = Renamer::new(&r);
    for i in 0..5 {
        let it = item(&format!("f_{i}.txt"), false);
        assert_eq!(renamer.new_name(&it, i).unwrap(), oc_rename::new_name(&it, i, &r).unwrap());
    }
    assert_eq!(renamer.expand("[U][N]", &item("ab.c", false), 0).unwrap(), "AB");
    assert_eq!(oc_rename::expand("[E]", &item("ab.c", false), 0, &r).unwrap(), "c");
}

// -- the Python version's outputs ---------------------------------------------------------------

fn rule_from(json: &serde_json::Value) -> Rule {
    let mut r = Rule::default();
    let s = |k: &str| json.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let i = |k: &str| json.get(k).and_then(|v| v.as_i64());
    let b = |k: &str| json.get(k).and_then(|v| v.as_bool());
    if let Some(v) = s("name_mask") {
        r.name_mask = v;
    }
    if let Some(v) = s("ext_mask") {
        r.ext_mask = v;
    }
    if let Some(v) = s("search") {
        r.search = v;
    }
    if let Some(v) = s("replace") {
        r.replace = v;
    }
    if let Some(v) = s("case") {
        r.case = Case::parse(&v).unwrap();
    }
    r.regex = b("regex").unwrap_or(false);
    r.case_sensitive = b("case_sensitive").unwrap_or(false);
    r.counter_start = i("counter_start").unwrap_or(1);
    r.counter_step = i("counter_step").unwrap_or(1);
    r.counter_digits = i("counter_digits").unwrap_or(1);
    r
}

/// Thousands of names × masks × search/replace × case, recorded from `rename.py`
/// by `tests/data/gen_python_reference.py` (run it with the Python version's venv):
/// the output must be byte-identical.
#[test]
fn matches_python_reference() {
    let text = std::fs::read_to_string(common::data("python_reference.jsonl")).unwrap();
    let root = mem_root();
    let mut failures = Vec::new();
    let mut count = 0;
    for line in text.lines() {
        let case: serde_json::Value = serde_json::from_str(line).unwrap();
        let name = case["name"].as_str().unwrap();
        let mut it = Item {
            parent: "Holiday".into(),
            grandparent: "Photos".into(),
            ..Item::new(root.child(&[name]), name, case["is_dir"].as_bool().unwrap(), 1_772_874_304.0)
        };
        if let Some(meta) = case["meta"].as_object() {
            let m: HashMap<String, String> = meta.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string())).collect();
            it = it.with_meta(m);
        }
        let rule = rule_from(&case["rule"]);
        let got = oc_rename::new_name(&it, case["index"].as_u64().unwrap() as usize, &rule);
        let expect = &case["expect"];
        let ok = match (&got, expect.as_object().unwrap().keys().next().unwrap().as_str()) {
            (Ok(n), "ok") => n == expect["ok"].as_str().unwrap(),
            (Err(NameError::MetadataPending), "pending") => true,
            (Err(NameError::MissingMetadata(m)), "missing") => m == expect["missing"].as_str().unwrap(),
            (Err(NameError::Mask(e)), "mask_error") => {
                e.message == expect["mask_error"].as_str().unwrap() && e.position as u64 == expect["pos"].as_u64().unwrap()
            }
            // Expression syntax errors are worded by the regex engine, not Python.
            (Err(NameError::Search(_)), "pattern_error") => true,
            (Err(NameError::Search(m)), "search_error") => m == expect["search_error"].as_str().unwrap(),
            _ => false,
        };
        count += 1;
        if !ok {
            failures.push(format!("{line}\n    got: {got:?}"));
        }
    }
    assert!(count > 6000);
    assert!(failures.is_empty(), "{} of {count} differ:\n{}", failures.len(), failures.join("\n"));
}

// -- planning -----------------------------------------------------------------------------------

fn items_in(folder: &Path, names: &[&str]) -> Vec<Item> {
    let root = local(folder);
    names
        .iter()
        .map(|n| {
            let loc = root.child(&[n]);
            item_for(&loc, &loc.stat().unwrap())
        })
        .collect()
}

fn named(mask: &str) -> Rule {
    Rule { name_mask: mask.into(), ..rule() }
}

#[test]
fn plan_finds_problems() {
    let tmp = tempfile::tempdir().unwrap();
    for n in ["a.txt", "b.txt", "keep.txt"] {
        std::fs::write(tmp.path().join(n), n).unwrap();
    }
    let items = items_in(tmp.path(), &["a.txt", "b.txt"]);
    let same = plan(&items, &named("same"));
    assert!(same.problems().next().unwrap().error.starts_with("same new name"));
    assert_eq!(same.changes[0].error, "same new name as b.txt");
    assert_eq!(same.changes[1].error, "same new name as a.txt");
    assert!(!same.can_run());
    let clash = plan(&items[..1], &named("keep"));
    assert_eq!(clash.problems().next().unwrap().error, "a file with this name already exists");
    let bad = plan(&items, &named("[N"));
    assert!(bad.mask_error.contains("never closed") && !bad.can_run());
    assert_eq!(bad.mask_error, "“[” is never closed (at position 1)");
    assert!(bad.changes.iter().all(|c| !c.changed()));
    let regex = plan(&items, &Rule { search: "(".into(), regex: true, ..rule() });
    assert!(regex.mask_error.starts_with("Search: "), "{}", regex.mask_error);
    assert_eq!(plan(&items, &named("x/[N]")).problems().next().unwrap().error, "contains “/”");
    let nothing = plan(&items, &rule());
    assert!(!nothing.can_run() && nothing.problems().next().is_none() && nothing.to_rename().next().is_none());
}

#[test]
fn plan_target_taken_by_unchanged_item() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "b").unwrap();
    let items = items_in(tmp.path(), &["a.txt", "b.txt"]);
    let p = plan(&items, &Rule { search: "a".into(), replace: "b".into(), ..rule() }); // a→b, b stays b
    assert!(!p.can_run());
    assert_eq!(p.problems().next().unwrap().item.name, "a.txt");
}

#[test]
fn swap_and_rotate() {
    let tmp = tempfile::tempdir().unwrap();
    for n in ["1.txt", "2.txt", "3.txt"] {
        std::fs::write(tmp.path().join(n), format!("content {n}")).unwrap();
    }
    let items = items_in(tmp.path(), &["1.txt", "2.txt", "3.txt"]);
    let p = plan(&items, &named("[C2]")); // 1→2, 2→3, 3→4
    assert!(p.can_run(), "{:?}", p.problems().collect::<Vec<_>>());
    let mut progress = Vec::new();
    let done = oc_rename::execute(&p, Some(&mut |n, total| progress.push((n, total))));
    assert!(done.errors.is_empty(), "{:?}", done.errors);
    assert_eq!(progress, [(1, 3), (2, 3), (3, 3)]);
    let contents = |dir: &Path| {
        names_in(dir).into_iter().map(|n| (n.clone(), std::fs::read_to_string(dir.join(&n)).unwrap())).collect::<Vec<_>>()
    };
    assert_eq!(
        contents(tmp.path()),
        [("2.txt", "content 1.txt"), ("3.txt", "content 2.txt"), ("4.txt", "content 3.txt")]
            .map(|(a, b)| (a.to_string(), b.to_string()))
    );
    let moved = oc_rename::renamed_locations(&done);
    assert_eq!(moved[&local(tmp.path()).child(&["1.txt"])], local(tmp.path()).child(&["2.txt"]));
    let undone = oc_rename::undo(&done);
    assert!(undone.errors.is_empty());
    assert_eq!(
        contents(tmp.path()),
        [("1.txt", "content 1.txt"), ("2.txt", "content 2.txt"), ("3.txt", "content 3.txt")]
            .map(|(a, b)| (a.to_string(), b.to_string()))
    );
}

#[test]
fn rename_into_a_name_another_item_frees() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "A").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "B").unwrap();
    let items = items_in(tmp.path(), &["a.txt", "b.txt"]);
    // a→b while b→c: "b.txt" is taken now but freed by the batch.
    let p = plan(&items, &Rule { search: "b|a".into(), replace: "c|b".into(), ..rule() });
    assert_eq!(p.changes.iter().map(|c| c.new.as_str()).collect::<Vec<_>>(), ["b.txt", "c.txt"]);
    assert!(p.can_run(), "{:?}", p.problems().collect::<Vec<_>>());
    let done = oc_rename::execute(&p, None);
    assert!(done.errors.is_empty());
    assert_eq!(names_in(tmp.path()), ["b.txt", "c.txt"]);
    assert_eq!(std::fs::read_to_string(tmp.path().join("b.txt")).unwrap(), "A");
}

#[test]
fn real_swap_through_temporary_names() {
    let fs = Arc::new(MemFS::new());
    fs.add_file("d/a.txt", "A");
    fs.add_file("d/b.txt", "B");
    let root = VPath::new(fs.clone(), "");
    let items: Vec<Item> = ["a.txt", "b.txt"]
        .iter()
        .map(|n| {
            let l = root.child(&["d", n]);
            item_for(&l, &l.stat().unwrap())
        })
        .collect();
    // b.txt first gets counter 1 -> "a" isn't expressible by masks alone; build the plan by hand.
    let mut p = plan(&items, &rule());
    p.changes[0].new = "b.txt".into();
    p.changes[1].new = "a.txt".into();
    let done = oc_rename::execute(&p, None);
    assert!(done.errors.is_empty(), "{:?}", done.errors);
    assert_eq!(fs.read_file("d/a.txt").unwrap(), b"B");
    assert_eq!(fs.read_file("d/b.txt").unwrap(), b"A");
    assert_eq!(fs.paths(), ["d", "d/a.txt", "d/b.txt"]); // no temporary names left
}

#[test]
fn case_only_change() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("photo.JPG"), "x").unwrap();
    let p = plan(&items_in(tmp.path(), &["photo.JPG"]), &Rule { ext_mask: "[L][E]".into(), ..rule() });
    assert!(p.can_run(), "{:?}", p.problems().collect::<Vec<_>>());
    let done = oc_rename::execute(&p, None);
    assert!(done.errors.is_empty(), "{:?}", done.errors);
    assert_eq!(names_in(tmp.path()), ["photo.jpg"]);
}

#[test]
fn folder_and_its_contents() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("docs")).unwrap();
    std::fs::write(tmp.path().join("docs/one.txt"), "1").unwrap();
    let root = local(tmp.path());
    let items: Vec<Item> = [root.child(&["docs"]), root.child(&["docs", "one.txt"])]
        .iter()
        .map(|l| item_for(l, &l.stat().unwrap()))
        .collect();
    let p = plan(&items, &named("[N]_x"));
    let problem = p.problems().next().unwrap();
    assert!(problem.error.contains("renamed too"), "{}", problem.error);
    assert_eq!(problem.error, "inside docs, which is renamed too (rename separately)");
    assert_eq!(problem.item.name, "one.txt");
}

#[test]
fn failure_keeps_old_name() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
    let flaky: Arc<dyn FileSystem> = Arc::new(Flaky { inner: Arc::new(common::LocalFs), fail_on: 2, calls: AtomicUsize::new(0) });
    let loc = VPath::new(flaky, tmp.path().join("a.txt").to_string_lossy().into_owned());
    let p = plan(&[item_for(&loc, &loc.stat().unwrap())], &named("b"));
    let done = oc_rename::execute(&p, None); // the final step fails
    assert!(done.renames.is_empty());
    assert!(done.errors[0].starts_with("a.txt: ") && done.errors[0].contains("nope"), "{:?}", done.errors);
    assert_eq!(names_in(tmp.path()), ["a.txt"]);
}

#[test]
fn failed_swap_never_overwrites() {
    // a <-> b where b's final step fails: putting b back would replace the
    // file now called b (a's content) on a disk whose rename overwrites.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "A").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "B").unwrap();
    let flaky: Arc<dyn FileSystem> = Arc::new(Flaky { inner: Arc::new(common::LocalFs), fail_on: 4, calls: AtomicUsize::new(0) });
    let root = VPath::new(flaky, tmp.path().to_string_lossy().into_owned());
    let items: Vec<Item> = ["a.txt", "b.txt"].iter().map(|n| item_for(&root.child(&[n]), &root.child(&[n]).stat().unwrap())).collect();
    let mut p = plan(&items, &rule());
    p.changes[0].new = "b.txt".into();
    p.changes[1].new = "a.txt".into();
    let done = oc_rename::execute(&p, None);
    assert_eq!(done.errors.len(), 2, "{:?}", done.errors);
    assert_eq!(done.errors[0], "b.txt: nope");
    assert!(done.errors[1].starts_with("b.txt is now called .orchard-rename-"), "{:?}", done.errors);
    let names = names_in(tmp.path());
    assert_eq!(names.len(), 2);
    assert_eq!(std::fs::read_to_string(tmp.path().join("b.txt")).unwrap(), "A");
    assert_eq!(std::fs::read_to_string(tmp.path().join(&names[0])).unwrap(), "B"); // the temporary name
}

#[test]
fn target_taken_after_planning_is_not_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "A").unwrap();
    let p = plan(&items_in(tmp.path(), &["a.txt"]), &named("b"));
    assert!(p.can_run());
    std::fs::write(tmp.path().join("b.txt"), "someone else's").unwrap();
    let done = oc_rename::execute(&p, None);
    assert!(done.renames.is_empty());
    assert!(done.errors[0].starts_with("a.txt: already exists: "), "{:?}", done.errors);
    assert_eq!(names_in(tmp.path()), ["a.txt", "b.txt"]);
    assert_eq!(std::fs::read_to_string(tmp.path().join("b.txt")).unwrap(), "someone else's");
    assert_eq!(std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(), "A");
}

#[test]
fn failure_in_first_phase_and_stuck_temp() {
    let mem = Arc::new(MemFS::new());
    mem.add_file("a.txt", "a");
    mem.add_file("b.txt", "b");
    let fs: Arc<dyn FileSystem> = Arc::new(Flaky { inner: mem.clone(), fail_on: 1, calls: AtomicUsize::new(0) });
    let root = VPath::new(fs, "");
    let items: Vec<Item> = ["a.txt", "b.txt"].iter().map(|n| item_for(&root.child(&[n]), &root.child(&[n]).stat().unwrap())).collect();
    let done = oc_rename::execute(&plan(&items, &named("[N]_x")), None);
    assert_eq!(done.errors, ["a.txt: nope"]); // a stays, b goes through
    assert_eq!(mem.paths(), ["a.txt", "b_x.txt"]);
    assert_eq!(done.renames.len(), 1);
    assert_eq!(done.renames[0].old_name, "b.txt");
    assert_eq!(done.renames[0].new_name, "b_x.txt");
}

#[test]
fn works_on_other_file_systems() {
    let mem = Arc::new(MemFS::new());
    mem.add_file("x.txt", "x");
    let loc = VPath::new(mem.clone(), "x.txt");
    let p = plan(&[item_for(&loc, &loc.stat().unwrap())], &named("y"));
    let done = oc_rename::execute(&p, None);
    assert!(done.errors.is_empty());
    assert!(mem.contains("y.txt") && !mem.contains("x.txt"));
    assert!(oc_rename::rename_supported(&loc));
    assert!(!oc_rename::rename_supported(&MemFS::with_capabilities(oc_vfs::Capabilities::default()).root()));
}

#[test]
fn name_problems() {
    use oc_rename::name_problem;
    assert_eq!(name_problem(""), Some("empty name"));
    assert_eq!(name_problem("."), Some("empty name"));
    assert_eq!(name_problem(".."), Some("empty name"));
    assert_eq!(name_problem(&"a".repeat(300)), Some("name too long"));
    assert_eq!(name_problem(&"ä".repeat(128)), Some("name too long")); // bytes, not characters
    assert_eq!(name_problem("a\0b"), Some("contains “/”"));
    #[cfg(not(windows))]
    assert_eq!(name_problem("fine name.txt"), None);
}

// -- metadata states in plans -------------------------------------------------------------------

#[test]
fn pending_and_skipped_states() {
    let pending = item("a.mp3", false);
    let tagged = item("b.mp3", false).with_meta(HashMap::from([("artist".to_string(), "X".to_string())]));
    let untagged = Item { meta_error: String::new(), ..item("c.mp3", false).with_meta(HashMap::new()) };
    let failed = Item { meta_error: "cloud-only file, not downloaded".into(), ..item("d.mp3", false).with_meta(HashMap::new()) };
    let p = plan(&[pending, tagged, untagged, failed], &named("[=artist] [N]"));
    assert_eq!(p.pending(), 1);
    assert_eq!(p.changes[0].note, "reading metadata…");
    assert_eq!(p.changes[1].new, "X b.mp3");
    assert_eq!(p.changes[2].note, "skipped: no artist");
    assert_eq!(p.changes[3].note, "skipped: cloud-only file, not downloaded");
    assert_eq!(p.skipped().count(), 2);
    assert!(!p.can_run()); // still waiting for the first
}

// -- existing names -----------------------------------------------------------------------------

#[test]
fn existing_names_lists_each_folder_once() {
    let mem = Arc::new(MemFS::new());
    mem.add_file("d/a.txt", "");
    mem.add_file("d/.hidden", "");
    mem.add_file("e/b.txt", "");
    let root = VPath::new(mem.clone(), "");
    let items: Vec<Item> = ["d/a.txt", "e/b.txt"]
        .iter()
        .map(|p| {
            let l = VPath::new(mem.clone(), *p);
            item_for(&l, &l.stat().unwrap())
        })
        .collect();
    let names = existing_names(&items);
    assert!(names.exists(&root.child(&["d", "a.txt"])));
    assert!(names.exists(&root.child(&["d", ".hidden"]))); // hidden files count
    assert!(!names.exists(&root.child(&["d", "b.txt"])));
    assert!(names.exists(&root.child(&["e", "b.txt"])));
    // Folders not listed fall back to asking the file system.
    mem.add_file("f/c.txt", "");
    assert!(names.exists(&root.child(&["f", "c.txt"])));
    let renamer = Renamer::new(&Rule { name_mask: ".hidden".into(), ext_mask: String::new(), ..rule() });
    let p = plan_with(&renamer, &items[..1], &|l: &VPath| names.exists(l));
    assert_eq!(p.changes[0].error, "a file with this name already exists");
}

#[cfg(target_os = "macos")]
#[test]
fn case_insensitive_disks_fold_names() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
    std::fs::write(tmp.path().join("B.TXT"), "b").unwrap();
    let items = items_in(tmp.path(), &["a.txt"]);
    let names = existing_names(&items);
    assert!(names.exists(&local(tmp.path()).child(&["b.txt"])));
    let p = plan(&items, &named("b"));
    assert_eq!(p.changes[0].new, "b.txt");
    assert_eq!(p.changes[0].error, "a file with this name already exists");
    // New names differing in case only collide on such disks.
    std::fs::write(tmp.path().join("x1.txt"), "").unwrap();
    std::fs::write(tmp.path().join("X2.txt"), "").unwrap();
    let p = plan(&items_in(tmp.path(), &["x1.txt", "X2.txt"]), &named("[N1]"));
    assert_eq!(p.changes[0].error, "same new name as X2.txt");
}

/// Random batches over two folders (renamed folders, nested items, case-only
/// collisions, taken names, metadata states) planned by `rename.py`
/// (`tests/data/gen_python_plans.py`): every change, error, note and the
/// verdict must agree. Python folded names on macOS local disks, so this
/// runs where Rust folds too.
#[cfg(target_os = "macos")]
#[test]
fn plans_match_python_reference() {
    use std::collections::HashSet;
    let text = std::fs::read_to_string(common::data("python_plans.jsonl")).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(common::LocalFs);
    let mut failures = Vec::new();
    for line in text.lines() {
        let case: serde_json::Value = serde_json::from_str(line).unwrap();
        let rule = rule_from(&case["rule"]);
        let items: Vec<Item> = case["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                let location = VPath::new(fs.clone(), i["path"].as_str().unwrap());
                let name = i["name"].as_str().unwrap();
                let it = Item {
                    parent: "P".into(),
                    grandparent: "G".into(),
                    ..Item::new(location, name, i["is_dir"].as_bool().unwrap(), 1_772_874_304.0)
                };
                match i["meta"].as_object() {
                    Some(m) => it.with_meta(m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string())).collect()),
                    None => it,
                }
            })
            .collect();
        let fold = |s: &str| caseless::default_case_fold_str(s);
        let taken: HashSet<String> = case["taken"].as_array().unwrap().iter().map(|t| fold(t.as_str().unwrap())).collect();
        let p = plan_with(&Renamer::new(&rule), &items, &|l: &VPath| taken.contains(&fold(&l.path)));
        let got: Vec<serde_json::Value> =
            p.changes.iter().map(|c| serde_json::json!([c.new, c.error, c.note, c.pending])).collect();
        if got != *case["changes"].as_array().unwrap()
            || p.mask_error != case["mask_error"].as_str().unwrap()
            || p.can_run() != case["can_run"].as_bool().unwrap()
        {
            failures.push(format!("{line}\n    got: {got:?} {:?}", p.mask_error));
        }
    }
    assert!(failures.is_empty(), "{} differ:\n{}", failures.len(), failures.join("\n"));
}
