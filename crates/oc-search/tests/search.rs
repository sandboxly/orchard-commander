//! Find files: the walk, content matching, archives, other file systems and
//! results as a virtual folder. Port of `tests/test_search.py` (non-Qt parts).

mod common;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{MAIN_SEPARATOR, Path};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use common::*;
use oc_search::{CHUNK, Hit, Kinds, Query, SearchError, SearchProgress, SearchResultsFS};
use oc_vfs::{FileSystem, ListOptions, VPath};
use tempfile::TempDir;

/// report.pdf, Report-2026.PDF, big-report.fakezip(docs/inner.txt), notes.md,
/// src/main.py, src/util.py, src/deep/deeper/config.py, .hidden/secret.txt
fn project() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir_all(root.join("src/deep/deeper")).unwrap();
    fs::create_dir(root.join(".hidden")).unwrap();
    fs::write(root.join("report.pdf"), b"%PDF-1.4 annual numbers").unwrap();
    let mut big = b"%PDF-1.4 more numbers".to_vec();
    big.extend(std::iter::repeat_n(b'x', 5000));
    fs::write(root.join("Report-2026.PDF"), big).unwrap();
    fs::write(root.join("notes.md"), "# Notes\nThe Quick brown fox\nnothing else\n").unwrap();
    fs::write(root.join("src/main.py"), "import os\n\ndef main():\n    print('hello world')\n").unwrap();
    fs::write(root.join("src/util.py"), "def helper():\n    return 42\n").unwrap();
    fs::write(root.join("src/deep/deeper/config.py"), "DEBUG = True\n").unwrap();
    fs::write(root.join(".hidden/secret.txt"), "hello hidden").unwrap();
    let inner = "hello from inside the zip ".repeat(5);
    fs::write(root.join("big-report.fakezip"), fakezip(&[("docs/inner.txt", &inner)])).unwrap();
    (tmp, root)
}

/// Search the local tree through both the parallel local walker and the
/// generic walkers; they must agree.
fn find_both(root: &Path, configure: impl Fn(&mut Query)) -> Vec<String> {
    let fast = find(local(root), &configure);
    let generic = find(opaque(root), &configure);
    assert_eq!(fast, generic, "local and generic walkers disagree");
    fast
}

// -- names and walking ------------------------------------------------------------

#[test]
fn walk_options() {
    let (_tmp, project) = project();
    let p = &project;
    assert_eq!(find_both(p, |q| q.names = "*.pdf".into()), ["Report-2026.PDF", "report.pdf"]);
    assert_eq!(
        find_both(p, |q| q.names = "*.py".into()),
        ["src/deep/deeper/config.py", "src/main.py", "src/util.py"]
    );
    assert_eq!(
        find_both(p, |q| {
            q.names = "*.py".into();
            q.max_depth = Some(1);
        }),
        ["src/main.py", "src/util.py"]
    );
    assert!(find_both(p, |q| {
        q.names = "*.py".into();
        q.recursive = false;
    })
    .is_empty());
    assert_eq!(find_both(p, |q| q.names = "*.txt".into()), [".hidden/secret.txt"]);
    assert!(find_both(p, |q| {
        q.names = "*.txt".into();
        q.include_hidden = false;
    })
    .is_empty());
    assert_eq!(
        find_both(p, |q| {
            q.names = "d*".into();
            q.kinds = Kinds::Dirs;
        }),
        ["src/deep", "src/deep/deeper"]
    );
    assert_eq!(
        find_both(p, |q| {
            q.kinds = Kinds::Files;
            q.min_size = Some(1000);
        }),
        ["Report-2026.PDF"]
    );
    assert_eq!(
        find_both(p, |q| {
            q.names = "*.pdf".into();
            q.max_size = Some(100);
        }),
        ["report.pdf"]
    );

    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64();
    let old = SystemTime::now() - Duration::from_secs(86400 * 30);
    fs::File::options().write(true).open(p.join("notes.md")).unwrap().set_modified(old).unwrap();
    assert!(find_both(p, |q| {
        q.names = "*.md".into();
        q.newer_than = Some(now - 86400.0);
    })
    .is_empty());
    assert_eq!(
        find_both(p, |q| {
            q.names = "*.md".into();
            q.older_than = Some(now - 86400.0);
        }),
        ["notes.md"]
    );
}

