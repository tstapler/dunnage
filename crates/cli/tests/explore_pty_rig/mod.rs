#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! A real pty around the real `dunnage explore` binary, shared by the Gate F journeys (Story 6.5.6):
//! `explore_pty.rs`, `explore_delete_pty.rs` and `explore_fulldisk.rs`. The binary runs under the fixture
//! environment of `Fixture::command_for`, in a shell that saves `stty -a` before it and after it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub const ENTER_ALT: &[u8] = b"\x1b[?1049h";
pub const LEAVE_ALT: &[u8] = b"\x1b[?1049l";
pub const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
pub const RESET: &[u8] = b"\x1b[0m";
pub const WAIT: Duration = Duration::from_secs(30);

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

pub fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

pub fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

pub struct Pty {
    pub session: expectrl::Session,
    pub seen: Vec<u8>,
    /// `before`, `after` and `pid` live here, outside the fixture HOME.
    pub dir: PathBuf,
    pub shell_pid: u32,
}

/// What the shell around the binary does. `Plain` is `sh -c`; `JobControl` is an interactive `bash` that runs the
/// command as one job, so Ctrl-Z stops it and `fg` continues it.
pub enum Shell {
    Plain,
    JobControl,
}

pub struct Spawn<'a> {
    pub fixture: &'a super::common::Fixture,
    /// Arguments after `explore`, as one shell-quoted string already.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    pub shell: Shell,
}

impl<'a> Spawn<'a> {
    pub fn new(fixture: &'a super::common::Fixture, args: &[&Path]) -> Spawn<'a> {
        Spawn {
            fixture,
            args: args
                .iter()
                .map(|a| sh_quote(&a.to_string_lossy()))
                .collect(),
            env: Vec::new(),
            rows: 30,
            cols: 100,
            shell: Shell::Plain,
        }
    }

    pub fn flag(mut self, flag: &str) -> Spawn<'a> {
        self.args.insert(0, sh_quote(flag));
        self
    }

    pub fn env(mut self, k: &str, v: &str) -> Spawn<'a> {
        self.env.push((k.to_string(), v.to_string()));
        self
    }

    pub fn job_control(mut self) -> Spawn<'a> {
        self.shell = Shell::JobControl;
        self
    }

    pub fn run(self, bin: &Path) -> Pty {
        let dir = self.fixture.root().join("pty");
        fs::create_dir_all(&dir).unwrap();
        let explore = format!("'{}' explore {}", bin.display(), self.args.join(" "));
        let body = format!(
            "stty -a > '{d}/before'; {explore}; echo EXIT=$?; stty -a > '{d}/after'",
            d = dir.display()
        );
        let mut cmd = match self.shell {
            Shell::Plain => {
                let mut c = self.fixture.command_for("sh");
                c.arg("-c").arg(format!(
                    "echo $$ > '{d}/pid'; stty rows {r} cols {c}; {body}",
                    d = dir.display(),
                    r = self.rows,
                    c = self.cols
                ));
                c
            }
            Shell::JobControl => {
                let mut c = self.fixture.command_for("bash");
                c.args(["--norc", "--noprofile", "--noediting", "-i"]);
                c.env("PS1", "PROMPT> ");
                c.env("HISTFILE", "/dev/null");
                c
            }
        };
        cmd.env("TERM", "xterm");
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let mut session = expectrl::Session::spawn(cmd).unwrap();
        session.set_expect_timeout(Some(WAIT));
        let shell_pid = session.get_process().pid().as_raw() as u32;
        let mut p = Pty {
            session,
            seen: Vec::new(),
            dir,
            shell_pid,
        };
        if matches!(self.shell, Shell::JobControl) {
            p.session
                .get_process_mut()
                .set_window_size(self.cols, self.rows)
                .unwrap();
            p.wait_for("PROMPT> ");
            // The binary is a plain command here (a job bash can stop and continue); the end of the session is
            // typed by `after_job` once the job is done.
            p.send(
                format!(
                    "echo $$ > '{d}/pid'; stty -a > '{d}/before'\n",
                    d = p.dir.display()
                )
                .as_bytes(),
            );
            p.wait_for_after("PROMPT> ", 1);
            p.send(format!("'{}' explore {}\n", bin.display(), self.args.join(" ")).as_bytes());
        }
        p
    }
}

