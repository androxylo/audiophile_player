// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

use std::fs;
use std::path::{Path, PathBuf};

use ap_core::config::{ConvolutionConfig, PlayerConfig};

#[test]
fn parses_convolution_toml_with_windows_literal_and_escaped_paths() {
    let toml = "\u{feff}# UTF-8 BOM + comment
[convolution]
enabled = true # inline comment
dir = 'C:\\Program Files\\EqualizerAPO\\config' # path with spaces
file_352800 = \"D:\\\\Filters\\\\room #1 (352k).wav\"
file_384000 = 'D:\\Filters\\room #1 (384k).wav'
";
    let cfg = PlayerConfig::from_toml_str(toml).unwrap();
    assert!(cfg.convolution.enabled);
    assert_eq!(
        cfg.convolution.dir,
        Some(PathBuf::from(r"C:\Program Files\EqualizerAPO\config"))
    );
    assert_eq!(
        cfg.convolution.file_352800,
        Some(PathBuf::from(r"D:\Filters\room #1 (352k).wav"))
    );
    assert_eq!(
        cfg.convolution.file_384000,
        Some(PathBuf::from(r"D:\Filters\room #1 (384k).wav"))
    );
}

#[test]
fn omitted_enabled_auto_enables_when_source_present_and_disables_when_empty() {
    let with_dir = "[convolution]\ndir = 'C:\\REW\\filters'\n";
    let cfg = PlayerConfig::from_toml_str(with_dir).unwrap();
    assert!(cfg.convolution.enabled);
    assert_eq!(cfg.convolution.dir, Some(PathBuf::from(r"C:\REW\filters")));

    let empty = "# empty config\n[convolution]\n";
    let cfg_empty = PlayerConfig::from_toml_str(empty).unwrap();
    assert!(!cfg_empty.convolution.enabled);
    assert_eq!(cfg_empty.convolution, ConvolutionConfig::disabled());

    let explicitly_disabled = "[convolution]\nenabled = false\ndir = 'C:\\REW\\filters'\n";
    let cfg_off = PlayerConfig::from_toml_str(explicitly_disabled).unwrap();
    assert!(!cfg_off.convolution.enabled);
    assert_eq!(
        cfg_off.convolution.dir,
        Some(PathBuf::from(r"C:\REW\filters"))
    );
}

#[test]
fn rejects_invalid_sections_keys_escapes_and_enabled_without_source() {
    // enabled = true with no path
    let err = PlayerConfig::from_toml_str("[convolution]\nenabled = true\n").unwrap_err();
    assert!(err.contains("none of 'dir'"), "unexpected err: {err}");

    // Unknown section
    let err = PlayerConfig::from_toml_str("[volume]\ngain = 0\n").unwrap_err();
    assert!(err.contains("unknown section"), "unexpected err: {err}");

    // Unknown key typo
    let err = PlayerConfig::from_toml_str("[convolution]\ndirectory = 'C:\\REW'\n").unwrap_err();
    assert!(err.contains("unknown key 'directory'"), "unexpected err: {err}");

    // Unescaped backslash in double-quoted string gives helpful Windows hint
    let err = PlayerConfig::from_toml_str("[convolution]\ndir = \"C:\\Program Files\"\n").unwrap_err();
    assert!(err.contains("single quotes"), "unexpected err: {err}");
}

