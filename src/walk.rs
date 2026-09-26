//! Parallel directory walker.
//!
//! Design overview (see README "Architecture" for the full picture):
//!
//! 1. `walk()` classifies the user-supplied roots: plain files are filtered
//!    and returned immediately; directories become seed jobs.
//! 2. A fixed pool of worker threads shares one work queue — a
//!    `Mutex<VecDeque<DirJob>>` guarded by a `Condvar`. Directory jobs carry
//!    their depth and the index of the root they belong to.
//! 3. Each worker pops a job, scans the directory with `fs::read_dir`, and
//!    for every accepted child file sends it over an `mpsc` channel; every
//!    accepted child directory is pushed back onto the queue.
//! 4. Termination is tracked with a `pending` counter: it is incremented
//!    *before* a job is pushed and decremented after the job has been fully
//!    processed. Workers exit when the queue is empty **and** `pending`
//!    hits zero; the last finishing worker wakes everyone via `notify_all`.
//!    This is the classic monitor pattern and is free of lost wakeups
//!    because pushes happen under the same lock the waiters re-check.
//! 5. The main thread collects results after `thread::scope` joins all
//!    workers, then sorts the file list so output is deterministic no
//!    matter which worker found what first.
//!
//! Depth semantics mirror ripgrep: files at depth `d` (root children are at
//! depth 1) are included when `d <= max_depth`; directories are only
//! descended into when doing so could still yield includable files.
//!
//! Symlinks are skipped unless `follow` is set. A hard ceiling
//! ([`MAX_WALK_DEPTH`]) bounds recursion even with `--follow`, so symlink
//! loops terminate instead of hanging the tool.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::error::io_message;
use crate::filters::{FileFilter, IgnoreSet, SKIP_DIRS};
use crate::stats::Stats;

/// Absolute recursion ceiling, protecting against symlink loops with
/// `--follow`. Normal trees never come close.
pub const MAX_WALK_DEPTH: usize = 256;

/// Worker-thread cap for the walker: the scan is I/O bound and extra
/// threads beyond this add contention, not throughput.
const MAX_WALK_WORKERS: usize = 64;

/// Options controlling a single walk.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Maximum recursion depth (`None` = unlimited; 1 = top level only).
    pub max_depth: Option<usize>,
    /// Traverse symlinks to directories and files.
    pub follow: bool,
    /// Include hidden entries (dot files/directories).
    pub hidden: bool,
    /// Skip loading `.ignore`/`.gitignore` files from the search roots.
    pub no_ignore: bool,
    /// Number of worker threads to run.
    pub threads: usize,
}

/// A per-file failure discovered while walking.
#[derive(Debug, Clone)]
pub struct WalkError {
    /// The path that could not be processed.
    pub path: PathBuf,
    /// Short human-readable explanation (no trailing newline).
    pub message: String,
}

/// Everything `walk()` produced.
#[derive(Debug)]
pub struct WalkOutcome {
    /// Accepted files, sorted lexicographically for deterministic output.
    pub files: Vec<PathBuf>,
    /// Non-fatal failures; the tool keeps searching after each one.
    pub errors: Vec<WalkError>,
}

/// A directory waiting to be scanned.
struct DirJob {
    /// Index into `Shared::roots`; used to resolve relative paths and to
    /// pick the right ignore set.
    root: usize,
    dir: PathBuf,
    /// Depth of `dir` itself (the root directory has depth 0).
    depth: usize,
}

/// Messages sent from workers to the collector.
enum WalkMsg {
    File(PathBuf),
    Failure(WalkError),
}

/// What kind of filesystem entry an [`EntryInfo`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Dir,
    File,
    Symlink,
    Other,
}

/// One directory entry, pre-resolved enough for filtering decisions.
struct EntryInfo {
    path: PathBuf,
    name: String,
    kind: EntryKind,
    /// File length in bytes (0 for non-files).
    size: u64,
}