#[test]
fn everything_by_default_and_hits_carry_entries() {
    let (_tmp, project) = project();
    let all = find_both(&project, |_| {});
    assert_eq!(
        all,
        [
            ".hidden",
            ".hidden/secret.txt",
            "Report-2026.PDF",
            "big-report.fakezip",
            "notes.md",
            "report.pdf",
            "src",
            "src/deep",
            "src/deep/deeper",
            "src/deep/deeper/config.py",
            "src/main.py",
            "src/util.py",
        ]
    );
    let q = Query { names: "util.py".into(), ..Query::new(vec![local(&project)]) };
    let (hits, progress) = collect(&q);
    let hit = &hits[0];
    assert_eq!(hit.entry.name, "util.py");
    assert_eq!(hit.entry.size, 28);
    assert!(hit.entry.mtime > 0.0 && !hit.entry.is_dir);
    assert_eq!(hit.location.path, project.join("src").join("util.py").to_string_lossy());
    assert_eq!(progress.hits(), 1);
    assert_eq!(progress.folders(), 5); // project, .hidden, src, deep, deeper
    assert_eq!(progress.files(), 8);
}

#[test]
fn regex_names_and_case() {
    let (_tmp, project) = project();
    assert_eq!(
        find_both(&project, |q| {
            q.names = r"^report-\d{4}".into();
            q.name_regex = true;
        }),
        ["Report-2026.PDF"]
    );
    assert!(find_both(&project, |q| {
        q.names = "*.PY".into();
        q.name_case = true;
    })
    .is_empty());
    assert_eq!(find_both(&project, |q| q.names = "*.fakezip *.pdf|report*".into()), ["big-report.fakezip"]);
}

#[test]
fn invalid_patterns_are_reported_up_front() {
    let progress = SearchProgress::new();
    let q = Query { names: "(".into(), name_regex: true, ..Query::default() };
    assert!(matches!(oc_search::run(&q, |_| {}, &progress, || false), Err(SearchError::NamePattern(_))));
    let q = Query { text: "[".into(), text_regex: true, ..Query::default() };
    let err = oc_search::run(&q, |_| {}, &progress, || false).unwrap_err();
    assert!(err.to_string().starts_with("invalid text pattern"));
}

// -- contents -------------------------------------------------------------------

#[test]
fn content_search() {
    let (_tmp, project) = project();
    let p = &project;
    let text = |t: &str| {
        let t = t.to_string();
        move |q: &mut Query| q.text = t.clone()
    };
    assert_eq!(find_both(p, text("hello")), [".hidden/secret.txt", "src/main.py"]);
    assert_eq!(find_both(p, text("quick")), ["notes.md"]); // case-insensitive
    assert!(find_both(p, |q| {
        q.text = "quick".into();
        q.text_case = true;
    })
    .is_empty());
    assert_eq!(
        find_both(p, |q| {
            q.names = "*.py".into();
            q.text = r"def \w+\(\)".into();
            q.text_regex = true;
        }),
        ["src/main.py", "src/util.py"]
    );
    // "nothing" isn't the word "no".
    assert!(find_both(p, |q| {
        q.names = "*.md".into();
        q.text = "no".into();
        q.whole_words = true;
    })
    .is_empty());
    assert_eq!(
        find_both(p, |q| {
            q.names = "*.py".into();
            q.text = "hello".into();
            q.not_containing = true;
        }),
        ["src/deep/deeper/config.py", "src/util.py"]
    );
    for root in [local(p), opaque(p)] {
        let q = Query { names: "*.py".into(), text: "print".into(), ..Query::new(vec![root]) };
        let (hits, _) = collect(&q);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "print('hello world')");
    }
}

#[test]
fn content_criteria_skip_folders() {
    let (_tmp, project) = project();
    let found = find_both(&project, |q| {
        q.text = "zzz-not-there".into();
        q.not_containing = true;
    });
    assert!(!found.contains(&"src".to_string()));
    assert!(found.contains(&"notes.md".to_string()));
}

