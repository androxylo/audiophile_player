//! Offline TDD unit tests for resilient O(depth) random subfolder-walk FLAC picker.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ap_source::library::{
    SequentialFlacCursor, default_user_music_dir, discover_flac_files, natural_str_cmp,
    pick_random_flac, pick_random_flac_walk, pick_random_flac_walk_with_seed,
    pick_random_flac_with_seed, read_dir_frame,
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

#[cfg(windows)]
#[test]
fn walk_and_sequential_pickers_traverse_windows_ntfs_junctions() {
    let root = make_temp_tree("junc_walk_root");
    let target = make_temp_tree("junc_walk_target");
    fs::write(target.join("favorite_song.flac"), b"").unwrap();

    let junction = root.join("Favorite");
    let status = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &junction.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .status()
        .expect("mklink /J failed");
    assert!(status.success());

    let picked = pick_random_flac_walk_with_seed(&root, 42)
        .expect("walk picker must traverse NTFS junction");
    assert!(picked.ends_with("favorite_song.flac"));

    let mut cur = SequentialFlacCursor::new(root.clone()).unwrap();
    let seq = cur.next_track().expect("sequential cursor must traverse NTFS junction");
    assert!(seq.ends_with("favorite_song.flac"));

    let _ = fs::remove_dir(&junction);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&target);
}

