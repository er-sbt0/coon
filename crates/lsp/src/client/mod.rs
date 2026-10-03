use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

mod parsing_impl;
mod requests;

pub struct LspClient {
    process: Option<Child>,
    writer: ChildStdin,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    stderr_handle: Option<tokio::task::JoinHandle<()>>,
    next_id: i64,
    pub(crate) pending_requests: HashMap<i64, String>,
}

/// Arguments every clangd this crate spawns is given.
///
/// `--limit-results` defaults to 100, and clangd applies that cap to
/// `workspace/symbol` *before* the reply reaches
/// `handle_workspace_symbols_response`, which then filters it down to functions
/// in project files. On jemalloc the same 100 results filtered to anywhere
/// between 15 and 57 functions depending on which 100 the index happened to
/// return — and since those functions are the only seeds the edge warmup ever
/// expands, the whole graph came out a different size on every run. The seed
/// for a call graph cannot be a 100-symbol sample.
///
/// Nor can it be one unbounded reply: clangd builds that in memory at ~25 KB a
/// result, which on the Linux kernel's 344,769 was past 8 GB. So the limit is
/// [`crate::DISCOVERY_CHUNK`], added in [`base_args`] rather than here because
/// it is spelled from that constant, and the empty query that means "the whole
/// project" is answered by a sweep of chunks that each stay under it — see
/// `service::discovery`. The chunk size and the limit must be the same number:
/// the sweep reads a reply of exactly that many as "cut off, split it".
///
/// `--malloc-trim` asks clangd to hand freed memory back to the OS
/// periodically (it calls `malloc_trim` after requests). Without it, glibc
/// arenas keep whatever the peak touched: on a kernel-scale project the
/// warmup's open documents push RSS past 9 GB, and every byte of that peak
/// is then held for the life of the process whether or not clangd freed the
/// structures underneath. Measured on linux-7.2 (3,089 TUs): clangd's RSS
/// converges on `OPEN_DOCUMENT_LIMIT × preamble`, and a kernel preamble is
/// ~300 MB against jemalloc's ~15 MB.
const CLANGD_ARGS: &[&str] = &["--malloc-trim"];

/// Environment override for how many workers clangd runs (`-j`).
///
/// Unset, no `-j` is passed and clangd sizes its pools to the machine, which is
/// the configuration everything in this crate was measured and tuned against.
/// It is not capped by default, and that was tried: `-j=4` was added on the
/// theory that the background indexer's workers were a kernel-sized project's
/// startup spike. They were not — the spike was the discovery query (see
/// `service::discovery`), and `-j=4` spiked exactly as the default did. It
/// also cost edges: on jemalloc it lost 80–150 of 10,545, silently and a
/// different number each run, where the default holds within the ~20 that
/// `HEAD` wobbles by on its own. So the knob stays for a machine that needs
/// fewer indexing workers on a cold cache, and is left alone otherwise.
///
/// Its own variable rather than part of [`CLANGD_EXTRA_ARGS_ENV`] because
/// LLVM's option parser rejects a `-j` given twice instead of letting the
/// later one win.
const CLANGD_JOBS_ENV: &str = "COON_CLANGD_JOBS";

/// The `-j=<n>` argument, when `COON_CLANGD_JOBS` asks for one.
fn jobs_arg() -> Option<String> {
    let jobs = std::env::var(CLANGD_JOBS_ENV)
        .ok()?
        .parse::<usize>()
        .ok()
        .filter(|v| *v > 0)?;
    log::info!("passing clangd -j={jobs} from {CLANGD_JOBS_ENV}");
    Some(format!("-j={jobs}"))
}

/// Environment override for extra command-line arguments to clangd.
///
/// Whitespace-split and appended after [`CLANGD_ARGS`]. The escape hatch for
/// per-machine or per-project tuning that should not be baked into the
/// binary — `--background-index-priority=low` and the like. A run that
/// needs it spells it out; the startup log records what was passed.
const CLANGD_EXTRA_ARGS_ENV: &str = "COON_CLANGD_EXTRA_ARGS";

/// The arguments every spawned clangd gets: [`CLANGD_ARGS`], the result limit
/// that bounds a discovery chunk, a `-j` if [`jobs_arg`] was asked for one,
/// plus whatever [`CLANGD_EXTRA_ARGS_ENV`] adds for this run.
fn base_args() -> Vec<String> {
    let mut args: Vec<String> = CLANGD_ARGS.iter().map(|s| s.to_string()).collect();
    args.push(format!("--limit-results={}", crate::DISCOVERY_CHUNK));
    args.extend(jobs_arg());
    if let Some(extra) = std::env::var_os(CLANGD_EXTRA_ARGS_ENV) {
        let extra = extra.to_string_lossy().into_owned();
        let split: Vec<String> = extra.split_whitespace().map(|s| s.to_string()).collect();
        if !split.is_empty() {
            log::info!("passing clangd extra args from {CLANGD_EXTRA_ARGS_ENV}: {}", split.join(" "));
            args.extend(split);
        }
    }
    args
}

