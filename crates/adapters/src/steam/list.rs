//! `steam list`, the read-only installed-game ranking (Story 4.2.3), ported from `steam-clean list`.
//!
//! Nothing here writes: it reads `libraryfolders.vdf`, every library's `appmanifest_*.acf` and the
//! largest `userdata/*/config/localconfig.vdf`. Unlike the cleaning gate in `library.rs` it is lenient:
//! an unmounted library only drops that library's games from the listing (a note says so).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use steamlocate::SteamDir;

const GIB: f64 = 1_073_741_824.0;
/// Steam's value of `LastPlayed` for a game never launched.
const NEVER_PLAYED_MAX: u64 = 86_400;
const NEVER_PLAYED_DAYS: u64 = 3_650;
const SMALL_TOOL_BYTES: u64 = 200_000_000;
const MIN_HOURS: f64 = 0.5;
pub const RETIRED_TIDY_LINE: &str =
    "To remove a game, use Steam; 'steam-clean tidy' has been retired.";
pub const READ_ONLY_LINE: &str = "read-only: nothing is deleted by this command";

/// Whole words (split on non-alphanumerics) that mark a name as a tool, not a game. Matching words
/// rather than substrings keeps `Ghostrunner` visible (the old `ost` substring rule hid it).
const TOOL_WORDS: [&str; 8] = [
    "runtime",
    "redistributable",
    "redistributables",
    "soundtrack",
    "ost",
    "sdk",
    "steamworks",
    "proton",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Game {
    pub appid: u64,
    pub name: String,
    pub size_bytes: u64,
    pub install_dir: String,
    pub library: PathBuf,
    /// Epoch seconds; 0 or Steam's "never" sentinel when never played.
    pub last_played: u64,
    pub playtime_min: u64,
}

impl Game {
    pub fn size_gib(&self) -> f64 {
        self.size_bytes as f64 / GIB
    }

    pub fn hours(&self) -> f64 {
        self.playtime_min as f64 / 60.0
    }

    pub fn never_played(&self) -> bool {
        self.last_played <= NEVER_PLAYED_MAX
    }

    pub fn days_idle(&self, now: u64) -> u64 {
        if self.never_played() {
            return NEVER_PLAYED_DAYS;
        }
        now.saturating_sub(self.last_played) / 86_400
    }

    /// `size_GiB * days_idle / max(hours, 0.5)`: big, long idle, barely played ranks first.
    pub fn score(&self, now: u64) -> f64 {
        self.size_gib() * self.days_idle(now) as f64 / self.hours().max(MIN_HOURS)
    }

    fn is_tool(&self) -> bool {
        if self.size_bytes < SMALL_TOOL_BYTES && self.last_played == 0 {
            return true;
        }
        let lower = self.name.to_lowercase();
        let words: Vec<&str> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        words.iter().any(|w| TOOL_WORDS.contains(w))
            || words.windows(2).any(|p| p == ["dedicated", "server"])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Score,
    Size,
    Date,
    Playtime,
    Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListError {
    /// No Steam directory under HOME.
    NotInstalled,
    /// `libraryfolders.vdf` missing, unparseable or listing no library.
    Unrecognised(String),
    /// Every listed library is unmounted or unreadable.
    NoReadableLibrary(Vec<String>),
}

impl ListError {
    pub fn text(&self) -> String {
        match self {
            Self::NotInstalled => "Steam not found; no library to list".into(),
            Self::Unrecognised(why) => format!("unrecognised steam output; nothing listed ({why})"),
            Self::NoReadableLibrary(notes) => {
                format!("no Steam library is readable; {}", notes.join("; "))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub games: Vec<Game>,
    /// One line per library or file that was left out.
    pub notes: Vec<String>,
}

/// Reads the games of the Steam install at `root`, tools filtered out. Order is by library then app id.
pub fn load(root: &Path) -> Result<Listing, ListError> {
    if !root.is_dir() {
        return Err(ListError::NotInstalled);
    }
    let dir = SteamDir::from_dir(root).map_err(|e| ListError::Unrecognised(e.to_string()))?;
    let libraries = dir
        .library_paths()
        .map_err(|e| ListError::Unrecognised(e.to_string()))?;
    if libraries.is_empty() {
        return Err(ListError::Unrecognised("no libraries listed".into()));
    }
    let mut notes = Vec::new();
    let play = load_play_records(root, &mut notes);
    let mut games = Vec::new();
    let mut readable = 0;
    for lib in &libraries {
        let apps = lib.join("steamapps");
        let Ok(entries) = fs::read_dir(&apps) else {
            notes.push(format!(
                "Steam library {} not mounted; its games are not listed",
                lib.display()
            ));
            continue;
        };
        readable += 1;
        let mut manifests: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("appmanifest_")
                        && Path::new(n).extension().is_some_and(|e| e == "acf")
                })
            })
            .collect();
        manifests.sort();
        for m in manifests {
            match parse_manifest(&m, lib, &play) {
                Some(g) if !g.is_tool() => games.push(g),
                Some(_) => {}
                None => notes.push(format!("{} unrecognised; skipped", m.display())),
            }
        }
    }
    if readable == 0 {
        return Err(ListError::NoReadableLibrary(notes));
    }
    Ok(Listing { games, notes })
}

pub fn sort_games(games: &mut [Game], key: SortKey, now: u64) {
    match key {
        SortKey::Score => games.sort_by(|a, b| {
            b.score(now)
                .total_cmp(&a.score(now))
                .then_with(|| a.name.cmp(&b.name))
        }),
        SortKey::Size => games.sort_by(|a, b| {
            b.size_bytes
                .cmp(&a.size_bytes)
                .then_with(|| a.name.cmp(&b.name))
        }),
        SortKey::Date => games.sort_by(|a, b| {
            a.last_played
                .cmp(&b.last_played)
                .then_with(|| a.name.cmp(&b.name))
        }),
        SortKey::Playtime => games.sort_by(|a, b| {
            b.playtime_min
                .cmp(&a.playtime_min)
                .then_with(|| a.name.cmp(&b.name))
        }),
        SortKey::Name => games.sort_by(|a, b| a.name.cmp(&b.name)),
    }
}

fn last_played_text(g: &Game, now: u64) -> String {
    if g.never_played() {
        "never".into()
    } else {
        format!("{}d", g.days_idle(now))
    }
}

/// The S6 table: header, one row per game, a total, then the notes and the retirement line.
pub fn render(games: &[Game], notes: &[String], now: u64) -> String {
    let mut out = format!("{READ_ONLY_LINE}\n");
    for n in notes {
        let _ = writeln!(out, "{n}");
    }
    if games.is_empty() {
        out.push_str("no installed games\n");
    } else {
        let rows: Vec<[String; 5]> = games
            .iter()
            .map(|g| {
                [
                    format!("{:.1}", g.score(now)),
                    format!("{:.1} GiB", g.size_gib()),
                    last_played_text(g, now),
                    format!("{:.1}", g.hours()),
                    g.name.clone(),
                ]
            })
            .collect();
        let head = ["SCORE", "SIZE", "LAST PLAYED", "HOURS", "GAME"];
        let mut w = [0usize; 4];
        for (i, wi) in w.iter_mut().enumerate() {
            *wi = rows
                .iter()
                .map(|r| r[i].chars().count())
                .chain([head[i].len()])
                .max()
                .unwrap_or(0);
        }
        let line = |cells: [&str; 5]| {
            format!(
                "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}\n",
                cells[0],
                cells[1],
                cells[2],
                cells[3],
                cells[4],
                w0 = w[0],
                w1 = w[1],
                w2 = w[2],
                w3 = w[3],
            )
        };
        out.push_str(&line(head));
        for r in &rows {
            out.push_str(&line([&r[0], &r[1], &r[2], &r[3], &r[4]]));
        }
        let total: f64 = games.iter().map(Game::size_gib).sum();
        let _ = writeln!(out, "{} games, {total:.1} GiB", games.len());
    }
    let _ = writeln!(out, "{RETIRED_TIDY_LINE}");
    out
}

/// One JSON document (`schema_version` 1) for `--json`.
pub fn to_json(games: &[Game], notes: &[String], now: u64, exit_code: i32) -> String {
    let items: Vec<serde_json::Value> = games
        .iter()
        .map(|g| {
            serde_json::json!({
                "appid": g.appid,
                "name": g.name,
                "size_bytes": g.size_bytes,
                "last_played": if g.never_played() { None } else { Some(g.last_played) },
                "playtime_minutes": g.playtime_min,
                "score": g.score(now),
                "library": g.library.to_string_lossy(),
            })
        })
        .collect();
    let doc = serde_json::json!({
        "schema_version": 1,
        "mode": "steam-list",
        "read_only": true,
        "games": items,
        "notes": notes,
        "errors": [],
        "exit_code": exit_code,
    });
    let mut s = serde_json::to_string_pretty(&doc).unwrap_or_default();
    s.push('\n');
    s
}

#[derive(Debug, Clone, Copy)]
struct PlayRecord {
    last_played: u64,
    playtime_min: u64,
}

fn load_play_records(root: &Path, notes: &mut Vec<String>) -> HashMap<u64, PlayRecord> {
    let Ok(users) = fs::read_dir(root.join("userdata")) else {
        return HashMap::new();
    };
    let best = users
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path().join("config/localconfig.vdf"))
        .filter_map(|p| fs::metadata(&p).ok().map(|m| (m.len(), p)))
        .max();
    let Some((_, path)) = best else {
        return HashMap::new();
    };
    let parsed = fs::read_to_string(&path)
        .ok()
        .and_then(|s| parse_vdf(&s))
        .and_then(|v| {
            v.path(&["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"])
                .cloned()
        });
    let Some(Vdf::Map(apps)) = parsed else {
        notes.push(format!(
            "{} unrecognised; play history not used",
            path.display()
        ));
        return HashMap::new();
    };
    apps.iter()
        .filter_map(|(k, v)| {
            let appid = k.parse::<u64>().ok()?;
            Some((
                appid,
                PlayRecord {
                    last_played: v.get("LastPlayed").and_then(Vdf::as_u64).unwrap_or(0),
                    playtime_min: v.get("Playtime").and_then(Vdf::as_u64).unwrap_or(0),
                },
            ))
        })
        .collect()
}

fn parse_manifest(path: &Path, library: &Path, play: &HashMap<u64, PlayRecord>) -> Option<Game> {
    let vdf = parse_vdf(&fs::read_to_string(path).ok()?)?;
    let s = vdf.get("AppState")?;
    let appid = s.get("appid").and_then(Vdf::as_u64)?;
    let own_last = s.get("LastPlayed").and_then(Vdf::as_u64).unwrap_or(0);
    let (last_played, playtime_min) = match play.get(&appid) {
        Some(r) => (r.last_played.max(own_last), r.playtime_min),
        None => (own_last, 0),
    };
    Some(Game {
        appid,
        name: s.get("name").and_then(Vdf::as_str).unwrap_or("?").into(),
        size_bytes: s.get("SizeOnDisk").and_then(Vdf::as_u64).unwrap_or(0),
        install_dir: s
            .get("installdir")
            .and_then(Vdf::as_str)
            .unwrap_or("")
            .into(),
        library: library.to_path_buf(),
        last_played,
        playtime_min,
    })
}

#[derive(Debug, Clone, PartialEq)]
enum Vdf {
    Str(String),
    Map(Vec<(String, Vdf)>),
}

impl Vdf {
    /// Steam writes keys in mixed case across files, so lookups ignore ASCII case.
    fn get(&self, key: &str) -> Option<&Vdf> {
        match self {
            Vdf::Map(m) => m
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Vdf::Str(_) => None,
        }
    }

    fn path(&self, keys: &[&str]) -> Option<&Vdf> {
        keys.iter().try_fold(self, |v, k| v.get(k))
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Vdf::Str(s) => Some(s),
            Vdf::Map(_) => None,
        }
    }

    fn as_u64(&self) -> Option<u64> {
        self.as_str()?.parse().ok()
    }
}

