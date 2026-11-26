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
///
/// Uses word-boundary tokenization, exact full-string overrides, multi-word phrases, and
/// head-noun resolution (last matching word wins) rather than unordered substring matching
/// (Review #7 F13).
pub fn classify_genre_token(token: &str) -> GenreClass {
    let raw = token.trim().to_ascii_lowercase();
    if raw.is_empty() {
        return GenreClass::Unknown;
    }

    // Normalize common punctuation variants before word splitting.
    let normalized = raw
        .replace("o.s.t.", " ost ")
        .replace("o.s.t", " ost ")
        .replace("r & b", " r&b ")
        .replace("drum & bass", " drum and bass ")
        .replace("drum'n'bass", " drum and bass ")
        .replace("drum n bass", " drum and bass ");

    let words: Vec<&str> = normalized
        .split(|c: char| !c.is_alphanumeric() && c != '&')
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return GenreClass::Unknown;
    }
    let joined = words.join(" ");

    // 1. Exact full-string table for well-known compound genres and special cases.
    const EXACT: &[(&str, GenreClass)] = &[
        // Classical
        ("classical", GenreClass::Classical),
        ("modern classical", GenreClass::Classical),
        ("contemporary classical", GenreClass::Classical),
        ("early music", GenreClass::Classical),
        ("chamber music", GenreClass::Classical),
        // Jazz / Acoustic
        ("chamber jazz", GenreClass::JazzAcoustic),
        ("vocal jazz", GenreClass::JazzAcoustic),
        ("post bop", GenreClass::JazzAcoustic),
        ("hard bop", GenreClass::JazzAcoustic),
        ("singer songwriter", GenreClass::JazzAcoustic),
        ("bossa nova", GenreClass::JazzAcoustic),
        ("jazz fusion", GenreClass::JazzAcoustic),
        // Rock / Pop / Electronic (including compounds whose modifiers look classical/jazz)
        ("symphonic metal", GenreClass::RockPopElectronic),
        ("symphonic rock", GenreClass::RockPopElectronic),
        ("rock opera", GenreClass::RockPopElectronic),
        ("pop opera", GenreClass::RockPopElectronic),
        ("new romantic", GenreClass::RockPopElectronic),
        ("baroque pop", GenreClass::RockPopElectronic),
        ("chamber pop", GenreClass::RockPopElectronic),
        ("folk rock", GenreClass::RockPopElectronic),
        ("blues rock", GenreClass::RockPopElectronic),
        ("acoustic rock", GenreClass::RockPopElectronic),
        ("jazz rock", GenreClass::RockPopElectronic),
        ("post rock", GenreClass::RockPopElectronic),
        ("post punk", GenreClass::RockPopElectronic),
        ("hip hop", GenreClass::RockPopElectronic),
        ("drum and bass", GenreClass::RockPopElectronic),
        // Ambient / Soundtrack
        ("ost", GenreClass::AmbientSoundtrack),
        ("film score", GenreClass::AmbientSoundtrack),
        ("original soundtrack", GenreClass::AmbientSoundtrack),
        ("new age", GenreClass::AmbientSoundtrack),
        ("chill out", GenreClass::AmbientSoundtrack),
    ];
    // Review #8 C3: singularize tokens up front (guarding "ss" and denylist words like "blues",
    // "bass", "chaos") so EXACT and PHRASES match pluralized compounds ("New Romantics",
    // "Rock Operas", "Pop Operas", "Film Scores") before falling back to single head nouns.
    fn singular(w: &str) -> &str {
        const DENY: &[&str] = &["blues", "bass", "chaos"];
        if w.len() >= 5 && !w.ends_with("ss") && !DENY.contains(&w) {
            w.strip_suffix('s').unwrap_or(w)
        } else {
            w
        }
    }
    let sing_words: Vec<&str> = words.iter().map(|w| singular(w)).collect();
    let joined_sing = sing_words.join(" ");

    if let Some((_, class)) = EXACT
        .iter()
        .find(|(k, _)| *k == joined || *k == joined_sing)
    {
        return *class;
    }

    // 2. Multi-word phrase matches on contiguous word slices (rightmost phrase wins; matches both
    //    raw words and singularized words so plural compounds never fall through to head-noun traps).
    const PHRASES: &[(&[&str], GenreClass)] = &[
        (&["modern", "classical"], GenreClass::Classical),
        (&["contemporary", "classical"], GenreClass::Classical),
        (&["early", "music"], GenreClass::Classical),
        (&["chamber", "music"], GenreClass::Classical),
        (&["chamber", "jazz"], GenreClass::JazzAcoustic),
        (&["vocal", "jazz"], GenreClass::JazzAcoustic),
        (&["post", "bop"], GenreClass::JazzAcoustic),
        (&["hard", "bop"], GenreClass::JazzAcoustic),
        (&["singer", "songwriter"], GenreClass::JazzAcoustic),
        (&["bossa", "nova"], GenreClass::JazzAcoustic),
        (&["symphonic", "metal"], GenreClass::RockPopElectronic),
        (&["symphonic", "rock"], GenreClass::RockPopElectronic),
        (&["rock", "opera"], GenreClass::RockPopElectronic),
        (&["pop", "opera"], GenreClass::RockPopElectronic),
        (&["new", "romantic"], GenreClass::RockPopElectronic),
        (&["baroque", "pop"], GenreClass::RockPopElectronic),
        (&["chamber", "pop"], GenreClass::RockPopElectronic),
        (&["folk", "rock"], GenreClass::RockPopElectronic),
        (&["blues", "rock"], GenreClass::RockPopElectronic),
        (&["acoustic", "rock"], GenreClass::RockPopElectronic),
        (&["jazz", "rock"], GenreClass::RockPopElectronic),
        (&["hip", "hop"], GenreClass::RockPopElectronic),
        (&["drum", "and", "bass"], GenreClass::RockPopElectronic),
        (&["film", "score"], GenreClass::AmbientSoundtrack),
        (&["new", "age"], GenreClass::AmbientSoundtrack),
        (&["chill", "out"], GenreClass::AmbientSoundtrack),
    ];

    let mut phrase_hit: Option<(usize, GenreClass)> = None;
    for &(pat, class) in PHRASES {
        if pat.len() <= words.len() {
            for start in 0..=(words.len() - pat.len()) {
                if &words[start..start + pat.len()] == pat
                    || &sing_words[start..start + pat.len()] == pat
                {
                    let end = start + pat.len();
                    if phrase_hit.is_none_or(|(prev_end, _)| end >= prev_end) {
                        phrase_hit = Some((end, class));
                    }
                }
            }
        }
    }
    if let Some((_, class)) = phrase_hit {
        return class;
    }

    // 3. Single whole-word head-noun rule: last matching word wins ("symphonic metal" -> metal,
    //    "chamber jazz" -> jazz, "folk rock" -> rock). `"ost"` only matches as a whole word.
    //    Includes common plurals & compounds (Review #7 re-review R5).
    const CLASSICAL_WORDS: &[&str] = &[
        "classical",
        "neoclassical",
        "orchestral",
        "symphony",
        "symphonies",
        "symphonic",
        "chamber",
        "opera",
        "operas",
        "baroque",
        "choral",
        "concerto",
        "concertos",
        "sonata",
        "sonatas",
        "romantic",
        "renaissance",
    ];
    const JAZZ_WORDS: &[&str] = &[
        "jazz",
        "blues",
        "acoustic",
        "folk",
        "bluegrass",
        "bossa",
        "swing",
        "bebop",
        "bop",
        "fusion",
        "americana",
        "flamenco",
        "country",
    ];
    const ROCK_WORDS: &[&str] = &[
        "rock",
        "pop",
        "britpop",
        "electropop",
        "artpop",
        "krautrock",
        "electronic",
        "electronica",
        "folktronica",
        "edm",
        "techno",
        "house",
        "trance",
        "dubstep",
        "dnb",
        "breakbeat",
        "breakbeats",
        "synth",
        "synthpop",
        "synthwave",
        "darkwave",
        "chillwave",
        "vaporwave",
        "metal",
        "metalcore",
        "deathcore",
        "mathcore",
        "grindcore",
        "punk",
        "indie",
        "alternative",
        "rap",
        "r&b",
        "rnb",
        "soul",
        "funk",
        "disco",
        "dance",
        "dancehall",
        "reggae",
        "grunge",
        "prog",
        "progressive",
        "shoegaze",
        "industrial",
        "hardcore",
    ];
    const AMBIENT_WORDS: &[&str] = &[
        "ambient",
        "soundtrack",
        "soundtracks",
        "score",
        "scores",
        "soundscape",
        "soundscapes",
        "ost",
        "drone",
        "downtempo",
        "chillout",
    ];

    let lookup_exact = |w: &str| -> Option<GenreClass> {
        if ROCK_WORDS.contains(&w) {
            Some(GenreClass::RockPopElectronic)
        } else if JAZZ_WORDS.contains(&w) {
            Some(GenreClass::JazzAcoustic)
        } else if CLASSICAL_WORDS.contains(&w) {
            Some(GenreClass::Classical)
        } else if AMBIENT_WORDS.contains(&w) {
            Some(GenreClass::AmbientSoundtrack)
        } else {
            None
        }
    };

    let word_class = |w: &str| -> Option<GenreClass> {
        lookup_exact(w).or_else(|| {
            let s = singular(w);
            if s != w { lookup_exact(s) } else { None }
        })
    };

    for &w in words.iter().rev() {
        if let Some(c) = word_class(w) {
            return c;
        }
    }
    GenreClass::Unknown
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
/// or `"1987/05"`. Operates on raw bytes so multi-byte UTF-8 dates never panic (Review #7 F3).
pub fn parse_year_tag(raw: &str) -> Option<u16> {
    let b = raw.trim().as_bytes();
    let p = b.get(..4)?;
    if !p.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if b.get(4).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let year = p.iter().fold(0_u16, |acc, &d| acc * 10 + u16::from(d - b'0'));
    (1900..=2100).contains(&year).then_some(year)
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

    #[test]
    fn parse_year_tag_non_ascii_never_panics_and_rejects_5_digit_prefix() {
        for bad in ["199é", "平成10年", "é", "1é", "😀1987", "19875", "1899", "2101"] {
            assert_eq!(parse_year_tag(bad), None, "expected None for {bad:?}");
        }
        assert_eq!(parse_year_tag("1987"), Some(1987));
        assert_eq!(parse_year_tag("1987-05-14"), Some(1987));
        assert_eq!(parse_year_tag(" 1995/01 "), Some(1995));

        let fields = vec!["DATE=平成10年".to_string(), "GENRE=Jazz".to_string()];
        let meta = parse_track_metadata_fields(&fields);
        assert_eq!(meta.original_year, None);
        assert_eq!(meta.genre_class, GenreClass::JazzAcoustic);
    }

    #[test]
    fn genre_token_word_boundaries_and_head_noun_classification() {
        let cases = [
            ("post-bop", GenreClass::JazzAcoustic),
            ("chamber jazz", GenreClass::JazzAcoustic),
            ("symphonic metal", GenreClass::RockPopElectronic),
            ("symphonic rock", GenreClass::RockPopElectronic),
            ("rock opera", GenreClass::RockPopElectronic),
            ("pop opera", GenreClass::RockPopElectronic),
            ("new romantic", GenreClass::RockPopElectronic),
            ("baroque pop", GenreClass::RockPopElectronic),
            ("chamber pop", GenreClass::RockPopElectronic),
            ("folk rock", GenreClass::RockPopElectronic),
            ("blues rock", GenreClass::RockPopElectronic),
            ("acoustic rock", GenreClass::RockPopElectronic),
            ("jazz rock", GenreClass::RockPopElectronic),
            ("post-rock", GenreClass::RockPopElectronic),
            ("OST", GenreClass::AmbientSoundtrack),
            ("O.S.T.", GenreClass::AmbientSoundtrack),
            ("Film Score", GenreClass::AmbientSoundtrack),
            ("Classical", GenreClass::Classical),
            ("Symphony", GenreClass::Classical),
            // R5 plurals & compounds:
            ("Soundtracks", GenreClass::AmbientSoundtrack),
            ("Scores", GenreClass::AmbientSoundtrack),
            ("Operas", GenreClass::Classical),
            ("Concertos", GenreClass::Classical),
            ("neoclassical", GenreClass::Classical),
            ("metalcore", GenreClass::RockPopElectronic),
            ("britpop", GenreClass::RockPopElectronic),
            ("electropop", GenreClass::RockPopElectronic),
            ("dancehall", GenreClass::RockPopElectronic),
            ("folktronica", GenreClass::RockPopElectronic),
            ("ghost", GenreClass::Unknown),
            ("minimalism", GenreClass::Unknown),
            // Review #8 C3 plural compounds & denylist/over-match guards:
            ("New Romantics", GenreClass::RockPopElectronic),
            ("Rock Operas", GenreClass::RockPopElectronic),
            ("Pop Operas", GenreClass::RockPopElectronic),
            ("Film Scores", GenreClass::AmbientSoundtrack),
            ("Blues", GenreClass::JazzAcoustic),
            ("Ghosts", GenreClass::Unknown),
            ("Hosts", GenreClass::Unknown),
        ];
        for (tok, expected) in cases {
            assert_eq!(
                classify_genre_token(tok),
                expected,
                "genre token {tok:?} misclassified"
            );
        }
    }
}