/// Shared walker state: the job queue plus immutable per-walk configuration.
struct Queue {
    jobs: Mutex<VecDeque<DirJob>>,
    available: Condvar,
    /// Jobs pushed but not yet fully processed; drives termination.
    pending: AtomicUsize,
}

/// Everything the workers need, shared behind an `Arc`.
struct Shared<'a> {
    queue: Queue,
    roots: Vec<PathBuf>,
    ignore: Vec<IgnoreSet>,
    filter: FileFilter,
    options: WalkOptions,
    stats: &'a Stats,
}

/// Walk `roots` in parallel and return accepted files plus per-file errors.
///
/// Plain-file roots bypass the queue entirely: they are filtered here
/// (explicitly passed files are honored even when hidden) and appended to
/// the result. Directory roots are seeded into the shared queue and scanned
/// by the worker pool.
pub fn walk(roots: &[PathBuf], options: WalkOptions, filter: &FileFilter, stats: &Stats) -> WalkOutcome {
    let mut dir_roots: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut errors: Vec<WalkError> = Vec::new();

    for root in roots {
        match fs::metadata(root) {
            Ok(meta) => {
                if meta.is_dir() {
                    dir_roots.push(root.clone());
                } else {
                    let rel = root
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| root.to_string_lossy().into_owned());
                    if filter.accepts(&rel, meta.len()) {
                        stats.bump(&stats.files_found, 1);
                        files.push(root.clone());
                    }
                }
            }
            Err(err) => errors.push(WalkError {
                path: root.clone(),
                message: io_message(&err),
            }),
        }
    }

    if dir_roots.is_empty() {
        files.sort();
        return WalkOutcome { files, errors };
    }

    let ignore: Vec<IgnoreSet> = if options.no_ignore {
        vec![IgnoreSet::empty(); dir_roots.len()]
    } else {
        dir_roots.iter().map(|dir| load_ignore_sets(dir)).collect()
    };

    let workers = options.threads.max(1).min(MAX_WALK_WORKERS);
    let shared = Arc::new(Shared {
        queue: Queue {
            jobs: Mutex::new(VecDeque::new()),
            available: Condvar::new(),
            pending: AtomicUsize::new(0),
        },
        roots: dir_roots,
        ignore,
        filter: filter.clone(),
        options,
        stats,
    });

    // Seed the queue before any worker can observe an empty, "finished"
    // state; every seed increments `pending` first.
    {
        let mut jobs = shared.queue.jobs.lock().unwrap();
        for (index, dir) in shared.roots.iter().enumerate() {
            jobs.push_back(DirJob {
                root: index,
                dir: dir.clone(),
                depth: 0,
            });
            shared.queue.pending.fetch_add(1, Ordering::SeqCst);
        }
    }

    let (tx, rx) = mpsc::channel::<WalkMsg>();
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..workers {
            let worker_shared = Arc::clone(&shared);
            let worker_tx = tx.clone();
            handles.push(scope.spawn(move || worker(&worker_shared, &worker_tx)));
        }
        for handle in handles {
            // Join errors (worker panics) are deliberately ignored: the
            // search should degrade gracefully rather than abort.
            let _ = handle.join();
        }
    });
    // All worker-side senders died with the scope; drop ours so the
    // receiver loop below can observe end-of-stream.
    drop(tx);

    for message in rx {
        match message {
            WalkMsg::File(path) => files.push(path),
            WalkMsg::Failure(error) => errors.push(error),
        }
    }
    files.sort();
    WalkOutcome { files, errors }
}

