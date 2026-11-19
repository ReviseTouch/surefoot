//! The walk against real directories.

use std::collections::BTreeSet;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use surefoot::{Event, Threads, Visit, Walk};

/// A tree wide and deep enough that eight threads all get work:
/// `fan` directories at each of `depth` levels, `files` files in each.
fn tree(root: &Path, fan: usize, depth: usize, files: usize) {
    for f in 0..files {
        fs::write(root.join(format!("f{f}.txt")), b"x").unwrap();
    }
    if depth == 0 {
        return;
    }
    for d in 0..fan {
        let dir = root.join(format!("d{d}"));
        fs::create_dir(&dir).unwrap();
        tree(&dir, fan, depth - 1, files);
    }
}

/// What a walk nobody could get wrong finds.
fn reference(root: &Path) -> BTreeSet<PathBuf> {
    let mut all = BTreeSet::from([root.to_owned()]);
    let mut todo = vec![root.to_owned()];
    while let Some(dir) = todo.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                todo.push(entry.path());
            }
            all.insert(entry.path());
        }
    }
    all
}

fn paths(walk: &Walk) -> BTreeSet<PathBuf> {
    let mut all = BTreeSet::new();
    walk.for_each(false, |entry| {
        let entry = entry.expect("nothing here is unreadable");
        assert!(all.insert(entry.path.clone()), "{:?} twice", entry.path);
        ControlFlow::Continue(())
    });
    all
}

#[test]
fn every_entry_is_found_once_on_one_thread_on_eight_and_on_a_number_that_moves() {
    let tmp = tempfile::tempdir().unwrap();
    tree(tmp.path(), 4, 4, 3);
    let expect = reference(tmp.path());
    assert_eq!(expect.len(), 1 + 340 + 341 * 3);
    for threads in [
        Threads::Fixed(1),
        Threads::Fixed(8),
        Threads::Static,
        Threads::Auto,
        Threads::Range { min: 1, max: 8 },
    ] {
        let walk = Walk::new([tmp.path()]).threads(threads);
        assert_eq!(paths(&walk), expect, "{threads:?}");
    }
}

#[test]
fn the_report_counts_what_was_seen_and_names_the_threads() {
    let tmp = tempfile::tempdir().unwrap();
    tree(tmp.path(), 3, 2, 2);
    let report = Walk::new([tmp.path()])
        .threads(Threads::Fixed(3))
        .for_each(false, |_| ControlFlow::Continue(()));
    assert_eq!(report.seen as usize, reference(tmp.path()).len());
    assert_eq!(report.dirs, 1 + 3 + 9);
    assert_eq!(report.unreadable, 0);
    assert!(!report.cancelled);
    assert_eq!((report.threads.first, report.threads.last), (3, 3));
    // A fixed number asked nothing about the disk.
    assert_eq!(report.medium, None);
}

#[test]
fn several_roots_are_all_walked_and_a_root_that_is_a_file_is_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b, lone) = (
        tmp.path().join("a"),
        tmp.path().join("b"),
        tmp.path().join("lone"),
    );
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    fs::write(a.join("x"), b"").unwrap();
    fs::write(b.join("y"), b"").unwrap();
    fs::write(&lone, b"").unwrap();
    let found = paths(&Walk::new([&a, &b, &lone]));
    let expect: BTreeSet<PathBuf> = [a.clone(), a.join("x"), b.clone(), b.join("y"), lone].into();
    assert_eq!(found, expect);
}

