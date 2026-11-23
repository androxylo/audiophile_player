//! Offline TDD unit tests for resilient O(depth) random subfolder-walk FLAC picker.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ap_source::library::{
    SequentialFlacCursor, default_user_music_dir, discover_flac_files, pick_random_flac,
    pick_random_flac_walk, pick_random_flac_walk_with_seed, pick_random_flac_with_seed,
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
fn walk_picker_handles_empty_and_non_flac_subfolders_resiliently() {
    let root = make_temp_tree("walk_resilient");
    // Empty subfolders & non-FLAC decoy folders (must backtrack cleanly)
    fs::create_dir_all(root.join("EmptyA").join("NestedEmpty")).unwrap();
    fs::create_dir_all(root.join("ArtworkOnly")).unwrap();
    fs::write(root.join("ArtworkOnly").join("cover.jpg"), b"jpg").unwrap();
    fs::write(root.join("ArtworkOnly").join("readme.txt"), b"txt").unwrap();

    // Real nested FLAC albums
    let album1 = root.join("Artist1").join("Album1");
    let album2 = root.join("Artist2").join("Album2").join("Disc1");
    fs::create_dir_all(&album1).unwrap();
    fs::create_dir_all(&album2).unwrap();
    fs::write(album1.join("01.flac"), b"").unwrap();
    fs::write(album1.join("02.FLAC"), b"").unwrap();
    fs::write(album2.join("03.flac"), b"").unwrap();

    // Across 60 distinct seeds, every single pick must succeed and return a real FLAC
    let mut seen = HashSet::new();
    for seed in 0..60_u64 {
        let picked = pick_random_flac_walk_with_seed(&root, seed)
            .expect("walk picker must backtrack past empty/decoy folders and find a FLAC");
        assert!(
            picked
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("flac")),
            "picked non-flac path: {}",
            picked.display()
        );
        seen.insert(picked);
    }
    // Should reach both Artist1 and Artist2 across 60 seeds
    assert_eq!(seen.len(), 3, "expected all 3 tracks reachable, saw: {seen:?}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn walk_picker_reaches_both_root_flacs_and_subfolder_flacs_in_mixed_dir() {
    let root = make_temp_tree("walk_mixed");
    let sub = root.join("SubAlbum");
    fs::create_dir_all(&sub).unwrap();
    fs::write(root.join("loose.flac"), b"").unwrap();
    fs::write(sub.join("nested.flac"), b"").unwrap();

    let mut seen = HashSet::new();
    for seed in 0..40_u64 {
        seen.insert(pick_random_flac_walk_with_seed(&root, seed).unwrap());
    }
    assert!(seen.iter().any(|p| p.ends_with("loose.flac")));
    assert!(seen.iter().any(|p| p.ends_with("nested.flac")));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn walk_picker_returns_error_when_no_flac_exists_anywhere() {
    let root = make_temp_tree("walk_empty");
    fs::create_dir_all(root.join("Empty1")).unwrap();
    fs::create_dir_all(root.join("Empty2").join("Deep")).unwrap();
    fs::write(root.join("Empty2").join("notes.txt"), b"hello").unwrap();

    let err = pick_random_flac_walk_with_seed(&root, 7).unwrap_err();
    assert!(err.to_lowercase().contains("no .flac"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn walk_picker_wall_clock_helper_works_on_valid_tree() {
    let root = make_temp_tree("walk_clock");
    fs::create_dir_all(root.join("A")).unwrap();
    fs::write(root.join("A").join("song.flac"), b"").unwrap();

    let picked = pick_random_flac_walk(&root).unwrap();
    assert!(picked.ends_with("song.flac"));

    let _ = fs::remove_dir_all(&root);
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
fn sequential_cursor_yields_flacs_in_sorted_order_lazily_skipping_empty_dirs() {
    let root = make_temp_tree("seq_cursor");
    fs::create_dir_all(root.join("00_Empty")).unwrap();
    fs::create_dir_all(root.join("01_AlbumA")).unwrap();
    fs::create_dir_all(root.join("02_AlbumB")).unwrap();

    fs::write(root.join("00_root.flac"), b"").unwrap();
    fs::write(root.join("01_AlbumA").join("02.flac"), b"").unwrap();
    fs::write(root.join("01_AlbumA").join("01.flac"), b"").unwrap();
    fs::write(root.join("02_AlbumB").join("01.FLAC"), b"").unwrap();
    fs::write(root.join("02_AlbumB").join("cover.jpg"), b"").unwrap();

    let mut cur = SequentialFlacCursor::new(root.clone()).unwrap();
    let mut yielded = Vec::new();
    while let Some(p) = cur.next_track() {
        yielded.push(p.file_name().unwrap().to_string_lossy().to_string());
    }
    assert_eq!(yielded, vec!["00_root.flac", "01.flac", "02.flac", "01.FLAC"]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn default_user_music_dir_resolves_to_music_folder() {
    let dir = default_user_music_dir().expect("expected HOME/USERPROFILE to resolve");
    assert!(dir.ends_with("Music"));
}
