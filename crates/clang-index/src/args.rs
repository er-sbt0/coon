//! A compilation database entry, turned into what libclang's parser takes.
//!
//! libclang wants the arguments *without* the compiler and without the file
//! being parsed — the file goes to `Index::parser` on its own — and it resolves
//! every relative path against the process's working directory, not the
//! entry's. So the input file is dropped, anything that would write a file is
//! dropped, and `-working-directory` is added so `-Isrc` means what it meant to
//! the build.
//!
//! The compiler is dropped too, but not before it is read: libclang always
//! calls itself `clang`, so what the build's driver name said — a cross
//! compiler's target, a C++ driver's language — has to be spelled out as flags
//! or the parse reads different code from the build.
//!
//! Not done: asking a cross compiler for its system include paths, which is
//! what clangd's `--query-driver` is for. clang finds a Debian-style
//! `gcc-cross` installation for a `--target` on its own, and a freestanding
//! build (the kernel's `-nostdinc`) needs none; anything else shows up as
//! missing-include errors in the report. Doing it means running the build's
//! compiler, which is not something a parse should do unasked.

use std::path::{Path, PathBuf};

use compdb::{CompileCommand, FlavourKey};

use crate::normalise;

/// One translation unit to parse: the file, and the arguments to parse it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub file: PathBuf,
    /// The entry's `directory`, which libclang reports a header found through a
    /// relative `-I` relative to.
    pub directory: PathBuf,
    pub arguments: Vec<String>,
    /// Which build of the database this TU belongs to. A header's inline
    /// function is walked once per flavour rather than once per run, because
    /// two flavours can compile the same definition into different code.
    pub flavour: FlavourKey,
}

impl Job {
    pub fn from_entry(entry: &CompileCommand) -> Self {
        let file = entry.path();
        let directory = Path::new(&entry.directory);
        let all = entry.arguments();
        let (compiler, rest) = split_compiler(&all);
        let mut arguments = compiler
            .map(|compiler| implied_by_driver(compiler, rest, &file))
            .unwrap_or_default();
        arguments.extend(parser_arguments(rest, directory, &file));
        arguments.extend(QUIET.iter().map(|s| s.to_string()));
        arguments.push(format!("-working-directory={}", entry.directory));
        Self {
            file,
            directory: directory.to_path_buf(),
            arguments,
            flavour: entry.flavour_key(),
        }
    }
}

/// Appended to every parse. A parse has no use for warnings — nothing reads
/// them, and the report counts errors only — but a build's `-Werror` promotes
/// them, and a gcc build's `-Wno-maybe-uninitialized` or `-falign-jumps=1` is a
/// warning in clang that `-Werror` turns into an error that is not one: the
/// code read fine. `-w` silences every warning, unknown-warning-option and
/// unused-argument included, so `-Wno-unknown-warning-option` and
/// `-Qunused-arguments` would add nothing to it.
const QUIET: &[&str] = &["-w"];

/// Programs that run the compiler named by the next argument: ccache, distcc
/// and the like put themselves in front of the compiler in the database.
const WRAPPERS: &[&str] = &["ccache", "sccache", "distcc", "icecc", "buildcache"];

/// The compiler and everything after it, past any wrapper in front of it.
fn split_compiler(arguments: &[String]) -> (Option<&str>, &[String]) {
    let Some((first, rest)) = arguments.split_first() else {
        return (None, arguments);
    };
    if WRAPPERS.contains(&program_name(first)) {
        if let Some((compiler, rest)) = rest.split_first() {
            if !compiler.starts_with('-') {
                return (Some(compiler), rest);
            }
        }
    }
    (Some(first), rest)
}

/// A program's name without its directory or `.exe`.
fn program_name(path: &str) -> &str {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    name.strip_suffix(".exe").unwrap_or(name)
}

/// Driver names a target prefix can stand in front of, and whether each one
/// compiles every input as C++.
const DRIVERS: &[(&str, bool)] = &[
    ("gcc", false),
    ("cc", false),
    ("clang", false),
    ("g++", true),
    ("c++", true),
    ("clang++", true),
];