/// Parses `"key" "value"` / `"key" { ... }` text; `None` on any syntax error or truncation.
fn parse_vdf(s: &str) -> Option<Vdf> {
    let mut chars = s.chars().peekable();
    let root = parse_map(&mut chars, true)?;
    Some(root)
}

fn parse_map(it: &mut std::iter::Peekable<std::str::Chars<'_>>, top: bool) -> Option<Vdf> {
    let mut entries = Vec::new();
    loop {
        skip_ws_and_comments(it);
        match it.peek() {
            None => return top.then_some(Vdf::Map(entries)),
            Some('}') => {
                it.next();
                return (!top).then_some(Vdf::Map(entries));
            }
            Some('"') => {}
            Some(_) => return None,
        }
        let key = read_string(it)?;
        skip_ws_and_comments(it);
        match it.peek()? {
            '{' => {
                it.next();
                entries.push((key, parse_map(it, false)?));
            }
            '"' => entries.push((key, Vdf::Str(read_string(it)?))),
            _ => return None,
        }
    }
}

fn skip_ws_and_comments(it: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    loop {
        match it.peek() {
            Some(c) if c.is_whitespace() => {
                it.next();
            }
            Some('/') => {
                for c in it.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            _ => return,
        }
    }
}

fn read_string(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    if it.next()? != '"' {
        return None;
    }
    let mut out = String::new();
    loop {
        match it.next()? {
            '"' => return Some(out),
            '\\' => match it.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::format_push_string)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = 86_400;

    fn game(name: &str, gib: f64, played_days_ago: Option<u64>, hours: f64) -> Game {
        Game {
            appid: 1,
            name: name.into(),
            size_bytes: (gib * GIB) as u64,
            install_dir: name.into(),
            library: PathBuf::from("/lib"),
            last_played: played_days_ago.map_or(0, |d| NOW - d * DAY),
            playtime_min: (hours * 60.0) as u64,
        }
    }

    /// A Steam root whose `libraryfolders.vdf` lists `libs`; each library is created unless named in `missing`.
    struct Fixture {
        dir: TempDir,
    }

    impl Fixture {
        fn new(libs: &[&str], missing: &[&str]) -> Fixture {
            let dir = TempDir::new().unwrap();
            let root = dir.path().join("steam");
            fs::create_dir_all(root.join("steamapps")).unwrap();
            let mut vdf = String::from("\"libraryfolders\"\n{\n");
            for (i, l) in libs.iter().enumerate() {
                let p = dir.path().join(l);
                if !missing.contains(l) {
                    fs::create_dir_all(p.join("steamapps")).unwrap();
                }
                vdf.push_str(&format!(
                    "\t\"{i}\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n",
                    p.display()
                ));
            }
            vdf.push_str("}\n");
            fs::write(root.join("steamapps/libraryfolders.vdf"), vdf).unwrap();
            Fixture { dir }
        }

        fn root(&self) -> PathBuf {
            self.dir.path().join("steam")
        }

        fn manifest(&self, lib: &str, appid: u64, name: &str, bytes: u64, last_played: u64) {
            let body = format!(
                "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"{name}\"\n\t\"installdir\"\t\t\"{name}\"\n\t\"SizeOnDisk\"\t\t\"{bytes}\"\n\t\"LastPlayed\"\t\t\"{last_played}\"\n}}\n"
            );
            let f = self
                .dir
                .path()
                .join(lib)
                .join(format!("steamapps/appmanifest_{appid}.acf"));
            fs::write(f, body).unwrap();
        }

        fn localconfig(&self, appid: u64, last_played: u64, playtime: u64) {
            let d = self.root().join("userdata/1234/config");
            fs::create_dir_all(&d).unwrap();
            let body = format!(
                "\"UserLocalConfigStore\"\n{{\n\t\"Software\"\n\t{{\n\t\t\"Valve\"\n\t\t{{\n\t\t\t\"Steam\"\n\t\t\t{{\n\t\t\t\t\"apps\"\n\t\t\t\t{{\n\t\t\t\t\t\"{appid}\"\n\t\t\t\t\t{{\n\t\t\t\t\t\t\"LastPlayed\"\t\t\"{last_played}\"\n\t\t\t\t\t\t\"Playtime\"\t\t\"{playtime}\"\n\t\t\t\t\t}}\n\t\t\t\t}}\n\t\t\t}}\n\t\t}}\n\t}}\n}}\n"
            );
            fs::write(d.join("localconfig.vdf"), body).unwrap();
        }
    }

    #[test]
    fn score_should_be_size_times_idle_days_over_hours() {
        let g = game("Ghostrunner", 12.0, Some(200), 3.0);
        assert!((g.score(NOW) - 800.0).abs() < 1e-9, "{}", g.score(NOW));
    }

    #[test]
    fn score_should_floor_playtime_at_half_an_hour_and_treat_never_played_as_ten_years() {
        let g = game("Idle", 1.0, None, 0.0);
        assert!((g.score(NOW) - 3_650.0 / 0.5).abs() < 1e-9);
    }

    #[test]
    fn tool_filter_should_hide_proton_but_not_ghostrunner() {
        assert!(game("Proton 9.0", 1.0, None, 0.0).is_tool());
        assert!(game("Steam Linux Runtime 3.0 (sniper)", 0.5, Some(3), 0.0).is_tool());
        assert!(game("Half-Life 2: OST", 1.0, Some(3), 0.0).is_tool());
        assert!(game("Foo Dedicated Server", 2.0, Some(3), 0.0).is_tool());
        assert!(game("Tiny Unlaunched", 0.01, None, 0.0).is_tool());
        assert!(!game("Ghostrunner", 12.0, Some(200), 3.0).is_tool());
        assert!(!game("Frostpunk", 4.0, Some(2), 9.0).is_tool());
    }

    #[test]
    fn list_should_rank_ghostrunner_at_800_and_hide_proton_from_fixture_libraries() {
        let f = Fixture::new(&["lib0"], &[]);
        let gib = 1_073_741_824u64;
        f.manifest("lib0", 1, "Ghostrunner", 12 * gib, NOW - 200 * DAY);
        f.manifest("lib0", 2, "Proton 9.0", gib, 0);
        f.localconfig(1, NOW - 200 * DAY, 180);
        let l = load(&f.root()).unwrap();
        assert!(l.notes.is_empty(), "{:?}", l.notes);
        assert_eq!(l.games.len(), 1);
        assert_eq!(l.games[0].name, "Ghostrunner");
        assert!((l.games[0].score(NOW) - 800.0).abs() < 1e-9);
        let text = render(&l.games, &l.notes, NOW);
        assert!(text.starts_with(READ_ONLY_LINE), "{text}");
        assert!(text.contains("800.0"), "{text}");
        assert!(text.contains("12.0 GiB"), "{text}");
        assert!(text.contains("200d"), "{text}");
        assert!(text.contains("3.0"), "{text}");
        assert!(text.contains(RETIRED_TIDY_LINE), "{text}");
        assert!(!text.contains("Proton"), "{text}");
    }

    #[test]
    fn unmounted_library_should_be_noted_and_the_other_libraries_still_listed() {
        let f = Fixture::new(&["lib0", "lib1"], &["lib1"]);
        f.manifest("lib0", 1, "Ghostrunner", 5_000_000_000, NOW - DAY);
        let l = load(&f.root()).unwrap();
        assert_eq!(l.games.len(), 1);
        assert_eq!(l.notes.len(), 1, "{:?}", l.notes);
        assert!(l.notes[0].starts_with("Steam library "), "{:?}", l.notes);
        assert!(
            l.notes[0].ends_with(" not mounted; its games are not listed"),
            "{:?}",
            l.notes
        );
    }

    #[test]
    fn no_readable_library_should_be_an_error() {
        let f = Fixture::new(&["lib0"], &["lib0"]);
        let e = load(&f.root()).unwrap_err();
        assert!(matches!(e, ListError::NoReadableLibrary(_)), "{e:?}");
    }

    #[test]
    fn missing_steam_root_and_garbled_libraryfolders_should_be_errors_not_empty_lists() {
        let d = TempDir::new().unwrap();
        assert_eq!(load(&d.path().join("none")), Err(ListError::NotInstalled));
        let root = d.path().join("steam");
        fs::create_dir_all(root.join("steamapps")).unwrap();
        fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            "\"libraryfolders\" {",
        )
        .unwrap();
        assert!(matches!(load(&root), Err(ListError::Unrecognised(_))));
    }

    #[test]
    fn a_broken_manifest_should_be_skipped_with_a_note() {
        let f = Fixture::new(&["lib0"], &[]);
        f.manifest("lib0", 1, "Ghostrunner", 5_000_000_000, NOW - DAY);
        fs::write(
            f.dir.path().join("lib0/steamapps/appmanifest_9.acf"),
            "\"AppState\" { \"appid\"",
        )
        .unwrap();
        let l = load(&f.root()).unwrap();
        assert_eq!(l.games.len(), 1);
        assert!(l.notes[0].contains("appmanifest_9.acf unrecognised"));
    }

    #[test]
    fn unmounted_library_should_make_the_cleaning_gate_skip_the_whole_kind_for_prefixes_and_remnants(
    ) {
        use crate::steam::{SteamGate, SteamInstall};
        use dunnage_core::SkipReason;
        // Stories 4.2.1 (Skip) and 4.2.2 (Unavailable) disagree; the shared loader returns Skip, so a
        // remnant kind built on it is Skip(LibraryUnmounted) too. Recorded in phase5-findings.md.
        let f = Fixture::new(&["lib0", "lib1"], &["lib1"]);
        f.manifest("lib0", 1, "Ghostrunner", 5_000_000_000, NOW - DAY);
        assert_eq!(
            SteamInstall::load(&f.root()),
            Err(SteamGate::Skip(SkipReason::LibraryUnmounted))
        );
    }

    #[test]
    fn sort_should_order_by_each_key_with_name_as_the_tiebreak() {
        let mut v = vec![
            game("B", 1.0, Some(10), 1.0),
            game("A", 9.0, Some(10), 5.0),
            game("C", 1.0, Some(300), 0.1),
        ];
        sort_games(&mut v, SortKey::Size, NOW);
        assert_eq!(v[0].name, "A");
        sort_games(&mut v, SortKey::Date, NOW);
        assert_eq!(v[0].name, "C");
        sort_games(&mut v, SortKey::Playtime, NOW);
        assert_eq!(v[0].name, "A");
        sort_games(&mut v, SortKey::Name, NOW);
        assert_eq!(v[2].name, "C");
        sort_games(&mut v, SortKey::Score, NOW);
        assert_eq!(v[0].name, "C");
    }

    #[test]
    fn vdf_should_parse_nested_maps_escapes_and_reject_truncation() {
        let v = parse_vdf("\"a\" { \"B\" \"x\\\\y\" } // c\n").unwrap();
        assert_eq!(v.path(&["a", "b"]).and_then(Vdf::as_str), Some("x\\y"));
        assert!(parse_vdf("\"a\" { \"b\" ").is_none());
        assert!(parse_vdf("\"a\" }").is_none());
    }
}
