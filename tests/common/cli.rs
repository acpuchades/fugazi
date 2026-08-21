//! Driving the `fugazi` binary and reading back what it wrote.
//!
//! Every end-to-end CLI test shells out to the same binary, writes into a
//! scratch `--output-dir`, and reads the artefacts back. Doing that by hand
//! grew five near-copies of the same `Command::new(...).args([...])` block
//! across `run.rs` and `costs.rs`, three of which used `.status()` — so a
//! non-zero exit failed with `"exited with failure"` and **no stderr**, which
//! is exactly the moment you need it. [`Cmd`] always captures output and puts
//! stderr in the panic message.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A unique temp path per call. Never a fixed name in the shared `/tmp`: a
/// fixed name collides with a parallel run or another user's leftovers — an
/// unreadable, unremovable dir that surfaces as `PermissionDenied`. Any
/// extension is preserved so `--series @path` still sees a `.csv`.
pub fn unique_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let token = format!(
        "{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    let p = Path::new(name);
    let unique = match (
        p.file_stem().and_then(|s| s.to_str()),
        p.extension().and_then(|e| e.to_str()),
    ) {
        (Some(stem), Some(ext)) => format!("{stem}_{token}.{ext}"),
        _ => format!("{name}_{token}"),
    };
    std::env::temp_dir().join(unique)
}

/// An absolute path to a file in the repository, e.g. `repo("examples/candles.csv")`.
pub fn repo(relative: &str) -> String {
    format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative)
}

/// The `@`-prefixed spelling the CLI reads as "load this file" (as opposed to
/// an inline document).
pub fn at(relative: &str) -> String {
    format!("@{}", repo(relative))
}

/// Write `contents` to a fresh scratch file and return the `@path` spelling.
pub fn scratch_file(name: &str, contents: &str) -> (PathBuf, String) {
    let path = unique_path(name);
    std::fs::write(&path, contents).expect("write scratch file");
    let arg = format!("@{}", path.to_str().expect("utf-8 scratch path"));
    (path, arg)
}

/// One invocation of the `fugazi` binary, built up fluently.
///
/// ```ignore
/// let out = Cmd::new("run")
///     .arg(&at("examples/strategy.yml"))
///     .series(&at("examples/candles.csv"))
///     .args(&["--windowed", "10"])
///     .output_dir("windowed")
///     .ok();
/// assert_eq!(out.read("metrics.csv").lines().count(), 4);
/// ```
pub struct Cmd {
    args: Vec<String>,
    out_dir: Option<PathBuf>,
}

impl Cmd {
    /// Start an invocation of `subcommand` (`run` / `optimize` / `check` / …).
    pub fn new(subcommand: &str) -> Self {
        Self {
            args: vec![subcommand.to_string()],
            out_dir: None,
        }
    }

    pub fn arg(mut self, arg: &str) -> Self {
        self.args.push(arg.to_string());
        self
    }

    pub fn args(mut self, args: &[&str]) -> Self {
        self.args.extend(args.iter().map(|a| a.to_string()));
        self
    }

    /// Append one `--series <value>`; call repeatedly to stack series.
    pub fn series(self, value: &str) -> Self {
        self.args(&["--series", value])
    }

    /// Append one `--costs <term>`; call repeatedly, later terms winning.
    pub fn costs(self, term: &str) -> Self {
        self.args(&["--costs", term])
    }

    /// Direct output into a fresh scratch dir derived from `name`.
    pub fn output_dir(mut self, name: &str) -> Self {
        let dir = unique_path(name);
        let _ = std::fs::remove_dir_all(&dir);
        self.args.push("--output-dir".to_string());
        self.args
            .push(dir.to_str().expect("utf-8 scratch path").to_string());
        self.out_dir = Some(dir);
        self
    }

    /// Run it, and hand back stdout/stderr/status without judging them — for
    /// the tests that assert on a *failure*.
    pub fn run(self) -> Outcome {
        let output = Command::new(env!("CARGO_BIN_EXE_fugazi"))
            .args(&self.args)
            .output()
            .expect("failed to launch the fugazi binary");
        Outcome {
            args: self.args,
            dir: self.out_dir,
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Run it and assert it exited zero.
    #[track_caller]
    pub fn ok(self) -> Outcome {
        let out = self.run();
        assert!(
            out.status.success(),
            "`fugazi {}` exited {:?}\n--- stderr ---\n{}\n--- stdout ---\n{}",
            out.args.join(" "),
            out.status.code(),
            out.stderr,
            out.stdout
        );
        out
    }

    /// Run it and assert it exited non-zero, handing back the outcome so the
    /// caller can pin the diagnostic.
    #[track_caller]
    pub fn fails(self) -> Outcome {
        let out = self.run();
        assert!(
            !out.status.success(),
            "`fugazi {}` unexpectedly succeeded\n--- stdout ---\n{}",
            out.args.join(" "),
            out.stdout
        );
        out
    }
}

/// What one invocation produced.
pub struct Outcome {
    args: Vec<String>,
    dir: Option<PathBuf>,
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    /// The scratch `--output-dir`, if one was set.
    #[track_caller]
    pub fn dir(&self) -> &Path {
        self.dir.as_deref().expect("no --output-dir was configured")
    }

    /// Read an artefact the run wrote, panicking with the invocation if absent.
    #[track_caller]
    pub fn read(&self, file: &str) -> String {
        let path = self.dir().join(file);
        std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "`fugazi {}` did not write {}: {e}\n(dir holds: {:?})",
                self.args.join(" "),
                path.display(),
                std::fs::read_dir(self.dir())
                    .map(|d| d
                        .filter_map(|e| e.ok().map(|e| e.file_name()))
                        .collect::<Vec<_>>())
                    .unwrap_or_default()
            )
        })
    }

    /// Whether the run wrote `file` at all.
    pub fn wrote(&self, file: &str) -> bool {
        self.dir().join(file).exists()
    }

    /// Data rows of a CSV artefact — the header line dropped.
    pub fn rows(&self, file: &str) -> Vec<String> {
        self.read(file)
            .lines()
            .skip(1)
            .map(str::to_string)
            .collect()
    }

    /// The header line of a CSV artefact.
    #[track_caller]
    pub fn header(&self, file: &str) -> String {
        let text = self.read(file);
        text.lines().next().expect("empty CSV").to_string()
    }
}

/// The four artefacts every `fugazi run` writes.
pub struct Artefacts {
    pub fills: String,
    pub trades: String,
    pub returns: String,
    pub metrics: String,
}

impl Outcome {
    pub fn artefacts(&self) -> Artefacts {
        Artefacts {
            fills: self.read("fills.csv"),
            trades: self.read("trades.csv"),
            returns: self.read("returns.csv"),
            metrics: self.read("metrics.yml"),
        }
    }
}

/// The metrics YAML should be a top-level mapping carrying every section the
/// metrics document defines — enough to catch a missing section or a rename
/// without hard-coding numbers that move with the fixture.
#[track_caller]
pub fn assert_metrics_shape(metrics: &str) {
    for section in ["run:", "returns:", "risk_adjusted:", "drawdown:", "trades:"] {
        assert!(
            metrics.contains(section),
            "metrics.yml missing `{section}` section:\n{metrics}"
        );
    }
}
