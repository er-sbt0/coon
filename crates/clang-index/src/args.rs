//! A compilation database entry, turned into what libclang's parser takes.
//!
//! libclang wants the arguments *without* the compiler and without the file
//! being parsed — the file goes to `Index::parser` on its own — and it resolves
//! every relative path against the process's working directory, not the
//! entry's. So the compiler is dropped, the input file is dropped, anything that
//! would write a file is dropped, and `-working-directory` is added so `-Isrc`
//! means what it meant to the build.

use std::path::{Path, PathBuf};

use lsp::compile_commands::CompileCommand;

/// One translation unit to parse: the file, and the arguments to parse it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub file: PathBuf,
    /// The entry's `directory`, which libclang reports a header found through a
    /// relative `-I` relative to.
    pub directory: PathBuf,
    pub arguments: Vec<String>,
}

impl Job {
    pub fn from_entry(entry: &CompileCommand) -> Self {
        let file = entry.path();
        let directory = Path::new(&entry.directory);
        let mut arguments = parser_arguments(&entry.arguments(), directory, &file);
        arguments.push(format!("-working-directory={}", entry.directory));
        Self {
            file,
            directory: directory.to_path_buf(),
            arguments,
        }
    }
}

/// Flags that make the compiler write a dependency file, and whether each takes
/// the next argument as its value. A parse must not write into the project, and
/// `-MMD` beside a libclang parse does exactly that.
const DEPENDENCY_FLAGS: &[(&str, bool)] = &[
    ("-M", false),
    ("-MM", false),
    ("-MD", false),
    ("-MMD", false),
    ("-MP", false),
    ("-MG", false),
    ("-MF", true),
    ("-MT", true),
    ("-MQ", true),
];

/// `arguments` without the compiler, the input file, the output, and the
/// dependency-file flags.
fn parser_arguments(arguments: &[String], directory: &Path, file: &Path) -> Vec<String> {
    let mut out = Vec::with_capacity(arguments.len());
    // The first argument is the compiler, whatever it is called.
    let mut rest = arguments.iter().skip(1);
    while let Some(argument) = rest.next() {
        let argument = argument.as_str();
        if argument == "-c" {
            continue;
        }
        if argument == "-o" {
            rest.next();
            continue;
        }
        if argument.starts_with("-o") && argument.len() > 2 {
            continue;
        }
        if let Some(&(_, takes_value)) = DEPENDENCY_FLAGS.iter().find(|(f, _)| *f == argument) {
            if takes_value {
                rest.next();
            }
            continue;
        }
        if DEPENDENCY_FLAGS
            .iter()
            .any(|&(f, takes_value)| takes_value && argument.starts_with(f))
        {
            continue;
        }
        if !argument.starts_with('-') && names_file(argument, directory, file) {
            continue;
        }
        out.push(argument.to_string());
    }
    out
}

/// Whether a bare argument is the input file, however the database spelled it.
fn names_file(argument: &str, directory: &Path, file: &Path) -> bool {
    let candidate = Path::new(argument);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        directory.join(candidate)
    };
    candidate == file
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(arguments: &[&str], file: &str) -> CompileCommand {
        CompileCommand {
            directory: "/proj".into(),
            command: None,
            file: file.into(),
            arguments: Some(arguments.iter().map(|s| s.to_string()).collect()),
            output: None,
        }
    }

    #[test]
    fn drops_compiler_input_output_and_dependency_flags() {
        let job = Job::from_entry(&entry(
            &[
                "gcc", "-Wall", "-Isrc", "-MMD", "-MP", "-MF", "x.d", "-c", "main.c", "-o",
                "main.o",
            ],
            "main.c",
        ));
        assert_eq!(job.file, PathBuf::from("/proj/main.c"));
        assert_eq!(
            job.arguments,
            vec!["-Wall", "-Isrc", "-working-directory=/proj"]
        );
    }

    #[test]
    fn keeps_defines_and_absolute_input_is_dropped() {
        let job = Job::from_entry(&entry(
            &["cc", "-DX=1", "-c", "/proj/src/a.c", "-o/proj/a.o"],
            "src/a.c",
        ));
        assert_eq!(job.arguments, vec!["-DX=1", "-working-directory=/proj"]);
    }
}