/// What the build's driver name meant that libclang, which is always `clang`,
/// would otherwise not know: the flags to say it with.
///
/// The target comes from a triple in front of the name, as clang's own driver
/// and clangd read it — `aarch64-linux-gnu-gcc` builds for aarch64, and parsed
/// as the host it would see the host's `long`, the host's `__x86_64__` and the
/// host's half of every `#ifdef __aarch64__`. An explicit `--target` wins.
///
/// The language comes from the driver: `g++ a.c` compiles C++, where
/// `clang a.c` reads C — and the `class` in it is then a syntax error and the
/// function around it is gone. `--driver-mode=g++` is what clangd adds for the
/// same reason, but clang reads a `.c` file as C++ under it only with a
/// deprecation warning, so a `.c` input gets an explicit `-x c++` as well,
/// which does not depend on that surviving.
fn implied_by_driver(compiler: &str, rest: &[String], file: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut name = program_name(compiler);
    // `gcc-12`, `clang++-18`: the version says nothing about the parse.
    if let Some((stem, version)) = name.rsplit_once('-') {
        if !version.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.') {
            name = stem;
        }
    }
    let Some((prefix, cxx)) = DRIVERS.iter().find_map(|&(driver, cxx)| {
        if name == driver {
            Some((None, cxx))
        } else {
            name.strip_suffix(driver)
                .and_then(|p| p.strip_suffix('-'))
                .map(|p| (Some(p), cxx))
        }
    }) else {
        return out;
    };

    let has_target = rest.iter().any(|a| {
        a == "-target" || a == "--target" || a.starts_with("--target=") || a.starts_with("-target=")
    });
    if let Some(triple) = prefix.filter(|p| !has_target && is_known_triple(p)) {
        out.push(format!("--target={triple}"));
    }

    if cxx {
        if !rest.iter().any(|a| a.starts_with("--driver-mode")) {
            out.push("--driver-mode=g++".to_string());
        }
        let has_language = rest.iter().any(|a| a.starts_with("-x"));
        let is_c = file.extension().is_some_and(|e| e == "c");
        if is_c && !has_language {
            out.extend(["-x".to_string(), "c++".to_string()]);
        }
    }
    out
}

/// Whether `prefix` is a triple clang can parse for, by its architecture.
///
/// A triple clang does not know fails the whole TU — `sh4-linux-gnu` does,
/// with no diagnostic at all — so a prefix is believed only when its
/// architecture is one clang has a target for. Anything else is parsed as the
/// host, which reads the code with the wrong `#ifdef`s rather than not at all.
fn is_known_triple(prefix: &str) -> bool {
    let arch = prefix.split('-').next().unwrap_or("");
    const EXACT: &[&str] = &[
        "x86_64",
        "amd64",
        "i386",
        "i486",
        "i586",
        "i686",
        "aarch64",
        "aarch64_be",
        "riscv32",
        "riscv64",
        "s390x",
        "systemz",
        "loongarch32",
        "loongarch64",
        "avr",
        "msp430",
        "hexagon",
        "m68k",
        "csky",
        "xtensa",
        "arc",
        "wasm32",
        "wasm64",
        "bpf",
        "bpfel",
        "bpfeb",
        "lanai",
        "ve",
    ];
    const FAMILIES: &[&str] = &["arm", "thumb", "mips", "powerpc", "ppc", "sparc"];
    EXACT.contains(&arch) || FAMILIES.iter().any(|f| arch.starts_with(f))
}

/// Flags that make the compiler write a file beside the parse, and whether each
/// takes the next argument as its value. A parse must not write into the
/// project, and libclang honours these: `-MMD` writes a dependency file and
/// `-MJ` a compilation-database fragment, exactly as the build would.
const OUTPUT_FLAGS: &[(&str, bool)] = &[
    ("-o", true),
    ("-M", false),
    ("-MM", false),
    ("-MD", false),
    ("-MMD", false),
    ("-MP", false),
    ("-MG", false),
    ("-MF", true),
    ("-MT", true),
    ("-MQ", true),
    ("-MJ", true),
    ("--serialize-diagnostics", true),
];

/// The value-taking flags above that also take it joined: `-ofile`, `-MJfile`.
const JOINED_OUTPUT_FLAGS: &[&str] = &["-o", "-MF", "-MT", "-MQ", "-MJ"];