/// Worker main loop: pop jobs until the queue is empty and nothing is
/// pending anywhere.
fn worker(shared: &Shared, tx: &mpsc::Sender<WalkMsg>) {
    loop {
        let job = {
            let mut guard = shared.queue.jobs.lock().unwrap();
            loop {
                match guard.pop_front() {
                    Some(job) => break Some(job),
                    None => {
                        if shared.queue.pending.load(Ordering::SeqCst) == 0 {
                            break None;
                        }
                        guard = shared.queue.available.wait(guard).unwrap();
                    }
                }
            }
        };
        let job = match job {
            Some(job) => job,
            None => break,
        };
        process_dir(shared, tx, &job);
        let previous = shared.queue.pending.fetch_sub(1, Ordering::SeqCst);
        if previous == 1 {
            // We were the last in-flight job: wake every waiter so they can
            // observe the finished state.
            shared.queue.available.notify_all();
        }
    }
}

/// Scan one directory and dispatch its entries.
fn process_dir(shared: &Shared, tx: &mpsc::Sender<WalkMsg>, job: &DirJob) {
    let read = match fs::read_dir(&job.dir) {
        Ok(read) => read,
        Err(err) => {
            let _ = tx.send(WalkMsg::Failure(WalkError {
                path: job.dir.clone(),
                message: io_message(&err),
            }));
            return;
        }
    };

    let mut entries: Vec<EntryInfo> = Vec::new();
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                let _ = tx.send(WalkMsg::Failure(WalkError {
                    path: job.dir.clone(),
                    message: io_message(&err),
                }));
                continue;
            }
        };
        let kind = match entry.file_type() {
            Ok(file_type) => {
                if file_type.is_dir() {
                    EntryKind::Dir
                } else if file_type.is_file() {
                    EntryKind::File
                } else if file_type.is_symlink() {
                    EntryKind::Symlink
                } else {
                    EntryKind::Other
                }
            }
            Err(_) => continue,
        };
        let size = if kind == EntryKind::File {
            entry.metadata().map(|meta| meta.len()).unwrap_or(0)
        } else {
            0
        };
        entries.push(EntryInfo {
            path: entry.path(),
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            size,
        });
    }
    // Deterministic traversal order regardless of filesystem layout.
    entries.sort_by(|left, right| left.name.cmp(&right.name));

    let root_dir = &shared.roots[job.root];
    for info in entries {
        // Hidden entries are filtered before any syscall-heavy work.
        if !shared.options.hidden && is_hidden_name(&info.name) {
            continue;
        }

        let mut kind = info.kind;
        let mut size = info.size;
        if kind == EntryKind::Symlink {
            if !shared.options.follow {
                continue;
            }
            match fs::metadata(&info.path) {
                Ok(meta) => {
                    if meta.is_dir() {
                        kind = EntryKind::Dir;
                    } else if meta.is_file() {
                        kind = EntryKind::File;
                        size = meta.len();
                    } else {
                        continue;
                    }
                }
                Err(err) => {
                    let _ = tx.send(WalkMsg::Failure(WalkError {
                        path: info.path.clone(),
                        message: io_message(&err),
                    }));
                    continue;
                }
            }
        }

        let relative = match info.path.strip_prefix(root_dir) {
            Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
            Err(_) => info.name.clone(),
        };

        match kind {
            EntryKind::Dir => {
                if SKIP_DIRS.contains(&info.name.as_str()) {
                    continue;
                }
                if !shared.ignore.is_empty()
                    && shared.ignore[job.root].matches(&relative, true)
                {
                    continue;
                }
                let child_depth = job.depth + 1;
                if let Some(max) = shared.options.max_depth {
                    // A directory at depth >= max cannot contain includable
                    // files (they would sit at depth > max).
                    if child_depth >= max {
                        continue;
                    }
                }
                if child_depth > MAX_WALK_DEPTH {
                    let _ = tx.send(WalkMsg::Failure(WalkError {
                        path: info.path.clone(),
                        message: format!(
                            "maximum walk depth ({}) exceeded; refusing to descend",
                            MAX_WALK_DEPTH
                        ),
                    }));
                    continue;
                }
                // Increment pending *before* pushing so a waiter that wakes
                // up sees either the job or a non-zero pending count.
                shared.queue.pending.fetch_add(1, Ordering::SeqCst);
                {
                    let mut jobs = shared.queue.jobs.lock().unwrap();
                    jobs.push_back(DirJob {
                        root: job.root,
                        dir: info.path.clone(),
                        depth: child_depth,
                    });
                }
                shared.queue.available.notify_one();
            }
            EntryKind::File => {
                if let Some(max) = shared.options.max_depth {
                    if job.depth + 1 > max {
                        continue;
                    }
                }
                if !shared.ignore.is_empty()
                    && shared.ignore[job.root].matches(&relative, false)
                {
                    continue;
                }
                if !shared.filter.accepts(&relative, size) {
                    continue;
                }
                shared.stats.bump(&shared.stats.files_found, 1);
                let _ = tx.send(WalkMsg::File(info.path.clone()));
            }
            // Symlinks were resolved above; sockets/FIFOs/devices are never
            // interesting for a content search.
            EntryKind::Symlink | EntryKind::Other => continue,
        }
    }
}