#[test]
fn text_across_chunk_borders() {
    let tmp = TempDir::new().unwrap();
    let mut data = vec![b'x'; CHUNK - 3];
    data.extend_from_slice(b"NEEDLE");
    data.extend(std::iter::repeat_n(b'y', 100));
    fs::write(tmp.path().join("big.log"), data).unwrap();
    assert_eq!(
        find_both(tmp.path(), |q| {
            q.names = "*.log".into();
            q.text = "needle".into();
        }),
        ["big.log"]
    );
}

#[test]
fn unicode_content_case_insensitive() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("obst.txt"), "Wir kaufen GRÜNE ÄPFEL.\n").unwrap();
    fs::write(tmp.path().join("bin.dat"), b"\xff\xfe\x00 binary \x00 \xc3").unwrap();
    let found = find_both(tmp.path(), |q| q.text = "grüne äpfel".into());
    assert_eq!(found, ["obst.txt"]);
    assert_eq!(find_both(tmp.path(), |q| q.text = "binary".into()), ["bin.dat"]);
}

// -- archives -----------------------------------------------------------------------

#[test]
fn search_inside_archives() {
    let (_tmp, project) = project();
    let p = &project;
    assert!(find_both(p, |q| q.names = "inner.txt".into()).is_empty());
    assert_eq!(
        find_both(p, |q| {
            q.names = "inner.txt".into();
            q.search_archives = true;
        }),
        ["big-report.fakezip/docs/inner.txt"]
    );
    for root in [local(p), opaque(p)] {
        let q = Query {
            names: "*.txt".into(),
            text: "inside".into(),
            search_archives: true,
            archive_opener: Some(Arc::new(FakeZip)),
            ..Query::new(vec![root])
        };
        let (hits, progress) = collect(&q);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].location.fs.type_name(), "fake archive");
        assert_eq!(hits[0].location.path, "docs/inner.txt");
        assert!(hits[0].snippet.starts_with("hello from inside"));
        assert!(progress.errors().is_empty());
    }
}

#[test]
fn archives_need_an_opener_and_respect_depth() {
    let (_tmp, project) = project();
    let mut q = Query { names: "inner.txt".into(), search_archives: true, ..Query::new(vec![local(&project)]) };
    assert!(collect(&q).0.is_empty()); // no opener: archives stay closed
    q.archive_opener = Some(Arc::new(FakeZip));
    q.max_depth = Some(1); // the archive is a level, docs/ another one
    assert!(collect(&q).0.is_empty());
    q.max_depth = Some(2);
    assert_eq!(rels(&collect(&q).0), ["big-report.fakezip/docs/inner.txt"]);
}

#[test]
fn broken_archives_are_errors_not_failures() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("bad.fakezip"), "zz not hex").unwrap();
    fs::write(tmp.path().join("ok.txt"), "x").unwrap();
    for root in [local(tmp.path()), opaque(tmp.path())] {
        let q = Query {
            search_archives: true,
            archive_opener: Some(Arc::new(FakeZip)),
            ..Query::new(vec![root])
        };
        let (hits, progress) = collect(&q);
        assert_eq!(rels(&hits), ["bad.fakezip", "ok.txt"]);
        assert_eq!(progress.errors(), ["bad.fakezip: not a fake archive"]);
    }
}

#[test]
fn many_archives_hits_stay_readable() {
    let tmp = TempDir::new().unwrap();
    let count = 24;
    for i in 0..count {
        let track = format!("disc1/cd/track{i}.mp3");
        let content = format!("mp3 {i}");
        fs::write(tmp.path().join(format!("album{i}.fakezip")), fakezip(&[(&track, &content)])).unwrap();
    }
    let q = Query {
        names: "*.mp3".into(),
        search_archives: true,
        archive_opener: Some(Arc::new(FakeZip)),
        ..Query::new(vec![local(tmp.path())])
    };
    let results = SearchResultsFS::new(q);
    results.run(|| false).unwrap();
    assert_eq!(results.hits().len(), count);
    let mut contents: Vec<String> = results
        .hits()
        .iter()
        .map(|h| {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut h.location.open_read().unwrap(), &mut s).unwrap();
            s
        })
        .collect();
    contents.sort();
    let mut expected: Vec<String> = (0..count).map(|i| format!("mp3 {i}")).collect();
    expected.sort();
    assert_eq!(contents, expected);
}