#[test]
fn a_visitor_decides_what_is_taken_and_what_is_entered() {
    let tmp = tempfile::tempdir().unwrap();
    for dir in ["take", "shallow", "pass", "prune"] {
        fs::create_dir(tmp.path().join(dir)).unwrap();
        fs::write(tmp.path().join(dir).join("inside"), b"").unwrap();
    }
    let mut taken = BTreeSet::new();
    Walk::new([tmp.path()]).run(
        |found| {
            let path = found.path().to_owned();
            match found.file_name().to_str().unwrap() {
                "shallow" => Visit::TakeShallow(path),
                "pass" => Visit::Pass,
                "prune" => Visit::Prune,
                _ => Visit::Take(path),
            }
        },
        |event| {
            if let Event::Batch(batch) = event {
                taken.extend(batch);
            }
            ControlFlow::Continue(())
        },
    );
    let rel: BTreeSet<String> = taken
        .iter()
        .filter_map(|p| p.strip_prefix(tmp.path()).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    let expect: BTreeSet<String> = ["", "take", "take/inside", "shallow", "pass/inside"]
        .map(String::from)
        .into();
    assert_eq!(rel, expect);
}

#[test]
fn what_the_visitor_turns_away_is_never_asked_for_its_metadata() {
    // The visitor is the only thing that can ask, so this is its contract
    // rather than a measurement: metadata arrives only where it was called.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("wanted"), b"twelve bytes").unwrap();
    fs::write(tmp.path().join("unwanted"), b"x").unwrap();
    let mut sizes = Vec::new();
    Walk::new([tmp.path()]).run(
        |found| {
            if found.file_name() == "wanted" {
                Visit::Take(found.metadata().unwrap().len())
            } else {
                Visit::Pass
            }
        },
        |event| {
            if let Event::Batch(batch) = event {
                sizes.extend(batch);
            }
            ControlFlow::Continue(())
        },
    );
    assert_eq!(sizes, [12]);
}

#[test]
fn depth_stops_the_walk_where_it_is_told() {
    let tmp = tempfile::tempdir().unwrap();
    tree(tmp.path(), 2, 3, 1);
    let depth_of = |p: &Path| p.strip_prefix(tmp.path()).unwrap().components().count();
    for max in 0..4 {
        let found = paths(&Walk::new([tmp.path()]).max_depth(Some(max)));
        let expect: BTreeSet<PathBuf> = reference(tmp.path())
            .into_iter()
            .filter(|p| depth_of(p) <= max)
            .collect();
        assert_eq!(found, expect, "max depth {max}");
    }
}

#[test]
fn hidden_names_are_left_out_when_asked_and_a_hidden_root_is_still_walked() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(".root");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".git").join("config"), b"").unwrap();
    fs::write(root.join(".env"), b"").unwrap();
    fs::write(root.join("seen"), b"").unwrap();
    assert_eq!(paths(&Walk::new([&root])).len(), 5);
    let shown = paths(&Walk::new([&root]).skip_hidden(true));
    assert_eq!(shown, [root.clone(), root.join("seen")].into());
}

#[test]
fn a_root_that_is_not_there_is_reported_and_the_others_are_walked() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("x"), b"").unwrap();
    let gone = tmp.path().join("gone");
    let (mut found, mut refused) = (0, Vec::new());
    let report = Walk::new([gone.as_path(), tmp.path()]).for_each(false, |entry| {
        match entry {
            Ok(_) => found += 1,
            Err(refusal) => refused.push((refusal.path, refusal.error.kind())),
        }
        ControlFlow::Continue(())
    });
    assert_eq!(found, 2);
    assert_eq!(refused, [(gone, std::io::ErrorKind::NotFound)]);
    assert_eq!(report.unreadable, 1);
}

#[test]
#[cfg(unix)]
fn a_directory_that_refuses_is_named_and_the_walk_goes_on_around_it() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let shut = tmp.path().join("shut");
    fs::create_dir(&shut).unwrap();
    fs::write(shut.join("inside"), b"").unwrap();
    fs::write(tmp.path().join("beside"), b"").unwrap();
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&shut).is_ok() {
        // Root reads what it likes; nothing to learn here.
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let (mut found, mut refused) = (BTreeSet::new(), Vec::new());
    Walk::new([tmp.path()]).for_each(false, |entry| {
        match entry {
            Ok(entry) => drop(found.insert(entry.path)),
            Err(refusal) => refused.push((refusal.path, refusal.error.kind())),
        }
        ControlFlow::Continue(())
    });
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
    // The directory itself was listed by its parent; only its inside is dark.
    let expect: BTreeSet<PathBuf> = [
        tmp.path().to_owned(),
        shut.clone(),
        tmp.path().join("beside"),
    ]
    .into();
    assert_eq!(found, expect);
    assert_eq!(refused, [(shut, std::io::ErrorKind::PermissionDenied)]);
}

