use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Represents a compilation database entry from compile_commands.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileCommand {
    pub directory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Parse compile_commands.json to extract source file paths
pub async fn parse_compile_commands(
    project_path: &std::path::Path,
) -> anyhow::Result<Vec<PathBuf>> {
    let compile_commands_path = project_path.join("compile_commands.json");

    if !tokio::fs::try_exists(&compile_commands_path)
        .await
        .unwrap_or(false)
    {
        // Fallback to directory walking if no compile_commands.json exists
        log::info!("No compile_commands.json found, falling back to directory discovery");
        let project_path = project_path.to_owned();
        return tokio::task::spawn_blocking(move || discover_source_files_fallback(&project_path))
            .await?;
    }

    log::info!(
        "Reading compile_commands.json from: {}",
        compile_commands_path.display()
    );

    let content = tokio::fs::read_to_string(&compile_commands_path).await?;
    log::debug!("compile_commands.json content: {}", content);

    let compile_commands: Vec<CompileCommand> = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse compile_commands.json: {}", e))?;

    let mut source_files = Vec::new();
    for cmd in compile_commands {
        let file_path = if std::path::Path::new(&cmd.file).is_absolute() {
            PathBuf::from(cmd.file)
        } else {
            // Resolve relative path from the directory in the compile command
            let base_dir = std::path::Path::new(&cmd.directory);
            base_dir.join(&cmd.file)
        };

        // Canonicalize the path if it exists
        if tokio::fs::try_exists(&file_path).await.unwrap_or(false) {
            match tokio::fs::canonicalize(&file_path).await {
                Ok(canonical_path) => source_files.push(canonical_path),
                Err(e) => log::warn!("Failed to canonicalize path {}: {}", file_path.display(), e),
            }
        } else {
            log::warn!("Source file not found: {}", file_path.display());
        }
    }

    log::info!(
        "Found {} source files from compile_commands.json",
        source_files.len()
    );
    Ok(source_files)
}

/// Fallback directory walking when compile_commands.json is not available
fn discover_source_files_fallback(
    project_path: &std::path::Path,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut source_files = Vec::new();

    // Common source file extensions
    let extensions = [
        "rs", "c", "cpp", "cxx", "cc", "h", "hpp", "hxx", "py", "js", "ts", "java", "go", "php",
        "rb", "cs", "swift", "kt", "scala", "clj", "hs", "ml", "elm",
    ];

    // Walk the directory tree
    fn walk_dir(
        dir: &std::path::Path,
        extensions: &[&str],
        files: &mut Vec<std::path::PathBuf>,
        max_files: usize,
    ) -> anyhow::Result<()> {
        if files.len() >= max_files {
            return Ok(());
        }

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                // Skip common non-source directories
                if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                    if !matches!(
                        dir_name,
                        "target" | "build" | "node_modules" | ".git" | ".svn" | "dist" | "out"
                    ) {
                        walk_dir(&path, extensions, files, max_files)?;
                    }
                }
            } else if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if extensions.contains(&ext) {
                        files.push(path);
                        if files.len() >= max_files {
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    walk_dir(project_path, &extensions, &mut source_files, 100)?; // Limit to 100 files
    Ok(source_files)
}
