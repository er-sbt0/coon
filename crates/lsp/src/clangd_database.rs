//! The half of the compilation database only clangd needs.
//!
//! Everything about what a flavour *is* lives in the `compdb` crate, shared
//! with the libclang backend. What is here is how clangd is made to see one:
//! by a database of its own on disk, since clangd takes a directory and not a
//! list of commands — and, with no database at all, a walk of the tree for
//! files to open, which is a thing only clangd can make use of.

use compdb::Flavour;
use std::path::{Path, PathBuf};

/// Where a flavour's own compilation database lives: inside the project, one
/// directory per flavour.
///
/// Inside the project rather than a temp dir because clangd keys its
/// background-index storage by the directory the database is in. A temp dir
/// would mean a cold index on every launch; a stable per-flavour directory
/// means each build keeps its own shards and never mixes them with another's,
/// which is the property the whole flag exists for. It is a cache and can be
/// deleted at any time — the first run of a flavour indexes from cold again,
/// which is what `INDEX_WAIT_TIMEOUT` and the re-query on a later `end` in
/// `loader` are there to survive.
pub fn flavour_database_dir(project_path: &Path, flavour: &Flavour) -> PathBuf {
    project_path
        .join(".cache")
        .join("coon")
        .join("flavours")
        .join(flavour.key.short_hash())
}

/// Write `flavour`'s entries out as a compilation database of their own and
/// answer with the directory to point clangd at.
///
/// The entries go out verbatim: they are what the project's build system said
/// to run, and clangd is being told to read the same commands for fewer files,
/// not different commands.
pub async fn write_flavour_database(
    project_path: &Path,
    flavour: &Flavour,
) -> anyhow::Result<PathBuf> {
    let dir = flavour_database_dir(project_path, flavour);
    tokio::fs::create_dir_all(&dir).await?;
    let json = serde_json::to_string_pretty(&flavour.entries)?;
    tokio::fs::write(dir.join("compile_commands.json"), json).await?;
    log::info!(
        "Wrote a {}-entry compilation database for this flavour to {}",
        flavour.entries.len(),
        dir.display()
    );
    Ok(dir)
}

/// Source files for a project with no compilation database, found by walking
/// the tree.
///
/// Only clangd can use these: it will guess a command line for a file nothing
/// describes, where libclang refuses a project without a database outright.
pub async fn discover_source_files(project_path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    log::info!("No compile_commands.json found, falling back to directory discovery");
    let project_path = project_path.to_owned();
    tokio::task::spawn_blocking(move || discover_source_files_fallback(&project_path)).await?
}

/// Fallback directory walking when compile_commands.json is not available
fn discover_source_files_fallback(
    project_path: &std::path::Path,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut source_files = Vec::new();

    let extensions = ["c", "cpp", "cxx", "cc", "h", "hpp", "hxx"];

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