// -- other file systems ---------------------------------------------------------------

#[test]
fn search_on_another_file_system() {
    let mem = Arc::new(MemFs::new());
    mem.mkdir_p("photos/2026");
    mem.put("photos/2026/beach.jpg", b"jpg");
    mem.put("photos/notes.txt", b"beach day");
    let root = VPath::new(mem.clone(), "");
    assert_eq!(find(root.clone(), |q| q.names = "beach".into()), ["photos/2026/beach.jpg"]);
    assert_eq!(find(root.clone(), |q| q.text = "beach".into()), ["photos/notes.txt"]);
    // Unlimited depth uses one walk, no listing per folder.
    assert_eq!(mem.list_calls.load(Ordering::Relaxed), 0);
    // A depth limit lists folder by folder.
    assert_eq!(
        find(root, |q| {
            q.names = "*".into();
            q.max_depth = Some(1);
        }),
        ["photos", "photos/2026", "photos/notes.txt"]
    );
    assert_eq!(mem.list_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn walk_mode_prunes_hidden_folders() {
    let mem = Arc::new(MemFs::new());
    mem.put(".git/objects/x.txt", b"a");
    mem.put("a/.cache/y.txt", b"b");
    mem.put("a/z.txt", b"c");
    let root = VPath::new(mem, "");
    assert_eq!(
        find(root.clone(), |q| {
            q.names = "*.txt".into();
            q.include_hidden = false;
        }),
        ["a/z.txt"]
    );
    assert_eq!(find(root, |q| q.names = "*.txt".into()), [".git/objects/x.txt", "a/.cache/y.txt", "a/z.txt"]);
}

#[test]
fn cloud_only_files_skipped_for_content() {
    let cloud = Arc::new(MemFs { online_only: true, ..MemFs::new() });
    cloud.put("a.txt", b"needle");
    let root = VPath::new(cloud.clone(), "");
    let q = Query { text: "needle".into(), ..Query::new(vec![root.clone()]) };
    let (hits, progress) = collect(&q);
    assert!(hits.is_empty());
    assert_eq!(progress.hits(), 0);
    assert_eq!(progress.skipped_online_only(), 1);
    assert_eq!(cloud.bytes_read.load(Ordering::Relaxed), 0);
    // Also with Not containing: unknown contents are no hit either.
    let q = Query { text: "needle".into(), not_containing: true, ..Query::new(vec![root.clone()]) };
    assert!(collect(&q).0.is_empty());
    assert_eq!(
        find(root.clone(), |q| {
            q.text = "needle".into();
            q.include_online_only = true;
        }),
        ["a.txt"]
    );
    assert_eq!(find(root, |q| q.names = "*.txt".into()), ["a.txt"]); // names alone: no download needed
}

#[test]
fn cloud_only_archives_are_not_opened() {
    let cloud = Arc::new(MemFs { online_only: true, ..MemFs::new() });
    cloud.put("a.fakezip", &fakezip(&[("inner.txt", "x")]));
    let root = VPath::new(cloud.clone(), "");
    assert!(find(root.clone(), |q| {
        q.names = "inner.txt".into();
        q.search_archives = true;
    })
    .is_empty());
    assert_eq!(cloud.bytes_read.load(Ordering::Relaxed), 0);
    assert_eq!(
        find(root, |q| {
            q.names = "inner.txt".into();
            q.search_archives = true;
            q.include_online_only = true;
        }),
        ["a.fakezip/inner.txt"]
    );
}

// -- cancellation and errors ------------------------------------------------------------

#[test]
fn cancel_and_errors() {
    let (_tmp, project) = project();
    for root in [local(&project), opaque(&project)] {
        let delivered = Cell::new(0usize);
        let progress = SearchProgress::new();
        let q = Query::new(vec![root]);
        oc_search::run(&q, |_| delivered.set(delivered.get() + 1), &progress, || delivered.get() >= 2).unwrap();
        assert_eq!(delivered.get(), 2);
        assert_eq!(progress.hits(), 2);
    }
    for root in [local(&project.join("missing")), opaque(&project.join("missing"))] {
        let (hits, progress) = collect(&Query::new(vec![root]));
        assert!(hits.is_empty());
        let errors = progress.errors();
        assert!(!errors.is_empty() && errors[0].contains("missing"), "{errors:?}");
    }
}

#[test]
fn cancelled_before_start_delivers_nothing() {
    let (_tmp, project) = project();
    let progress = SearchProgress::new();
    let mut n = 0;
    oc_search::run(&Query::new(vec![local(&project)]), |_| n += 1, &progress, || true).unwrap();
    assert_eq!(n, 0);
}

#[cfg(unix)]
#[test]
fn folder_links_are_reported_but_not_followed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("real")).unwrap();
    fs::write(root.join("real/file.txt"), "x").unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
    std::os::unix::fs::symlink(root, root.join("real/loop")).unwrap();
    std::os::unix::fs::symlink(root.join("gone"), root.join("dangling.txt")).unwrap();
    assert_eq!(find_both(root, |q| q.names = "*.txt".into()), ["dangling.txt", "real/file.txt"]);
    assert_eq!(
        find_both(root, |q| q.kinds = Kinds::Dirs),
        ["link", "real", "real/loop"] // links to folders count as folders
    );
}

