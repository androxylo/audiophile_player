//! Offline TDD unit tests for recursive FLAC discovery and random track selection.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ap_source::library::{
    default_user_music_dir, discover_flac_files, pick_random_flac, pick_random_flac_with_seed,
};

fn make_temp_tree(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ap_library_test_{tag}_{}_{}",
        std::process::id(),
        stamp
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn discovers_nested_flac_files_case_insensitively_and_ignores_other_types() {
    let root = make_temp_tree("nested");
    let sub1 = root.join("AlbumA");
    let sub2 = root.join("AlbumB").join("Disc2");
    fs::create_dir_all(&sub1).unwrap();
    fs::create_dir_all(&sub2).unwrap();

    fs::write(root.join("top.flac"), b"").unwrap();
    fs::write(sub1.join("track1.FLAC"), b"").unwrap();
    fs::write(sub1.join("notes.txt"), b"ignore").unwrap();
    fs::write(sub2.join("track2.Flac"), b"").unwrap();
    fs::write(sub2.join("cover.jpg"), b"ignore").unwrap();

    let found = discover_flac_files(&root).expect("discovery failed");
    assert_eq!(found.len(), 3);
    assert!(found.iter().any(|p| p.ends_with("top.flac")));
    assert!(found.iter().any(|p| p.ends_with("track1.FLAC")));
    assert!(found.iter().any(|p| p.ends_with("track2.Flac")));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn discover_errors_on_missing_directory() {
    let missing = std::env::temp_dir().join("ap_definitely_does_not_exist_999999");
    assert!(discover_flac_files(&missing).is_err());
}

#[test]
fn random_picker_handles_empty_and_selects_valid_entries() {
    let empty: Vec<PathBuf> = Vec::new();
    assert!(pick_random_flac(&empty).is_none());
    assert!(pick_random_flac_with_seed(&empty, 42).is_none());

    let files: Vec<PathBuf> = (0..10)
        .map(|i| PathBuf::from(format!("track_{i}.flac")))
        .collect();
    let a = pick_random_flac_with_seed(&files, 12345).unwrap();
    let b = pick_random_flac_with_seed(&files, 12345).unwrap();
    assert_eq!(a, b);
    assert!(files.contains(a));

    let c = pick_random_flac(&files).unwrap();
    assert!(files.contains(c));
}

#[test]
fn default_user_music_dir_resolves_to_music_folder() {
    let dir = default_user_music_dir().expect("expected HOME/USERPROFILE to resolve");
    assert!(dir.ends_with("Music"));
}
