//! The walk.

use std::any::Any;
use std::ffi::OsStr;
use std::fs::{self, FileType, Metadata};
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::pace::{Pace, Plan};
use crate::probe::Medium;

/// How many threads read directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Threads {
    /// Start from what the medium is worth and adjust while the walk runs:
    /// another thread is kept only while it buys throughput, and one is given
    /// back when the consumer is what the walk waits for. A spinning disk
    /// stays on one.
    Auto,
    /// What the medium is worth, decided once: [`Medium::walk_threads`].
    Static,
    /// Exactly this many, whatever the medium.
    Fixed(usize),
    /// Adjusted while the walk runs, within these bounds.
    Range { min: usize, max: usize },
}

/// How the walking threads stand towards everything else on the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Normal,
    /// The lowest CPU and I/O priority the platform offers a thread. Linux
    /// only so far: ten more of `nice` and the idle I/O class, which the `bfq`
    /// scheduler honours and `kyber` and `none` do not.
    Background,
}

/// What a per-thread hook hands back; dropped when the thread ends.
type Guard = Box<dyn Any>;

/// A walk, described. Nothing is read until [`Walk::run`].
#[derive(Clone)]
pub struct Walk {
    roots: Vec<PathBuf>,
    threads: Threads,
    medium: Option<Medium>,
    priority: Priority,
    skip_hidden: bool,
    follow_links: bool,
    max_depth: Option<usize>,
    batch: usize,
    queue: usize,
    on_thread: Option<Arc<dyn Fn() -> Guard + Send + Sync>>,
}

/// One thing the walk has come to, before anything was asked about it beyond
/// its name and type.
pub struct Found<'a> {
    path: &'a Path,
    depth: usize,
    file_type: FileType,
    /// A symbolic link the walk is following: what is described is its target.
    followed: bool,
    entry: Option<&'a fs::DirEntry>,
}

impl Found<'_> {
    pub fn path(&self) -> &Path {
        self.path
    }

    /// The last component, or the whole path for a root that has none.
    pub fn file_name(&self) -> &OsStr {
        self.path.file_name().unwrap_or(self.path.as_os_str())
    }

    /// Zero for a root, one for what is directly inside it.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// From the directory listing: no `stat` was paid for it. With
    /// `follow_links`, a link's type is its target's.
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    pub fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }

    /// Reached through a symbolic link that the walk followed.
    pub fn path_is_symlink(&self) -> bool {
        self.followed
    }

    /// One `stat`, asked for rather than taken: an entry the visitor turns
    /// away never pays it. On Linux it goes through the open directory rather
    /// than the full path; on Windows the listing already held the answer.
    pub fn metadata(&self) -> io::Result<Metadata> {
        match self.entry {
            _ if self.followed => fs::metadata(self.path),
            Some(entry) => entry.metadata(),
            None => fs::symlink_metadata(self.path),
        }
    }
}

/// What a visitor decides about one [`Found`].
pub enum Visit<T> {
    /// Hand this on, and go into it if it is a directory.
    Take(T),
    /// Hand this on, and do not go into it.
    TakeShallow(T),
    /// Hand nothing on, but go into it.
    Pass,
    /// Hand nothing on and do not go into it.
    Prune,
}

/// What reaches the thread that called [`Walk::run`].
pub enum Event<T> {
    /// What the visitor took, a thread's worth at a time, in no promised
    /// order: a directory may arrive after what is inside it.
    Batch(Vec<T>),
    /// Somewhere the walk could not look.
    Unreadable(Unreadable),
}

/// Somewhere the walk could not look, and why. The walk goes on around it.
#[derive(Debug)]
pub struct Unreadable {
    pub path: PathBuf,
    pub depth: usize,
    pub error: io::Error,
}

impl Unreadable {
    /// A followed symbolic link that leads back to a directory above itself.
    pub fn is_loop(&self) -> bool {
        self.error
            .get_ref()
            .is_some_and(|inner| inner.is::<LinkLoop>())
    }
}

#[derive(Debug)]
struct LinkLoop;

impl std::fmt::Display for LinkLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a symbolic link that leads back above itself")
    }
}

impl std::error::Error for LinkLoop {}

/// One entry, for [`Walk::for_each`].
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub depth: usize,
    pub file_type: FileType,
    /// Filled when asked for; `None` too where the `stat` failed.
    pub metadata: Option<Metadata>,
}