#[test]
fn sequential_cursor_folder_only_does_not_descend_and_handles_missing_start_after() {
    let root = make_temp_tree("seq_folder_only");
    let sub = root.join("SubAlbum");
    fs::create_dir_all(&sub).unwrap();
    fs::write(root.join("01.flac"), b"").unwrap();
    fs::write(root.join("02.flac"), b"").unwrap();
    fs::write(root.join("04.flac"), b"").unwrap();
    fs::write(sub.join("99_sub.flac"), b"").unwrap();

    // Exact match: yields only remaining direct files, never descends into SubAlbum.
    let mut cur =
        SequentialFlacCursor::new_folder_only(root.clone(), Some(&root.join("01.flac"))).unwrap();
    let mut yielded = Vec::new();
    while let Some(p) = cur.next_track() {
        yielded.push(p.file_name().unwrap().to_string_lossy().to_string());
    }
    assert_eq!(yielded, vec!["02.flac", "04.flac"]);

    // Missing/deleted start_after ("03.flac"): resumes at first entry sorting after it ("04.flac").
    let mut cur2 =
        SequentialFlacCursor::new_folder_only(root.clone(), Some(&root.join("03.flac"))).unwrap();
    let mut yielded2 = Vec::new();
    while let Some(p) = cur2.next_track() {
        yielded2.push(p.file_name().unwrap().to_string_lossy().to_string());
    }
    assert_eq!(yielded2, vec!["04.flac"]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn sequential_cursor_starting_after_resumes_deep_in_tree_in_o_depth() {
    let root = make_temp_tree("seq_start_after");
    let album_a = root.join("AlbumA");
    let album_b = root.join("AlbumB");
    fs::create_dir_all(&album_a).unwrap();
    fs::create_dir_all(&album_b).unwrap();
    fs::write(root.join("00_root.flac"), b"").unwrap();
    fs::write(album_a.join("01.flac"), b"").unwrap();
    fs::write(album_a.join("02.flac"), b"").unwrap();
    fs::write(album_b.join("01.flac"), b"").unwrap();

    let mut cur =
        SequentialFlacCursor::new_starting_after(root.clone(), &album_a.join("01.flac")).unwrap();
    let mut yielded = Vec::new();
    while let Some(p) = cur.next_track() {
        let rel = p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        yielded.push(rel);
    }
    assert_eq!(yielded, vec!["AlbumA/02.flac", "AlbumB/01.flac"]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn natural_sorting_orders_case_insensitively_with_numeric_digit_runs() {
    use std::cmp::Ordering;

    assert_eq!(natural_str_cmp("2.flac", "10.flac"), Ordering::Less);
    assert_eq!(natural_str_cmp("abba", "Zappa"), Ordering::Less);
    assert_eq!(natural_str_cmp("track 9", "Track 10"), Ordering::Less);

    let root = make_temp_tree("natural_sort");
    fs::create_dir_all(root.join("Zappa")).unwrap();
    fs::create_dir_all(root.join("abba")).unwrap();
    fs::create_dir_all(root.join("Disc 10")).unwrap();
    fs::create_dir_all(root.join("Disc 2")).unwrap();

    fs::write(root.join("10.flac"), b"").unwrap();
    fs::write(root.join("1.flac"), b"").unwrap();
    fs::write(root.join("2.flac"), b"").unwrap();
    fs::write(root.join("abba").join("a.flac"), b"").unwrap();
    fs::write(root.join("Zappa").join("z.flac"), b"").unwrap();
    fs::write(root.join("Disc 2").join("t.flac"), b"").unwrap();
    fs::write(root.join("Disc 10").join("t.flac"), b"").unwrap();

    let frame = read_dir_frame(&root).unwrap();
    let dir_names: Vec<_> = frame
        .subdirs
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    let flac_names: Vec<_> = frame
        .flacs
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(dir_names, vec!["abba", "Disc 2", "Disc 10", "Zappa"]);
    assert_eq!(flac_names, vec!["1.flac", "2.flac", "10.flac"]);

    // Sequential cursor yields direct files first, then folders, all in natural order.
    let mut cur = SequentialFlacCursor::new(root.clone()).unwrap();
    let mut yielded = Vec::new();
    while let Some(p) = cur.next_track() {
        yielded.push(p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"));
    }
    assert_eq!(
        yielded,
        vec![
            "1.flac",
            "2.flac",
            "10.flac",
            "abba/a.flac",
            "Disc 2/t.flac",
            "Disc 10/t.flac",
            "Zappa/z.flac",
        ]
    );

    // Folder-only cursor starting after a missing "3.flac" must use natural comparison and yield "10.flac".
    let mut cur2 =
        SequentialFlacCursor::new_folder_only(root.clone(), Some(&root.join("3.flac"))).unwrap();
    let mut after3 = Vec::new();
    while let Some(p) = cur2.next_track() {
        after3.push(p.file_name().unwrap().to_string_lossy().to_string());
    }
    assert_eq!(after3, vec!["10.flac"]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn skips_appledouble_metadata_recycle_bin_and_hidden_while_keeping_ellipsis_albums() {
    let root = make_temp_tree("skip_hidden");
    fs::write(root.join("._01.flac"), b"appledouble").unwrap();
    fs::write(root.join(".DS_Store"), b"finder").unwrap();
    fs::write(root.join("01.flac"), b"").unwrap();
    fs::write(root.join("02.flac"), b"").unwrap();
    // Real leading-dot music titles MUST NOT be hidden!
    fs::write(root.join("...Baby One More Time.flac"), b"").unwrap();
    fs::create_dir_all(root.join("...And Justice for All")).unwrap();
    fs::write(
        root.join("...And Justice for All").join("01_blackened.flac"),
        b"",
    )
    .unwrap();

    fs::create_dir_all(root.join(".Trash")).unwrap();
    fs::write(root.join(".Trash").join("trashed.flac"), b"").unwrap();

    fs::create_dir_all(root.join("$RECYCLE.BIN")).unwrap();
    fs::write(root.join("$RECYCLE.BIN").join("deleted.flac"), b"").unwrap();

    fs::create_dir_all(root.join("System Volume Information")).unwrap();
    fs::write(root.join("System Volume Information").join("sys.flac"), b"").unwrap();

    #[cfg(windows)]
    {
        let hidden_file = root.join("hidden_attr.flac");
        let hidden_dir = root.join("HiddenFolder");
        fs::write(&hidden_file, b"").unwrap();
        fs::create_dir_all(&hidden_dir).unwrap();
        fs::write(hidden_dir.join("secret.flac"), b"").unwrap();

        let s1 = std::process::Command::new("attrib")
            .args(["+H", &hidden_file.to_string_lossy()])
            .status()
            .expect("attrib +H file failed");
        assert!(s1.success());
        let s2 = std::process::Command::new("attrib")
            .args(["+H", &hidden_dir.to_string_lossy()])
            .status()
            .expect("attrib +H dir failed");
        assert!(s2.success());
    }

    let frame = read_dir_frame(&root).unwrap();
    let subdir_names: Vec<_> = frame
        .subdirs
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(subdir_names, vec!["...And Justice for All"]);
    let flac_names: Vec<_> = frame
        .flacs
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        flac_names,
        vec!["...Baby One More Time.flac", "01.flac", "02.flac"]
    );

    let discovered: Vec<_> = discover_flac_files(&root)
        .unwrap()
        .into_iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        discovered,
        vec![
            "01_blackened.flac",
            "...Baby One More Time.flac",
            "01.flac",
            "02.flac"
        ]
    );

    let mut cur = SequentialFlacCursor::new(root.clone()).unwrap();
    let mut seq = Vec::new();
    while let Some(p) = cur.next_track() {
        seq.push(p.file_name().unwrap().to_string_lossy().to_string());
    }
    assert_eq!(
        seq,
        vec![
            "...Baby One More Time.flac",
            "01.flac",
            "02.flac",
            "01_blackened.flac"
        ]
    );

    for seed in 0..30_u64 {
        let picked = pick_random_flac_walk_with_seed(&root, seed).unwrap();
        let name = picked.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            matches!(
                name.as_str(),
                "01.flac" | "02.flac" | "...Baby One More Time.flac" | "01_blackened.flac"
            ),
            "unexpected hidden pick: {name}"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

#[cfg(windows)]
#[test]
fn junction_cycles_and_duplicate_junctions_never_loop_or_duplicate_tracks() {
    let root = make_temp_tree("junc_cycles");
    let album = root.join("Album");
    let shared_target = make_temp_tree("junc_shared_target");
    fs::create_dir_all(&album).unwrap();
    fs::write(album.join("only_once.flac"), b"").unwrap();
    fs::write(shared_target.join("shared_once.flac"), b"").unwrap();

    let self_loop = album.join("SelfLoop");
    let root_loop = album.join("RootLoop");
    let dup_a = root.join("DupA");
    let dup_b = root.join("DupB");

    for (link, target) in [
        (&self_loop, &album),
        (&root_loop, &root),
        (&dup_a, &shared_target),
        (&dup_b, &shared_target),
    ] {
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J", &link.to_string_lossy(), &target.to_string_lossy()])
            .status()
            .expect("mklink /J failed");
        assert!(st.success(), "mklink failed for {}", link.display());
    }

    // 1. discover_flac_files terminates quickly with zero duplicates.
    let found = discover_flac_files(&root).unwrap();
    assert_eq!(found.len(), 2, "expected 2 unique tracks, got {found:?}");

    // 2. SequentialFlacCursor terminates cleanly after 2 tracks (never replays 32x or 2^32x).
    let mut cur = SequentialFlacCursor::new(root.clone()).unwrap();
    let mut seq = Vec::new();
    while let Some(p) = cur.next_track() {
        seq.push(p);
        assert!(seq.len() <= 10, "sequential cursor looped infinitely: {seq:?}");
    }
    assert_eq!(seq.len(), 2, "expected 2 sequential tracks, got {seq:?}");

    // 3. Random walker terminates promptly across many seeds.
    for seed in 0..30_u64 {
        let picked = pick_random_flac_walk_with_seed(&root, seed).unwrap();
        assert!(
            picked.ends_with("only_once.flac") || picked.ends_with("shared_once.flac"),
            "unexpected pick: {}",
            picked.display()
        );
    }

    // 4. Empty directory with TWO self-loops must NOT blow up exponentially (2^32).
    let empty_cycle = make_temp_tree("junc_empty_cycle");
    let loop1 = empty_cycle.join("Loop1");
    let loop2 = empty_cycle.join("Loop2");
    for link in [&loop1, &loop2] {
        let st = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &empty_cycle.to_string_lossy(),
            ])
            .status()
            .expect("mklink /J failed");
        assert!(st.success());
    }
    assert!(pick_random_flac_walk_with_seed(&empty_cycle, 1).is_err());

    let _ = fs::remove_dir(&self_loop);
    let _ = fs::remove_dir(&root_loop);
    let _ = fs::remove_dir(&dup_a);
    let _ = fs::remove_dir(&dup_b);
    let _ = fs::remove_dir(&loop1);
    let _ = fs::remove_dir(&loop2);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&shared_target);
    let _ = fs::remove_dir_all(&empty_cycle);
}

#[test]
fn walk_picker_weights_large_folders_and_here_slot_fairly() {
    let root = make_temp_tree("walk_weighted");
    let big = root.join("BigCollection");
    let single = root.join("SingleFolder");
    fs::create_dir_all(&big).unwrap();
    fs::create_dir_all(&single).unwrap();

    fs::write(root.join("loose.flac"), b"").unwrap();
    fs::write(single.join("lone.flac"), b"").unwrap();
    // Non-FLAC artwork/logs inside SingleFolder must NOT inflate its weight above BigCollection!
    for i in 1..=80 {
        fs::write(single.join(format!("cover_{i:02}.jpg")), b"").unwrap();
    }
    for i in 1..=20 {
        fs::write(big.join(format!("track_{i:02}.flac")), b"").unwrap();
    }

    let mut big_hits = 0usize;
    let mut loose_hits = 0usize;
    let mut single_hits = 0usize;
    for seed in 0..200_u64 {
        let picked = pick_random_flac_walk_with_seed(&root, seed).unwrap();
        if picked.starts_with(&big) {
            big_hits += 1;
        } else if picked.starts_with(&single) {
            single_hits += 1;
        } else {
            loose_hits += 1;
        }
    }

    // BigCollection has weight 20 vs SingleFolder=1 and Here=1 (~90.9% expected).
    assert!(
        big_hits >= 140,
        "expected BigCollection to dominate weighted walk (got big={big_hits}, single={single_hits}, loose={loose_hits})"
    );
    assert!(
        loose_hits > 0 && single_hits > 0,
        "expected small branches to remain reachable (big={big_hits}, single={single_hits}, loose={loose_hits})"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn newly_populated_grandchild_folder_is_not_permanently_hidden_by_cached_zero() {
    // Review #3 Finding #5: an empty Artist\Album visited during ripping must NEVER cache 0
    // under Artist's mtime and make the subtree permanently unreachable once tracks land in Album.
    let root = make_temp_tree("no_cached_zero_poison");
    let artist = root.join("Artist");
    let album = artist.join("Album");
    let other = root.join("Other");
    fs::create_dir_all(&album).unwrap();
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("01.flac"), b"").unwrap();

    // Visit while Artist\Album is completely empty (would previously cache Artist=0 under Artist's mtime!).
    for seed in 0..20_u64 {
        let p = pick_random_flac_walk_with_seed(&root, seed).unwrap();
        assert_eq!(p, other.join("01.flac"));
    }

    // Now write a FLAC inside grandchild Album (Artist's own mtime does NOT change!).
    let new_track = album.join("02.flac");
    fs::write(&new_track, b"").unwrap();

    let mut reached_new = false;
    for seed in 0..100_u64 {
        if pick_random_flac_walk_with_seed(&root, seed).unwrap() == new_track {
            reached_new = true;
            break;
        }
    }
    assert!(
        reached_new,
        "newly populated grandchild track must be reachable (never poisoned by cached 0)"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reparse_target_allow_list_blocks_mixed_case_unc_and_global_verbatim_targets() {
    use ap_source::library::check_reparse_target_is_allowed;
    use std::path::Path;

    // Review #3 Finding #28: component + raw-prefix allow-list must reject mixed-case \\?\Unc\,
    // \\?\Global\UNC\, \??\UNC\, \\.\pipe\, and foreign UNC shares before any metadata syscall.
    for evil in [
        r"\\?\Unc\evil\share\a.flac",
        r"\\?\UNC\evil\share\a.flac",
        r"\\?\Global\UNC\evil\share\a.flac",
        r"\\?\GLOBALROOT\Device\LanmanRedirector\evil\share",
        r"\??\UNC\evil\share",
        r"\Device\Mup\evil\share\track.flac",
        r"\GLOBAL??\UNC\evil\share",
        r"\RPC Control\trap",
        r"\BaseNamedObjects\trap",
        r"\\.\pipe\ntlm_trap",
        r"\\evil\share\track.flac",
        r"//evil/share/track.flac",
    ] {
        assert!(
            check_reparse_target_is_allowed(Path::new(evil), None).is_err(),
            "unsafe link target MUST be rejected for local root: {evil}"
        );
        assert!(
            check_reparse_target_is_allowed(Path::new(evil), Some(("nas", "music"))).is_err(),
            "foreign/device link target MUST be rejected even with trusted NAS root: {evil}"
        );
    }

    // Allowed local and trusted-share targets.
    for safe_local in [
        r"C:\Music\Album",
        r"\\?\D:\Music\Album",
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\Music",
        "relative/subdir",
    ] {
        assert!(
            check_reparse_target_is_allowed(Path::new(safe_local), None).is_ok(),
            "safe local link target should be allowed: {safe_local}"
        );
    }
    assert!(
        check_reparse_target_is_allowed(
            Path::new(r"\\?\Unc\NAS\Music\Album"),
            Some(("nas", "music"))
        )
        .is_ok(),
        "trusted UNC share in mixed-case verbatim form should be allowed for its own root"
    );
}

#[cfg(windows)]
#[test]
fn samba_hidden_dot_prefixed_album_remains_visible_while_non_dot_hidden_is_skipped() {
    // Review #3 Finding #8: Samba sets FILE_ATTRIBUTE_HIDDEN on dot-prefixed entries by default
    // (`hide dot files = yes`). Real dot-prefixed albums (`...And Justice for All`) must remain
    // visible even with FILE_ATTRIBUTE_HIDDEN set, while non-dot hidden folders and excluded
    // dot-metadata (`.git`, `._track.flac`) are still skipped.
    let root = make_temp_tree("samba_hidden_dots");
    let ellipsis_album = root.join("...And Justice for All");
    let hidden_nondot = root.join("HiddenNonDot");
    let hidden_single_dot = root.join(".hidden_local_dir");
    let dot_git = root.join(".git");
    let dot_recycle = root.join(".recycle");
    let dot_stversions = root.join(".stversions");
    let dot_trash_1000 = root.join(".Trash-1000");
    let dot_snapshot = root.join(".snapshot");
    let ea_dir = root.join("@eaDir");
    for d in [
        &ellipsis_album,
        &hidden_nondot,
        &hidden_single_dot,
        &dot_git,
        &dot_recycle,
        &dot_stversions,
        &dot_trash_1000,
        &dot_snapshot,
        &ea_dir,
    ] {
        fs::create_dir_all(d).unwrap();
    }
    let ellipsis_track = ellipsis_album.join("...One.flac");
    fs::write(&ellipsis_track, b"").unwrap();
    fs::write(hidden_nondot.join("secret.flac"), b"").unwrap();
    fs::write(hidden_single_dot.join("hidden.flac"), b"").unwrap();
    fs::write(dot_git.join("ignored.flac"), b"").unwrap();
    fs::write(dot_recycle.join("deleted.flac"), b"").unwrap();
    fs::write(dot_stversions.join("track~20241101.flac"), b"").unwrap();
    fs::write(dot_trash_1000.join("trashed.flac"), b"").unwrap();
    fs::write(dot_snapshot.join("snap.flac"), b"").unwrap();
    fs::write(ea_dir.join("thumb.flac"), b"").unwrap();
    let apple_double = root.join("._01.flac");
    fs::write(&apple_double, b"").unwrap();

    for p in [
        &ellipsis_album,
        &ellipsis_track,
        &hidden_nondot,
        &hidden_single_dot,
        &dot_git,
        &dot_recycle,
        &dot_stversions,
        &dot_trash_1000,
        &apple_double,
    ] {
        let st = std::process::Command::new("attrib")
            .args(["+H", &p.to_string_lossy()])
            .status()
            .expect("attrib +H failed");
        assert!(st.success(), "attrib +H failed for {}", p.display());
    }

    let frame = read_dir_frame(&root).unwrap();
    let dir_names: Vec<String> = frame
        .subdirs
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        dir_names,
        vec!["...And Justice for All"],
        "hidden ellipsis album must stay visible while recycle/version/hidden local dot dirs are skipped"
    );
    assert!(frame.flacs.is_empty(), "._01.flac must stay excluded");

    let album_frame = read_dir_frame(&ellipsis_album).unwrap();
    assert_eq!(
        album_frame.flacs,
        vec![ellipsis_track],
        "hidden dot-prefixed track inside ellipsis album must stay visible"
    );

    let _ = fs::remove_dir_all(&root);
}

#[cfg(windows)]
#[test]
fn verify_symlink_chain_blocks_intermediate_junction_hop_before_following() {
    // Review #4 Finding #4 (MEDIUM):
    // Build a multi-hop local junction where `A` points to `B\nested` (an intermediate component
    // `B` is itself a junction). Verify that `verify_symlink_chain_is_allowed` walks every
    // intermediate component with `symlink_metadata`, detects cycles/hop limits through
    // intermediate components, AND allows safe local intermediate junctions!
    use ap_source::library::verify_symlink_chain_is_allowed;

    let root = make_temp_tree("intermediate_junc_check");
    let real_dir = root.join("Real");
    let nested = real_dir.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("ok.flac"), b"").unwrap();

    // Safe intermediate junction: B -> Real, A -> B\nested.
    let junc_b = root.join("B");
    let junc_a = root.join("A");
    for (link, target) in [(&junc_b, &real_dir), (&junc_a, &junc_b.join("nested"))] {
        let st = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .status()
            .expect("mklink /J failed");
        assert!(st.success(), "mklink failed for {}", link.display());
    }
    assert!(
        verify_symlink_chain_is_allowed(&junc_a, None).is_ok(),
        "safe local intermediate junction chain must be allowed"
    );

    // Cyclic intermediate junction: LoopB -> root, LoopA -> LoopB\LoopA (intermediate component
    // re-enters LoopB infinitely -> per-component walker MUST hit MAX_SYMLINK_HOPS and reject!).
    let loop_b = root.join("LoopB");
    let loop_a = root.join("LoopA");
    let st1 = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &loop_b.to_string_lossy(),
            &root.to_string_lossy(),
        ])
        .status()
        .expect("mklink /J failed");
    assert!(st1.success());
    let st2 = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &loop_a.to_string_lossy(),
            &loop_b.join("LoopA").to_string_lossy(),
        ])
        .status()
        .expect("mklink /J failed");
    assert!(st2.success());
    let err = verify_symlink_chain_is_allowed(&loop_a, None)
        .expect_err("intermediate junction cycle MUST be rejected via per-component inspection");
    assert!(err.contains("exceeded"), "unexpected error: {err}");

    let _ = fs::remove_dir(&junc_a);
    let _ = fs::remove_dir(&junc_b);
    let _ = fs::remove_dir(&loop_a);
    let _ = fs::remove_dir(&loop_b);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn review5_nas_denylist_excludes_qnap_and_synology_snapshot_and_recycle_dirs() {
    // Review #5 Finding #32: QNAP `@Recycle`, `@Recently-Snapshot` and Synology `#snapshot`
    // must be skipped even when visible (no HIDDEN/SYSTEM attributes).
    let root = make_temp_tree("nas_denylist_r5");
    for excluded in ["@Recycle", "@Recently-Snapshot", "#snapshot"] {
        let d = root.join(excluded);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("deleted_track.flac"), b"fLaC").unwrap();
    }
    let real_album = root.join("Real Album");
    fs::create_dir_all(&real_album).unwrap();
    let real_track = real_album.join("01.flac");
    fs::write(&real_track, b"fLaC").unwrap();

    let frame = read_dir_frame(&root).unwrap();
    assert_eq!(frame.subdirs, vec![real_album]);
    let discovered = discover_flac_files(&root).unwrap();
    assert_eq!(discovered, vec![real_track]);

    // Review #5 Finding #23: UNC paths report remote; local temp dir reports local.
    assert!(ap_source::library::is_remote_network_dir(std::path::Path::new(
        r"\\nas\music"
    )));
    assert!(!ap_source::library::is_remote_network_dir(&root));

    let _ = fs::remove_dir_all(&root);
}

#[cfg(windows)]
#[test]
fn review5_lexical_dotdot_resolution_blocks_missing_and_junction_parent_bypass() {
    // Review #5 Finding #10: Windows collapses `..` lexically BEFORE resolving reparse points.
    // Both `missing\..\evil` AND `J\..\evil` (where `J` is a junction to a clean folder and
    // `evil` is an unsafe/cyclic junction) MUST collapse to `evil` and be rejected!
    use ap_source::library::verify_resolved_path_is_allowed;

    let root = make_temp_tree("lexical_dotdot_r5");
    let clean_target = root.join("CleanTarget");
    fs::create_dir_all(&clean_target).unwrap();

    // `J` is a benign junction pointing to CleanTarget.
    let junc_j = root.join("J");
    let st_j = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &junc_j.to_string_lossy(),
            &clean_target.to_string_lossy(),
        ])
        .status()
        .unwrap();
    assert!(st_j.success());

    // `evil` is a cyclic/unsafe junction (or UNC-targeting link) in the same directory.
    // Using a self-cyclic junction (`evil -> evil`) reliably triggers rejection without needing
    // elevated SeCreateSymbolicLinkPrivilege, AND we also test a real UNC target if possible.
    let evil = root.join("evil");
    let st_e = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &evil.to_string_lossy(),
            &evil.to_string_lossy(),
        ])
        .status()
        .unwrap();
    assert!(st_e.success());

    // 1. `missing\..\evil`: before Finding #10 fix, statting `missing` returned NotFound -> Ok(())!
    let via_missing = root.join("missing").join("..").join("evil");
    assert!(
        verify_resolved_path_is_allowed(&via_missing, None).is_err(),
        "`missing\\..\\evil` MUST lexically collapse to `evil` and reject it"
    );

    // 2. `J\..\evil`: before Finding #10 fix, POSIX resolution followed `J` -> `CleanTarget\..`
    //    and checked `root\evil` or `CleanTarget\..\evil` inconsistently; lexical collapse MUST
    //    directly inspect `root\evil` and reject it!
    let via_junc = root.join("J").join("..").join("evil");
    assert!(
        verify_resolved_path_is_allowed(&via_junc, None).is_err(),
        "`J\\..\\evil` MUST lexically collapse to `evil` and reject it"
    );

    // 3. Intermediate NotFound component followed by further components must NOT return Ok(()).
    let missing_mid = root.join("no_such_dir").join("child.flac");
    assert!(
        verify_resolved_path_is_allowed(&missing_mid, None).is_err(),
        "NotFound on an intermediate component must NOT be treated as safe"
    );
    // Final NotFound component with existing parent IS safe.
    let missing_leaf = root.join("no_such_leaf.flac");
    assert!(
        verify_resolved_path_is_allowed(&missing_leaf, None).is_ok(),
        "NotFound on the final component with no remaining suffix should be allowed"
    );

    let _ = fs::remove_dir(&junc_j);
    let _ = fs::remove_dir(&evil);
    let _ = fs::remove_dir_all(&root);
}
