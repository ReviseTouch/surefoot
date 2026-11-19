# surefoot

A parallel directory walk for Rust that asks what it is standing on.

[Türkçe](README.tr.md)

Most walkers take a thread count and keep it. The right count depends on the
disk: eight threads are right for an NVMe volume and wrong for a spinning one,
where each extra reader is another seek. surefoot asks the path first, and then
keeps checking while it walks.

```rust
use std::ops::ControlFlow;
use surefoot::Walk;

let report = Walk::new(["/home"]).for_each(false, |entry| {
    if let Ok(entry) = entry {
        println!("{}", entry.path.display());
    }
    ControlFlow::Continue(())
});
eprintln!("{} entries, threads {:?}", report.seen, report.threads);
```

## What it does

- **Asks the medium.** `surefoot::probe` says whether a path is on solid state,
  a spinning disk, a network mount or memory; which physical disk it shares
  with other paths; what the filesystem calls itself; whether names are
  case-sensitive and whether file modes are real. Linux, Windows and macOS.
- **Paces itself.** With `Threads::Auto` the walk starts from what the medium is
  worth, adds a thread on trial, and keeps it only if throughput rose by a fair
  share of what a thread should add. It gives a thread back when the reading
  threads spend their time waiting for the consumer. A spinning disk stays on
  one thread.
- **Pays for nothing unused.** The visitor sees a name and a type and asks for
  the `stat` itself, so an entry it turns away costs none. On Linux the `stat`
  goes through the open directory instead of the full path; on Windows the
  listing already carries it.
- **Bounded.** What the visitor takes reaches the caller in batches over a
  bounded queue. A slow consumer slows the walk; it does not grow memory.
- **Reports what it could not read.** Every refusal arrives with its path and
  its `io::Error`, and the walk goes on around it.
- **Reads NTFS from its table.** `surefoot::mft` reads a volume's master file
  table end to end instead of listing it directory by directory, and hands
  over the same rows a walk would. It needs an administrator; `mft::open` says
  when it is not one, and the caller walks instead.
- **Every decision can be overridden.** `Threads::Fixed(n)`, `.medium(…)`,
  `.priority(…)`, `.batch(…)`, `.queue(…)`.

## The visitor

```rust
use std::ops::ControlFlow;
use surefoot::{Event, Priority, Threads, Visit, Walk};

let mut total = 0u64;
Walk::new(["/srv/data"])
    .threads(Threads::Auto)
    .priority(Priority::Background)
    .run(
        // On the reading threads: decide, and pay for a stat only if wanted.
        |found| {
            if found.file_name() == "node_modules" {
                return Visit::Prune;
            }
            match found.metadata() {
                Ok(md) if md.is_file() => Visit::Take(md.len()),
                _ => Visit::Pass,
            }
        },
        // On the calling thread: what was taken, a batch at a time.
        |event| {
            if let Event::Batch(sizes) = event {
                total += sizes.iter().sum::<u64>();
            }
            ControlFlow::Continue(())
        },
    );
```

## Measured

One machine, one tree: 5.6 million entries on btrfs over NVMe, cache warm,
variants run in alternation, medians of three.

| | 2 threads | 8 threads |
|---|---|---|
| `ignore` 0.4, `stat` by full path | 3.73 s | 1.18 s |
| surefoot, `stat` through the open directory | 2.92 s | 0.98 s |
| either, no `stat` | 1.4–1.6 s | 0.57 s |

`Threads::Auto` walked the same tree in 1.5 s, having started on two threads and
reached six. It does not beat a fixed eight on a walk this short; it is for the
caller who does not know the disk.

Not measured: a cold cache, a spinning disk, a network mount, Windows, macOS.
The tests run on all three systems on every push; they say it works there,
not how fast.

## Status

0.1, used by [Scour](https://github.com/ReviseTouch/Scour). The API may change.

- `Threads::Auto` is not yet a win where the consumer is slower than the
  disk. Inside Scour, whose indexer is the limit, it lost to a fixed two
  threads — 14.0 s against 11.9 s — because it kept trying threads the
  consumer could not use. Use `Threads::Fixed` there.
- The pacer sees its own throughput and its consumer. It does not see other
  processes; standing aside for them is `Priority::Background`, which is
  implemented on Linux only.
- macOS uses the portable path. `getattrlistbulk` is not used yet.
- `mft` reads a whole volume only, and its rows come in record order, not
  directory by directory. It does not follow `$MFT`'s run list into an
  extension record: a table that fragmented fails the read rather than return
  part of it.

## Licence

MIT or Apache-2.0, at your option.