/// Flags that write a file, in any spelling that begins this way. `-save-temps`
/// is here for a second reason: libclang does not merely write the temporaries,
/// it fails the TU.
const OUTPUT_PREFIXES: &[&str] = &["-save-temps", "--save-temps", "-save-stats", "-ftime-trace"];

/// Inside `-Wp,`, which hands its comma-separated parts to the preprocessor:
/// the dependency flags, and whether each takes the next part as its value.
/// Here `-MD` and `-MMD` do — `-Wp,-MD,file` is how the Linux kernel's build
/// asks for a dependency file on every TU it compiles.
const PREPROCESSOR_DEPENDENCY_FLAGS: &[(&str, bool)] = &[
    ("-M", false),
    ("-MM", false),
    ("-MD", true),
    ("-MMD", true),
    ("-MP", false),
    ("-MG", false),
    ("-MF", true),
    ("-MT", true),
    ("-MQ", true),
];

/// `arguments` (the compiler already gone) without `-c`, the input file, and
/// the flags that write files.
fn parser_arguments(arguments: &[String], directory: &Path, file: &Path) -> Vec<String> {
    let mut out = Vec::with_capacity(arguments.len());
    let mut rest = arguments.iter();
    while let Some(argument) = rest.next() {
        let argument = argument.as_str();
        if argument == "-c" {
            continue;
        }
        if let Some(&(_, takes_value)) = OUTPUT_FLAGS.iter().find(|(f, _)| *f == argument) {
            if takes_value {
                rest.next();
            }
            continue;
        }
        if JOINED_OUTPUT_FLAGS.iter().any(|f| argument.starts_with(f)) {
            continue;
        }
        if OUTPUT_PREFIXES.iter().any(|p| argument.starts_with(p)) {
            continue;
        }
        if let Some(parts) = argument.strip_prefix("-Wp,") {
            if let Some(kept) = without_dependency_output(parts) {
                out.push(format!("-Wp,{kept}"));
            }
            continue;
        }
        if !argument.starts_with('-') && names_file(argument, directory, file) {
            continue;
        }
        out.push(argument.to_string());
    }
    out
}

/// The parts of a `-Wp,` argument other than its dependency-file flags, or
/// `None` if nothing is left.
///
/// Only the dependency parts go, not the whole argument, because the same
/// spelling carries things the parse needs: `-Wp,-D_FORTIFY_SOURCE=2` is in
/// most distributions' default flags and changes what glibc's headers declare.
fn without_dependency_output(parts: &str) -> Option<String> {
    let mut kept = Vec::new();
    let mut parts = parts.split(',');
    while let Some(part) = parts.next() {
        if let Some(&(_, takes_value)) = PREPROCESSOR_DEPENDENCY_FLAGS
            .iter()
            .find(|(f, _)| *f == part)
        {
            if takes_value {
                parts.next();
            }
            continue;
        }
        if ["-MF", "-MT", "-MQ"].iter().any(|f| part.starts_with(f)) {
            continue;
        }
        kept.push(part);
    }
    (!kept.is_empty()).then(|| kept.join(","))
}

/// Whether a bare argument is the input file, however the database spelled it:
/// `src/../src/a.c` and `/proj/build/../src/a.c` are both `/proj/src/a.c`.
fn names_file(argument: &str, directory: &Path, file: &Path) -> bool {
    let candidate = Path::new(argument);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        directory.join(candidate)
    };
    candidate == file || normalise(&candidate) == normalise(file)
}

