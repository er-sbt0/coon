//! The project's compilation database: which files it holds, and which *build*
//! each one is compiled as.
//!
//! A header compiled under two different `-D` sets is two different pieces of
//! code, and a call graph is of one build. See [`Flavour`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

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

impl CompileCommand {
    /// The file this entry compiles, resolved against its `directory` but not
    /// canonicalised — that costs a syscall, and most callers only want a name.
    pub fn path(&self) -> PathBuf {
        let file = Path::new(&self.file);
        if file.is_absolute() {
            file.to_path_buf()
        } else {
            Path::new(&self.directory).join(file)
        }
    }

    /// The argument vector, however the database spells it.
    ///
    /// The format allows either `arguments` (already split) or `command` (one
    /// shell line); a generator picks one and no reader gets to choose. The
    /// shell split is the same one a shell would do, quotes and all, because a
    /// define's value is routinely quoted — `-DJEMALLOC_CONFIG='"..."'`.
    pub fn arguments(&self) -> Vec<String> {
        if let Some(arguments) = &self.arguments {
            return arguments.clone();
        }
        self.command
            .as_deref()
            .and_then(shlex::split)
            .unwrap_or_default()
    }

    /// Everything in this entry's command line that changes what the
    /// preprocessor sees, sorted and deduplicated: see [`FlavourKey`].
    pub fn flavour_key(&self) -> FlavourKey {
        let arguments = self.arguments();
        let mut defines: BTreeSet<String> = BTreeSet::new();
        let mut index = 0;

        while index < arguments.len() {
            let argument = arguments[index].as_str();
            index += 1;

            // The two-token forms. `-D FOO` and `-DFOO` are the same flag, so
            // they have to normalise to the same string or a database that
            // spells them differently reads as two builds.
            if let "-D" | "-U" | "-include" | "-imacros" = argument {
                if let Some(value) = arguments.get(index) {
                    index += 1;
                    defines.insert(normalised_define(argument, value));
                }
                continue;
            }

            for flag in ["-D", "-U", "-include", "-imacros"] {
                if let Some(value) = argument.strip_prefix(flag) {
                    if !value.is_empty() {
                        defines.insert(normalised_define(flag, value));
                        break;
                    }
                }
            }
        }

        FlavourKey(defines.into_iter().collect())
    }
}

/// `-D`/`-U` join their value; `-include`/`-imacros` keep the space, because
/// `-includefoo.h` is not a spelling every compiler accepts.
fn normalised_define(flag: &str, value: &str) -> String {
    match flag {
        "-D" | "-U" => format!("{flag}{value}"),
        _ => format!("{flag} {value}"),
    }
}

/// What makes two entries the same build: their preprocessor-defining
/// arguments, sorted and deduplicated.
///
/// Only those. Optimisation levels, warning flags and output paths differ
/// between entries that compile the identical code, and include paths are left
/// out for the same reason — they change where a header is found, not what it
/// says once found. `-D`, `-U`, `-include` and `-imacros` are what decide which
/// declarations a translation unit sees, which is what a call graph is built
/// out of.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FlavourKey(Vec<String>);

impl FlavourKey {
    pub fn defines(&self) -> &[String] {
        &self.0
    }

    /// A short, stable name for this key, used as the directory holding the
    /// flavour's own database and therefore its own clangd index shards.
    ///
    /// FNV-1a rather than `DefaultHasher`, whose output the standard library
    /// explicitly does not promise to keep across releases: the whole point of
    /// this name is that the same flavour lands on the same directory on every
    /// run, including after a toolchain upgrade.
    pub fn short_hash(&self) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.0.join("\u{0}").bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }
}

