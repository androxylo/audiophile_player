//! UPnP Device Description XML, SCPD XMLs, bounded SOAP/DIDL-Lite parsing, and duration helpers.

/// Metadata extracted from a DIDL-Lite XML payload (`CurrentURIMetaData` / `NextURIMetaData`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DidlTrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub protocol_info: Option<String>,
    pub duration_s: Option<f64>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub bits_per_sample: Option<u16>,
}

/// Escape text safely for XML element content and double-quoted attributes.
pub fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Unescape standard XML entities (`&lt;`, `&gt;`, `&amp;`, `&quot;`, `&apos;`, `&#...;`, `&#x...;`).
pub fn xml_unescape(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(semi) = after.find(';') {
            let ent = &after[..semi];
            let decoded = match ent {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => ent[1..]
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32),
                _ => None,
            };
            if let Some(ch) = decoded {
                out.push(ch);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Extract the unescaped text content of the first XML element matching `local_name`
/// (case-sensitive local name, ignoring any XML namespace prefix like `u:` or `dc:`).
pub fn extract_xml_element_text(xml: &str, local_name: &str) -> Option<String> {
    let bytes = xml.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(rel_lt) = xml[i..].find('<') else {
            break;
        };
        let lt = i + rel_lt;
        let after_lt = lt + 1;
        if after_lt >= bytes.len() {
            break;
        }
        // Skip comments, processing instructions, CDATA, and closing tags.
        if bytes[after_lt] == b'/' || bytes[after_lt] == b'!' || bytes[after_lt] == b'?' {
            i = after_lt;
            continue;
        }
        let Some(rel_gt) = xml[after_lt..].find('>') else {
            break;
        };
        let gt = after_lt + rel_gt;
        let tag_inner = xml[after_lt..gt].trim();
        let self_closing = tag_inner.ends_with('/');
        let tag_head = tag_inner.trim_end_matches('/').trim();
        let full_name = tag_head
            .split(|c: char| c.is_ascii_whitespace())
            .next()
            .unwrap_or("");
        let found_local = full_name.rsplit(':').next().unwrap_or(full_name);

        if found_local == local_name {
            if self_closing {
                return Some(String::new());
            }
            let content_start = gt + 1;
            // Find matching closing tag `</...local_name>`.
            let mut search = content_start;
            while search < bytes.len() {
                let Some(rel_close) = xml[search..].find("</") else {
                    return None;
                };
                let close_pos = search + rel_close;
                let Some(rel_end) = xml[close_pos + 2..].find('>') else {
                    return None;
                };
                let close_name = xml[close_pos + 2..close_pos + 2 + rel_end].trim();
                let close_local = close_name.rsplit(':').next().unwrap_or(close_name);
                if close_local == local_name {
                    return Some(xml_unescape(&xml[content_start..close_pos]));
                }
                search = close_pos + 2 + rel_end + 1;
            }
            return None;
        }
        i = gt + 1;
    }
    None
}

/// Extract an attribute value (case-insensitive attribute name) from the first opening tag
/// whose local name matches `tag_local`.
pub fn extract_xml_tag_attribute(
    xml: &str,
    tag_local: &str,
    attr_name: &str,
) -> Option<String> {
    let bytes = xml.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(rel_lt) = xml[i..].find('<') else {
            break;
        };
        let lt = i + rel_lt;
        let after_lt = lt + 1;
        if after_lt >= bytes.len() {
            break;
        }
        if bytes[after_lt] == b'/' || bytes[after_lt] == b'!' || bytes[after_lt] == b'?' {
            i = after_lt;
            continue;
        }
        let Some(rel_gt) = xml[after_lt..].find('>') else {
            break;
        };
        let gt = after_lt + rel_gt;
        let tag_inner = xml[after_lt..gt].trim().trim_end_matches('/');
        let mut parts = tag_inner.splitn(2, |c: char| c.is_ascii_whitespace());
        let full_name = parts.next().unwrap_or("");
        let found_local = full_name.rsplit(':').next().unwrap_or(full_name);
        if found_local == tag_local {
            if let Some(attrs) = parts.next() {
                if let Some(v) = parse_attribute_from_slice(attrs, attr_name) {
                    return Some(xml_unescape(&v));
                }
            }
        }
        i = gt + 1;
    }
    None
}