/// How many threads were reading, over the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadUse {
    pub first: usize,
    pub least: usize,
    pub most: usize,
    pub last: usize,
    /// How many times the number changed.
    pub changes: u32,
}

/// What a walk did.
#[derive(Debug, Clone)]
pub struct Report {
    /// Everything the walk came to, taken or not.
    pub seen: u64,
    /// Directories read.
    pub dirs: u64,
    pub unreadable: u64,
    /// The consumer said stop.
    pub cancelled: bool,
    pub elapsed: Duration,
    /// What the walk took itself to be standing on. `None` when nothing
    /// needed to know: a fixed thread count asks no question.
    pub medium: Option<Medium>,
    pub threads: ThreadUse,
    /// How long the reading threads waited for the consumer, summed over them.
    pub waited_on_consumer: Duration,
}

impl Walk {
    pub fn new<P: Into<PathBuf>>(roots: impl IntoIterator<Item = P>) -> Walk {
        Walk {
            roots: roots.into_iter().map(Into::into).collect(),
            threads: Threads::Auto,
            medium: None,
            priority: Priority::Normal,
            skip_hidden: false,
            follow_links: false,
            max_depth: None,
            batch: 512,
            queue: 64,
            on_thread: None,
        }
    }

    pub fn threads(mut self, threads: Threads) -> Walk {
        self.threads = threads;
        self
    }

    /// What the roots are on, when the caller already knows: the walk then
    /// asks nothing. Otherwise the slowest root decides, since one spinning
    /// disk makes eight threads wrong for all of them.
    pub fn medium(mut self, medium: Medium) -> Walk {
        self.medium = Some(medium);
        self
    }

    pub fn priority(mut self, priority: Priority) -> Walk {
        self.priority = priority;
        self
    }

    /// Leave out what the platform hides: a leading dot, and on Windows the
    /// hidden attribute. Roots are never left out. Off by default.
    pub fn skip_hidden(mut self, skip: bool) -> Walk {
        self.skip_hidden = skip;
        self
    }

    /// Go through symbolic links to directories and describe links by their
    /// targets. A link that leads back above itself is reported
    /// [`Unreadable`] and not entered. Off by default; a root that is itself
    /// a link is always entered.
    pub fn follow_links(mut self, follow: bool) -> Walk {
        self.follow_links = follow;
        self
    }

    /// The deepest thing to come to: zero is the roots alone.
    pub fn max_depth(mut self, depth: Option<usize>) -> Walk {
        self.max_depth = depth;
        self
    }

    /// How much a reading thread collects before handing it over. One send an
    /// entry made the channel the walk: 8,231,481 voluntary context switches
    /// for 870,000 entries, against 44,238 batched by 512.
    pub fn batch(mut self, entries: usize) -> Walk {
        self.batch = entries.max(1);
        self
    }

    /// How many batches may wait for the consumer. This times the batch is
    /// all the walk ever holds, however slow the consumer is.
    pub fn queue(mut self, batches: usize) -> Walk {
        self.queue = batches.max(1);
        self
    }

    /// Run on every reading thread before it reads anything, and keep what it
    /// returns until the thread ends: an identity to impersonate, a priority
    /// of the caller's own.
    pub fn on_thread(mut self, hook: impl Fn() -> Guard + Send + Sync + 'static) -> Walk {
        self.on_thread = Some(Arc::new(hook));
        self
    }