#[test]
fn discovery_order_prefers_explicit_then_local_then_user_then_disabled_default() {
    let tmp = std::env::temp_dir().join(format!("ap_core_cfg_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let explicit = tmp.join("explicit.toml");
    let local = tmp.join("audiophile-player.toml");
    let user = tmp.join("user_config.toml");

    // 1. Neither exists -> disabled default, source_path = None
    let loaded = PlayerConfig::discover_and_load_with_paths(None, Some(&local), Some(&user)).unwrap();
    assert_eq!(loaded.source_path, None);
    assert!(!loaded.config.convolution.enabled);

    // 2. Explicit missing path MUST error
    assert!(PlayerConfig::discover_and_load_with_paths(Some(&explicit), Some(&local), Some(&user)).is_err());

    // 3. User config exists -> loaded
    fs::write(&user, "[convolution]\ndir = 'C:\\UserFilters'\n").unwrap();
    let loaded = PlayerConfig::discover_and_load_with_paths(None, Some(&local), Some(&user)).unwrap();
    assert_eq!(loaded.source_path.as_deref(), Some(user.as_path()));
    assert_eq!(
        loaded.config.convolution.dir,
        Some(PathBuf::from(r"C:\UserFilters"))
    );

    // 4. Local config overrides user config
    fs::write(&local, "[convolution]\ndir = 'C:\\LocalFilters'\n").unwrap();
    let loaded = PlayerConfig::discover_and_load_with_paths(None, Some(&local), Some(&user)).unwrap();
    assert_eq!(loaded.source_path.as_deref(), Some(local.as_path()));
    assert_eq!(
        loaded.config.convolution.dir,
        Some(PathBuf::from(r"C:\LocalFilters"))
    );

    // 5. Explicit config overrides local config
    fs::write(&explicit, "[convolution]\nfile_384000 = 'C:\\Explicit384.wav'\n").unwrap();
    let loaded =
        PlayerConfig::discover_and_load_with_paths(Some(&explicit), Some(&local), Some(&user))
            .unwrap();
    assert_eq!(loaded.source_path.as_deref(), Some(explicit.as_path()));
    assert_eq!(
        loaded.config.convolution.file_384000,
        Some(PathBuf::from(r"C:\Explicit384.wav"))
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn shipped_example_toml_template_parses_cleanly() {
    let example_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("audiophile-player.example.toml");
    let cfg = PlayerConfig::load_from_file(&example_path).unwrap();
    assert!(!cfg.convolution.enabled);
}

#[test]
fn rejects_duplicate_keys_and_duplicate_convolution_section() {
    let dup_enabled = "[convolution]\nenabled = true\ndir = 'C:\\REW'\nenabled = false\n";
    let err = PlayerConfig::from_toml_str(dup_enabled).unwrap_err();
    assert!(
        err.contains("duplicate key 'enabled'"),
        "expected duplicate key error, got: {err}"
    );

    let dup_dir = "[convolution]\ndir = 'C:\\REW\\A'\ndir = 'C:\\REW\\B'\n";
    let err = PlayerConfig::from_toml_str(dup_dir).unwrap_err();
    assert!(
        err.contains("duplicate key 'dir'"),
        "expected duplicate key error, got: {err}"
    );

    let dup_section = "[convolution]\ndir = 'C:\\REW\\A'\n[convolution]\n";
    let err = PlayerConfig::from_toml_str(dup_section).unwrap_err();
    assert!(
        err.contains("duplicate section"),
        "expected duplicate section error, got: {err}"
    );
}

#[test]
fn validate_paths_rejects_missing_dir_or_file_when_enabled_and_allows_when_disabled() {
    let tmp = std::env::temp_dir().join(format!(
        "ap_core_cfg_paths_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    let missing_dir = tmp.join("does_not_exist");
    let existing_file = tmp.join("ir.wav");
    fs::write(&existing_file, b"RIFF").unwrap();
    let missing_file = tmp.join("missing.wav");

    // Disabled config skips disk existence checks.
    let disabled_missing = ConvolutionConfig {
        enabled: false,
        dir: Some(missing_dir.clone()),
        ..ConvolutionConfig::default()
    };
    disabled_missing.validate_paths().unwrap();

    // Enabled config with missing dir must error.
    let err = ConvolutionConfig::auto_dir(missing_dir).validate_paths().unwrap_err();
    assert!(err.contains("does not exist"), "unexpected err: {err}");

    // Enabled config with existing dir succeeds.
    ConvolutionConfig::auto_dir(tmp.clone()).validate_paths().unwrap();

    // Enabled config with missing file must error.
    let err = ConvolutionConfig::explicit_file(missing_file).validate_paths().unwrap_err();
    assert!(err.contains("does not exist"), "unexpected err: {err}");

    // Enabled config with existing file succeeds.
    ConvolutionConfig::explicit_file(existing_file).validate_paths().unwrap();

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn example_toml_auto_enables_convolution_when_only_dir_is_uncommented() {
    // Review #3 Finding #15: example template must not ship uncommented `enabled = false`
    // that silently disables convolution when the user uncomments `dir = ...`.
    let template = include_str!("../../../audiophile-player.example.toml");
    let unmodified = PlayerConfig::from_toml_str(template).unwrap();
    assert!(
        !unmodified.convolution.enabled,
        "unmodified template without uncommented sources should default to disabled"
    );

    let with_dir = template.replace(
        "# dir = 'C:\\Program Files\\EqualizerAPO\\config'",
        "dir = 'C:\\Program Files\\EqualizerAPO\\config'",
    );
    assert_ne!(
        with_dir, template,
        "template must contain the Option A dir example line"
    );
    let enabled_cfg = PlayerConfig::from_toml_str(&with_dir).unwrap();
    assert!(
        enabled_cfg.convolution.enabled,
        "uncommenting only `dir` in example template must auto-enable convolution"
    );
}

#[test]
fn parses_and_validates_upnp_section_with_sensible_defaults() {
    let default_cfg = PlayerConfig::default();
    assert!(default_cfg.upnp.enabled);
    assert_eq!(default_cfg.upnp.friendly_name, "Audiophile Player");
    assert_eq!(default_cfg.upnp.port, None);
    assert_eq!(default_cfg.upnp.bind_ip, None);

    let toml = r#"
[upnp]
enabled = true
friendly_name = "Gungnir 2 Living Room"
port = 49160
bind_ip = "192.168.0.187"
"#;
    let cfg = PlayerConfig::from_toml_str(toml).unwrap();
    assert!(cfg.upnp.enabled);
    assert_eq!(cfg.upnp.friendly_name, "Gungnir 2 Living Room");
    assert_eq!(cfg.upnp.port, Some(49160));
    assert_eq!(cfg.upnp.bind_ip.as_deref(), Some("192.168.0.187"));

    assert!(PlayerConfig::from_toml_str("[upnp]\nport = 0\n").is_err());
    assert!(PlayerConfig::from_toml_str("[upnp]\nbind_ip = 'not-an-ip'\n").is_err());
    assert!(PlayerConfig::from_toml_str("[upnp]\nfriendly_name = ''\n").is_err());
}

#[test]
fn parses_and_validates_quantizer_section_with_strict_dac_bits_and_shaper() {
    let default_cfg = PlayerConfig::default();
    assert_eq!(default_cfg.quantizer.dac_bits, 24);
    assert_eq!(default_cfg.quantizer.shaper, None);

    for bits in [16_u8, 18, 20, 24] {
        let cfg = PlayerConfig::from_toml_str(&format!("[quantizer]\ndac_bits = {bits}\n")).unwrap();
        assert_eq!(cfg.quantizer.dac_bits, bits);
        assert_eq!(cfg.quantizer.shaper, None);
    }

    let custom = PlayerConfig::from_toml_str(
        "[quantizer]\ndac_bits = 18\nshaper = 'ns-gentle-2'\n",
    )
    .unwrap();
    assert_eq!(custom.quantizer.dac_bits, 18);
    assert_eq!(custom.quantizer.shaper.as_deref(), Some("ns-gentle-2"));

    let auto_cfg = PlayerConfig::from_toml_str("[quantizer]\nshaper = 'auto'\n").unwrap();
    assert_eq!(auto_cfg.quantizer.dac_bits, 24);
    assert_eq!(auto_cfg.quantizer.shaper, None);

    // Invalid dac_bits, unknown shaper, duplicate section, duplicate key.
    for bad_bits in [0, 8, 12, 15, 22, 32] {
        assert!(
            PlayerConfig::from_toml_str(&format!("[quantizer]\ndac_bits = {bad_bits}\n")).is_err(),
            "dac_bits = {bad_bits} must be rejected"
        );
    }
    assert!(PlayerConfig::from_toml_str("[quantizer]\nshaper = 'bogus'\n").is_err());
    assert!(PlayerConfig::from_toml_str("[quantizer]\ndac_bits = 24\ndac_bits = 16\n").is_err());
    assert!(PlayerConfig::from_toml_str("[quantizer]\ndac_bits = 24\n[quantizer]\n").is_err());
    assert!(PlayerConfig::from_toml_str("[quantizer]\nunknown_key = 1\n").is_err());
}

#[test]
fn parses_and_validates_ultimate_and_trackdb_sections_with_locked_defaults() {
    let default_cfg = PlayerConfig::default();
    assert!(!default_cfg.ultimate.enabled);
    assert!(default_cfg.ultimate.genre_rules);
    assert!(default_cfg.ultimate.learn_on_play);
    assert_eq!(default_cfg.trackdb.dir, None);
    assert!(!default_cfg.trackdb.sidecar);

    let toml = r#"
[ultimate]
enabled = true
genre_rules = false
learn_on_play = false

[trackdb]
dir = 'D:\AudioCache\trackdb'
sidecar = true
"#;
    let cfg = PlayerConfig::from_toml_str(toml).unwrap();
    assert!(cfg.ultimate.enabled);
    assert!(!cfg.ultimate.genre_rules);
    assert!(!cfg.ultimate.learn_on_play);
    assert_eq!(cfg.trackdb.dir, Some(PathBuf::from(r"D:\AudioCache\trackdb")));
    assert!(cfg.trackdb.sidecar);

    // Reject duplicate sections, duplicate keys, and unknown keys (including removed OQ-36/OQ-40 keys).
    assert!(PlayerConfig::from_toml_str("[ultimate]\nenabled = true\n[ultimate]\n").is_err());
    assert!(PlayerConfig::from_toml_str("[ultimate]\nenabled = true\nenabled = false\n").is_err());
    assert!(PlayerConfig::from_toml_str("[ultimate]\nalbum_context = true\n").is_err());
    assert!(PlayerConfig::from_toml_str("[trackdb]\nsidecar = true\n[trackdb]\n").is_err());
    assert!(PlayerConfig::from_toml_str("[trackdb]\nsidecar = true\nsidecar = false\n").is_err());
    assert!(PlayerConfig::from_toml_str("[trackdb]\nunknown = 1\n").is_err());
}

#[test]
fn parses_and_validates_capture_section() {
    let default_cfg = PlayerConfig::default();
    assert!(default_cfg.capture.enabled);

    let cfg = PlayerConfig::from_toml_str("[capture]\nenabled = false\n").unwrap();
    assert!(!cfg.capture.enabled);

    let cfg_on = PlayerConfig::from_toml_str("[capture]\nenabled = true\n").unwrap();
    assert!(cfg_on.capture.enabled);

    assert!(PlayerConfig::from_toml_str("[capture]\nenabled = true\n[capture]\n").is_err());
    assert!(
        PlayerConfig::from_toml_str("[capture]\nenabled = true\nenabled = false\n").is_err()
    );
    assert!(PlayerConfig::from_toml_str("[capture]\nunknown = true\n").is_err());
}

#[test]
fn parses_and_validates_filters_and_roots_sections() {
    use ap_core::user_filters::FilterMode;

    let default_cfg = PlayerConfig::default();
    assert_eq!(default_cfg.filters.mode, FilterMode::Auto);
    assert_eq!(default_cfg.filters.dir, None);
    assert_eq!(default_cfg.filters.choices_file, None);
    assert!(default_cfg.filters.roots.is_empty());

    let toml = r#"
[filters]
mode = "mine"
dir = 'D:\AudioCache\filterdb'
choices_file = 'D:\AudioCache\my-filters.yaml'
root.main = 'D:\Music'

[filters.roots]
classical = 'E:\Classical'
"#;
    let cfg = PlayerConfig::from_toml_str(toml).unwrap();
    assert_eq!(cfg.filters.mode, FilterMode::Mine);
    assert_eq!(cfg.filters.dir, Some(PathBuf::from(r"D:\AudioCache\filterdb")));
    assert_eq!(
        cfg.filters.choices_file,
        Some(PathBuf::from(r"D:\AudioCache\my-filters.yaml"))
    );
    assert_eq!(
        cfg.filters.roots.get("main"),
        Some(&PathBuf::from(r"D:\Music"))
    );
    assert_eq!(
        cfg.filters.roots.get("classical"),
        Some(&PathBuf::from(r"E:\Classical"))
    );

    assert!(PlayerConfig::from_toml_str("[filters]\nmode = 'bogus'\n").is_err());
    assert!(PlayerConfig::from_toml_str("[filters]\nmode = 'mine'\nmode = 'suggested'\n").is_err());
    assert!(
        PlayerConfig::from_toml_str("[filters]\nroot.main = 'D:\\Music'\n[filters.roots]\nmain = 'E:\\Music'\n")
            .is_err()
    );
    assert!(PlayerConfig::from_toml_str("[filters]\nunknown = 1\n").is_err());
}

#[test]
fn parses_player_section_and_comment_preserving_config_editor_roundtrips() {
    use ap_core::config::{
        canonicalize_config_key, format_toml_value_for_key, unset_toml_key_preserving_comments,
        upsert_toml_key_preserving_comments,
    };

    assert_eq!(canonicalize_config_key("device"), Some("player.device"));
    assert_eq!(canonicalize_config_key("music_dir"), Some("player.music_dir"));
    assert_eq!(canonicalize_config_key("ultimate.enabled"), Some("ultimate.enabled"));

    let initial = "# User notes kept intact\n[player]\n# Preferred DAC\ndevice = 'Old DAC'\n";
    let v_dev = format_toml_value_for_key("player.device", "Gungnir 2").unwrap();
    let step1 = upsert_toml_key_preserving_comments(Some(initial), "player.device", &v_dev).unwrap();
    assert!(step1.contains("# User notes kept intact"));
    assert!(step1.contains("# Preferred DAC"));
    assert!(step1.contains("device = 'Gungnir 2'"));

    let v_dir = format_toml_value_for_key("player.music_dir", r"D:\Music\FLAC").unwrap();
    let step2 = upsert_toml_key_preserving_comments(Some(&step1), "player.music_dir", &v_dir).unwrap();
    let v_ult = format_toml_value_for_key("ultimate.enabled", "true").unwrap();
    let step3 = upsert_toml_key_preserving_comments(Some(&step2), "ultimate.enabled", &v_ult).unwrap();

    let parsed = PlayerConfig::from_toml_str(&step3).unwrap();
    assert_eq!(parsed.player.device.as_deref(), Some("Gungnir 2"));
    assert_eq!(parsed.player.music_dir, Some(PathBuf::from(r"D:\Music\FLAC")));
    assert!(parsed.ultimate.enabled);

    let (step4, removed) = unset_toml_key_preserving_comments(&step3, "player.device").unwrap();
    assert!(removed);
    let parsed2 = PlayerConfig::from_toml_str(&step4).unwrap();
    assert_eq!(parsed2.player.device, None);
    assert!(step4.contains("# Preferred DAC"));
}
