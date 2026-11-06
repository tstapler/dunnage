#![allow(clippy::unwrap_used, clippy::expect_used)]

use disk_cleanup_core::{
    AbortFlag, AllocatedBytes, ApparentBytes, ApplyToken, DeleteError, DeleteFailure, Freed,
    Identity, RootDev,
};

#[test]
fn delete_error_and_delete_failure_should_be_clone_eq_and_not_copy_when_command_failed_carries_a_message(
) {
    let f = DeleteFailure {
        error: DeleteError::CommandFailed {
            status: 1,
            message: "m".into(),
        },
        freed: Freed::default(),
    };
    assert_eq!(f.clone(), f);
    let r: Result<Freed, DeleteFailure> = Err(f.clone());
    assert_eq!(std::hint::black_box(r).unwrap_err(), f);
}

#[test]
fn executor_facing_code_should_match_no_space_without_importing_adapters() {
    let e = DeleteError::NoSpace;
    assert!(matches!(e, DeleteError::NoSpace));
}

#[test]
fn identity_on_dev_should_compare_through_root_dev() {
    let id = Identity { dev: 28, ino: 5 };
    assert!(id.on_dev(RootDev(28)));
    assert!(!id.on_dev(RootDev(29)));
}

#[test]
fn identity_of_should_read_dev_and_ino_from_metadata() {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(".").unwrap();
    assert_eq!(
        Identity::of(&m),
        Identity {
            dev: m.dev(),
            ino: m.ino()
        }
    );
}

#[test]
fn byte_newtypes_should_default_to_zero_and_order() {
    assert_eq!(AllocatedBytes::default(), AllocatedBytes(0));
    assert!(ApparentBytes(1) < ApparentBytes(2));
    assert_eq!(Freed::default().inodes_freed, 0);
}

#[test]
fn abort_flag_should_be_set_once_and_shared_across_threads() {
    let a = AbortFlag::new();
    assert!(!a.is_set());
    std::thread::scope(|s| {
        s.spawn(|| a.set());
    });
    assert!(a.is_set());
}

#[test]
fn apply_token_for_tests_should_exist_under_test_support() {
    let _t: ApplyToken = ApplyToken::for_tests();
}

#[test]
fn build_flavor_should_name_test_support_in_test_builds() {
    assert_eq!(
        disk_cleanup_core::build_flavor(),
        "dc-flavor=test-support;9f3ac7e1"
    );
}