    /// Walk. `visit` runs on the reading threads, once for everything the
    /// walk comes to, and decides what is taken and what is entered; `sink`
    /// runs on this thread and receives what was taken. Returning
    /// [`ControlFlow::Break`] from `sink` ends the walk.
    pub fn run<T, V, S>(&self, visit: V, mut sink: S) -> Report
    where
        T: Send,
        V: Fn(&Found<'_>) -> Visit<T> + Sync,
        S: FnMut(Event<T>) -> ControlFlow<()>,
    {
        let started = Instant::now();
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let plan = Plan::of(self.threads, self.medium, &self.roots, cores);

        let shared = Shared {
            state: Mutex::new(State {
                // Popped from the end, so the first root is read first.
                stack: self.roots.iter().rev().map(|r| Job::root(r)).collect(),
                busy: 0,
            }),
            wake: Condvar::new(),
            limit: AtomicUsize::new(plan.start),
            hungry: AtomicUsize::new(0),
            quit: AtomicBool::new(false),
            seen: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            unreadable: AtomicU64::new(0),
            waited_ns: AtomicU64::new(0),
        };
        let over = (Mutex::new(false), Condvar::new());
        let mut threads = ThreadUse {
            first: plan.start,
            least: plan.start,
            most: plan.start,
            last: plan.start,
            changes: 0,
        };
        let mut cancelled = false;

        let (tx, rx) = sync_channel::<Event<T>>(self.queue);
        std::thread::scope(|scope| {
            for index in 0..plan.max {
                let tx = tx.clone();
                let (shared, visit) = (&shared, &visit);
                scope.spawn(move || {
                    Worker {
                        walk: self,
                        shared,
                        visit,
                        index,
                        local: Vec::new(),
                        out: Out {
                            tx,
                            buf: Vec::with_capacity(self.batch),
                            batch: self.batch,
                            shared,
                        },
                    }
                    .run()
                });
            }
            // The workers hold the senders; the loop below ends when they do.
            drop(tx);

            let pacer = plan.adaptive().then(|| {
                let (shared, over) = (&shared, &over);
                scope.spawn(move || Pace::new(plan).run(shared, over))
            });

            for event in rx {
                // Drained to the end even after a stop, so no reading thread
                // is left on a full channel.
                if cancelled {
                    continue;
                }
                if sink(event).is_break() {
                    cancelled = true;
                    shared.stop();
                }
            }

            *over.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
            over.1.notify_all();
            if let Some(Ok(used)) = pacer.map(|p| p.join()) {
                threads = used;
            }
        });

        Report {
            seen: shared.seen.load(Relaxed),
            dirs: shared.dirs.load(Relaxed),
            unreadable: shared.unreadable.load(Relaxed),
            cancelled,
            elapsed: started.elapsed(),
            medium: plan.medium,
            threads,
            waited_on_consumer: Duration::from_nanos(shared.waited_ns.load(Relaxed)),
        }
    }

    /// Every entry and every refusal, one at a time on this thread, with
    /// metadata when `metadata` asks for it.
    pub fn for_each(
        &self,
        metadata: bool,
        mut each: impl FnMut(Result<Entry, Unreadable>) -> ControlFlow<()>,
    ) -> Report {
        self.run(
            |found| {
                Visit::Take(Entry {
                    path: found.path().to_owned(),
                    depth: found.depth(),
                    file_type: found.file_type(),
                    metadata: metadata.then(|| found.metadata().ok()).flatten(),
                })
            },
            |event| match event {
                Event::Batch(entries) => entries
                    .into_iter()
                    .try_for_each(|entry| each(Ok(entry))),
                Event::Unreadable(refusal) => each(Err(refusal)),
            },
        )
    }
}

/// A directory to read, or a root to look at first.
struct Job {
    path: PathBuf,
    depth: usize,
    root: bool,
}

impl Job {
    fn root(path: &Path) -> Job {
        Job {
            path: path.to_owned(),
            depth: 0,
            root: true,
        }
    }
}

struct State {
    /// Directories nobody has taken yet.
    stack: Vec<Job>,
    /// Workers holding a job: while there is one, more work may appear.
    busy: usize,
}

/// What the reading threads and the pacer have in common.
pub(crate) struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    /// Workers numbered below this read; the rest wait.
    limit: AtomicUsize,
    /// Workers allowed to read that have nothing to read.
    hungry: AtomicUsize,
    quit: AtomicBool,
    pub(crate) seen: AtomicU64,
    dirs: AtomicU64,
    unreadable: AtomicU64,
    pub(crate) waited_ns: AtomicU64,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A visitor that panicked poisoned nothing worth protecting: the
        // state is two counters and a list of paths.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stop(&self) {
        let _held = self.lock();
        self.quit.store(true, Relaxed);
        self.wake.notify_all();
    }

    /// Under the lock, so a worker deciding whether to wait cannot miss it.
    pub(crate) fn set_limit(&self, limit: usize) {
        let _held = self.lock();
        self.limit.store(limit, Relaxed);
        self.wake.notify_all();
    }

    /// The next directory for worker `index`, or `None` when the walk is over.
    /// Whatever the worker still held goes back first: a worker comes here
    /// with work in hand only when it has been told to stand down.
    fn next(&self, index: usize, local: &mut Vec<Job>, busy: &mut bool) -> Option<Job> {
        let mut state = self.lock();
        if std::mem::take(busy) {
            state.busy -= 1;
        }
        if !local.is_empty() {
            state.stack.append(local);
            self.wake.notify_all();
        }
        loop {
            if self.quit.load(Relaxed) {
                return None;
            }
            let allowed = index < self.limit.load(Relaxed);
            if allowed && let Some(job) = state.stack.pop() {
                state.busy += 1;
                *busy = true;
                return Some(job);
            }
            if state.busy == 0 && state.stack.is_empty() {
                self.wake.notify_all();
                return None;
            }
            if allowed {
                self.hungry.fetch_add(1, Relaxed);
            }
            state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            if allowed {
                self.hungry.fetch_sub(1, Relaxed);
            }
        }
    }

    /// Hand directories to whoever is waiting for one.
    fn share(&self, jobs: impl Iterator<Item = Job>) {
        let mut state = self.lock();
        state.stack.extend(jobs);
        self.wake.notify_all();
    }
}

