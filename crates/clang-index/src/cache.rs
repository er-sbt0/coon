//! What a translation unit came to, kept on disk so the next run need not
//! parse it again.
//!
//! A run is every TU of the build, and reopening a project or refreshing it
//! after an edit to one file used to parse all of them again. An entry here is
//! one TU's result, under `<project>/.cache/coon/libclang/`, beside the
//! flavour databases in `.cache/coon/flavours` and for the same reasons:
//! inside the project, so it goes where the project goes and is found again by
//! path, and a cache, so it can be deleted at any time. `COON_INDEX_CACHE=0`
//! turns it off.
//!
//! **An entry is keyed by what the parse was asked**: the file, the directory
//! and the job's arguments, the project root (every path a result names is
//! resolved against it), the libclang version, and [`FORMAT`] and
//! [`EXTRACTION`]. The arguments are the [`Job`]'s, not the request's: a run
//! leaves out an argument once libclang has refused it, and which have been
//! refused depends on which TUs were parsed before this one, so a key built
//! from the request would differ with the order of the run. A run that caches
//! sends every job's arguments whole instead (see `Plan::request`), so the
//! key is also exactly what was parsed.
//!
//! **And it is valid while what the parse read is unchanged**: every file
//! libclang included, the TU's own among them, with its size, its mtime and a
//! hash of its contents. A file whose size and mtime match is taken as
//! unchanged without reading it; one whose mtime moved but whose size did not
//! is read and hashed, so a `touch`, a checkout that comes back to the same
//! content, or a `configure` that regenerates a header byte for byte costs a
//! hash rather than a parse. A file that is gone, or that differs, invalidates
//! the entry. What this cannot see is a file the parse did *not* read that
//! would now be found first — a new header earlier on the include path — which
//! is the limit of every cache keyed on inclusions; and a TU whose parse had
//! errors is never stored at all, because the commonest error is an include
//! that was not found, and a header that appears later is exactly that kind of
//! file.
//!
//! **An entry is a complete result.** A parser normally walks a header's body
//! only the first time *it* meets it in a flavour, so what a TU's result holds
//! depends on what that parser read before it — fine for a run, whose merge
//! takes each body from the lowest job to define it, and which that job always
//! walked. A cached result outlives its run, and the TU below it in the next
//! one may be a hit while this one is parsed, or the other way round; so a
//! parse that stores walks every body it defines (`Walk::complete`). The
//! merge's rule then holds with any mix of hits and parses: the lowest job to
//! define a body either is a hit, which has it, or was parsed, and no parser
//! can have met it in an earlier job of its own.
//!
//! Entries are written by whoever parsed the TU — a worker, or this process —
//! to a temporary file renamed into place, so a reader sees a whole entry or
//! none. Not synced: an entry cut short by a power cut does not parse, and an
//! entry that does not parse, or does not match its key, is a miss.
//!
//! Nothing is ever deleted. An entry's name is its key, so an edited TU
//! overwrites its own entry rather than adding one; what accumulates is an
//! entry per set of arguments a file has ever been compiled with. Dropping
//! whatever this run did not ask for would be wrong in the common case — a
//! `--flavour` run asks for a fraction of the build, and would throw away the
//! rest of it for the next run without one.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Job, Parsed};

/// The layout of an entry on disk. Part of the directory name, so a new layout
/// starts an empty directory rather than missing on every old entry.
pub(crate) const FORMAT: u32 = 1;

/// What the walk records. Bump it with any change to what a parse puts in its
/// result — which entities count as definitions, which references as calls,
/// how a location or a name is spelled — or the cache hands back the old
/// extraction's results as this one's.
pub(crate) const EXTRACTION: u32 = 1;

/// How recent a file's mtime may be, against the start of the parse that read
/// it, before it is not trusted. A file written during the parse may have been
/// read before the write or after it, and its mtime says nothing about which:
/// storing it would record the new mtime against what may be the old content.
/// Two seconds because that is the coarsest mtime a file system in use still
/// keeps (FAT); the cost is that a TU whose file was saved within two seconds
/// of the run is parsed again next time.
const TOO_NEW: Duration = Duration::from_secs(2);

/// Where entries go, and the part of every key that is the same for a run.
pub(crate) struct Cache {
    dir: PathBuf,
    prefix: String,
}

/// Where one job's entry is, and the key it is stored under. Travels with the
/// request to whoever parses the job, since they write it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Store {
    pub(crate) path: PathBuf,
    pub(crate) key: String,
}

/// A file a parse read, as it was then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Stat {
    size: u64,
    /// Seconds and nanoseconds since the epoch, negative before it.
    mtime: (i64, u32),
}

