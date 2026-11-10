#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The cache table per OS (Story 1.3.2; validation T-120, T-124, T-306).

use disk_cleanup_core::roots::CacheTool;
use disk_cleanup_core::{
    cache_table, cache_table_with_availability, CacheRow, CacheSource, Platform, CACHE_ROWS,
};

fn homes(rows: &[CacheRow]) -> Vec<&'static str> {
    rows.iter()
        .filter_map(|r| match r.source {
            CacheSource::Home(p) => Some(p),
            _ => None,
        })
        .collect()
}

fn labels(rows: &[CacheRow]) -> Vec<&'static str> {
    rows.iter().map(|r| r.label).collect()
}

#[test]
fn cache_table_should_list_linux_set_and_macos_set_When_platform_given() {
    let linux = cache_table(Platform::Linux);
    let h = homes(&linux);
    for want in [
        ".npm/_cacache",
        ".gradle/caches",
        ".cargo/registry/cache",
        ".cargo/registry/src",
        ".cache/ms-playwright",
    ] {
        assert!(h.contains(&want), "linux lacks {want}");
    }
    let apt = linux.iter().find(|r| r.is_apt()).expect("apt on linux");
    assert!(apt.needs_root, "apt is NeedsRoot");
    for banned in ["Homebrew", "Xcode", "JetBrains"] {
        assert!(
            !labels(&linux).iter().any(|l| l.contains(banned)),
            "linux shows {banned}"
        );
    }

    let mac = cache_table(Platform::Macos);
    let l = labels(&mac);
    for want in ["Homebrew", "Xcode", "JetBrains"] {
        assert!(l.iter().any(|x| x.contains(want)), "macos lacks {want}");
    }
    assert!(!mac.iter().any(CacheRow::is_apt), "macos shows apt");
    assert!(
        !homes(&mac).contains(&".cache/ms-playwright"),
        "macos shows the Linux playwright path"
    );
}

#[test]
fn maven_should_not_be_a_candidate_on_linux_or_macos_not_even_snapshot_dirs_When_default_config() {
    for p in [Platform::Linux, Platform::Macos] {
        for r in cache_table(p) {
            let text = format!("{} {:?}", r.label, r.source).to_lowercase();
            assert!(
                !text.contains("m2") && !text.contains("maven") && !text.contains("snapshot"),
                "{p:?}: {text}"
            );
        }
    }
    assert!(CACHE_ROWS
        .iter()
        .all(|r| !format!("{:?}", r.source).contains(".m2")));
}

#[test]
fn linux_only_and_macos_only_kinds_should_be_skipped_on_the_other_os_with_a_stated_reason_When_kinds_listed(
) {
    let on_mac = cache_table_with_availability(Platform::Macos);
    let apt = on_mac.iter().find(|a| a.row.is_apt()).unwrap();
    assert_eq!(apt.unsupported.as_deref(), Some("not supported on macOS"));
    let on_linux = cache_table_with_availability(Platform::Linux);
    let brew = on_linux
        .iter()
        .find(|a| a.row.label.contains("Homebrew"))
        .unwrap();
    assert_eq!(brew.unsupported.as_deref(), Some("not supported on Linux"));
    // every row of every platform is listed, none silently omitted
    assert_eq!(on_mac.len(), CACHE_ROWS.len());
    assert_eq!(on_linux.len(), CACHE_ROWS.len());
    // a supported row carries no reason
    let npm = on_mac
        .iter()
        .find(|a| a.row.label == "npm _cacache")
        .unwrap();
    assert_eq!(npm.unsupported, None);
}

#[test]
fn go_and_pip_should_be_tool_owned_rows_on_both_platforms() {
    for p in [Platform::Linux, Platform::Macos] {
        let tools: Vec<_> = cache_table(p)
            .into_iter()
            .filter_map(|r| match r.source {
                CacheSource::Tool(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tools, vec![CacheTool::Go, CacheTool::Pip]);
    }
}
