#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Story 6.2.3 (Task 6.2.3d): `shell_quote` against real shells. Each hostile name is quoted, spliced into
//! three printed command templates and run with `sh -c` equivalents in bash, zsh and (where installed) dash
//! with `PATH` set to recording stubs; the path must arrive as ONE argument byte-equal to the name and no
//! injection stub may run.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use dunnage_core::shell_quote::sandbox::{find_on_path, run_printed_line};
use dunnage_core::shell_quote::shell_quote;

fn hostile() -> Vec<Vec<u8>> {
    let s = |t: &str| t.as_bytes().to_vec();
    vec![
        s("plain"),
        s("a b"),
        s("x /etc"),
        s("it's"),
        s("say \"hi\""),
        s("$(touch pwned)"),
        s("`touch pwned`"),
        s("a\nb"),
        s("-rf"),
        s("--help"),
        s("a\tb\x1b[2Jc"),
        s("bidi\u{202e}gpj"),
        s("c1\u{9b}31m"),
        s("osc\u{9d}0;t\x07"),
        s("zw\u{200b}x"),
        b"bad\xffbyte".to_vec(),
        s("a'; rm -rf ~; '"),
        s("a;b&c|d"),
        s("*?[x]{a,b}"),
        s("~root"),
        s("=ls"),
        s("#c"),
        s("back\\slash"),
        s("!!"),
        s("caf\u{e9}"),
        s("\u{65e5}\u{672c}"),
        s("nl\n/etc"),
        s("$HOME"),
        s("a  b"),
        s(" lead"),
        s("trail "),
        s("%1"),
        s("\\x27"),
        s("\u{1F389} party"),
        s("/abs/dir/with space/it's here"),
    ]
}

fn q(name: &[u8], ascii_only: bool) -> String {
    shell_quote(OsStr::from_bytes(name), ascii_only)
}

#[test]
fn the_rule_should_give_bare_empty_single_quoted_and_dollar_quoted_forms() {
    assert_eq!(q(b"", false), "''");
    assert_eq!(q(b"/a/b_c-d.e+f:g,h@i", false), "/a/b_c-d.e+f:g,h@i");
    assert_eq!(q(b"-rf", false), "'-rf'");
    assert_eq!(q(b"a b", false), "'a b'");
    assert_eq!(q(b"it's", false), "'it'\\''s'");
    assert_eq!(q("caf\u{e9}".as_bytes(), false), "'caf\u{e9}'");
    assert_eq!(q("caf\u{e9}".as_bytes(), true), "$'caf\\xc3\\xa9'");
    assert_eq!(q(b"a\nb", false), "$'a\\x0ab'");
    assert_eq!(q(b"back\\slash", false), "'back\\slash'");
    assert_eq!(q(b"bad\xff\\", false), "$'bad\\xff\\\\'");
    assert_eq!(q(b"a\x01!'", false), "$'a\\x01\\x21\\x27'");
    assert_eq!(q("x\u{202e}y".as_bytes(), false), "$'x\\xe2\\x80\\xaey'");
}

#[test]
fn the_sequence_backslash_quote_should_never_be_produced() {
    for name in hostile() {
        for ascii in [false, true] {
            let out = q(&name, ascii);
            assert!(!out.contains("\\'") || out.contains("'\\''"), "{out}");
            // Inside a `$'` form every quote is \x27; a `\'` pair can only be the `'\''` splice.
            if out.starts_with("$'") {
                assert!(!out[2..out.len() - 1].contains('\''), "{out}");
            }
        }
    }
}

#[test]
fn one_definition_of_shell_quote_should_exist_and_cli_should_re_export_it() {
    let mut defs = Vec::new();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n != "target") {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                for (i, l) in text.lines().enumerate() {
                    if l.contains("fn shell_quote(") && !l.contains("l.contains") {
                        defs.push(format!("{}:{}", p.display(), i + 1));
                    }
                }
            }
        }
    }
    assert_eq!(defs.len(), 1, "{defs:?}");
    assert!(defs[0].contains("core/src/shell_quote.rs"));
}

fn templates(quoted: &str) -> [(String, &'static str, Vec<&'static str>); 3] {
    [
        (format!("ls -ld -- {quoted}"), "ls", vec!["-ld", "--"]),
        (
            format!("dunnage explore {quoted}"),
            "dunnage",
            vec!["explore"],
        ),
        (
            format!("dunnage --kinds build-dirs --only {quoted}"),
            "dunnage",
            vec!["--kinds", "build-dirs", "--only"],
        ),
    ]
}

#[test]
fn a_quoted_hostile_name_should_reach_the_command_as_one_argument_and_inject_nothing() {
    let mut shells = vec!["bash", "zsh"];
    if find_on_path("dash").is_some() {
        shells.push("dash");
    }
    let mut runs = 0;
    let mut dash_inert = 0;
    for shell in shells {
        assert!(
            find_on_path(shell).is_some(),
            "{shell} must be installed here"
        );
        for name in hostile() {
            for ascii in [false, true] {
                let quoted = q(&name, ascii);
                if ascii {
                    assert!(quoted.is_ascii(), "{quoted:?}");
                }
                for (line, cmd, lead) in templates(&quoted) {
                    let dir = tempfile::tempdir().unwrap();
                    let run = run_printed_line(dir.path(), shell, &line).unwrap().unwrap();
                    if shell == "dash" && quoted.starts_with("$'") {
                        // dash has no `$'..'`: it reads `$` plus a single-quoted word, so the name cannot arrive
                        // intact (it cannot spell a control byte without one in the line). The line must still
                        // be inert.
                        assert!(
                            run.injected.is_empty(),
                            "{shell}: {line:?} ran {:?}",
                            run.injected
                        );
                        assert!(run.success, "{shell}: {line:?}");
                        dash_inert += 1;
                        continue;
                    }
                    let want: Vec<Vec<u8>> = lead
                        .iter()
                        .map(|a| a.as_bytes().to_vec())
                        .chain([name.clone()])
                        .collect();
                    assert_eq!(
                        run.argv_of(cmd),
                        Some(&want[..]),
                        "{shell}: {line:?} stderr={}",
                        run.stderr
                    );
                    assert!(
                        run.injected.is_empty(),
                        "{shell}: {line:?} ran {:?}",
                        run.injected
                    );
                    assert!(run.success, "{shell}: {line:?}");
                    runs += 1;
                }
            }
        }
    }
    eprintln!("dash: {dash_inert} lines in the $'..' form ran inert but not intact (dash has no $'..')");
    assert!(runs >= 2 * 35 * 2 * 3, "{runs}");
}

#[test]
fn the_sandbox_should_detect_an_injection_when_the_name_is_not_quoted() {
    let dir = tempfile::tempdir().unwrap();
    let run = run_printed_line(dir.path(), "bash", "ls -ld -- $(touch pwned)")
        .unwrap()
        .unwrap();
    assert_eq!(run.injected, ["touch"]);
    let dir = tempfile::tempdir().unwrap();
    let run = run_printed_line(dir.path(), "bash", "ls -ld -- x /etc")
        .unwrap()
        .unwrap();
    assert_eq!(run.argv_of("ls").unwrap().len(), 4);
    assert!(run_printed_line(dir.path(), "no-such-shell-xyz", "ls")
        .unwrap()
        .is_none());
}
