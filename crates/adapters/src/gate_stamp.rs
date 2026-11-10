//! The I/O half of the gate-stamp check (Story 2.2.1, Task 2.2.1d): the stamp file, the hash of the running
//! binary and the break-glass confirmation on the controlling terminal. The decision is the pure
//! `disk_cleanup_core::check_stamp`.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use disk_cleanup_core::{
    break_glass_reason, BreakGlass, GateStamp, BREAK_GLASS_WORD, STAMP_SCHEMA,
};
use sha2::{Digest, Sha256};

/// Where `make install-gated` puts the stamp: next to `audit.jsonl` in the state dir.
pub fn stamp_path(state_dir: &Path) -> PathBuf {
    state_dir.join("gate-stamp.json")
}

/// Lowercase hex sha256 of a file, streamed.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// Hash of the running executable.
pub fn current_exe_sha256() -> io::Result<String> {
    sha256_file(&std::env::current_exe()?)
}

/// The installed stamp. Absent or unparsable is `None` (a refusal for the caller); a stamp of another schema
/// is returned with only its `schema` set so the refusal can name it.
pub fn read_stamp(path: &Path) -> Option<GateStamp> {
    let bytes = std::fs::read(path).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let schema = u32::try_from(v.get("schema")?.as_u64()?).ok()?;
    if schema != STAMP_SCHEMA {
        return Some(GateStamp {
            schema,
            gate: String::new(),
            commit: String::new(),
            binary_sha256: String::new(),
        });
    }
    serde_json::from_value(v).ok()
}

/// Why the break-glass did not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakGlassRefusal {
    /// `DISK_CLEANUP_UNGATED` is unset or empty.
    NotRequested,
    /// `/dev/tty` cannot be opened: an agent session, cron, or `ssh` without `-t`.
    NoTerminal,
    /// The person typed another word, or the terminal ended.
    NotConfirmed,
}

/// The controlling terminal, read and written. Never stdin: an apply never reads stdin.
pub fn open_controlling_tty() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

/// Asks for [`BREAK_GLASS_WORD`] on `tty` and accepts only that line.
pub fn confirm_bypass(reason: &str, tty: &mut (impl BufRead + Write)) -> io::Result<bool> {
    write!(
        tty,
        "Gate check bypass requested ({reason}).\nThis run deletes without a gate stamp. Type {BREAK_GLASS_WORD} to continue: "
    )?;
    tty.flush()?;
    let mut line = String::new();
    let n = tty.read_line(&mut line)?;
    Ok(n > 0 && line.trim_end_matches(['\r', '\n']) == BREAK_GLASS_WORD)
}

/// The audited break-glass: needs the variable's reason and a person typing the word on the terminal that
/// `open_tty` opens. `raw_var` is the value of `DISK_CLEANUP_UNGATED`.
pub fn break_glass(
    raw_var: Option<&str>,
    open_tty: &dyn Fn() -> io::Result<File>,
) -> Result<BreakGlass, BreakGlassRefusal> {
    let reason = break_glass_reason(raw_var).ok_or(BreakGlassRefusal::NotRequested)?;
    let tty = open_tty().map_err(|_| BreakGlassRefusal::NoTerminal)?;
    let mut rw = ReadWrite {
        r: BufReader::new(&tty),
        w: &tty,
    };
    match confirm_bypass(&reason, &mut rw) {
        Ok(true) => Ok(BreakGlass {
            reason,
            tty_confirmed: true,
        }),
        _ => Err(BreakGlassRefusal::NotConfirmed),
    }
}

struct ReadWrite<R, W> {
    r: R,
    w: W,
}

impl<R: Read, W> Read for ReadWrite<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.r.read(buf)
    }
}

impl<R: BufRead, W> BufRead for ReadWrite<R, W> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.r.fill_buf()
    }
    fn consume(&mut self, amt: usize) {
        self.r.consume(amt);
    }
}

impl<R, W: Write> Write for ReadWrite<R, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.w.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}