#[test]
#[cfg(unix)]
fn a_link_is_a_link_until_links_are_followed_and_a_loop_is_named_not_walked() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    fs::write(b.join("file"), b"").unwrap();
    // Two links that only close the circle together: neither points above itself.
    symlink(&b, a.join("to_b")).unwrap();
    symlink(&a, b.join("to_a")).unwrap();

    let mut kinds = Vec::new();
    Walk::new([&a]).for_each(false, |entry| {
        let entry = entry.unwrap();
        kinds.push((entry.path, entry.file_type.is_symlink()));
        ControlFlow::Continue(())
    });
    kinds.sort();
    assert_eq!(kinds, [(a.clone(), false), (a.join("to_b"), true)]);

    let (mut found, mut loops) = (BTreeSet::new(), Vec::new());
    Walk::new([&a]).follow_links(true).for_each(false, |entry| {
        match entry {
            Ok(entry) => {
                assert!(!entry.file_type.is_symlink(), "described by its target");
                found.insert(entry.path);
            }
            Err(refusal) => {
                assert!(refusal.is_loop(), "{refusal:?}");
                loops.push(refusal.path);
            }
        }
        ControlFlow::Continue(())
    });
    let through = a.join("to_b");
    assert_eq!(
        found,
        [a.clone(), through.clone(), through.join("file")].into()
    );
    assert_eq!(loops, [through.join("to_a")]);
}

#[test]
fn a_consumer_that_says_stop_ends_the_walk_even_with_every_queue_full() {
    let tmp = tempfile::tempdir().unwrap();
    tree(tmp.path(), 4, 4, 3);
    let mut got = 0;
    let report = Walk::new([tmp.path()])
        .threads(Threads::Fixed(8))
        .batch(1)
        .queue(1)
        .for_each(false, |_| {
            got += 1;
            if got == 10 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
    assert_eq!(got, 10);
    assert!(report.cancelled);
    assert!((report.seen as usize) < reference(tmp.path()).len());
}

#[test]
fn a_slow_consumer_loses_nothing_and_its_slowness_is_measured() {
    let tmp = tempfile::tempdir().unwrap();
    tree(tmp.path(), 3, 3, 2);
    let mut all = BTreeSet::new();
    let report = Walk::new([tmp.path()])
        .threads(Threads::Fixed(4))
        .batch(4)
        .queue(1)
        .run(
            |found| Visit::Take(found.path().to_owned()),
            |event| {
                if let Event::Batch(batch) = event {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    all.extend(batch);
                }
                ControlFlow::Continue(())
            },
        );
    assert_eq!(all, reference(tmp.path()));
    assert!(!report.waited_on_consumer.is_zero());
}

#[test]
fn the_thread_hook_runs_on_every_reading_thread_before_it_reads_and_is_let_go_after() {
    static ALIVE: AtomicUsize = AtomicUsize::new(0);
    static STARTED: AtomicUsize = AtomicUsize::new(0);
    static READ_WITHOUT: AtomicUsize = AtomicUsize::new(0);
    thread_local!(static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            ALIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    tree(tmp.path(), 3, 3, 2);
    let ids = Mutex::new(std::collections::HashSet::new());
    Walk::new([tmp.path()])
        .threads(Threads::Fixed(3))
        .on_thread(|| {
            STARTED.fetch_add(1, Ordering::SeqCst);
            ALIVE.fetch_add(1, Ordering::SeqCst);
            HELD.set(true);
            Box::new(Guard)
        })
        .run(
            |_| {
                if !HELD.get() {
                    READ_WITHOUT.fetch_add(1, Ordering::SeqCst);
                }
                ids.lock().unwrap().insert(std::thread::current().id());
                Visit::<()>::Pass
            },
            |_| ControlFlow::Continue(()),
        );
    assert_eq!(STARTED.load(Ordering::SeqCst), 3);
    assert_eq!(READ_WITHOUT.load(Ordering::SeqCst), 0);
    assert_eq!(ALIVE.load(Ordering::SeqCst), 0);
    assert!(ids.lock().unwrap().len() <= 3);
}
