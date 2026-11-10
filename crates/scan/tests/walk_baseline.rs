#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Story 6.1.2 performance harness: median wall time of `WalkerBackend::collect` over a 2,000-directory fixture,
//! to compare before and after the streaming change (+5% bound). Run by hand:
//! `cargo test -p disk-cleanup-scan --release --test walk_baseline -- --ignored --nocapture`.

use disk_cleanup_scan::index::{Backend, CollectRequest};
use disk_cleanup_scan::walker::WalkerBackend;
use std::fs;
use std::time::Instant;

#[test]
#[ignore = "timing harness, run by hand"]
fn collect_median_over_2000_dirs() {
    let base = std::env::temp_dir().join(format!("dc-scan-baseline-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    for a in 0..20 {
        for b in 0..10 {
            for c in 0..10 {
                let d = base.join(format!("a{a}/b{b}/c{c}"));
                fs::create_dir_all(&d).unwrap();
                for f in 0..5 {
                    fs::write(d.join(format!("f{f}")), [0u8; 100]).unwrap();
                }
            }
        }
    }
    let roots = vec![base.clone()];
    let mut ms = Vec::new();
    for _ in 0..9 {
        let t = Instant::now();
        let got = WalkerBackend::default()
            .collect(&CollectRequest {
                roots: &roots,
                cross_fs: false,
                deadline: None,
                threads: 4,
            })
            .unwrap();
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        assert!(got.dirs.len() > 2000);
    }
    ms.sort_by(f64::total_cmp);
    println!(
        "dirs={} median_ms={:.2} min_ms={:.2} all={ms:.2?}",
        1 + 20 + 200 + 2000,
        ms[ms.len() / 2],
        ms[0]
    );
    let _ = fs::remove_dir_all(&base);
}