#[derive(Serialize, Deserialize)]
struct Input {
    path: PathBuf,
    stat: Stat,
    hash: u64,
}

/// What is written: the key in full, so that two keys whose hashes meet are
/// told apart; what the parse read; and its result, every body walked.
#[derive(Serialize)]
struct EntryOut<'a> {
    key: &'a str,
    inputs: Vec<Input>,
    parsed: &'a Parsed,
}

#[derive(Deserialize)]
struct EntryIn {
    key: String,
    inputs: Vec<Input>,
    parsed: Parsed,
}

impl Cache {
    /// The cache for a run under `root` with libclang `version`.
    pub(crate) fn open(root: &Path, version: &str) -> Self {
        let root = root
            .canonicalize()
            .unwrap_or_else(|_| crate::normalise(root));
        Self {
            dir: root.join(".cache/coon/libclang").join(format!("v{FORMAT}")),
            prefix: format!(
                "coon libclang cache {FORMAT}\0extraction {EXTRACTION}\0{version}\0{}",
                root.display()
            ),
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where `job`'s entry is. Named by the file's stem and a hash of the key,
    /// so that the directory can be read by someone wondering what is in it.
    pub(crate) fn store_for(&self, job: &Job) -> Store {
        let mut key = format!(
            "{}\0{}\0{}",
            self.prefix,
            job.directory.display(),
            job.file.display()
        );
        for argument in &job.arguments {
            key.push('\0');
            key.push_str(argument);
        }
        let stem = job
            .file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Store {
            path: self
                .dir
                .join(format!("{stem}-{:016x}.json", fnv(key.as_bytes()))),
            key,
        }
    }

    /// The result stored for `store`, if there is one and every file it was
    /// parsed from is unchanged. Anything unreadable is a miss.
    pub(crate) fn load(&self, store: &Store, files: &mut Files) -> Option<Parsed> {
        let bytes = std::fs::read(&store.path).ok()?;
        let entry: EntryIn = match serde_json::from_slice(&bytes) {
            Ok(entry) => entry,
            Err(e) => {
                log::debug!(
                    "[clang-index] cache entry {} unreadable ({e}); parsing again",
                    store.path.display()
                );
                return None;
            }
        };
        if entry.key != store.key {
            return None;
        }
        for input in &entry.inputs {
            if !files.unchanged(input) {
                log::debug!(
                    "[clang-index] {} changed since {} was cached",
                    input.path.display(),
                    store.path.display()
                );
                return None;
            }
        }
        Some(entry.parsed)
    }
}

/// Write `parsed`, the result of a parse that started at `started` and read
/// `inputs`, to `store`. `Err` says why it was not, which is not a failure of
/// the parse.
pub(crate) fn save(
    store: &Store,
    inputs: &[PathBuf],
    started: SystemTime,
    parsed: &Parsed,
    files: &mut Files,
) -> Result<(), String> {
    let mut read = Vec::with_capacity(inputs.len());
    for path in inputs {
        // Not memoised across TUs, unlike the hashes: a file edited between
        // two TUs of a run must be recorded as the later one read it.
        let stat = stat(path).ok_or_else(|| format!("{} is gone", path.display()))?;
        if mtime_after(stat, started - TOO_NEW) {
            return Err(format!("{} was modified during the parse", path.display()));
        }
        let hash = files
            .hash(path, stat)
            .ok_or_else(|| format!("{} could not be read", path.display()))?;
        read.push(Input {
            path: path.clone(),
            stat,
            hash,
        });
    }
    let entry = EntryOut {
        key: &store.key,
        inputs: read,
        parsed,
    };
    let bytes = serde_json::to_vec(&entry).map_err(|e| e.to_string())?;
    write_atomically(&store.path, &bytes).map_err(|e| format!("{}: {e}", store.path.display()))
}

/// Write `bytes` to a file beside `path` and rename it over `path`, so that no
/// reader — another run, another worker storing the same TU — ever sees a
/// half-written entry.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
    let written = std::fs::File::create(&temporary)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Files as this process has found them: their stats, for checking entries
/// against, and their hashes by stat, so a header every TU includes is read
/// once per process rather than once per TU.
#[derive(Default)]
pub(crate) struct Files {
    stats: HashMap<PathBuf, Option<Stat>>,
    hashes: HashMap<(PathBuf, Stat), Option<u64>>,
}

impl Files {
    fn unchanged(&mut self, input: &Input) -> bool {
        let now = *self
            .stats
            .entry(input.path.clone())
            .or_insert_with(|| stat(&input.path));
        match now {
            None => false,
            Some(now) if now == input.stat => true,
            Some(now) if now.size != input.stat.size => false,
            Some(now) => self.hash(&input.path, now) == Some(input.hash),
        }
    }

    fn hash(&mut self, path: &Path, stat: Stat) -> Option<u64> {
        *self
            .hashes
            .entry((path.to_path_buf(), stat))
            .or_insert_with(|| std::fs::read(path).ok().map(|bytes| fnv(&bytes)))
    }
}

fn stat(path: &Path) -> Option<Stat> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata.modified().ok()?;
    let mtime = match mtime.duration_since(UNIX_EPOCH) {
        Ok(after) => (after.as_secs() as i64, after.subsec_nanos()),
        Err(before) => {
            let before = before.duration();
            (-(before.as_secs() as i64), before.subsec_nanos())
        }
    };
    Some(Stat {
        size: metadata.len(),
        mtime,
    })
}

fn mtime_after(stat: Stat, time: SystemTime) -> bool {
    let Ok(since) = time.duration_since(UNIX_EPOCH) else {
        return true;
    };
    stat.mtime >= (since.as_secs() as i64, since.subsec_nanos())
}

/// FNV-1a, 64 bits: what `compdb::FlavourKey::short_hash` names a directory
/// with, for the same reason — std's hasher is not promised to give the same
/// answer after a toolchain upgrade, and a hash on disk must. A collision in
/// a name is caught by the key stored inside; one in a file's contents would
/// need a same-size edit whose mtime also moved.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// libclang's version string, which a result depends on as much as on the
/// source, asked in this process: for a run that parses here anyway. A run in
/// workers asks its first worker instead, because loading libclang costs the
/// parent ~55 MB at its peak, measured, and memory is what this backend is for.
/// Once per process; `None` if libclang does not load, and the run then fails
/// for the same reason.
pub(crate) fn libclang_version() -> Option<String> {
    static VERSION: OnceLock<String> = OnceLock::new();
    if let Some(version) = VERSION.get() {
        return Some(version.clone());
    }
    let clang = clang::Clang::new().ok()?;
    let version = clang::get_version();
    drop(clang);
    Some(VERSION.get_or_init(|| version).clone())
}

/// Every file `tu` read — the TU's own file, and everything it included.
///
/// The `clang` crate has no binding for `clang_getInclusions`, nor any way to
/// the `CXTranslationUnit` it wraps, so it is read out of the wrapper: a
/// pointer and a `PhantomData`, whose one non-zero-sized field must sit at
/// offset zero of a struct its own size, which the assertion holds it to. The
/// crate is pinned to the version that was checked (`=2.1.0`); a new one is to
/// be read again before the pin moves.
pub(crate) fn inclusions(tu: &clang::TranslationUnit<'_>, directory: &Path) -> Vec<PathBuf> {
    use clang_sys::{
        clang_disposeString, clang_getCString, clang_getFileName, clang_getInclusions,
        CXClientData, CXFile, CXSourceLocation, CXString, CXTranslationUnit,
    };
    use std::ffi::{c_uint, CStr};

    const _: () = assert!(
        std::mem::size_of::<clang::TranslationUnit<'static>>()
            == std::mem::size_of::<CXTranslationUnit>()
    );

    unsafe fn text(string: CXString) -> String {
        let c = clang_getCString(string);
        let out = if c.is_null() {
            String::new()
        } else {
            CStr::from_ptr(c).to_string_lossy().into_owned()
        };
        clang_disposeString(string);
        out
    }

    extern "C" fn visit(
        file: CXFile,
        _stack: *mut CXSourceLocation,
        _depth: c_uint,
        data: CXClientData,
    ) {
        // SAFETY: `data` is the `Vec` handed to `clang_getInclusions` below,
        // which outlives the call, and `file` is libclang's for this TU.
        unsafe {
            let out = &mut *(data as *mut Vec<String>);
            out.push(text(clang_getFileName(file)));
        }
    }

    let mut names: Vec<String> = Vec::new();
    // SAFETY: see above for the pointer; libclang is loaded on this thread,
    // since `tu` was parsed on it.
    unsafe {
        let raw = *(tu as *const clang::TranslationUnit<'_>).cast::<CXTranslationUnit>();
        clang_getInclusions(raw, visit, (&mut names as *mut Vec<String>).cast());
    }
    let mut paths: Vec<PathBuf> = names
        .into_iter()
        .filter(|name| !name.is_empty())
        // Relative to the compile command's directory when a relative `-I`
        // found it, as `Scope` explains; not normalised, since a `..` after a
        // symlink is not the lexical parent, and a stat goes where libclang did.
        .map(|name| directory.join(name))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}