/// One build of the project: every entry of the compilation database whose
/// [`FlavourKey`] is the same.
///
/// A project is rarely one build. jemalloc's 217 entries are eight: 68 library
/// files with nothing but `-D_GNU_SOURCE -D_REENTRANT`, 112 unit tests with
/// `-DJEMALLOC_UNIT_TEST`, 14 integration tests, and five smaller sets. The
/// tests see `jet_*` declarations where the library sees `je_*`, out of the
/// same headers — so "does this function call that one" has a different answer
/// per build, and handing clangd all of them at once means the answer depends
/// on which translation unit published a header most recently.
#[derive(Debug, Clone)]
pub struct Flavour {
    pub key: FlavourKey,
    pub entries: Vec<CompileCommand>,
}

impl Flavour {
    /// The file that stands for this flavour in a listing, and the one a user
    /// passes to `--flavour`: the first of its files in sorted order, so it is
    /// the same file whatever order the database lists its entries in.
    pub fn representative(&self) -> PathBuf {
        let mut paths: Vec<PathBuf> = self.entries.iter().map(CompileCommand::path).collect();
        paths.sort();
        paths.into_iter().next().unwrap_or_default()
    }
}

/// Group a database's entries into the builds they belong to, widest first.
///
/// Ties go to the representative's path, so the order of the list — which is
/// the order the user reads it in — does not depend on the order the database
/// happens to be written in.
pub fn group_by_flavour(entries: Vec<CompileCommand>) -> Vec<Flavour> {
    let mut grouped: std::collections::HashMap<FlavourKey, Vec<CompileCommand>> =
        std::collections::HashMap::new();
    for entry in entries {
        grouped.entry(entry.flavour_key()).or_default().push(entry);
    }

    let mut flavours: Vec<Flavour> = grouped
        .into_iter()
        .map(|(key, entries)| Flavour { key, entries })
        .collect();
    flavours.sort_by(|a, b| {
        b.entries
            .len()
            .cmp(&a.entries.len())
            .then_with(|| a.representative().cmp(&b.representative()))
    });
    flavours
}

/// The defines that tell these flavours apart: each one's key minus what they
/// all share. Listing `-D_GNU_SOURCE` against every line says nothing.
fn distinguishing(flavours: &[Flavour]) -> Vec<Vec<String>> {
    let mut shared: BTreeSet<&str> = flavours
        .first()
        .map(|first| first.key.defines().iter().map(String::as_str).collect())
        .unwrap_or_default();
    for flavour in flavours.iter().skip(1) {
        let mine: BTreeSet<&str> = flavour.key.defines().iter().map(String::as_str).collect();
        shared = shared.intersection(&mine).copied().collect();
    }

    flavours
        .iter()
        .map(|flavour| {
            flavour
                .key
                .defines()
                .iter()
                .filter(|define| !shared.contains(define.as_str()))
                .cloned()
                .collect()
        })
        .collect()
}

/// The flavours as a user reads them: one line each, `count × file` followed by
/// the defines that make it that build.
///
/// The file is the one to hand `--flavour`, written relative to the project
/// wherever it lives inside it, because that is how it will be typed.
pub fn describe_flavours(flavours: &[Flavour], project_path: &Path) -> Vec<String> {
    let distinguishing = distinguishing(flavours);
    flavours
        .iter()
        .zip(distinguishing)
        .map(|(flavour, defines)| {
            let representative = flavour.representative();
            let shown = representative
                .strip_prefix(project_path)
                .unwrap_or(&representative);
            let defines = if defines.is_empty() {
                "no defines of its own".to_string()
            } else {
                defines.join(" ")
            };
            format!(
                "{:>5} × {}    {defines}",
                flavour.entries.len(),
                shown.display()
            )
        })
        .collect()
}