#[cfg(unix)]
#[test]
fn unreadable_files_are_errors() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("locked.txt");
    fs::write(&f, "secret").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&f).is_ok() {
        return; // running as root
    }
    let q = Query { text: "secret".into(), ..Query::new(vec![local(tmp.path())]) };
    let (hits, progress) = collect(&q);
    assert!(hits.is_empty());
    assert_eq!(progress.error_count(), 1);
    assert!(progress.errors()[0].starts_with("locked.txt: "));
}

// -- results as a folder --------------------------------------------------------------

fn listing(root: &VPath) -> Vec<String> {
    root.list_dir(ListOptions::default())
        .unwrap()
        .into_iter()
        .map(|e| e.name.replace(MAIN_SEPARATOR, "/"))
        .collect()
}

#[test]
fn results_folder_acts_on_real_files() {
    let (tmp, project) = project();
    let query = Query { names: "*.py".into(), ..Query::new(vec![local(&project)]) };
    let results = Arc::new(SearchResultsFS::new(query));
    assert!(results.status_text().unwrap().starts_with("searching… "));
    results.run(|| false).unwrap();
    assert!(!results.is_running() && !results.was_cancelled());
    let root = VPath::new(results.clone(), "");
    assert_eq!(listing(&root), ["src/deep/deeper/config.py", "src/main.py", "src/util.py"]);
    assert_eq!(root.parent(), Some(local(&project))); // ".." goes back to where the search started
    assert!(root.display().starts_with("Search: *.py in"));
    assert_eq!(root.display(), "Search: *.py in project");
    assert_eq!(root.name(), "Search results");
    assert_eq!(results.status_text().unwrap(), "3 hits");
    assert!(root.is_dir() && root.exists());
    assert!(!results.capabilities().writable);

    let hit = root.child(&[&format!("src{MAIN_SEPARATOR}util.py")]);
    assert_eq!(hit.resolved(), local(&project.join("src").join("util.py")));
    assert_eq!(hit.local_path(), Some(project.join("src").join("util.py")));
    assert_eq!(hit.parent(), Some(root.clone()));
    assert!(hit.exists() && !hit.is_dir());
    assert_eq!(hit.stat().unwrap().name, "util.py");
    let mut text = String::new();
    std::io::Read::read_to_string(&mut hit.open_read().unwrap(), &mut text).unwrap();
    assert!(text.starts_with("def helper"));
    assert!(root.child(&["nope"]).stat().unwrap_err().is_not_found());
    assert!(results.list_dir("src", ListOptions::default()).is_err());
    assert!(results.mkdir("x", false).is_err());

    // Bulk move of all hits into one folder, then the results forget what moved.
    let dest = tmp.path().join("collected");
    fs::create_dir(&dest).unwrap();
    for e in root.list_dir(ListOptions::default()).unwrap() {
        let real = root.child(&[&e.name]).resolved();
        let src = Path::new(&real.path);
        fs::rename(src, dest.join(src.file_name().unwrap())).unwrap();
    }
    let mut moved: Vec<String> =
        fs::read_dir(&dest).unwrap().map(|d| d.unwrap().file_name().to_string_lossy().into_owned()).collect();
    moved.sort();
    assert_eq!(moved, ["config.py", "main.py", "util.py"]);
    let before = results.changes();
    assert_eq!(results.prune(), 3);
    assert!(results.changes() > before);
    assert!(listing(&root).is_empty());
    assert_eq!(results.prune(), 0);
}

