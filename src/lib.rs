//! A parallel directory walk that asks what it is standing on.
//!
//! ```no_run
//! use std::ops::ControlFlow;
//! use surefoot::Walk;
//!
//! let report = Walk::new(["/home"]).for_each(false, |entry| {
//!     if let Ok(entry) = entry {
//!         println!("{}", entry.path.display());
//!     }
//!     ControlFlow::Continue(())
//! });
//! eprintln!("{} entries on {:?}", report.seen, report.threads);
//! ```
//!
//! Three things set it apart from a walk that takes a thread count:
//!
//! - **The medium decides first.** [`probe`] says whether a path is on solid
//!   state, a spinning disk, a network mount or memory, and which physical
//!   disk it shares with others. A spinning disk is walked on one thread.
//! - **Then the walk decides again.** With [`Threads::Auto`] a thread is added
//!   on trial and kept only while it buys throughput, and one is given back
//!   when the consumer is what the walk waits for.
//! - **Nothing is paid for that is not used.** The visitor sees a name and a
//!   type and asks for the `stat` itself, so what it turns away costs none;
//!   what it takes is batched to the caller over a bounded queue.
//!
//! On Windows, [`mft`] reads an NTFS volume from its master file table when the
//! process is an administrator, which is seconds where a walk is minutes.
//!
//! Every automatic decision has a setting that overrides it.

pub mod mft;
mod pace;
pub mod probe;
mod walk;

pub use probe::{FsTraits, Medium, Probe};
pub use walk::{
    Entry, Event, Found, Priority, Report, ThreadUse, Threads, Unreadable, Visit, Walk, stand_aside,
};