/// The argument a driver diagnostic says libclang refused, if it is one.
///
/// A gcc build is full of flags clang does not have — the kernel passes
/// `-fconserve-stack`, `-mindirect-branch=thunk-extern`, `-mrecord-mcount` and a
/// dozen more on every TU. libclang does not fail the TU over one: it reports
/// an error with no source location, ignores the flag, and parses the rest as
/// asked, every later `-D` included. So the code was read; the error is about
/// the command line. The caller knows the diagnostic had no location; this
/// finds which of `arguments` it names, by the quoted spans clang puts an
/// argument in — "unknown argument: '-fconserve-stack'", "unsupported option
/// '-mrecord-mcount' for target '…'", or "unsupported argument 'all' to option
/// '-mharden-sls='", where the argument is the option and its value joined.
/// Matching against the arguments rather than the wording is what keeps this
/// from depending on a message clang may rephrase.
pub fn rejected_argument(diagnostic: &str, arguments: &[String]) -> Option<String> {
    let quoted: Vec<&str> = diagnostic.split('\'').skip(1).step_by(2).collect();
    let joined = quoted
        .windows(2)
        .map(|pair| format!("{}{}", pair[1], pair[0]));
    quoted
        .iter()
        .map(|q| q.to_string())
        .chain(joined)
        .find(|candidate| candidate.starts_with('-') && arguments.contains(candidate))
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

    /// The arguments libclang gets, less the two every job ends with.
    fn arguments(arguments: &[&str], file: &str) -> Vec<String> {
        let mut job = Job::from_entry(&entry(arguments, file));
        assert_eq!(job.arguments.pop().unwrap(), "-working-directory=/proj");
        assert_eq!(job.arguments.pop().unwrap(), "-w");
        job.arguments
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
            vec!["-Wall", "-Isrc", "-w", "-working-directory=/proj"]
        );
    }

    #[test]
    fn keeps_defines_and_absolute_input_is_dropped() {
        assert_eq!(
            arguments(
                &["cc", "-DX=1", "-c", "/proj/src/a.c", "-o/proj/a.o"],
                "src/a.c"
            ),
            vec!["-DX=1"]
        );
    }

    #[test]
    fn an_input_spelled_through_dot_dot_is_still_the_input() {
        assert_eq!(
            arguments(&["cc", "-c", "src/../src/./a.c"], "src/a.c"),
            Vec::<String>::new()
        );
        // And the entry's own `file` may be the one with the `..` in it.
        assert_eq!(
            arguments(&["cc", "-c", "/src/a.c"], "../src/a.c"),
            Vec::<String>::new()
        );
        // Another file is not the input.
        assert_eq!(arguments(&["cc", "-c", "b.c", "a.c"], "a.c"), vec!["b.c"]);
    }

    #[test]
    fn the_kernels_preprocessor_dependency_flag_is_dropped() {
        assert_eq!(
            arguments(
                &["gcc", "-Wp,-MD,fs/.inode.o.d", "-nostdinc", "-c", "a.c"],
                "a.c"
            ),
            vec!["-nostdinc"]
        );
        assert_eq!(
            arguments(&["gcc", "-Wp,-MMD,x.d,-MT,x.o", "-c", "a.c"], "a.c"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_preprocessor_define_beside_a_dependency_flag_is_kept() {
        assert_eq!(
            arguments(
                &["gcc", "-Wp,-D_FORTIFY_SOURCE=2,-MD,x.d", "-c", "a.c"],
                "a.c"
            ),
            vec!["-Wp,-D_FORTIFY_SOURCE=2"]
        );
        assert_eq!(
            arguments(&["gcc", "-Wp,-D_FORTIFY_SOURCE=2", "-c", "a.c"], "a.c"),
            vec!["-Wp,-D_FORTIFY_SOURCE=2"]
        );
    }

    #[test]
    fn every_flag_that_writes_a_file_is_dropped() {
        assert_eq!(
            arguments(
                &[
                    "clang",
                    "-MJ",
                    "a.json",
                    "-MJb.json",
                    "--serialize-diagnostics",
                    "a.dia",
                    "-save-temps=obj",
                    "-save-stats",
                    "-ftime-trace",
                    "-ftime-trace=t.json",
                    "-O2",
                    "-c",
                    "a.c",
                ],
                "a.c"
            ),
            vec!["-O2"]
        );
    }

    #[test]
    fn a_cross_compilers_prefix_is_its_target() {
        assert_eq!(
            arguments(&["/opt/bin/arm-none-eabi-gcc", "-c", "a.c"], "a.c"),
            vec!["--target=arm-none-eabi"]
        );
        assert_eq!(
            arguments(&["aarch64-linux-gnu-gcc-12", "-c", "a.c"], "a.c"),
            vec!["--target=aarch64-linux-gnu"]
        );
        assert_eq!(
            arguments(&["riscv64-unknown-elf-clang", "-c", "a.c"], "a.c"),
            vec!["--target=riscv64-unknown-elf"]
        );
    }

    #[test]
    fn an_explicit_target_wins_over_the_prefix() {
        assert_eq!(
            arguments(
                &[
                    "aarch64-linux-gnu-gcc",
                    "--target=x86_64-linux-gnu",
                    "-c",
                    "a.c"
                ],
                "a.c"
            ),
            vec!["--target=x86_64-linux-gnu"]
        );
        assert_eq!(
            arguments(
                &[
                    "arm-none-eabi-clang",
                    "-target",
                    "thumbv7m-none-eabi",
                    "-c",
                    "a.c"
                ],
                "a.c"
            ),
            vec!["-target", "thumbv7m-none-eabi"]
        );
    }

    #[test]
    fn a_name_that_is_not_a_triple_implies_no_target() {
        // clang fails the whole TU on a triple it does not know.
        assert!(arguments(&["sh4-linux-gnu-gcc", "-c", "a.c"], "a.c").is_empty());
        assert!(arguments(&["my-wrapped-gcc", "-c", "a.c"], "a.c").is_empty());
        assert!(arguments(&["/usr/bin/cc", "-c", "a.c"], "a.c").is_empty());
        assert!(arguments(&["gcc-12", "-c", "a.c"], "a.c").is_empty());
    }

    #[test]
    fn a_cxx_driver_compiles_a_c_file_as_cxx() {
        assert_eq!(
            arguments(&["g++", "-c", "a.c"], "a.c"),
            vec!["--driver-mode=g++", "-x", "c++"]
        );
        assert_eq!(
            arguments(&["clang++-18", "-c", "a.cpp"], "a.cpp"),
            vec!["--driver-mode=g++"]
        );
        assert_eq!(
            arguments(&["aarch64-linux-gnu-c++", "-c", "a.c"], "a.c"),
            vec![
                "--target=aarch64-linux-gnu",
                "--driver-mode=g++",
                "-x",
                "c++"
            ]
        );
        // The build said which language; that stands.
        assert_eq!(
            arguments(&["g++", "-x", "c", "-c", "a.c"], "a.c"),
            vec!["--driver-mode=g++", "-x", "c"]
        );
        // A C driver changes nothing.
        assert!(arguments(&["gcc", "-c", "a.c"], "a.c").is_empty());
    }

    #[test]
    fn a_wrapper_is_looked_through_to_the_compiler() {
        assert_eq!(
            arguments(&["ccache", "g++", "-DX", "-c", "a.c"], "a.c"),
            vec!["--driver-mode=g++", "-x", "c++", "-DX"]
        );
        assert_eq!(
            arguments(
                &["/usr/bin/distcc", "arm-none-eabi-gcc", "-c", "a.c"],
                "a.c"
            ),
            vec!["--target=arm-none-eabi"]
        );
        // The compiler itself is never kept as an argument.
        assert_eq!(
            arguments(&["sccache", "cc", "-O2", "-c", "a.c"], "a.c"),
            vec!["-O2"]
        );
    }

    #[test]
    fn a_rejected_argument_is_read_off_the_diagnostic() {
        let args: Vec<String> = [
            "-fconserve-stack",
            "-mrecord-mcount",
            "-mharden-sls=all",
            "-O2",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            rejected_argument("unknown argument: '-fconserve-stack'", &args).as_deref(),
            Some("-fconserve-stack")
        );
        assert_eq!(
            rejected_argument(
                "unsupported option '-mrecord-mcount' for target 'x86_64-pc-linux-gnu'",
                &args
            )
            .as_deref(),
            Some("-mrecord-mcount")
        );
        assert_eq!(
            rejected_argument(
                "unsupported argument 'all' to option '-mharden-sls='",
                &args
            )
            .as_deref(),
            Some("-mharden-sls=all")
        );
        // A diagnostic that names nothing on the command line is not about it.
        assert_eq!(
            rejected_argument("unknown argument: '-fother'", &args),
            None
        );
        assert_eq!(rejected_argument("too many errors emitted", &args), None);
    }
}