#[test]
fn results_forget_what_was_trashed() {
    let (_tmp, project) = project();
    let query = Query { names: "*.py".into(), ..Query::new(vec![local(&project)]) };
    let results = Arc::new(SearchResultsFS::new(query));
    results.run(|| false).unwrap();
    let root = VPath::new(results.clone(), "");
    let before = results.changes();
    // A folder takes the hits below it along; nothing is looked up on disk.
    let removed = HashSet::from([local(&project.join("src").join("deep")), local(&project.join("src").join("util.py"))]);
    assert_eq!(results.forget(&removed), 2);
    assert!(results.changes() > before);
    assert_eq!(listing(&root), ["src/main.py"]);
    assert_eq!(results.forget(&removed), 0);
    assert_eq!(results.forget(&HashSet::new()), 0);
}

#[test]
fn results_same_relative_path_from_two_roots() {
    let tmp = TempDir::new().unwrap();
    for side in ["a", "b"] {
        fs::create_dir(tmp.path().join(side)).unwrap();
        fs::write(tmp.path().join(side).join("x.txt"), side).unwrap();
    }
    let query = Query {
        names: "x.txt".into(),
        ..Query::new(vec![local(&tmp.path().join("a")), local(&tmp.path().join("b"))])
    };
    let results = Arc::new(SearchResultsFS::new(query));
    results.run(|| false).unwrap();
    // Several roots: prefixed with the root's name.
    assert_eq!(listing(&VPath::new(results.clone(), "")), ["a/x.txt", "b/x.txt"]);
    assert!(results.display("").ends_with(" in a, b"));
}

#[test]
fn duplicate_relative_paths_get_numbered() {
    let tmp = TempDir::new().unwrap();
    for side in ["one/same", "two/same"] {
        fs::create_dir_all(tmp.path().join(side)).unwrap();
        fs::write(tmp.path().join(side).join("x.txt"), side).unwrap();
    }
    let roots = vec![local(&tmp.path().join("one/same")), local(&tmp.path().join("two/same"))];
    let results = Arc::new(SearchResultsFS::new(Query { names: "x.txt".into(), ..Query::new(roots) }));
    results.run(|| false).unwrap();
    let names = listing(&VPath::new(results.clone(), ""));
    assert_eq!(names, ["same/x.txt", "same/x.txt (2)"]);
    let second = VPath::new(results.clone(), "same/x.txt (2)".replace('/', std::path::MAIN_SEPARATOR_STR));
    assert!(second.resolved().path.contains("two"));
}

#[test]
fn relocate_follows_renamed_files_and_folders() {
    let mem = Arc::new(MemFs::new());
    mem.put("docs/a.txt", b"a");
    mem.put("docs/sub/b.txt", b"b");
    mem.put("other/c.txt", b"c");
    let root = VPath::new(mem.clone(), "");
    let results = Arc::new(SearchResultsFS::new(Query { names: "*.txt".into(), ..Query::new(vec![root.clone()]) }));
    results.run(|| false).unwrap();
    let sep = MAIN_SEPARATOR.to_string();

    // Rename a file.
    mem.rename("other/c.txt", "other/c2.txt").unwrap();
    let moved = HashMap::from([(root.child(&["other", "c.txt"]), root.child(&["other", "c2.txt"]))]);
    results.relocate(&moved);
    // Rename a folder holding two hits.
    mem.rename("docs", "papers").unwrap();
    results.relocate(&HashMap::from([(root.child(&["docs"]), root.child(&["papers"]))]));

    let res_root = VPath::new(results.clone(), "");
    assert_eq!(listing(&res_root), ["other/c2.txt", "papers/a.txt", "papers/sub/b.txt"]);
    let b = res_root.child(&[&["papers", "sub", "b.txt"].join(&sep)]);
    assert_eq!(b.resolved(), root.child(&["papers", "sub", "b.txt"]));
    assert_eq!(b.stat().unwrap().name, "b.txt");
    let c = results.hits().into_iter().find(|h| h.entry.name == "c2.txt").unwrap();
    assert_eq!(c.relative, ["other", "c2.txt"].join(&sep));
    assert_eq!(results.prune(), 0); // all still exist under their new names

    let changes = results.changes();
    results.relocate(&HashMap::new());
    assert_eq!(results.changes(), changes);
}