/// Re-log one line of clangd's stderr at the level clangd gave it.
///
/// clangd prefixes each line with its own severity — `I[12:00:00.000]`,
/// `E[...]`, `V[...]` — and logging the lot as an error buried the app's own
/// errors in tens of thousands of routine index messages. Keeping the level
/// means `grep ERROR` finds clangd actually failing, while the running
/// commentary (which requests it received, which it replied to, and how long
/// each took) stays available a level down.
fn log_clangd_line(line: &str) {
    match line.as_bytes().first() {
        Some(b'E') => log::error!("clangd: {line}"),
        Some(b'W') => log::warn!("clangd: {line}"),
        // `I` is where clangd logs the request/reply traffic that pairs with the
        // ids `LspWorkerState::track_request` writes out.
        Some(b'I') => log::debug!("clangd: {line}"),
        _ => log::trace!("clangd: {line}"),
    }
}

/// Environment override for which clangd binary to run.
///
/// The index is only ever as good as the clangd that built it, and the two
/// questions that follow a wrong-looking graph — is this clangd new enough,
/// and is it the same one my editor uses — can only be answered by naming the
/// binary explicitly. A distro `clangd` on `PATH` is frequently several
/// releases behind the toolchain a project is actually built with, and
/// re-pointing `PATH` is not something a user should have to do to answer a
/// question about one project. Setting this names the choice instead, and the
/// startup log records which way it went.
const CLANGD_PATH_ENV: &str = "COON_CLANGD";

/// Decide which clangd to spawn, and log which it is and where it came from.
///
/// An unset `COON_CLANGD` means `clangd` from `PATH`. A set one is taken
/// literally and checked: pointing it at a missing or non-executable file is a
/// mistake worth naming, because falling back to `PATH` would mean the user
/// asked for one toolchain's clangd and silently got another's — the exact
/// trap a hardcoded path used to set here.
fn resolve_clangd_path() -> Result<String> {
    let Some(configured) = std::env::var_os(CLANGD_PATH_ENV) else {
        log::info!("using clangd `clangd` from PATH (set {CLANGD_PATH_ENV} to override)");
        return Ok("clangd".to_string());
    };

    let path = std::path::PathBuf::from(&configured);
    if !is_executable_file(&path) {
        return Err(anyhow::anyhow!(
            "{CLANGD_PATH_ENV} is set to `{}`, but no executable was found there",
            path.display()
        ));
    }

    log::info!("using clangd `{}` from {CLANGD_PATH_ENV}", path.display());
    Ok(path.to_string_lossy().into_owned())
}

/// Whether `path` is a file this process could actually exec.
fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

