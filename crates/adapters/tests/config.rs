#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use disk_cleanup_adapters::config::{config_path, read_config, Config, ConfigError};
use disk_cleanup_core::Dirs;
use std::path::PathBuf;

#[test]
fn config_path_should_sit_under_the_config_dir_in_a_disk_cleanup_folder() {
    let d = Dirs {
        home: "/h".into(),
        state: "/s".into(),
        cache: "/c".into(),
        config: "/h/.config".into(),
    };
    assert_eq!(
        config_path(&d),
        PathBuf::from("/h/.config/disk-cleanup/config.toml")
    );
}

#[test]
fn read_config_should_return_roots_When_the_file_names_them_and_default_When_missing_or_keyless() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("config.toml");
    assert_eq!(read_config(&p).unwrap(), Config::default());
    std::fs::write(&p, "roots = [\"/h/work\", \"~/x\"]\nfuture_key = 3\n").unwrap();
    assert_eq!(
        read_config(&p).unwrap().roots,
        Some(vec![PathBuf::from("/h/work"), PathBuf::from("~/x")])
    );
    std::fs::write(&p, "other = 1\n").unwrap();
    assert_eq!(read_config(&p).unwrap().roots, None);
    std::fs::write(&p, "roots = []\n").unwrap();
    assert_eq!(read_config(&p).unwrap().roots, Some(vec![]));
}

#[test]
fn read_config_should_name_the_file_When_toml_is_invalid_or_roots_has_the_wrong_shape() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("config.toml");
    std::fs::write(&p, "roots = [").unwrap();
    assert!(matches!(read_config(&p), Err(ConfigError::Parse { .. })));
    for bad in ["roots = \"/h\"\n", "roots = [1]\n"] {
        std::fs::write(&p, bad).unwrap();
        let e = read_config(&p).unwrap_err();
        assert!(matches!(e, ConfigError::RootsShape { .. }));
        assert!(e.to_string().contains("config.toml"));
    }
    assert!(matches!(
        read_config(t.path()),
        Err(ConfigError::Read { .. })
    ));
}