pub fn sh_quote(s: &str) -> String {
    if s.starts_with("--") && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Pty {
    /// `explore <roots>` in a plain `sh -c` over `cmd`, a `sh` command whose environment the caller built. `dir`
    /// holds `before`, `after` and `pid`.
    pub fn spawn_with(
        cmd: Command,
        bin: &Path,
        roots: &[&Path],
        dir: &Path,
        rows: u16,
        cols: u16,
    ) -> Pty {
        let args: Vec<String> = roots
            .iter()
            .map(|r| sh_quote(&r.to_string_lossy()))
            .collect();
        let body = format!("'{}' explore {}", bin.display(), args.join(" "));
        Pty::spawn_script(cmd, &body, dir, rows, cols)
    }

    /// Runs the shell command `body` (it ends with the status that `EXIT=` reports) between the saved
    /// `stty -a` of `before` and `after`.
    pub fn spawn_script(mut cmd: Command, body: &str, dir: &Path, rows: u16, cols: u16) -> Pty {
        fs::create_dir_all(dir).unwrap();
        cmd.arg("-c").arg(format!(
            "echo $$ > '{d}/pid'; stty rows {rows} cols {cols}; stty -a > '{d}/before'; {body}; echo EXIT=$?; stty -a > '{d}/after'",
            d = dir.display(),
        ));
        cmd.env("TERM", "xterm");
        let mut session = expectrl::Session::spawn(cmd).unwrap();
        session.set_expect_timeout(Some(WAIT));
        let shell_pid = session.get_process().pid().as_raw() as u32;
        Pty {
            session,
            seen: Vec::new(),
            dir: dir.to_path_buf(),
            shell_pid,
        }
    }

    pub fn pump(&mut self) {
        let mut buf = [0u8; 16384];
        while let Ok(n) = self.session.try_read(&mut buf) {
            if n == 0 {
                break;
            }
            self.seen.extend_from_slice(&buf[..n]);
        }
    }

    /// The transcript as a person reads the screen: a cursor move is a space, other escapes vanish.
    pub fn screen_text(&self) -> String {
        screen_text_of(&self.seen)
    }

    /// Waits until `needle` is on the screen text after the first `from` bytes of the transcript.
    pub fn wait_for_after(&mut self, needle: &str, from: usize) -> usize {
        let end = Instant::now() + WAIT;
        loop {
            self.pump();
            let text = screen_text_of(&self.seen[from.min(self.seen.len())..]);
            if let Some(at) = text.find(needle).or_else(|| fuzzy_find(&text, needle)) {
                return at;
            }
            if Instant::now() > end {
                let text = self.screen_text();
                panic!(
                    "never saw {needle:?} after byte {from}; end of the screen text: {:?}",
                    &text[text.len().saturating_sub(2500)..]
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn wait_for(&mut self, needle: &str) -> usize {
        self.wait_for_after(needle, 0)
    }

    /// Waits for raw bytes in the transcript after `from`.
    pub fn wait_bytes(&mut self, needle: &[u8], from: usize) {
        let end = Instant::now() + WAIT;
        loop {
            self.pump();
            if find(&self.seen[from.min(self.seen.len())..], needle).is_some() {
                return;
            }
            assert!(
                Instant::now() < end,
                "never saw {needle:?} after byte {from}; end of transcript: {:?}",
                self.tail()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn tail(&self) -> String {
        let t = &self.seen;
        String::from_utf8_lossy(&t[t.len().saturating_sub(600)..]).into_owned()
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.session.send(bytes).unwrap();
    }

    /// Sends the keys one after another with a pause, so the loop reads each as its own key press.
    pub fn keys(&mut self, keys: &[&[u8]]) {
        for k in keys {
            self.send(k);
            std::thread::sleep(Duration::from_millis(120));
            self.pump();
        }
    }

    pub fn exit_code(&mut self) -> i32 {
        let end = Instant::now() + WAIT;
        loop {
            self.pump();
            let text = self.screen_text();
            if let Some(at) = text.find("EXIT=") {
                let digits: String = text[at + 5..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if !digits.is_empty() {
                    return digits.parse().unwrap();
                }
            }
            assert!(
                Instant::now() < end,
                "no EXIT= line; end of transcript: {:?}",
                self.tail()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Job-control shell only: once the job has ended and bash shows its prompt again (after `from` bytes of the
    /// transcript), types the exit marker and the closing `stty -a`.
    pub fn after_job(&mut self, from: usize) {
        self.wait_for_after("PROMPT> ", from);
        self.send(format!("echo EXIT=$?; stty -a > '{}/after'\n", self.dir.display()).as_bytes());
    }

    /// Reads until the shell has run the trailing `stty -a`.
    pub fn finish(&mut self) {
        let end = Instant::now() + WAIT;
        while fs::read_to_string(self.dir.join("after")).map_or(true, |t| t.is_empty())
            && Instant::now() < end
        {
            self.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));
        self.pump();
    }

    /// The pid of the `dunnage` process the shell started.
    pub fn binary_pid(&self) -> u32 {
        let end = Instant::now() + WAIT;
        loop {
            if let Some(p) = descendant_named(self.shell_pid_for_children(), "dunnage") {
                return p;
            }
            assert!(Instant::now() < end, "no dunnage child of the shell");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn shell_pid_for_children(&self) -> u32 {
        // The job-control shell runs the command in a subshell; its pid is what `echo $$` wrote first.
        let written = fs::read_to_string(self.dir.join("pid"))
            .ok()
            .and_then(|t| t.trim().parse().ok());
        written.unwrap_or(self.shell_pid)
    }

    pub fn kill(&self, sig: &str) {
        let pid = self.binary_pid();
        let st = Command::new("kill")
            .arg(format!("-{sig}"))
            .arg(pid.to_string())
            .status()
            .unwrap();
        assert!(st.success(), "kill -{sig} {pid}");
    }

    /// The assertions every ending shares: the alternate screen was entered and left, the cursor was shown,
    /// `stty -a` equals the state saved before, and past the last restore only the guard's own reset, the cursor
    /// show and the closing text follow (no raw ESC).
    pub fn assert_restored(&self) {
        let t = &self.seen;
        assert!(
            find(t, ENTER_ALT).is_some(),
            "no alternate screen: {}",
            self.tail()
        );
        let leave = rfind(t, LEAVE_ALT).expect("the alternate screen was left");
        assert!(
            rfind(t, SHOW_CURSOR).is_some(),
            "the cursor was never shown again"
        );
        let before = fs::read_to_string(self.dir.join("before")).unwrap();
        let after = fs::read_to_string(self.dir.join("after")).unwrap();
        assert!(!before.is_empty());
        assert_eq!(
            without_size(&before),
            without_size(&after),
            "stty -a differs after the session"
        );
        let mut tail = t[leave + LEAVE_ALT.len()..].to_vec();
        strip(&mut tail, RESET);
        strip(&mut tail, SHOW_CURSOR);
        assert!(
            !tail.contains(&0x1b),
            "raw ESC after the restore: {:?}",
            String::from_utf8_lossy(&tail)
        );
    }

    /// The text outside the alternate screen (before the first entry and after the last leave), with the guard's
    /// reset and cursor show removed.
    pub fn outside_alt(&self) -> String {
        let t = &self.seen;
        let enter = find(t, ENTER_ALT).unwrap_or(t.len());
        let leave = rfind(t, LEAVE_ALT).map_or(t.len(), |i| i + LEAVE_ALT.len());
        let mut out = t[..enter].to_vec();
        if leave <= t.len() && leave >= enter {
            out.extend_from_slice(&t[leave..]);
        }
        strip(&mut out, RESET);
        strip(&mut out, SHOW_CURSOR);
        String::from_utf8_lossy(&out).replace('\r', "")
    }
}

/// `bytes` as a person reads the screen: a cursor move is a space, other escapes vanish.
pub fn screen_text_of(bytes: &[u8]) -> String {
    let raw = String::from_utf8_lossy(bytes).into_owned();
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            let mut last = ' ';
            for n in chars.by_ref() {
                last = n;
                if n.is_ascii_alphabetic() || n == '~' {
                    break;
                }
            }
            if last == 'H' {
                out.push(' ');
            }
        }
    }
    out
}

/// `stty -a` without the window size, which a resize journey changes on purpose.
fn without_size(t: &str) -> String {
    t.lines()
        .map(|l| {
            if l.starts_with("speed ") {
                l.split(';')
                    .filter(|f| {
                        // GNU: "rows 30; columns 100"; BSD/macOS: "30 rows; 100 columns"
                        let f = f.trim();
                        !(f.starts_with("rows ")
                            || f.starts_with("columns ")
                            || f.ends_with(" rows")
                            || f.ends_with(" columns"))
                    })
                    .collect::<Vec<_>>()
                    .join(";")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The painter redraws only the cells that changed, so a phrase can reach the wire with letters missing where
/// the old frame already had them. This finds `needle` as an in-order subsequence over a span of at most
/// twice its length (letters only; used for waiting, never for an assertion about content).
pub fn fuzzy_find(hay: &str, needle: &str) -> Option<usize> {
    let want: Vec<char> = needle.chars().filter(|c| !c.is_whitespace()).collect();
    let chars: Vec<(usize, char)> = hay.char_indices().collect();
    if want.len() < 8 {
        return None;
    }
    for (i, (at, c)) in chars.iter().enumerate() {
        if *c != want[0] && *c != want[1] {
            continue;
        }
        // One needle letter may be missing wherever the old frame already showed it.
        let (mut k, mut missed) = (
            usize::from(*c == want[0]) + 2 * usize::from(*c != want[0]),
            0,
        );
        for (_, h) in chars[i + 1..].iter().take(want.len() * 3) {
            if k < want.len() && *h == want[k] {
                k += 1;
            } else if k + 1 < want.len() && *h == want[k + 1] && missed < want.len() / 6 {
                k += 2;
                missed += 1;
            }
        }
        if k >= want.len() {
            return Some(*at);
        }
    }
    None
}

fn strip(bytes: &mut Vec<u8>, pat: &[u8]) {
    while let Some(i) = find(bytes, pat) {
        bytes.drain(i..i + pat.len());
    }
}

/// A descendant of `ancestor` (child, grandchild, ...) whose command name starts with `name`. Read with
/// `ps -A -o pid=,ppid=,comm=` (procps and BSD alike; there is no /proc on macOS).
fn descendant_named(ancestor: u32, name: &str) -> Option<u32> {
    let mut parent_of = std::collections::HashMap::new();
    let mut named = Vec::new();
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
        .ok()?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut f = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (
            f.next().and_then(|s| s.parse::<u32>().ok()),
            f.next().and_then(|s| s.parse::<u32>().ok()),
        ) else {
            continue;
        };
        let comm = f.collect::<Vec<_>>().join(" ");
        parent_of.insert(pid, ppid);
        // macOS prints the full path of the executable; Linux the (truncated) command name
        let base = Path::new(&comm)
            .file_name()
            .map_or_else(|| comm.clone(), |b| b.to_string_lossy().into_owned());
        if base.starts_with(name) {
            named.push(pid);
        }
    }
    named.into_iter().find(|pid| {
        let mut cur = *pid;
        for _ in 0..16 {
            match parent_of.get(&cur) {
                Some(p) if *p == ancestor => return true,
                Some(p) if *p > 1 => cur = *p,
                _ => return false,
            }
        }
        false
    })
}