impl LspClient {
    /// Create a new LspClient with a custom clangd path
    pub async fn with_path(tx: mpsc::Sender<Value>, clangd_path: &str) -> Result<Self> {
        let process = Command::new(clangd_path)
            .args(base_args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Self::from_process(tx, process).await
    }

    /// Create a new LspClient with a specified working directory
    pub async fn with_working_dir(
        tx: mpsc::Sender<Value>,
        clangd_path: &str,
        working_dir: &std::path::Path,
    ) -> Result<Self> {
        let process = Command::new(clangd_path)
            .args(base_args())
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Self::from_process(tx, process).await
    }

    /// Create a new LspClient running the clangd `resolve_clangd_path` picked.
    ///
    /// That is `$COON_CLANGD` when it is set, and `clangd` from `PATH`
    /// otherwise; a `COON_CLANGD` pointing at nothing runnable is an error
    /// rather than a fallback.
    pub async fn new(tx: mpsc::Sender<Value>) -> Result<Self> {
        Self::new_with_args(tx, &[]).await
    }

    /// The same, plus arguments this particular run needs.
    ///
    /// [`CLANGD_ARGS`] is what every clangd gets; `extra_args` is what one load
    /// asks for on top — today, the `--compile-commands-dir` naming the build
    /// flavour the user chose. A per-run argument cannot join the `const`,
    /// because it is not a property of this crate but of this launch.
    pub async fn new_with_args(tx: mpsc::Sender<Value>, extra_args: &[String]) -> Result<Self> {
        let clangd_path = resolve_clangd_path()?;
        if !extra_args.is_empty() {
            log::info!("passing clangd {}", extra_args.join(" "));
        }
        let process = Command::new(&clangd_path)
            .args(base_args())
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Self::from_process(tx, process).await
    }

    async fn from_process(tx: mpsc::Sender<Value>, mut process: Child) -> Result<Self> {
        let writer = process
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("clangd process stdin not captured"))?;
        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("clangd process stdout not captured"))?;
        let stderr = process
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("clangd process stderr not captured"))?;

        let stderr_handle = tokio::spawn(async move {
            let mut stderr_reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                match stderr_reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        log_clangd_line(line.trim());
                        line.clear();
                    }
                    Err(e) => {
                        log::error!("failed to read from stderr: {}", e);
                        break;
                    }
                }
            }
        });

        let mut reader_half = BufReader::new(stdout);

        let reader_handle = tokio::spawn(async move {
            let mut buffer = String::new();
            loop {
                match reader_half.read_line(&mut buffer).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        if let Some(len_str) = buffer.strip_prefix("Content-Length: ") {
                            if let Ok(len) = len_str.trim().parse::<usize>() {
                                buffer.clear(); // Clear for reading the body
                                                // Read the '\r\n' after the header
                                if reader_half.read_line(&mut buffer).await.is_ok() {
                                    buffer.clear();
                                    let mut content = vec![0; len];
                                    if reader_half.read_exact(&mut content).await.is_ok() {
                                        if let Ok(msg) = serde_json::from_slice::<Value>(&content) {
                                            if tx.send(msg).await.is_err() {
                                                break; // Channel closed
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        buffer.clear();
                    }
                    Err(_) => break, // Error reading line
                }
            }
        });

        Ok(Self {
            process: Some(process),
            writer,
            reader_handle: Some(reader_handle),
            stderr_handle: Some(stderr_handle),
            next_id: 0,
            pending_requests: HashMap::new(),
        })
    }

    /// The server's resident set size in bytes, as the OS sees it.
    ///
    /// Read from `/proc/<pid>/status` on Linux; `None` on other platforms,
    /// and every budget expressed in bytes is then simply off — the document
    /// *count* limit still applies. This is the only honest unit for a memory
    /// ceiling: a document count means 0.5 GB on jemalloc and 9 GB on the
    /// Linux kernel, because the count says nothing about what a document
    /// costs. See `service::document::clangd_rss_budget_bytes`.
    pub fn rss_bytes(&self) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            let pid = self.process.as_ref()?.id()?;
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            let kb: u64 = status
                .lines()
                .find_map(|line| line.strip_prefix("VmRSS:"))?
                .split_whitespace()
                .next()?
                .parse()
                .ok()?;
            Some(kb * 1024)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    pub async fn send_request(&mut self, method: &str, params: Value) -> Result<i64> {
        let id = self.next_id;
        self.next_id += 1;

        // Track the request method for response handling
        self.pending_requests.insert(id, method.to_string());

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let request_str = serde_json::to_string(&request)?;
        let content_length = format!("Content-Length: {}\r\n\r\n", request_str.len());
        self.writer.write_all(content_length.as_bytes()).await?;
        self.writer.write_all(request_str.as_bytes()).await?;
        self.writer.flush().await?;

        Ok(id)
    }

    /// Send a notification to the LSP server (no response expected)
    pub async fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let notification_str = serde_json::to_string(&notification)?;
        let content_length = format!("Content-Length: {}\r\n\r\n", notification_str.len());
        self.writer.write_all(content_length.as_bytes()).await?;
        self.writer.write_all(notification_str.as_bytes()).await?;
        self.writer.flush().await?;

        Ok(())
    }

    /// Answer a request the *server* sent us, with an empty result.
    ///
    /// clangd asks for exactly one thing: `window/workDoneProgress/create`,
    /// before it starts reporting background-index progress. It will not send a
    /// single `$/progress` for a token whose create request is still
    /// outstanding, so leaving this unanswered is indistinguishable from a
    /// clangd that does not report progress at all.
    pub async fn respond_ok(&mut self, id: Value) -> Result<()> {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": Value::Null,
        });

        let response_str = serde_json::to_string(&response)?;
        let content_length = format!("Content-Length: {}\r\n\r\n", response_str.len());
        self.writer.write_all(content_length.as_bytes()).await?;
        self.writer.write_all(response_str.as_bytes()).await?;
        self.writer.flush().await?;

        Ok(())
    }

    /// Gracefully shut down the LSP server by sending `shutdown` then `exit`,
    /// then kill the child process and abort background tasks.
    pub async fn shutdown(&mut self) -> Result<()> {
        // 1. Send the LSP "shutdown" request
        if let Err(e) = self.send_request("shutdown", serde_json::json!(null)).await {
            log::warn!("Failed to send LSP shutdown request: {}", e);
        }

        // Give clangd a moment to process the shutdown request
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // 2. Send the LSP "exit" notification
        if let Err(e) = self
            .send_notification("exit", serde_json::json!(null))
            .await
        {
            log::warn!("Failed to send LSP exit notification: {}", e);
        }

        // 3. Wait briefly for the process to exit on its own
        if let Some(ref mut child) = self.process {
            match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
                Ok(Ok(status)) => {
                    log::info!("clangd exited with status: {}", status);
                }
                Ok(Err(e)) => {
                    log::warn!("Error waiting for clangd to exit: {}", e);
                }
                Err(_) => {
                    // Timed out — force kill
                    log::warn!("clangd did not exit in time, killing");
                    if let Err(e) = child.kill().await {
                        log::error!("Failed to kill clangd: {}", e);
                    }
                }
            }
        }
        self.process = None;

        // 4. Abort background reader tasks
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_handle.take() {
            handle.abort();
        }

        Ok(())
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Safety net: if shutdown() was not called, forcibly clean up.
        if let Some(mut child) = self.process.take() {
            // We can't do async in Drop, so use start_kill (non-blocking).
            let _ = child.start_kill();
            log::warn!("LspClient dropped without shutdown — killed clangd");
        }
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_handle.take() {
            handle.abort();
        }
    }
}