#[test]
fn status_text_while_running_and_after() {
    let mem = Arc::new(MemFs::new());
    let results = SearchResultsFS::new(Query::new(vec![VPath::new(mem.clone(), "")]));
    assert_eq!(results.status_text().unwrap(), "searching… 0 folders, 0 files · 0 hits");
    let hit = Hit {
        location: VPath::new(mem.clone(), "a"),
        entry: oc_vfs::Entry::file("a", 1, 0.0),
        relative: "a".into(),
        snippet: String::new(),
    };
    results.add(hit);
    assert_eq!(results.status_text().unwrap(), "searching… 0 folders, 0 files · 1 hit");
    results.finish(true);
    assert_eq!(results.status_text().unwrap(), "1 hit · stopped");
    assert!(results.elapsed() <= results.elapsed() + Duration::from_millis(1));

    let cloud = Arc::new(MemFs { online_only: true, ..MemFs::new() });
    cloud.put("x.txt", b"x");
    let q = Query {
        text: "x".into(),
        ..Query::new(vec![VPath::new(cloud, ""), VPath::new(mem, "missing")])
    };
    let results = SearchResultsFS::new(q);
    results.run(|| false).unwrap();
    assert_eq!(results.status_text().unwrap(), "0 hits · 1 cloud-only files not searched · 1 unreadable");
}

#[test]
fn describe_query() {
    let (_tmp, project) = project();
    let mut q = Query { names: "  ".into(), ..Query::new(vec![local(&project)]) };
    assert_eq!(q.describe(), "* in project");
    q.text = "total".into();
    assert_eq!(q.describe(), "* containing “total” in project");
    q.not_containing = true;
    assert_eq!(q.describe(), "* without “total” in project");
    let mem = Arc::new(MemFs::new());
    let q = Query::new(vec![VPath::new(mem, "")]);
    assert_eq!(q.describe(), "* in mem:/"); // nameless root: its display
}

#[test]
fn background_search_can_be_cancelled() {
    let tmp = TempDir::new().unwrap();
    for i in 0..50 {
        let d = tmp.path().join(format!("d{i}"));
        fs::create_dir(&d).unwrap();
        for j in 0..20 {
            fs::write(d.join(format!("f{j}.txt")), "x").unwrap();
        }
    }
    let results = Arc::new(SearchResultsFS::new(Query::new(vec![local(tmp.path())])));
    let handle = oc_search::start(&results).unwrap();
    handle.join();
    assert!(!results.is_running());
    assert_eq!(results.hit_count(), 50 + 50 * 20);

    let results = Arc::new(SearchResultsFS::new(Query::new(vec![local(tmp.path())])));
    let handle = oc_search::start(&results).unwrap();
    handle.cancel();
    handle.join();
    assert!(!results.is_running() && results.was_cancelled());
    assert!(results.status_text().unwrap().contains("stopped"));

    let bad = Arc::new(SearchResultsFS::new(Query { names: "(".into(), name_regex: true, ..Query::default() }));
    assert!(oc_search::start(&bad).is_err());
    assert!(!bad.is_running());
}