/// The flavour that compiles `wanted`, or a message naming every flavour there
/// is to choose from.
///
/// `wanted` is taken as the user typed it: absolute, or relative to the project
/// directory. Both sides are canonicalised before they are compared, because a
/// database routinely spells a path through a symlink or a `..` that the user
/// never types.
pub async fn select_flavour<'a>(
    flavours: &'a [Flavour],
    project_path: &Path,
    wanted: &Path,
) -> Result<&'a Flavour, String> {
    let candidate = if wanted.is_absolute() {
        wanted.to_path_buf()
    } else {
        project_path.join(wanted)
    };
    let candidate = tokio::fs::canonicalize(&candidate)
        .await
        .unwrap_or(candidate);

    for flavour in flavours {
        for entry in &flavour.entries {
            let path = entry.path();
            let path = tokio::fs::canonicalize(&path).await.unwrap_or(path);
            if path == candidate {
                return Ok(flavour);
            }
        }
    }

    let listing = describe_flavours(flavours, project_path).join("\n  ");
    Err(format!(
        "`{}` is not in this project's compile_commands.json.\n\
         Pass one of these files to --flavour, whichever build you want the \
         call graph to be of:\n  {listing}",
        wanted.display()
    ))
}

/// Check a `--flavour` choice against the project, for a frontend that would
/// rather fail at startup than open a window onto a failed load.
///
/// The loader resolves the flavour again when it runs — this is a check, not a
/// handover — because the two happen on different threads at different moments
/// and passing a resolved `Flavour` between them would only mean the answer
/// could be stale in one of them.
pub async fn check_flavour(project_path: &Path, wanted: &Path) -> Result<(), String> {
    let entries = read_entries(project_path)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "{} has no compile_commands.json, so there are no builds to choose between",
                project_path.display()
            )
        })?;
    select_flavour(&group_by_flavour(entries), project_path, wanted)
        .await
        .map(|_| ())
}

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

/// Every entry of the project's compilation database, or `None` when there
/// isn't one.
pub async fn read_entries(project_path: &Path) -> anyhow::Result<Option<Vec<CompileCommand>>> {
    let compile_commands_path = project_path.join("compile_commands.json");

    if !tokio::fs::try_exists(&compile_commands_path)
        .await
        .unwrap_or(false)
    {
        return Ok(None);
    }

    log::info!(
        "Reading compile_commands.json from: {}",
        compile_commands_path.display()
    );
    let content = tokio::fs::read_to_string(&compile_commands_path).await?;
    let entries: Vec<CompileCommand> = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse compile_commands.json: {}", e))?;
    Ok(Some(entries))
}

/// The files these entries compile: existing ones only, canonicalised, sorted.
///
/// Sorted because the loader opens the first of them and must open the same one
/// on every run; existence-checked because a database outlives the tree it
/// describes.
pub async fn canonical_files(entries: &[CompileCommand]) -> Vec<PathBuf> {
    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = entry.path();
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            log::warn!("Source file not found: {}", path.display());
            continue;
        }
        match tokio::fs::canonicalize(&path).await {
            Ok(canonical) => files.push(canonical),
            Err(e) => log::warn!("Failed to canonicalize path {}: {}", path.display(), e),
        }
    }
    files.sort();
    files.dedup();
    files
}

