//! `process_named` (Story 4.2.1): matches `/proc/<pid>/comm` exactly. Where `/proc` cannot be listed
//! (macOS, restricted mounts) nothing is known to be absent, so the answer is `Unknown`.

use std::fs;

use disk_cleanup_core::Tri;

/// The kernel truncates `comm` to 15 bytes, so longer names are compared on their first 15.
fn comm_matches(comm: &str, name: &str) -> bool {
    let want: String = name.chars().take(15).collect();
    comm.trim_end_matches('\n') == want
}

pub(super) fn process_named(name: &str) -> Tri {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Tri::Unknown;
    };
    let mut saw_any = false;
    for e in entries.flatten() {
        let fname = e.file_name();
        if !fname.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        saw_any = true;
        // A process that exits mid-scan has no comm; it cannot be the one we are looking for.
        if let Ok(comm) = fs::read_to_string(e.path().join("comm")) {
            if comm_matches(&comm, name) {
                return Tri::Running;
            }
        }
    }
    if saw_any {
        Tri::NotRunning
    } else {
        Tri::Unknown
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn comm_is_matched_exactly_and_truncated_like_the_kernel() {
        assert!(comm_matches("steam\n", "steam"));
        assert!(!comm_matches("steamwebhelper\n", "steam"));
        assert!(comm_matches("abcdefghijklmno\n", "abcdefghijklmnopqrs"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_this_process_and_not_a_made_up_one() {
        let exe = std::env::current_exe().unwrap();
        let me = exe.file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(process_named(&me), Tri::Running);
        assert_eq!(process_named("no-such-process-dc-xyz"), Tri::NotRunning);
    }
}
