//! What is left of a shell script once the logic is Rust: running a program,
//! copying a tree, and a directory that removes itself.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A failure with something to say. Every task here ends either in success or
/// in one sentence a person reading a job log can act on, so the error type is
/// that sentence.
#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `Error(format!(...))`, which is nearly every construction here.
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => { $crate::util::Error(format!($($arg)*)) };
}

/// A program to run, its arguments, its environment and where it runs.
///
/// The environment is per child rather than per process, which is the one
/// thing this arrangement gets for free over the script it replaces: the web
/// build exported `RUSTUP_TOOLCHAIN=nightly` into its own process and every
/// descendant, so anything else the script had gone on to do would silently
/// have been a nightly build too.
pub struct Run {
    cmd: Command,
    shown: String,
}

impl Run {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let program = program.as_ref().to_owned();
        let shown = program.to_string_lossy().into_owned();
        Run {
            cmd: Command::new(program),
            shown,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        let arg = arg.as_ref().to_owned();
        let _ = write!(self.shown, " {}", arg.to_string_lossy());
        self.cmd.arg(arg);
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self = self.arg(arg);
        }
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.cmd.env(key, value);
        self
    }

    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.cmd.current_dir(dir);
        self
    }

    /// Run it, inheriting both streams, and fail on a non-zero status.
    pub fn run(mut self) -> Result<()> {
        let status = self
            .cmd
            .status()
            .map_err(|e| err!("could not run `{}`: {e}", self.shown))?;
        if status.success() {
            return Ok(());
        }
        Err(err!("`{}` failed with {status}", self.shown))
    }

    /// Run it and hand back the status without judging it, for the callers
    /// that read one — a `git push` losing its lease is an ordinary answer.
    pub fn status(mut self) -> Result<Option<i32>> {
        let status = self
            .cmd
            .status()
            .map_err(|e| err!("could not run `{}`: {e}", self.shown))?;
        Ok(status.code())
    }

    /// Run it quietly and hand back what it wrote.
    pub fn output(mut self) -> Result<Output> {
        self.cmd
            .stdin(Stdio::null())
            .output()
            .map_err(|e| err!("could not run `{}`: {e}", self.shown))
    }

    /// Run it, require success, and hand back its trimmed standard output.
    pub fn read(self) -> Result<String> {
        let shown = self.shown.clone();
        let out = self.output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(err!(
                "`{shown}` failed with {}: {}",
                out.status,
                stderr.trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// A temporary directory that is removed when it goes out of scope, which is
/// the `trap 'rm -rf "$work"' EXIT` the script had — and better, because it
/// also fires on the error paths that used to `exit 1` from the middle.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> Result<Self> {
        // Enough to not collide with a sibling job on the same runner, which
        // is all this needs: the directory is ours and nobody looks for it.
        let mut base = std::env::temp_dir();
        let unique = format!("{prefix}-{}-{:x}", std::process::id(), nonce());
        base.push(unique);
        fs::create_dir_all(&base).map_err(|e| err!("could not create {}: {e}", base.display()))?;
        Ok(TempDir(base))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A per-call number, from the address of a counter rather than from a clock:
/// `Instant::now` and `SystemTime::now` are both denied workspace-wide.
fn nonce() -> u64 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) as u64;
    // The stack address varies per run under ASLR, which is the entropy here.
    let here = &n as *const u64 as u64;
    here ^ (n << 32) ^ n
}

/// `cp -R <from>/. <to>/`: the *contents*, into a directory that exists.
pub fn copy_dir_contents(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).map_err(|e| err!("could not create {}: {e}", to.display()))?;
    for entry in fs::read_dir(from).map_err(|e| err!("could not read {}: {e}", from.display()))? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir_contents(&entry.path(), &target)?;
        } else {
            // A symlink is copied as what it points at, which is what `cp -R`
            // of a build output does and all a bundle ever contains.
            fs::copy(entry.path(), &target).map_err(|e| {
                err!(
                    "could not copy {} to {}: {e}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(())
}

/// `rm -rf`, on a path that may be a file, a directory, or absent.
pub fn remove(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err!("could not stat {}: {e}", path.display())),
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(path).map_err(|e| err!("could not remove {}: {e}", path.display()))
        }
        Ok(_) => {
            fs::remove_file(path).map_err(|e| err!("could not remove {}: {e}", path.display()))
        }
    }
}

/// The repository this binary was built from.
///
/// Baked in at compile time, so a task can be run from any directory and
/// still find `web/` — the shell version resolved `dirname $0` for the same
/// reason. The manifest lives one directory under the root.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the xtask manifest is a directory under the repository root")
        .to_path_buf()
}

/// A required environment variable, named in the error when it is missing —
/// the `: "${VAR:?}"` idiom, which is most of what the scripts used the shell
/// for.
pub fn need_env(key: &str) -> Result<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(err!("{key} is required and is not set")),
    }
}

/// An optional environment variable with a default, the `${VAR:-default}`
/// idiom. Empty is treated as unset, as it is there.
pub fn env_or(key: &str, fallback: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => fallback.to_string(),
    }
}

/// Append a line to one of the runner's collector files (`$GITHUB_OUTPUT`,
/// `$GITHUB_STEP_SUMMARY`). Silently does nothing off a runner, so every task
/// here can be run by hand.
pub fn append_github_file(var: &str, contents: &str) -> Result<()> {
    let Ok(path) = std::env::var(var) else {
        return Ok(());
    };
    if path.is_empty() {
        return Ok(());
    }
    use std::io::Write as _;
    let mut f = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| err!("could not open ${var} ({path}): {e}"))?;
    f.write_all(contents.as_bytes())
        .map_err(|e| err!("could not write ${var} ({path}): {e}"))?;
    if !contents.ends_with('\n') {
        f.write_all(b"\n")
            .map_err(|e| err!("could not write ${var} ({path}): {e}"))?;
    }
    Ok(())
}
