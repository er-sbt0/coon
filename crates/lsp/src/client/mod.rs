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

impl LspClient {
    /// Create a new LspClient with a custom clangd path
    pub async fn with_path(tx: mpsc::Sender<Value>, clangd_path: &str) -> Result<Self> {
        let process = Command::new(clangd_path)
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
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Self::from_process(tx, process).await
    }

    /// Create a new LspClient that tries to find clangd in PATH
    pub async fn new(tx: mpsc::Sender<Value>) -> Result<Self> {
        // First try the hardcoded path from the original code
        if std::path::Path::new("/home/eransa/opt/llvm/llvm-20.1.8-build/bin/clangd").exists() {
            return Self::with_path(tx, "/home/eransa/opt/llvm/llvm-20.1.8-build/bin/clangd").await;
        }

        // Then try to find clangd in PATH
        Self::with_path(tx, "clangd").await
    }

    async fn from_process(tx: mpsc::Sender<Value>, mut process: Child) -> Result<Self> {
        let writer = process.stdin.take().unwrap();
        let stdout = process.stdout.take().unwrap();
        let stderr = process.stderr.take().unwrap();

        let stderr_handle = tokio::spawn(async move {
            let mut stderr_reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                match stderr_reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        log::error!("LSP stderr: {}", line.trim());
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
