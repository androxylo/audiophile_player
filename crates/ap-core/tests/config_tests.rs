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
