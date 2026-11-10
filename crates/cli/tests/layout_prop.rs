#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Task 1.2.3c: the row layout of `design/ux.md` 3.6, property test over widths 20 to 200 (a port of the
//! self-test of `implementation/prototypes/layout_rows.py`).

use disk_cleanup_cli::layout::{hard_wrap, word_wrap, RowCells, Table, PLAN_COLS};

/// xorshift64: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[(self.next() % v.len() as u64) as usize]
    }
}

fn random_path(r: &mut Rng) -> String {
    let alpha: Vec<char> = "abcdefghij/-_.~\\ABC123".chars().collect();
    let len = *r.pick(&[1, 12, 60, 100, 300]);
    let mut p: String = (0..len)
        .map(|_| *r.pick(&alpha))
        .collect::<String>()
        .replace(' ', "_");
    p = p.replace('\\', "\\\\");
    if r.next() % 10 < 3 {
        p.push_str("\\xff\\u{1f600}\\n");
    }
    p
}

fn random_reason(r: &mut Rng) -> String {
    let n = *r.pick(&[0, 1, 4, 12]);
    (0..n)
        .map(|_| {
            let w = *r.pick(&[1, 3, 7, 25, 90]);
            (0..w)
                .map(|_| *r.pick(&['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn cells(kw: usize, path: &str, reason: &str) -> RowCells {
    RowCells {
        status: "RISK".into(),
        kind: "k".repeat(kw),
        cells: vec!["destructive".into(), "123.4 GiB".into(), "365d".into()],
        path: path.into(),
        reason: reason.into(),
    }
}

#[test]
fn no_row_line_should_exceed_the_width_and_nothing_should_be_truncated_for_widths_20_to_200() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let mut rows = 0;
    for w in (20..=200).chain([250]) {
        for _ in 0..60 {
            let kw = *r.pick(&[5, 8, 10, 14, 18]);
            let (path, reason) = (random_path(&mut r), random_reason(&mut r));
            let table = Table::new(w, kw, &PLAN_COLS, 1);
            let lines = table.row(&cells(kw, &path, &reason));
            assert!(
                lines.iter().all(|l| l.chars().count() <= w),
                "w={w}: {lines:?}"
            );
            if lines.len() == 1 {
                assert!(
                    lines[0].contains(&path) && (reason.is_empty() || lines[0].contains(&reason))
                );
            } else {
                let head = if w >= 40 { 1 } else { 2 };
                let body: Vec<&str> = lines[head..].iter().map(|l| &l[2..]).collect();
                let path_lines = hard_wrap(&path, w - 2).len();
                let tail = body[body.len() - path_lines..].concat();
                assert_eq!(
                    tail, path,
                    "the path lines concatenate back to the escaped path"
                );
                let words: Vec<&str> = body[..body.len() - path_lines]
                    .iter()
                    .flat_map(|l| l.split(' '))
                    .collect();
                let want: Vec<&str> = reason.split_whitespace().collect();
                if reason.split_whitespace().all(|x| x.len() <= w - 2) {
                    assert_eq!(
                        words
                            .iter()
                            .filter(|x| !x.is_empty())
                            .copied()
                            .collect::<Vec<_>>(),
                        want
                    );
                }
            }
            rows += 1;
        }
    }
    assert!(rows > 10_000);
}

#[test]
fn an_escape_sequence_should_never_be_split_across_lines() {
    for w in 10..=30 {
        let s = "\\xff".repeat(20) + "\\u{1f600}" + "\\\\" + "\\n" + &"a".repeat(30);
        let lines = hard_wrap(&s, w);
        assert_eq!(lines.concat(), s);
        for l in &lines {
            assert!(l.chars().count() <= w);
            assert!(!l.ends_with('\\') || l.ends_with("\\\\"), "{l}");
            assert!(!l.ends_with("\\x") && !l.ends_with("\\u"), "{l}");
        }
    }
}

#[test]
fn a_long_path_at_80_columns_should_become_a_block_inside_80_columns() {
    let table = Table::new(80, 10, &PLAN_COLS, 1);
    let path = format!("/h/{}", "x".repeat(150));
    let lines = table.row(&cells(10, &path, "(not selected)"));
    assert!(
        lines.len() >= 3 && lines.iter().all(|l| l.chars().count() <= 80),
        "{lines:?}"
    );
}

#[test]
fn word_wrap_should_split_a_word_longer_than_the_width() {
    let lines = word_wrap(&format!("short {} tail", "w".repeat(45)), 20);
    assert!(lines.iter().all(|l| l.chars().count() <= 20), "{lines:?}");
    assert_eq!(
        lines.join("").replace(' ', ""),
        format!("short{}tail", "w".repeat(45))
    );
}
