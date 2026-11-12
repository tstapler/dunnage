//! The one quoting function of every printed command line that holds a path (N-13, `design/ux.md` 3.6).
//!
//! Supported shells: the POSIX sh family and zsh (bash, zsh, dash, busybox ash, ksh). fish is not supported:
//! it rejects `$'` and reads `\\` and `\'` inside single quotes as escapes, so run a printed line under
//! `sh -c` there. Command lines print absolute paths only (`~` would not expand inside quotes).

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;

use crate::escape::needs_escape;

fn is_bare_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_/.+:,@".contains(&b)
}

/// One printed argument. The output is, in order:
///
/// 1. the bare text when it matches `^[A-Za-z0-9_/.+:,@][A-Za-z0-9_/.+:,@-]*$`;
/// 2. `''` for an empty name;
/// 3. single quotes with each `'` written `'\''` when the name is valid UTF-8, holds no character for which
///    [`needs_escape`] is true and, with `ascii_only`, no non-ASCII character;
/// 4. otherwise `$'...'`, in which `\` is `\\`, printable ASCII other than `'` and `!` is itself and every
///    other byte (each byte of a multi-byte character, the quote as `\x27`, the bang as `\x21`, invalid
///    UTF-8) is `\xNN`.
///
/// The sequence `\'` is never produced: `dash` reads `$'` as `$` plus a plain single-quoted word, and `\'`
/// would end that quote early. `ascii_only` is true whenever stdout is not a TTY.
pub fn shell_quote(path: &OsStr, ascii_only: bool) -> String {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return "''".to_string();
    }
    if is_bare_byte(bytes[0]) && bytes[1..].iter().all(|&b| is_bare_byte(b) || b == b'-') {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        let plain = !text.chars().any(needs_escape) && (!ascii_only || text.is_ascii());
        if plain {
            return format!("'{}'", text.replace('\'', "'\\''"));
        }
    }
    let mut out = String::from("$'");
    for &b in bytes {
        if b == b'\\' {
            out.push_str("\\\\");
        } else if (0x20..0x7f).contains(&b) && b != b'\'' && b != b'!' {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    out.push('\'');
    out
}

/// Runs a printed command line in a real shell against recording stubs, so a test can prove that a quoted
/// path reaches the command as ONE argument and nothing else runs.
#[cfg(feature = "test-support")]
pub mod sandbox {
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Commands whose stub appends its name to `INJECTED`: none of them may ever run.
    const TRAPS: [&str; 8] = ["sudo", "chown", "rm", "touch", "cat", "echo", "curl", "sh"];
    /// Commands whose stub records its argv, NUL-separated, to `argv.<name>`.
    const RECORDERS: [&str; 2] = ["ls", "dunnage"];

    /// What one run of a printed line did.
    #[derive(Debug)]
    pub struct PrintedRun {
        pub success: bool,
        /// The argv of each recorder, by command name, when it ran.
        pub argv: Vec<(String, Vec<Vec<u8>>)>,
        /// The names of the trap commands that ran, one per run.
        pub injected: Vec<String>,
        pub stderr: String,
    }

    impl PrintedRun {
        pub fn argv_of(&self, command: &str) -> Option<&[Vec<u8>]> {
            self.argv
                .iter()
                .find(|(c, _)| c == command)
                .map(|(_, a)| a.as_slice())
        }
    }

    /// The first executable called `name` on the current `PATH`.
    pub fn find_on_path(name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    }

    fn write_stub(dir: &Path, name: &str, body: &str) -> io::Result<()> {
        let bash = find_on_path("bash").ok_or_else(|| io::Error::other("bash not found"))?;
        let path = dir.join("bin").join(name);
        fs::write(&path, format!("#!{}\n{body}", bash.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
    }

    /// Writes the stub directory into the empty `dir` (the caller's scratch directory) and runs
    /// `<shell> -c <line>` with `PATH` set to the stubs alone. `shell` is a name found on the current `PATH`
    /// (`bash`, `zsh`, `dash`); `None` when it is not installed.
    pub fn run_printed_line(dir: &Path, shell: &str, line: &str) -> io::Result<Option<PrintedRun>> {
        let Some(shell_path) = find_on_path(shell) else {
            return Ok(None);
        };
        fs::create_dir_all(dir.join("bin"))?;
        for name in RECORDERS {
            let out = dir.join(format!("argv.{name}"));
            write_stub(
                dir,
                name,
                &format!(
                    ": > '{out}'\nfor a in \"$@\"; do printf '%s\\0' \"$a\" >> '{out}'; done\n",
                    out = out.display()
                ),
            )?;
        }
        for name in TRAPS {
            write_stub(
                dir,
                name,
                &format!(
                    "printf '%s\\n' '{name}' >> '{}'\n",
                    dir.join("INJECTED").display()
                ),
            )?;
        }
        let output = Command::new(&shell_path)
            .arg("-c")
            .arg(line)
            .env_clear()
            .env("PATH", dir.join("bin"))
            .current_dir(dir)
            .output()?;
        let mut argv = Vec::new();
        for name in RECORDERS {
            if let Ok(raw) = fs::read(dir.join(format!("argv.{name}"))) {
                let mut args: Vec<Vec<u8>> = raw.split(|&b| b == 0).map(<[u8]>::to_vec).collect();
                args.pop();
                argv.push((name.to_string(), args));
            }
        }
        let injected = fs::read_to_string(dir.join("INJECTED"))
            .map(|t| t.lines().map(str::to_string).collect())
            .unwrap_or_default();
        Ok(Some(PrintedRun {
            success: output.status.success(),
            argv,
            injected,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }))
    }
}