/// Parse compile_commands.json to extract source file paths
pub async fn parse_compile_commands(project_path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let Some(entries) = read_entries(project_path).await? else {
        // Fallback to directory walking if no compile_commands.json exists
        log::info!("No compile_commands.json found, falling back to directory discovery");
        let project_path = project_path.to_owned();
        return tokio::task::spawn_blocking(move || discover_source_files_fallback(&project_path))
            .await?;
    };

    let source_files = canonical_files(&entries).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn with_command(file: &str, command: &str) -> CompileCommand {
        CompileCommand {
            directory: "/project".to_string(),
            command: Some(command.to_string()),
            file: file.to_string(),
            arguments: None,
            output: None,
        }
    }

    fn with_arguments(file: &str, arguments: &[&str]) -> CompileCommand {
        CompileCommand {
            directory: "/project".to_string(),
            command: None,
            file: file.to_string(),
            arguments: Some(arguments.iter().map(|a| a.to_string()).collect()),
            output: None,
        }
    }

    fn defines(entry: &CompileCommand) -> Vec<String> {
        entry.flavour_key().defines().to_vec()
    }

    #[test]
    fn only_what_the_preprocessor_reads_is_part_of_the_key() {
        // -O2, -Wall and the output path differ between entries compiling the
        // identical code; the include path changes where a header is found,
        // not what it says.
        let entry = with_arguments(
            "a.c",
            &[
                "cc",
                "-O2",
                "-Wall",
                "-Iinclude",
                "-DFOO",
                "-c",
                "a.c",
                "-o",
                "a.o",
            ],
        );
        assert_eq!(defines(&entry), vec!["-DFOO"]);
    }

    #[test]
    fn the_two_spellings_of_a_define_are_one_build() {
        let joined = with_arguments("a.c", &["cc", "-DFOO=1", "-UBAR"]);
        let split = with_arguments("b.c", &["cc", "-D", "FOO=1", "-U", "BAR"]);
        assert_eq!(joined.flavour_key(), split.flavour_key());
    }

    #[test]
    fn a_key_does_not_depend_on_the_order_or_the_repetition() {
        let one = with_arguments("a.c", &["cc", "-DB", "-DA", "-DB"]);
        let other = with_arguments("b.c", &["cc", "-DA", "-DB"]);
        assert_eq!(one.flavour_key(), other.flavour_key());
    }

    #[test]
    fn a_command_line_is_split_like_a_shell_would() {
        // The quoting is not decoration: jemalloc spells configuration strings
        // this way, and a naive split on spaces makes every such entry unique.
        let entry = with_command("a.c", r#"cc -DCONF='"a b"' -DPLAIN -c a.c"#);
        assert_eq!(defines(&entry), vec!["-DCONF=\"a b\"", "-DPLAIN"]);
    }

    #[test]
    fn an_included_header_is_part_of_the_build() {
        let entry = with_arguments("a.c", &["cc", "-include", "config.h", "-imacros", "m.h"]);
        assert_eq!(defines(&entry), vec!["-imacros m.h", "-include config.h"]);
    }

    #[test]
    fn entries_group_into_builds_widest_first() {
        let flavours = group_by_flavour(vec![
            with_arguments("test/b.c", &["cc", "-DTEST"]),
            with_arguments("src/a.c", &["cc"]),
            with_arguments("test/a.c", &["cc", "-DTEST"]),
        ]);

        assert_eq!(flavours.len(), 2);
        assert_eq!(flavours[0].entries.len(), 2, "the test build is wider");
        assert_eq!(
            flavours[0].representative(),
            PathBuf::from("/project/test/a.c")
        );
        assert_eq!(
            flavours[1].representative(),
            PathBuf::from("/project/src/a.c")
        );
    }

    #[test]
    fn a_listing_names_only_the_defines_that_differ() {
        let flavours = group_by_flavour(vec![
            with_arguments("test/a.c", &["cc", "-D_GNU_SOURCE", "-DTEST"]),
            with_arguments("test/b.c", &["cc", "-D_GNU_SOURCE", "-DTEST"]),
            with_arguments("src/a.c", &["cc", "-D_GNU_SOURCE"]),
        ]);

        let lines = describe_flavours(&flavours, Path::new("/project"));
        assert!(lines[0].contains("2 × test/a.c"), "{}", lines[0]);
        assert!(lines[0].ends_with("-DTEST"), "{}", lines[0]);
        assert!(
            !lines[0].contains("_GNU_SOURCE"),
            "a define every build has tells them apart from nothing: {}",
            lines[0]
        );
        assert!(lines[1].contains("no defines of its own"), "{}", lines[1]);
    }

    #[test]
    fn a_flavour_is_named_by_its_defines_and_nothing_else() {
        let one = group_by_flavour(vec![with_arguments("a.c", &["cc", "-DFOO"])]);
        let other = group_by_flavour(vec![with_arguments("z.c", &["cc", "-D", "FOO"])]);
        assert_eq!(one[0].key.short_hash(), other[0].key.short_hash());

        let different = group_by_flavour(vec![with_arguments("a.c", &["cc", "-DBAR"])]);
        assert_ne!(one[0].key.short_hash(), different[0].key.short_hash());
    }
}