/// Timing on a real tree: `OC_SEARCH_BENCH=/some/dir cargo test --release -p oc-search -- --ignored bench`.
#[test]
#[ignore]
fn bench_real_tree() {
    let Ok(dir) = std::env::var("OC_SEARCH_BENCH") else { return };
    for (label, configure) in [
        ("names *.rs", Box::new(|q: &mut Query| q.names = "*.rs".into()) as Box<dyn Fn(&mut Query)>),
        ("content fn main", Box::new(|q: &mut Query| {
            q.names = "*.rs".into();
            q.text = "fn main".into();
        })),
    ] {
        for root in [local(Path::new(&dir)), opaque(Path::new(&dir))] {
            let mut q = Query::new(vec![root.clone()]);
            configure(&mut q);
            let t = std::time::Instant::now();
            let (hits, p) = collect(&q);
            eprintln!(
                "{label} local={} : {} hits, {} files, {} folders in {:?}",
                root.local_path().is_some(),
                hits.len(),
                p.files(),
                p.folders(),
                t.elapsed()
            );
        }
    }
}

/// Delegates to a MemFs but panics when a file is opened (a buggy back end).
struct PanickyFs(MemFs);

impl FileSystem for PanickyFs {
    fn type_name(&self) -> String {
        "panicky".into()
    }
    fn list_dir(&self, path: &str, opts: ListOptions) -> oc_vfs::Result<Vec<oc_vfs::Entry>> {
        self.0.list_dir(path, opts)
    }
    fn stat(&self, path: &str) -> oc_vfs::Result<oc_vfs::Entry> {
        self.0.stat(path)
    }
    fn parent_path(&self, path: &str) -> Option<String> {
        self.0.parent_path(path)
    }
    fn join(&self, path: &str, names: &[&str]) -> String {
        self.0.join(path, names)
    }
    fn basename(&self, path: &str) -> String {
        self.0.basename(path)
    }
    fn display(&self, path: &str) -> String {
        self.0.display(path)
    }
    fn open_read(&self, _path: &str) -> oc_vfs::Result<Box<dyn oc_vfs::ReadStream>> {
        panic!("back end bug")
    }
    fn walk<'a>(&'a self, path: &str) -> oc_vfs::Result<oc_vfs::WalkIter<'a>> {
        self.0.walk(path)
    }
}

#[test]
fn a_panicking_file_system_still_finishes_the_results() {
    let mem = MemFs::new();
    mem.put("a.txt", b"x");
    mem.put("b.txt", b"x");
    let root = VPath::new(Arc::new(PanickyFs(mem)), "");
    // Names alone never open a file: two hits.
    let results = Arc::new(SearchResultsFS::new(Query::new(vec![root.clone()])));
    oc_search::start(&results).unwrap().join();
    assert_eq!(results.hit_count(), 2);
    // A content search panics in the back end: reported, not "searching…" forever.
    let results = Arc::new(SearchResultsFS::new(Query { text: "x".into(), ..Query::new(vec![root]) }));
    oc_search::start(&results).unwrap().join();
    assert!(!results.is_running());
    assert_eq!(results.progress().error_count(), 1);
    assert!(results.progress().errors()[0].starts_with("search failed: "));
    assert_eq!(results.status_text().unwrap(), "0 hits · 1 unreadable");
}

#[test]
fn searching_from_a_hit_searches_the_real_folder() {
    let (_tmp, project) = project();
    let results = Arc::new(SearchResultsFS::new(Query { names: "src".into(), ..Query::new(vec![local(&project)]) }));
    results.run(|| false).unwrap();
    let alias = VPath::new(results.clone(), "src");
    assert_eq!(alias.resolved(), local(&project.join("src")));
    let q = Query { names: "*.py".into(), max_depth: Some(0), ..Query::new(vec![alias]) };
    let (hits, progress) = collect(&q);
    assert!(progress.errors().is_empty(), "{:?}", progress.errors());
    assert_eq!(rels(&hits), ["main.py", "util.py"]);
    assert!(hits.iter().all(|h| h.location.exists() && h.location.fs.type_name() == "local disk"));
}

#[test]
fn kinds_filter_without_stat_agrees_with_generic_walk() {
    let (_tmp, project) = project();
    let dirs = find_both(&project, |q| q.kinds = Kinds::Dirs);
    assert_eq!(dirs, [".hidden", "src", "src/deep", "src/deep/deeper"]);
    let files = find_both(&project, |q| {
        q.kinds = Kinds::Files;
        q.names = "*e*".into();
    });
    assert_eq!(files, [".hidden/secret.txt", "Report-2026.PDF", "big-report.fakezip", "notes.md", "report.pdf"]);
}
