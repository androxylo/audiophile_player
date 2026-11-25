//! Extended per-track metadata (`TrackMetadata`) and normalized genre taxonomy
//! (`FILTER_SELECTION_RESEARCH.md` §§1, 5; `docs/research/07-ultimate-mode.md` §5.5).
//!
//! Strictly per-track (`OQ-40`): metadata belongs only to its own track and never forms
//! cross-track or folder-level priors.

use std::fmt;

/// Normalized genre bucket for `FILTER_SELECTION_RESEARCH.md` Rule G (`GenreAware`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum GenreClass {
    /// No recognizable genre tags present.
    #[default]
    Unknown,
    /// Conflicting multi-genre tags across incompatible buckets -> falls back cleanly to rate default.
    Ambiguous,
    /// Classical, orchestral, chamber, opera, baroque, choral.
    Classical,
    /// Jazz, blues, acoustic, folk, solo vocal/instrumental.
    JazzAcoustic,
    /// Rock, pop, electronic, metal, hip-hop, R&B, funk, dance, EDM.
    RockPopElectronic,
    /// Ambient, soundtrack, film score, new age.
    AmbientSoundtrack,
}

impl GenreClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ambiguous => "ambiguous",
            Self::Classical => "classical",
            Self::JazzAcoustic => "jazz/acoustic",
            Self::RockPopElectronic => "rock/pop/electronic",
            Self::AmbientSoundtrack => "ambient/soundtrack",
        }
    }
}