fn parse_attribute_from_slice(attrs: &str, target_name: &str) -> Option<String> {
    let mut rest = attrs.trim();
    while !rest.is_empty() {
        let eq = rest.find('=')?;
        let raw_key = rest[..eq].trim();
        let key_local = raw_key.rsplit(':').next().unwrap_or(raw_key);
        let after_eq = rest[eq + 1..].trim_start();
        let quote = after_eq.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let val_start = 1;
        let rel_end = after_eq[val_start..].find(quote)?;
        let val = &after_eq[val_start..val_start + rel_end];
        if key_local.eq_ignore_ascii_case(target_name) {
            return Some(val.to_string());
        }
        rest = after_eq[val_start + rel_end + 1..].trim_start();
    }
    None
}

/// Parse common track metadata and audio-format hints from UPnP DIDL-Lite XML.
pub fn parse_didl_lite(didl_xml: &str) -> DidlTrackMetadata {
    if didl_xml.trim().is_empty() {
        return DidlTrackMetadata::default();
    }
    let title = extract_xml_element_text(didl_xml, "title")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let artist = extract_xml_element_text(didl_xml, "artist")
        .or_else(|| extract_xml_element_text(didl_xml, "creator"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let album = extract_xml_element_text(didl_xml, "album")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let protocol_info = extract_xml_tag_attribute(didl_xml, "res", "protocolInfo")
        .filter(|s| !s.is_empty());
    let duration_s = extract_xml_tag_attribute(didl_xml, "res", "duration")
        .and_then(|s| parse_upnp_duration(&s));
    let sample_rate_hz = extract_xml_tag_attribute(didl_xml, "res", "sampleFrequency")
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&r| r > 0);
    let channels = extract_xml_tag_attribute(didl_xml, "res", "nrAudioChannels")
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|&c| c > 0);
    let bits_per_sample = extract_xml_tag_attribute(didl_xml, "res", "bitsPerSample")
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|&b| b > 0);

    DidlTrackMetadata {
        title,
        artist,
        album,
        protocol_info,
        duration_s,
        sample_rate_hz,
        channels,
        bits_per_sample,
    }
}

/// Format seconds as UPnP `HH:MM:SS` (e.g. `00:03:45`).
pub fn format_upnp_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "00:00:00".to_string();
    }
    let total = seconds.floor() as u64;
    let hrs = (total / 3600).min(99);
    let mins = (total % 3600) / 60;
    let secs = total % 60;
    format!("{hrs:02}:{mins:02}:{secs:02}")
}

