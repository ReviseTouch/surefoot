//! Walk a directory and say what it cost.
//!
//! ```text
//! cargo run --release --example walk -- /home auto stat
//! cargo run --release --example walk -- /mnt/disk 4
//! ```

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use surefoot::{Event, Threads, Visit, Walk};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = args.get(1).map_or(".", String::as_str);
    let threads = match args.get(2).map(String::as_str) {
        None | Some("auto") => Threads::Auto,
        Some("static") => Threads::Static,
        Some(n) => Threads::Fixed(n.parse().expect("auto, static or a number")),
    };
    let stat = args.get(3).is_some_and(|a| a == "stat");

    let bytes = AtomicU64::new(0);
    let mut taken = 0u64;
    let report = Walk::new([root]).threads(threads).run(
        |found| {
            if stat && let Ok(md) = found.metadata() {
                bytes.fetch_add(md.len(), Relaxed);
            }
            Visit::Take(())
        },
        |event| {
            match event {
                Event::Batch(batch) => taken += batch.len() as u64,
                Event::Unreadable(refusal) if std::env::var_os("VERBOSE").is_some() => {
                    eprintln!("{}: {}", refusal.path.display(), refusal.error)
                }
                Event::Unreadable(_) => {}
            }
            ControlFlow::Continue(())
        },
    );
    println!(
        "{taken} entries, {} directories, {} unreadable, {} ms, medium {}, threads {}→{} (least {}, most {}, {} changes), waited on consumer {} ms, {} bytes",
        report.dirs,
        report.unreadable,
        report.elapsed.as_millis(),
        report.medium.map_or("not asked", |m| m.label()),
        report.threads.first,
        report.threads.last,
        report.threads.least,
        report.threads.most,
        report.threads.changes,
        report.waited_on_consumer.as_millis(),
        bytes.load(Relaxed),
    );
}