/// `true` for names starting with a dot.
fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.')
}

/// Load `.ignore` and `.gitignore` from `dir` into one combined set.
///
/// Both files are advisory configuration; read failures are silently
/// ignored, matching how most search tools treat unreadable ignore files.
fn load_ignore_sets(dir: &Path) -> IgnoreSet {
    let mut set = IgnoreSet::empty();
    for file_name in [".ignore", ".gitignore"] {
        let path = dir.join(file_name);
        if let Ok(text) = fs::read_to_string(&path) {
            set.extend(&text);
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// Create a unique scratch directory for one test.
    fn temp_root(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("searchlight-walk-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    /// Write `content` to `path`, creating parent directories.
    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, content).expect("write file");
    }

    fn opts() -> WalkOptions {
        WalkOptions {
            max_depth: None,
            follow: false,
            hidden: false,
            no_ignore: false,
            threads: 1,
        }
    }

    /// Walk `root` and return the sorted file names (no directories).
    fn names(root: &Path, options: WalkOptions) -> Vec<String> {
        let stats = Stats::new();
        let outcome = walk(
            &[root.to_path_buf()],
            options,
            &FileFilter::empty(),
            &stats,
        );
        outcome
            .files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn discovers_files_recursively() {
        let root = temp_root("recursive");
        write_file(&root.join("top.txt"), "1");
        write_file(&root.join("src/main.rs"), "2");
        write_file(&root.join("src/deep/util.rs"), "3");
        write_file(&root.join("docs/readme.md"), "4");

        let found = names(&root, opts());
        assert_eq!(
            found,
            vec!["main.rs", "readme.md", "top.txt", "util.rs"],
            "results must be sorted and complete"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn max_depth_limits_recursion() {
        let root = temp_root("depth");
        write_file(&root.join("top.txt"), "1");
        write_file(&root.join("one/mid.txt"), "2");
        write_file(&root.join("one/two/deep.txt"), "3");

        let mut options = opts();
        options.max_depth = Some(1);
        assert_eq!(names(&root, options.clone()), vec!["top.txt"]);

        options.max_depth = Some(2);
        assert_eq!(names(&root, options), vec!["mid.txt", "top.txt"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hidden_entries_are_skipped_unless_requested() {
        let root = temp_root("hidden");
        write_file(&root.join("visible.txt"), "1");
        write_file(&root.join(".hidden.txt"), "2");
        write_file(&root.join(".config/settings.txt"), "3");

        assert_eq!(names(&root, opts()), vec!["visible.txt"]);

        let mut options = opts();
        options.hidden = true;
        assert_eq!(
            names(&root, options),
            vec![".hidden.txt", "settings.txt", "visible.txt"]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn well_known_directories_are_never_descended() {
        let root = temp_root("skipdirs");
        write_file(&root.join("app.rs"), "1");
        write_file(&root.join("node_modules/lib/index.js"), "2");
        write_file(&root.join("target/debug/app"), "3");
        write_file(&root.join("__pycache__/mod.pyc"), "4");

        assert_eq!(names(&root, opts()), vec!["app.rs"]);

        // --no-ignore lifts ignore files but not the built-in skip list.
        let mut options = opts();
        options.no_ignore = true;
        assert_eq!(names(&root, options), vec!["app.rs"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gitignore_rules_are_respected_and_can_be_disabled() {
        let root = temp_root("gitignore");
        write_file(&root.join(".gitignore"), "*.log\n!keep.log\n");
        write_file(&root.join("app.rs"), "1");
        write_file(&root.join("debug.log"), "2");
        write_file(&root.join("keep.log"), "3");

        assert_eq!(names(&root, opts()), vec!["app.rs", "keep.log"]);

        let mut options = opts();
        options.no_ignore = true;
        assert_eq!(names(&root, options), vec!["app.rs", "debug.log", "keep.log"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn size_and_glob_filters_apply_during_the_walk() {
        let root = temp_root("filter");
        write_file(&root.join("small.txt"), "1");
        write_file(&root.join("big.txt"), "12345678");
        write_file(&root.join("other.md"), "1234");

        let filter = FileFilter::new(&["*.txt".to_string()], &[], Some(4), None)
            .expect("valid filter");
        let stats = Stats::new();
        let outcome = walk(&[root.clone()], opts(), &filter, &stats);
        let found: Vec<String> = outcome
            .files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(found, vec!["big.txt"]);
        assert_eq!(outcome.errors.len(), 0);
        assert_eq!(stats.snapshot().files_found, 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn multi_threaded_walk_matches_single_threaded() {
        let root = temp_root("threads");
        for index in 0..40 {
            let dir = root.join(format!("dir{}", index % 5));
            write_file(&dir.join(format!("file{}.txt", index)), "content");
        }

        let mut options = opts();
        options.threads = 1;
        let single = names(&root, options.clone());

        options.threads = 8;
        let parallel = names(&root, options);

        assert_eq!(single, parallel, "order must be deterministic");
        assert_eq!(single.len(), 40);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_roots_are_recorded_as_errors() {
        let stats = Stats::new();
        let missing = std::env::temp_dir().join("searchlight-does-not-exist-xyz");
        let outcome = walk(&[missing.clone()], opts(), &FileFilter::empty(), &stats);
        assert!(outcome.files.is_empty());
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].path, missing);
        assert_eq!(outcome.errors[0].message, "no such file or directory");
    }

    #[test]
    fn plain_file_roots_bypass_the_queue() {
        let root = temp_root("fileroot");
        let file = root.join("direct.txt");
        write_file(&file, "content");
        write_file(&root.join("ignored-by-filter.log"), "content");

        let filter = FileFilter::new(&["*.txt".to_string()], &[], None, None)
            .expect("valid filter");
        let stats = Stats::new();
        let outcome = walk(&[file.clone()], opts(), &filter, &stats);
        assert_eq!(outcome.files, vec![file]);
        assert_eq!(outcome.errors.len(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_directories_yield_nothing() {
        let root = temp_root("empty");
        fs::create_dir_all(root.join("a/b/c")).expect("nested dirs");
        let outcome = walk(&[root.clone()], opts(), &FileFilter::empty(), &Stats::new());
        assert!(outcome.files.is_empty());
        assert!(outcome.errors.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_unless_following() {
        use std::os::unix::fs::symlink;

        let root = temp_root("symlinks");
        write_file(&root.join("real.txt"), "content");
        write_file(&root.join("subdir/inner.txt"), "content");
        symlink(root.join("subdir"), root.join("link")).expect("dir symlink");

        // Without --follow the link is invisible; inner.txt is found only
        // through the real directory.
        assert_eq!(names(&root, opts()), vec!["inner.txt", "real.txt"]);

        let mut options = opts();
        options.follow = true;
        let found = names(&root, options);
        assert!(found.contains(&"inner.txt".to_string()));
        assert!(found.contains(&"real.txt".to_string()));
        let _ = fs::remove_dir_all(&root);
    }
}