/// Parse UPnP duration (`H:MM:SS[.fff]` or `MM:SS`) into seconds.
pub fn parse_upnp_duration(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("NOT_IMPLEMENTED") {
        return None;
    }
    let parts: Vec<&str> = trimmed.split(':').collect();
    match parts.as_slice() {
        [h, m, s] => {
            let hh = h.parse::<f64>().ok()?;
            let mm = m.parse::<f64>().ok()?;
            let ss = s.parse::<f64>().ok()?;
            if hh >= 0.0 && mm >= 0.0 && ss >= 0.0 {
                Some(hh * 3600.0 + mm * 60.0 + ss)
            } else {
                None
            }
        }
        [m, s] => {
            let mm = m.parse::<f64>().ok()?;
            let ss = s.parse::<f64>().ok()?;
            if mm >= 0.0 && ss >= 0.0 {
                Some(mm * 60.0 + ss)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Generate a deterministic, RFC-4122-shaped UPnP UDN (`uuid:...`) for this host & friendly name
/// so foobar2000 remembers the selected UPnP output device across player restarts.
pub fn deterministic_udn(friendly_name: &str) -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "windows-host".to_string());
    let seed = format!("audiophile-player:upnp:renderer:v1:{host}:{friendly_name}");

    // Derive 16 deterministic bytes via two independent 64-bit FNV-1a passes.
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x8422_2325_cbf2_9ce4;
    for (idx, b) in seed.bytes().enumerate() {
        h1 ^= u64::from(b);
        h1 = h1.wrapping_mul(0x0000_0100_0000_01b3);
        h2 ^= u64::from(b).wrapping_add((idx as u64).wrapping_mul(31));
        h2 = h2.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&h1.to_be_bytes());
    bytes[8..].copy_from_slice(&h2.to_be_bytes());
    // Set UUID v5 + RFC 4122 variant bits.
    bytes[6] = (bytes[6] & 0x0F) | 0x50;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;

    format!(
        "uuid:{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// Render `/description.xml` for `urn:schemas-upnp-org:device:MediaRenderer:1`.
pub fn render_device_description_xml(udn: &str, friendly_name: &str, dac_name: &str) -> String {
    let fname = xml_escape(friendly_name);
    let model_desc = xml_escape(&format!(
        "Headless Windows Kernel Streaming (WDM-KS) Pure-f64 Audiophile Renderer — {dac_name}"
    ));
    let udn_esc = xml_escape(udn);
    let version = xml_escape(env!("CARGO_PKG_VERSION"));
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<root xmlns=\"urn:schemas-upnp-org:device-1-0\" xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">\n\
  <specVersion>\n\
    <major>1</major>\n\
    <minor>0</minor>\n\
  </specVersion>\n\
  <device>\n\
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>\n\
    <friendlyName>{fname}</friendlyName>\n\
    <manufacturer>Audiophile Player</manufacturer>\n\
    <manufacturerURL>https://github.com/androxylo/audiophile_player</manufacturerURL>\n\
    <modelDescription>{model_desc}</modelDescription>\n\
    <modelName>Audiophile Player</modelName>\n\
    <modelNumber>{version}</modelNumber>\n\
    <modelURL>https://github.com/androxylo/audiophile_player</modelURL>\n\
    <serialNumber>AP-WDMKS-01</serialNumber>\n\
    <UDN>{udn_esc}</UDN>\n\
    <dlna:X_DLNADOC xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">DMR-1.50</dlna:X_DLNADOC>\n\
    <serviceList>\n\
      <service>\n\
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>\n\
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>\n\
        <SCPDURL>/scpd/AVTransport.xml</SCPDURL>\n\
        <controlURL>/upnp/control/AVTransport</controlURL>\n\
        <eventSubURL>/upnp/event/AVTransport</eventSubURL>\n\
      </service>\n\
      <service>\n\
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>\n\
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>\n\
        <SCPDURL>/scpd/ConnectionManager.xml</SCPDURL>\n\
        <controlURL>/upnp/control/ConnectionManager</controlURL>\n\
        <eventSubURL>/upnp/event/ConnectionManager</eventSubURL>\n\
      </service>\n\
      <service>\n\
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>\n\
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>\n\
        <SCPDURL>/scpd/RenderingControl.xml</SCPDURL>\n\
        <controlURL>/upnp/control/RenderingControl</controlURL>\n\
        <eventSubURL>/upnp/event/RenderingControl</eventSubURL>\n\
      </service>\n\
    </serviceList>\n\
  </device>\n\
</root>\n"
    )
}

/// Compliant UPnP `AVTransport:1` SCPD XML (`/scpd/AVTransport.xml`).
pub const AVTRANSPORT_SCPD_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action>
      <name>SetAVTransportURI</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>CurrentURI</name><direction>in</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
        <argument><name>CurrentURIMetaData</name><direction>in</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>SetNextAVTransportURI</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>NextURI</name><direction>in</direction><relatedStateVariable>NextAVTransportURI</relatedStateVariable></argument>
        <argument><name>NextURIMetaData</name><direction>in</direction><relatedStateVariable>NextAVTransportURIMetaData</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetMediaInfo</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>NrTracks</name><direction>out</direction><relatedStateVariable>NumberOfTracks</relatedStateVariable></argument>
        <argument><name>MediaDuration</name><direction>out</direction><relatedStateVariable>CurrentMediaDuration</relatedStateVariable></argument>
        <argument><name>CurrentURI</name><direction>out</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
        <argument><name>CurrentURIMetaData</name><direction>out</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
        <argument><name>NextURI</name><direction>out</direction><relatedStateVariable>NextAVTransportURI</relatedStateVariable></argument>
        <argument><name>NextURIMetaData</name><direction>out</direction><relatedStateVariable>NextAVTransportURIMetaData</relatedStateVariable></argument>
        <argument><name>PlayMedium</name><direction>out</direction><relatedStateVariable>PlaybackStorageMedium</relatedStateVariable></argument>
        <argument><name>RecordMedium</name><direction>out</direction><relatedStateVariable>RecordStorageMedium</relatedStateVariable></argument>
        <argument><name>WriteStatus</name><direction>out</direction><relatedStateVariable>RecordMediumWriteStatus</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetTransportInfo</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>CurrentTransportState</name><direction>out</direction><relatedStateVariable>TransportState</relatedStateVariable></argument>
        <argument><name>CurrentTransportStatus</name><direction>out</direction><relatedStateVariable>TransportStatus</relatedStateVariable></argument>
        <argument><name>CurrentSpeed</name><direction>out</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetPositionInfo</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Track</name><direction>out</direction><relatedStateVariable>CurrentTrack</relatedStateVariable></argument>
        <argument><name>TrackDuration</name><direction>out</direction><relatedStateVariable>CurrentTrackDuration</relatedStateVariable></argument>
        <argument><name>TrackMetaData</name><direction>out</direction><relatedStateVariable>CurrentTrackMetaData</relatedStateVariable></argument>
        <argument><name>TrackURI</name><direction>out</direction><relatedStateVariable>CurrentTrackURI</relatedStateVariable></argument>
        <argument><name>RelTime</name><direction>out</direction><relatedStateVariable>RelativeTimePosition</relatedStateVariable></argument>
        <argument><name>AbsTime</name><direction>out</direction><relatedStateVariable>AbsoluteTimePosition</relatedStateVariable></argument>
        <argument><name>RelCount</name><direction>out</direction><relatedStateVariable>RelativeCounterPosition</relatedStateVariable></argument>
        <argument><name>AbsCount</name><direction>out</direction><relatedStateVariable>AbsoluteCounterPosition</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetDeviceCapabilities</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>PlayMedia</name><direction>out</direction><relatedStateVariable>PossiblePlaybackStorageMedia</relatedStateVariable></argument>
        <argument><name>RecMedia</name><direction>out</direction><relatedStateVariable>PossibleRecordStorageMedia</relatedStateVariable></argument>
        <argument><name>RecQualityModes</name><direction>out</direction><relatedStateVariable>PossibleRecordQualityModes</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetTransportSettings</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>PlayMode</name><direction>out</direction><relatedStateVariable>CurrentPlayMode</relatedStateVariable></argument>
        <argument><name>RecQualityMode</name><direction>out</direction><relatedStateVariable>CurrentRecordQualityMode</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>Stop</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>Play</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Speed</name><direction>in</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>Pause</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>Seek</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Unit</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekMode</relatedStateVariable></argument>
        <argument><name>Target</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekTarget</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>Next</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>Previous</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetCurrentTransportActions</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Actions</name><direction>out</direction><relatedStateVariable>CurrentTransportActions</relatedStateVariable></argument>
      </argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="yes"><name>LastChange</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>TransportState</name><dataType>string</dataType><allowedValueList><allowedValue>STOPPED</allowedValue><allowedValue>PLAYING</allowedValue><allowedValue>TRANSITIONING</allowedValue><allowedValue>PAUSED_PLAYBACK</allowedValue><allowedValue>NO_MEDIA_PRESENT</allowedValue></allowedValueList></stateVariable>
    <stateVariable sendEvents="no"><name>TransportStatus</name><dataType>string</dataType><allowedValueList><allowedValue>OK</allowedValue><allowedValue>ERROR_OCCURRED</allowedValue></allowedValueList></stateVariable>
    <stateVariable sendEvents="no"><name>PlaybackStorageMedium</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>RecordStorageMedium</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>PossiblePlaybackStorageMedia</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>PossibleRecordStorageMedia</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentPlayMode</name><dataType>string</dataType><defaultValue>NORMAL</defaultValue></stateVariable>
    <stateVariable sendEvents="no"><name>TransportPlaySpeed</name><dataType>string</dataType><allowedValueList><allowedValue>1</allowedValue></allowedValueList></stateVariable>
    <stateVariable sendEvents="no"><name>RecordMediumWriteStatus</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentRecordQualityMode</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>PossibleRecordQualityModes</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>NumberOfTracks</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrack</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrackDuration</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentMediaDuration</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrackMetaData</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTrackURI</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AVTransportURI</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AVTransportURIMetaData</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>NextAVTransportURI</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>NextAVTransportURIMetaData</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>RelativeTimePosition</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AbsoluteTimePosition</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>RelativeCounterPosition</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>AbsoluteCounterPosition</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>CurrentTransportActions</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekMode</name><dataType>string</dataType><allowedValueList><allowedValue>REL_TIME</allowedValue></allowedValueList></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekTarget</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>ui4</dataType></stateVariable>
  </serviceStateTable>
</scpd>
"#;

/// Compliant UPnP `ConnectionManager:1` SCPD XML (`/scpd/ConnectionManager.xml`).
pub const CONNECTION_MANAGER_SCPD_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action>
      <name>GetProtocolInfo</name>
      <argumentList>
        <argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>
        <argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetCurrentConnectionIDs</name>
      <argumentList>
        <argument><name>ConnectionIDs</name><direction>out</direction><relatedStateVariable>CurrentConnectionIDs</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetCurrentConnectionInfo</name>
      <argumentList>
        <argument><name>ConnectionID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
        <argument><name>RcsID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_RcsID</relatedStateVariable></argument>
        <argument><name>AVTransportID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_AVTransportID</relatedStateVariable></argument>
        <argument><name>ProtocolInfo</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ProtocolInfo</relatedStateVariable></argument>
        <argument><name>PeerConnectionManager</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionManager</relatedStateVariable></argument>
        <argument><name>PeerConnectionID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
        <argument><name>Direction</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Direction</relatedStateVariable></argument>
        <argument><name>Status</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionStatus</relatedStateVariable></argument>
      </argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="yes"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="yes"><name>CurrentConnectionIDs</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionStatus</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionManager</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Direction</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ProtocolInfo</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_AVTransportID</name><dataType>i4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_RcsID</name><dataType>i4</dataType></stateVariable>
  </serviceStateTable>
</scpd>
"#;

/// Compliant UPnP `RenderingControl:1` SCPD XML (`/scpd/RenderingControl.xml`).
pub const RENDERING_CONTROL_SCPD_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <actionList>
    <action>
      <name>ListPresets</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>CurrentPresetNameList</name><direction>out</direction><relatedStateVariable>PresetNameList</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>SelectPreset</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>PresetName</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_PresetName</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetMute</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
        <argument><name>CurrentMute</name><direction>out</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>SetMute</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
        <argument><name>DesiredMute</name><direction>in</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetVolume</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
        <argument><name>CurrentVolume</name><direction>out</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>SetVolume</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
        <argument><name>DesiredVolume</name><direction>in</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetVolumeDB</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
        <argument><name>CurrentVolume</name><direction>out</direction><relatedStateVariable>VolumeDB</relatedStateVariable></argument>
      </argumentList>
    </action>
    <action>
      <name>GetVolumeDBRange</name>
      <argumentList>
        <argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
        <argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
        <argument><name>MinValue</name><direction>out</direction><relatedStateVariable>VolumeDB</relatedStateVariable></argument>
        <argument><name>MaxValue</name><direction>out</direction><relatedStateVariable>VolumeDB</relatedStateVariable></argument>
      </argumentList>
    </action>
  </actionList>
  <serviceStateTable>
    <stateVariable sendEvents="yes"><name>LastChange</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>PresetNameList</name><dataType>string</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>Mute</name><dataType>boolean</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>Volume</name><dataType>ui2</dataType><allowedValueRange><minimum>0</minimum><maximum>100</maximum><step>1</step></allowedValueRange></stateVariable>
    <stateVariable sendEvents="no"><name>VolumeDB</name><dataType>i2</dataType><allowedValueRange><minimum>0</minimum><maximum>0</maximum></allowedValueRange></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_Channel</name><dataType>string</dataType><allowedValueList><allowedValue>Master</allowedValue></allowedValueList></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>ui4</dataType></stateVariable>
    <stateVariable sendEvents="no"><name>A_ARG_TYPE_PresetName</name><dataType>string</dataType></stateVariable>
  </serviceStateTable>
</scpd>
"#;