/// One reading thread's outgoing buffer.
struct Out<'a, T> {
    tx: SyncSender<Event<T>>,
    buf: Vec<T>,
    batch: usize,
    shared: &'a Shared,
}

impl<T> Out<'_, T> {
    fn push(&mut self, taken: T) {
        self.buf.push(taken);
        if self.buf.len() >= self.batch {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let full = std::mem::replace(&mut self.buf, Vec::with_capacity(self.batch));
        self.send(Event::Batch(full));
    }

    fn refuse(&mut self, path: PathBuf, depth: usize, error: io::Error) {
        self.shared.unreadable.fetch_add(1, Relaxed);
        self.send(Event::Unreadable(Unreadable { path, depth, error }));
    }

    /// Timed only when the channel is full: that wait is the consumer's, and
    /// it is what tells the pacer that more reading threads would buy nothing.
    fn send(&mut self, event: Event<T>) {
        let event = match self.tx.try_send(event) {
            Ok(()) => return,
            Err(TrySendError::Full(event)) => event,
            Err(TrySendError::Disconnected(_)) => return self.shared.quit.store(true, Relaxed),
        };
        let since = Instant::now();
        if self.tx.send(event).is_err() {
            self.shared.quit.store(true, Relaxed);
        }
        self.shared
            .waited_ns
            .fetch_add(since.elapsed().as_nanos() as u64, Relaxed);
    }
}

struct Worker<'a, T, V> {
    walk: &'a Walk,
    shared: &'a Shared,
    visit: &'a V,
    index: usize,
    /// Directories this thread found and will read itself, deepest last.
    local: Vec<Job>,
    out: Out<'a, T>,
}