impl fmt::Display for GenreClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Optional explicit user phase override parsed from `AP_PHASE` tag (`min` | `ip` | `lin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhaseTagOverride {
    Linear,
    Minimum,
    Intermediate,
}

impl PhaseTagOverride {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "lin" | "linear" | "lp" => Some(Self::Linear),
            "min" | "minimum" | "mp" => Some(Self::Minimum),
            "ip" | "intermediate" | "inter" | "asym" => Some(Self::Intermediate),
            _ => None,
        }
    }
}

/// Strict per-track metadata extracted from FLAC `VORBIS_COMMENT` and `CUESHEET` blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMetadata {
    /// Cleaned raw genre strings (split on `;`, repeatable `GENRE` tags preserved).
    pub genres: Vec<String>,
    /// Normalized genre bucket for Rule G (`Unknown` / `Ambiguous` when unclear).
    pub genre_class: GenreClass,
    /// Earliest plausible 4-digit release year (`ORIGINALDATE`/`ORIGINALYEAR` preferred over `DATE`/`YEAR`).
    pub original_year: Option<u16>,
    /// Normalized uppercase `MEDIA` tag (e.g. `"CD"`, `"SACD"`, `"WEB"`, `"VINYL"`).
    pub media: Option<String>,
    /// Normalized lowercase `RELEASETYPE` tag (e.g. `"album"`, `"live"`, `"remaster"`).
    pub release_type: Option<String>,
    /// True when `CONDUCTOR` or `WORK` tags are present (classical signal).
    pub has_classical_credits: bool,
    /// Parsed `ORIGINALSAMPLERATE` in Hz (e.g. MQA or studio provenance tag).
    pub original_sample_rate_hz: Option<u32>,
    /// Raw `MQAENCODER` tag when present (Rule M warning).
    pub mqa_encoder: Option<String>,
    /// Raw `ENCODER` tag when present.
    pub encoder: Option<String>,
    /// True when tags hint at DSD/SACD provenance (`MEDIA=SACD`, `DSD` in encoder/comment).
    pub is_dsd_hint: bool,
    /// True when tags hint at a lossy encoder origin (`LAME`, `MP3`, `AAC`, `OPUS`, `VORBIS`).
    pub is_lossy_hint: bool,
    /// Explicit user `AP_FILTER` Vorbis tag override.
    pub ap_filter: Option<String>,
    /// Explicit user `AP_PHASE` Vorbis tag override (`min` / `ip` / `lin`).
    pub ap_phase: Option<PhaseTagOverride>,
    /// Explicit user `AP_DEEMPHASIS` Vorbis tag override (`Some(true/false)`).
    pub ap_deemphasis: Option<bool>,
    /// Explicit user `AP_APODIZING` Vorbis tag override (`Some(true/false)`).
    pub ap_apodizing: Option<bool>,
    /// True when FLAC `CUESHEET` audio track 1 sets the CD-DA Q-channel pre-emphasis bit (Rule P).
    pub cuesheet_pre_emphasis: bool,
    /// True when an album-image FLAC `CUESHEET` has mixed pre-emphasis flags across audio tracks (`OQ-41`).
    pub cuesheet_mixed_emphasis: bool,
    /// Number of non-lead-out audio tracks declared in `CUESHEET` (`0` if no CUESHEET block).
    pub cuesheet_audio_tracks: u16,
}

impl TrackMetadata {
    /// True when Rule E (early-digital CD era `1982..=1995`) lowers Detector A's threshold.
    pub fn is_early_digital_era(&self) -> bool {
        self.original_year.is_some_and(|y| (1982..=1995).contains(&y))
    }

    /// True when tags indicate a 1982–1989 CD era (used for report-only pre-emphasis shelf suggestion).
    pub fn is_pre_emphasis_era_cd(&self) -> bool {
        let era = self.original_year.is_some_and(|y| (1982..=1989).contains(&y));
        let is_cd = self
            .media
            .as_deref()
            .is_some_and(|m| m.contains("CD") && !m.contains("SACD"));
        era && is_cd
    }

    /// Effective de-emphasis flag from facts only (`AP_DEEMPHASIS` beats `CUESHEET` track 1).
    /// Analysers never turn de-emphasis on automatically (`07-ultimate-mode.md` §5.3 row 8).
    pub fn effective_deemphasis(&self) -> bool {
        self.ap_deemphasis.unwrap_or(self.cuesheet_pre_emphasis)
    }
}

/// Classify a single cleaned genre token into a [`GenreClass`] (`FILTER_SELECTION_RESEARCH.md` §5).
pub fn classify_genre_token(token: &str) -> GenreClass {
    let g = token.trim().to_ascii_lowercase();
    if g.is_empty() {
        return GenreClass::Unknown;
    }

    const CLASSICAL: &[&str] = &[
        "classical",
        "orchestral",
        "symphony",
        "symphonic",
        "chamber",
        "opera",
        "baroque",
        "choral",
        "concerto",
        "sonata",
        "romantic",
        "modern classical",
        "contemporary classical",
        "early music",
        "renaissance",
    ];
    const JAZZ_ACOUSTIC: &[&str] = &[
        "jazz",
        "blues",
        "acoustic",
        "folk",
        "bluegrass",
        "bossa",
        "swing",
        "bebop",
        "fusion",
        "singer-songwriter",
        "singer/songwriter",
        "americana",
        "chamber jazz",
        "vocal jazz",
        "flamenco",
        "country",
    ];
    const ROCK_POP_ELEC: &[&str] = &[
        "rock",
        "pop",
        "electronic",
        "electronica",
        "edm",
        "techno",
        "house",
        "trance",
        "dubstep",
        "drum and bass",
        "dnb",
        "synth",
        "metal",
        "punk",
        "indie",
        "alternative",
        "hip-hop",
        "hip hop",
        "rap",
        "r&b",
        "rnb",
        "soul",
        "funk",
        "disco",
        "dance",
        "reggae",
        "grunge",
        "prog",
        "progressive",
        "shoegaze",
        "industrial",
        "hardcore",
    ];
    const AMBIENT_OST: &[&str] = &[
        "ambient",
        "soundtrack",
        "film score",
        "score",
        "ost",
        "new age",
        "drone",
        "minimal",
        "downtempo",
        "chillout",
    ];

    if CLASSICAL.iter().any(|k| g.contains(k)) {
        GenreClass::Classical
    } else if JAZZ_ACOUSTIC.iter().any(|k| g.contains(k)) {
        GenreClass::JazzAcoustic
    } else if ROCK_POP_ELEC.iter().any(|k| g.contains(k)) {
        GenreClass::RockPopElectronic
    } else if AMBIENT_OST.iter().any(|k| g.contains(k)) {
        GenreClass::AmbientSoundtrack
    } else {
        GenreClass::Unknown
    }
}

/// Normalize a list of genre strings + classical credit hint into one [`GenreClass`].
/// Disagreement between distinct non-unknown classes resolves cleanly to [`GenreClass::Ambiguous`].
pub fn resolve_genre_class(genres: &[String], has_classical_credits: bool) -> GenreClass {
    let mut found: Option<GenreClass> = None;
    for g in genres {
        let c = classify_genre_token(g);
        if c == GenreClass::Unknown {
            continue;
        }
        match found {
            None => found = Some(c),
            Some(prev) if prev == c => {}
            Some(_) => return GenreClass::Ambiguous,
        }
    }
    match found {
        Some(c) => c,
        None if has_classical_credits => GenreClass::Classical,
        None => GenreClass::Unknown,
    }
}

/// Parse a strict 4-digit year in `1900..=2100` from a date string like `"1987"`, `"1987-05-14"`,
/// or `"1987/05"`.
pub fn parse_year_tag(raw: &str) -> Option<u16> {
    let s = raw.trim();
    if s.len() < 4 {
        return None;
    }
    let prefix = &s[..4];
    if !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year = prefix.parse::<u16>().ok()?;
    if (1900..=2100).contains(&year) {
        Some(year)
    } else {
        None
    }
}

/// Parse a boolean `AP_*` tag (`1`/`true`/`yes`/`on` vs `0`/`false`/`no`/`off`).
pub fn parse_bool_tag(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Parse `ORIGINALSAMPLERATE` (e.g. `"44100"`, `"44100 Hz"`, `"44.1 kHz"`, `"96k"`).
pub fn parse_sample_rate_tag_hz(raw: &str) -> Option<u32> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Some(khz_str) = s
        .strip_suffix("khz")
        .or_else(|| s.strip_suffix('k'))
    {
        let v = khz_str.trim().parse::<f64>().ok()?;
        if v.is_finite() && (8.0..=768.0).contains(&v) {
            return Some((v * 1_000.0).round() as u32);
        }
        return None;
    }
    let hz_str = s.strip_suffix("hz").unwrap_or(&s).trim();
    let v = hz_str.parse::<f64>().ok()?;
    if v.is_finite() && (8_000.0..=768_000.0).contains(&v) {
        Some(v.round() as u32)
    } else {
        None
    }
}

/// Strictly parse extended Vorbis comment fields (`NAME=value`) into [`TrackMetadata`].
pub fn parse_track_metadata_fields(fields: &[String]) -> TrackMetadata {
    let mut meta = TrackMetadata::default();
    let mut orig_year: Option<u16> = None;
    let mut date_year: Option<u16> = None;

    let check_dsd_lossy = |val: &str, is_dsd: &mut bool, is_lossy: &mut bool| {
        let up = val.to_ascii_uppercase();
        if up.contains("SACD") || up.contains("DSD") || up.contains("DXD") {
            *is_dsd = true;
        }
        if up.contains("LAME")
            || up.contains("MP3")
            || up.contains("AAC")
            || up.contains("FDK")
            || up.contains("OPUS")
            || up.contains("VORBIS")
            || up.contains("FRAUNHOFER")
        {
            *is_lossy = true;
        }
    };

    for field in fields {
        let Some((k, v)) = field.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_uppercase();
        let val = v.trim();
        if val.is_empty() {
            continue;
        }
        match key.as_str() {
            "GENRE" => {
                for part in val.split(';') {
                    let trimmed = part.trim();
                    if !trimmed.is_empty() {
                        meta.genres.push(trimmed.to_string());
                    }
                }
            }
            "ORIGINALDATE" | "ORIGINALYEAR" => {
                if orig_year.is_none() {
                    orig_year = parse_year_tag(val);
                }
            }
            "DATE" | "YEAR" => {
                if date_year.is_none() {
                    date_year = parse_year_tag(val);
                }
            }
            "MEDIA" | "SOURCEMEDIA" => {
                let up = val.to_ascii_uppercase();
                check_dsd_lossy(&up, &mut meta.is_dsd_hint, &mut meta.is_lossy_hint);
                if meta.media.is_none() {
                    meta.media = Some(up);
                }
            }
            "RELEASETYPE" => {
                if meta.release_type.is_none() {
                    meta.release_type = Some(val.to_ascii_lowercase());
                }
            }
            "CONDUCTOR" | "WORK" => {
                meta.has_classical_credits = true;
            }
            "ORIGINALSAMPLERATE" => {
                if meta.original_sample_rate_hz.is_none() {
                    meta.original_sample_rate_hz = parse_sample_rate_tag_hz(val);
                }
            }
            "MQAENCODER" => {
                if meta.mqa_encoder.is_none() {
                    meta.mqa_encoder = Some(val.to_string());
                }
            }
            "ENCODER" => {
                check_dsd_lossy(val, &mut meta.is_dsd_hint, &mut meta.is_lossy_hint);
                if meta.encoder.is_none() {
                    meta.encoder = Some(val.to_string());
                }
            }
            "COMMENT" | "DESCRIPTION" => {
                check_dsd_lossy(val, &mut meta.is_dsd_hint, &mut meta.is_lossy_hint);
            }
            "AP_FILTER" => {
                meta.ap_filter = Some(val.to_ascii_lowercase());
            }
            "AP_PHASE" => {
                meta.ap_phase = PhaseTagOverride::parse(val);
            }
            "AP_DEEMPHASIS" => {
                meta.ap_deemphasis = parse_bool_tag(val);
            }
            "AP_APODIZING" => {
                meta.ap_apodizing = parse_bool_tag(val);
            }
            _ => {}
        }
    }

    meta.original_year = orig_year.or(date_year);
    meta.genre_class = resolve_genre_class(&meta.genres, meta.has_classical_credits);
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_genres_dates_mqa_dsd_and_ap_overrides_strictly() {
        let fields = vec![
            "genre=Jazz; Acoustic".to_string(),
            "ORIGINALDATE=1985-03-11".to_string(),
            "DATE=2018".to_string(),
            "MEDIA=CD".to_string(),
            "MQAENCODER=MQAEncode v1.1".to_string(),
            "ORIGINALSAMPLERATE=44100".to_string(),
            "AP_FILTER=sinc-steep-4m".to_string(),
            "AP_PHASE=min".to_string(),
            "AP_DEEMPHASIS=1".to_string(),
            "AP_APODIZING=false".to_string(),
        ];
        let meta = parse_track_metadata_fields(&fields);
        assert_eq!(meta.genre_class, GenreClass::JazzAcoustic);
        assert_eq!(meta.original_year, Some(1985));
        assert!(meta.is_early_digital_era());
        assert!(meta.is_pre_emphasis_era_cd());
        assert_eq!(meta.original_sample_rate_hz, Some(44_100));
        assert_eq!(meta.mqa_encoder.as_deref(), Some("MQAEncode v1.1"));
        assert_eq!(meta.ap_filter.as_deref(), Some("sinc-steep-4m"));
        assert_eq!(meta.ap_phase, Some(PhaseTagOverride::Minimum));
        assert_eq!(meta.ap_deemphasis, Some(true));
        assert_eq!(meta.ap_apodizing, Some(false));
        assert!(meta.effective_deemphasis());
    }

    #[test]
    fn conflicting_genres_resolve_to_ambiguous_and_junk_is_ignored() {
        let fields = vec![
            "GENRE=Classical".to_string(),
            "GENRE=Electronic".to_string(),
            "ORIGINALDATE=bogus".to_string(),
            "ORIGINALSAMPLERATE=99999999".to_string(),
            "AP_DEEMPHASIS=maybe".to_string(),
        ];
        let meta = parse_track_metadata_fields(&fields);
        assert_eq!(meta.genre_class, GenreClass::Ambiguous);
        assert_eq!(meta.original_year, None);
        assert_eq!(meta.original_sample_rate_hz, None);
        assert_eq!(meta.ap_deemphasis, None);
    }
}