impl<T, V> Worker<'_, T, V>
where
    V: Fn(&Found<'_>) -> Visit<T>,
{
    fn run(mut self) {
        // Before the first read: an identity taken afterwards has already
        // read the root as somebody else.
        let _guard = self.walk.on_thread.as_ref().map(|hook| hook());
        if self.walk.priority == Priority::Background {
            stand_aside();
        }
        let mut busy = false;
        loop {
            let standing = self.index < self.shared.limit.load(Relaxed);
            let job = match self.local.pop() {
                Some(job) if standing => job,
                held => {
                    self.local.extend(held);
                    // Nothing to read, or told to stand down: either way what
                    // is collected should not wait for this thread's return.
                    self.out.flush();
                    match self.shared.next(self.index, &mut self.local, &mut busy) {
                        Some(job) => job,
                        None => break,
                    }
                }
            };
            if self.shared.quit.load(Relaxed) {
                break;
            }
            if job.root {
                self.look_at_root(job.path);
            } else {
                self.read(&job.path, job.depth);
            }
            // The oldest are the shallowest, and so the largest subtrees.
            if self.local.len() > 1 && self.shared.hungry.load(Relaxed) > 0 {
                let keep = self.local.len() - 1;
                self.shared.share(self.local.drain(..keep));
            }
        }
        self.out.flush();
    }

    fn deeper(&self, depth: usize) -> bool {
        self.walk.max_depth.is_none_or(|max| depth < max)
    }

    fn look_at_root(&mut self, path: PathBuf) {
        self.shared.seen.fetch_add(1, Relaxed);
        let own = match fs::symlink_metadata(&path) {
            Ok(md) => md.file_type(),
            Err(e) => return self.out.refuse(path, 0, e),
        };
        // A root that is a link is entered whatever `follow_links` says: the
        // caller named it. It is described as a link unless links are followed.
        let (file_type, is_dir) = match own.is_symlink().then(|| fs::metadata(&path)) {
            None => (own, own.is_dir()),
            Some(Ok(target)) if self.walk.follow_links => (target.file_type(), target.is_dir()),
            Some(Ok(target)) => (own, target.is_dir()),
            Some(Err(_)) => (own, false),
        };
        let found = Found {
            path: &path,
            depth: 0,
            file_type,
            followed: own.is_symlink() && self.walk.follow_links,
            entry: None,
        };
        let enter = match (self.visit)(&found) {
            Visit::Take(taken) => {
                self.out.push(taken);
                true
            }
            Visit::TakeShallow(taken) => {
                self.out.push(taken);
                false
            }
            Visit::Pass => true,
            Visit::Prune => false,
        };
        if enter && is_dir && self.deeper(0) {
            self.read(&path, 0);
        }
    }

    /// Read one directory: everything in it is seen, and its directories are
    /// left on this thread's own list.
    fn read(&mut self, dir: &Path, depth: usize) {
        let listing = match fs::read_dir(dir) {
            Ok(listing) => listing,
            Err(e) => return self.out.refuse(dir.to_owned(), depth, e),
        };
        self.shared.dirs.fetch_add(1, Relaxed);
        let below = depth + 1;
        let mut seen = 0u64;
        for entry in listing {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    // A listing that failed half way may fail the same way for
                    // ever; the directory is reported and left.
                    self.out.refuse(dir.to_owned(), depth, e);
                    break;
                }
            };
            if self.walk.skip_hidden && is_hidden(&entry) {
                continue;
            }
            seen += 1;
            let path = entry.path();
            let mut file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(e) => {
                    self.out.refuse(path, below, e);
                    continue;
                }
            };
            let mut followed = false;
            if self.walk.follow_links && file_type.is_symlink() {
                match fs::metadata(&path) {
                    Ok(target) => {
                        file_type = target.file_type();
                        followed = true;
                    }
                    Err(e) => {
                        self.out.refuse(path, below, e);
                        continue;
                    }
                }
                if file_type.is_dir() && leads_back(dir, &path) {
                    self.out.refuse(path, below, io::Error::other(LinkLoop));
                    continue;
                }
            }
            let found = Found {
                path: &path,
                depth: below,
                file_type,
                followed,
                entry: Some(&entry),
            };
            let enter = match (self.visit)(&found) {
                Visit::Take(taken) => {
                    self.out.push(taken);
                    true
                }
                Visit::TakeShallow(taken) => {
                    self.out.push(taken);
                    false
                }
                Visit::Pass => true,
                Visit::Prune => false,
            };
            if enter && file_type.is_dir() && self.deeper(below) {
                self.local.push(Job {
                    path,
                    depth: below,
                    root: false,
                });
            }
        }
        self.shared.seen.fetch_add(seen, Relaxed);
    }
}

/// Whether the link at `link`, inside `dir`, points at `dir` or at something
/// above it — judged by what each resolves to, since a second link on the way
/// makes the path's text say nothing.
fn leads_back(dir: &Path, link: &Path) -> bool {
    let Ok(target) = fs::canonicalize(link) else {
        return false;
    };
    dir.ancestors()
        .any(|above| fs::canonicalize(above).is_ok_and(|real| real == target))
}

fn is_hidden(entry: &fs::DirEntry) -> bool {
    let name = entry.file_name();
    if name.as_encoded_bytes().first() == Some(&b'.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        // Free: the listing carried the attributes.
        if entry
            .metadata()
            .is_ok_and(|md| md.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
        {
            return true;
        }
    }
    false
}

/// Step out of the way of everything else on the machine, for the calling
/// thread alone. Failing to become polite is not failing to walk.
pub fn stand_aside() {
    #[cfg(target_os = "linux")]
    // SAFETY: ordinary syscalls on the calling thread with constant arguments.
    unsafe {
        // Ten *more*: `nice` adds where `setpriority` sets.
        libc::nice(10);
        // IOPRIO_CLASS_IDLE (3) << IOPRIO_CLASS_SHIFT (13), for this thread.
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_IDLE: libc::c_int = 3;
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0,
            IOPRIO_CLASS_IDLE << 13,
        );
    }
}
