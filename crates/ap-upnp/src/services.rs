//! UPnP AV 1.0 SOAP service dispatchers:
//! - `AVTransport:1`
//! - `ConnectionManager:1` (lossless-only sink MIME types — zero lossy transcoding)
//! - `RenderingControl:1` (fixed volume 100 / unmuted — zero digital volume control)

use ap_source::http_stream::ParsedHttpUrl;

use crate::state::UpnpRendererState;
use crate::xml::{extract_xml_element_text, format_upnp_duration, xml_escape};

pub const AVTRANSPORT_SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const CONNECTION_MANAGER_SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
pub const RENDERING_CONTROL_SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

/// Lossless-only `ConnectionManager::GetProtocolInfo` Sink list.
///
/// Deliberately excludes all lossy formats (MP3, AAC, WMA, Vorbis, Opus) so foobar2000 and
/// BubbleUPnP never transcode lossless audio down, while advertising native FLAC, Ogg FLAC,
/// WAV, and L16/L24 LPCM across all rates (rate-following rule, Issue #8 §1 & §2.2).
pub const SINK_PROTOCOL_INFO: &str = "\
http-get:*:audio/flac:*,\
http-get:*:audio/x-flac:*,\
http-get:*:audio/ogg:*,\
http-get:*:application/ogg:*,\
http-get:*:audio/x-ogg:*,\
http-get:*:audio/wav:*,\
http-get:*:audio/x-wav:*,\
http-get:*:audio/wave:*,\
http-get:*:audio/L16:*,\
http-get:*:audio/L16;rate=44100;channels=2:*,\
http-get:*:audio/L16;rate=48000;channels=2:*,\
http-get:*:audio/L16;rate=88200;channels=2:*,\
http-get:*:audio/L16;rate=96000;channels=2:*,\
http-get:*:audio/L16;rate=176400;channels=2:*,\
http-get:*:audio/L16;rate=192000;channels=2:*,\
http-get:*:audio/L24:*,\
http-get:*:audio/L24;rate=44100;channels=2:*,\
http-get:*:audio/L24;rate=48000;channels=2:*,\
http-get:*:audio/L24;rate=88200;channels=2:*,\
http-get:*:audio/L24;rate=96000;channels=2:*,\
http-get:*:audio/L24;rate=176400;channels=2:*,\
http-get:*:audio/L24;rate=192000;channels=2:*,\
http-get:*:audio/L24;rate=352800;channels=2:*,\
http-get:*:audio/L24;rate=384000;channels=2:*";

/// Result of handling a UPnP SOAP action request.
#[derive(Debug, Clone)]
pub struct SoapResponse {
    pub status: &'static str,
    pub xml: String,
}

impl SoapResponse {
    pub fn ok(service_type: &str, action: &str, inner_fields: &str) -> Self {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\n\
             <s:Body>\n\
             <u:{action}Response xmlns:u=\"{service_type}\">{inner_fields}</u:{action}Response>\n\
             </s:Body>\n\
             </s:Envelope>\n"
        );
        Self {
            status: "200 OK",
            xml,
        }
    }

    pub fn fault(code: u16, description: &str) -> Self {
        let desc = xml_escape(description);
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\n\
             <s:Body>\n\
             <s:Fault>\n\
             <faultcode>s:Client</faultcode>\n\
             <faultstring>UPnPError</faultstring>\n\
             <detail>\n\
             <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\n\
             <errorCode>{code}</errorCode>\n\
             <errorDescription>{desc}</errorDescription>\n\
             </UPnPError>\n\
             </detail>\n\
             </s:Fault>\n\
             </s:Body>\n\
             </s:Envelope>\n"
        );
        Self {
            status: "500 Internal Server Error",
            xml,
        }
    }
}

/// Extract SOAP action name from the `SOAPACTION` HTTP header (e.g. `"urn:...#Play"`).
pub fn parse_soap_action_name(soap_action_header: Option<&str>) -> Option<String> {
    let raw = soap_action_header?.trim().trim_matches('"').trim_matches('\'');
    let (_, action) = raw.rsplit_once('#')?;
    let action = action.trim();
    if action.is_empty() {
        None
    } else {
        Some(action.to_string())
    }
}

/// Render the initial GENA `e:propertyset` XML for `ConnectionManager:1`.
pub fn render_connection_manager_event_xml() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
         <e:property><SourceProtocolInfo></SourceProtocolInfo></e:property>\
         <e:property><SinkProtocolInfo>{}</SinkProtocolInfo></e:property>\
         <e:property><CurrentConnectionIDs>0</CurrentConnectionIDs></e:property>\
         </e:propertyset>\n",
        xml_escape(SINK_PROTOCOL_INFO)
    )
}

/// Dispatch an `AVTransport:1` SOAP action.
pub fn handle_av_transport(
    action: &str,
    body_xml: &str,
    state: &UpnpRendererState,
) -> SoapResponse {
    match action {
        "SetAVTransportURI" => {
            let uri = extract_xml_element_text(body_xml, "CurrentURI").unwrap_or_default();
            let meta = extract_xml_element_text(body_xml, "CurrentURIMetaData").unwrap_or_default();
            if !uri.trim().is_empty() {
                if let Err(_reason) = ParsedHttpUrl::parse(&uri) {
                    return SoapResponse::fault(714, "Illegal MIME-type or unsupported URI scheme");
                }
            }
            state.set_av_transport_uri(uri, meta);
            SoapResponse::ok(AVTRANSPORT_SERVICE_TYPE, "SetAVTransportURI", "")
        }
        "SetNextAVTransportURI" => {
            let uri = extract_xml_element_text(body_xml, "NextURI").unwrap_or_default();
            let meta = extract_xml_element_text(body_xml, "NextURIMetaData").unwrap_or_default();
            if !uri.trim().is_empty() {
                if let Err(_reason) = ParsedHttpUrl::parse(&uri) {
                    return SoapResponse::fault(714, "Illegal MIME-type or unsupported URI scheme");
                }
            }
            state.set_next_av_transport_uri(uri, meta);
            SoapResponse::ok(AVTRANSPORT_SERVICE_TYPE, "SetNextAVTransportURI", "")
        }
        "Play" => match state.play() {
            Ok(()) => SoapResponse::ok(AVTRANSPORT_SERVICE_TYPE, "Play", ""),
            Err((code, desc)) => SoapResponse::fault(code, desc),
        },
        "Stop" => {
            state.stop();
            SoapResponse::ok(AVTRANSPORT_SERVICE_TYPE, "Stop", "")
        }
        "Pause" => {
            state.pause();
            SoapResponse::ok(AVTRANSPORT_SERVICE_TYPE, "Pause", "")
        }
        "Next" => match state.next_track() {
            Ok(()) => SoapResponse::ok(AVTRANSPORT_SERVICE_TYPE, "Next", ""),
            Err((code, desc)) => SoapResponse::fault(code, desc),
        },
        "Previous" => SoapResponse::fault(701, "Transition not available"),
        "Seek" => {
            let unit = extract_xml_element_text(body_xml, "Unit").unwrap_or_default();
            let target = extract_xml_element_text(body_xml, "Target").unwrap_or_default();
            match state.seek(&unit, &target) {
                Ok(()) => SoapResponse::ok(AVTRANSPORT_SERVICE_TYPE, "Seek", ""),
                Err((code, desc)) => SoapResponse::fault(code, desc),
            }
        }
        "GetTransportInfo" => {
            let snap = state.snapshot();
            let fields = format!(
                "<CurrentTransportState>{}</CurrentTransportState>\
                 <CurrentTransportStatus>{}</CurrentTransportStatus>\
                 <CurrentSpeed>1</CurrentSpeed>",
                snap.transport_state.as_str(),
                xml_escape(&snap.transport_status),
            );
            SoapResponse::ok(AVTRANSPORT_SERVICE_TYPE, "GetTransportInfo", &fields)
        }
        "GetPositionInfo" => {
            let snap = state.snapshot();
            let track = if snap.current.is_some() { 1 } else { 0 };
            let dur = snap
                .duration_s
                .map(format_upnp_duration)
                .unwrap_or_else(|| "00:00:00".to_string());
            let rel = format_upnp_duration(snap.elapsed_s);
            let uri = snap.current.as_ref().map(|c| c.uri.as_str()).unwrap_or("");
            let meta = snap
                .current
                .as_ref()
                .map(|c| c.raw_metadata.as_str())
                .unwrap_or("");
            let fields = format!(
                "<Track>{track}</Track>\
                 <TrackDuration>{dur}</TrackDuration>\
                 <TrackMetaData>{}</TrackMetaData>\
                 <TrackURI>{}</TrackURI>\
                 <RelTime>{rel}</RelTime>\
                 <AbsTime>{rel}</AbsTime>\
                 <RelCount>2147483647</RelCount>\
                 <AbsCount>2147483647</AbsCount>",
                xml_escape(meta),
                xml_escape(uri),
            );
            SoapResponse::ok(AVTRANSPORT_SERVICE_TYPE, "GetPositionInfo", &fields)
        }
        "GetMediaInfo" => {
            let snap = state.snapshot();
            let nr_tracks = if snap.current.is_some() { 1 } else { 0 };
            let dur = snap
                .duration_s
                .map(format_upnp_duration)
                .unwrap_or_else(|| "00:00:00".to_string());
            let cur_uri = snap.current.as_ref().map(|c| c.uri.as_str()).unwrap_or("");
            let cur_meta = snap
                .current
                .as_ref()
                .map(|c| c.raw_metadata.as_str())
                .unwrap_or("");
            let next_uri = snap.next.as_ref().map(|n| n.uri.as_str()).unwrap_or("");
            let next_meta = snap
                .next
                .as_ref()
                .map(|n| n.raw_metadata.as_str())
                .unwrap_or("");
            let fields = format!(
                "<NrTracks>{nr_tracks}</NrTracks>\
                 <MediaDuration>{dur}</MediaDuration>\
                 <CurrentURI>{}</CurrentURI>\
                 <CurrentURIMetaData>{}</CurrentURIMetaData>\
                 <NextURI>{}</NextURI>\
                 <NextURIMetaData>{}</NextURIMetaData>\
                 <PlayMedium>NETWORK</PlayMedium>\
                 <RecordMedium>NOT_IMPLEMENTED</RecordMedium>\
                 <WriteStatus>NOT_IMPLEMENTED</WriteStatus>",
                xml_escape(cur_uri),
                xml_escape(cur_meta),
                xml_escape(next_uri),
                xml_escape(next_meta),
            );
            SoapResponse::ok(AVTRANSPORT_SERVICE_TYPE, "GetMediaInfo", &fields)
        }
        "GetDeviceCapabilities" => SoapResponse::ok(
            AVTRANSPORT_SERVICE_TYPE,
            "GetDeviceCapabilities",
            "<PlayMedia>NETWORK</PlayMedia>\
             <RecMedia>NOT_IMPLEMENTED</RecMedia>\
             <RecQualityModes>NOT_IMPLEMENTED</RecQualityModes>",
        ),
        "GetTransportSettings" => SoapResponse::ok(
            AVTRANSPORT_SERVICE_TYPE,
            "GetTransportSettings",
            "<PlayMode>NORMAL</PlayMode><RecQualityMode>NOT_IMPLEMENTED</RecQualityMode>",
        ),
        "GetCurrentTransportActions" => {
            let snap = state.snapshot();
            let actions = if snap.current.is_some() {
                "Play,Stop,Pause,Seek"
            } else {
                "Stop"
            };
            SoapResponse::ok(
                AVTRANSPORT_SERVICE_TYPE,
                "GetCurrentTransportActions",
                &format!("<Actions>{actions}</Actions>"),
            )
        }
        _ => SoapResponse::fault(401, "Invalid Action"),
    }
}

/// Dispatch a `ConnectionManager:1` SOAP action.
pub fn handle_connection_manager(action: &str, state: &UpnpRendererState) -> SoapResponse {
    match action {
        "GetProtocolInfo" => {
            let fields = format!(
                "<Source></Source><Sink>{}</Sink>",
                xml_escape(SINK_PROTOCOL_INFO)
            );
            SoapResponse::ok(CONNECTION_MANAGER_SERVICE_TYPE, "GetProtocolInfo", &fields)
        }
        "GetCurrentConnectionIDs" => SoapResponse::ok(
            CONNECTION_MANAGER_SERVICE_TYPE,
            "GetCurrentConnectionIDs",
            "<ConnectionIDs>0</ConnectionIDs>",
        ),
        "GetCurrentConnectionInfo" => {
            let snap = state.snapshot();
            let proto = snap
                .current
                .as_ref()
                .and_then(|c| c.protocol_info.as_deref())
                .unwrap_or("");
            let fields = format!(
                "<RcsID>0</RcsID>\
                 <AVTransportID>0</AVTransportID>\
                 <ProtocolInfo>{}</ProtocolInfo>\
                 <PeerConnectionManager></PeerConnectionManager>\
                 <PeerConnectionID>-1</PeerConnectionID>\
                 <Direction>Input</Direction>\
                 <Status>OK</Status>",
                xml_escape(proto)
            );
            SoapResponse::ok(
                CONNECTION_MANAGER_SERVICE_TYPE,
                "GetCurrentConnectionInfo",
                &fields,
            )
        }
        _ => SoapResponse::fault(401, "Invalid Action"),
    }
}

/// Dispatch a `RenderingControl:1` SOAP action (fixed 100 / unmuted — no digital volume control).
pub fn handle_rendering_control(action: &str, state: &UpnpRendererState) -> SoapResponse {
    match action {
        "ListPresets" => SoapResponse::ok(
            RENDERING_CONTROL_SERVICE_TYPE,
            "ListPresets",
            "<CurrentPresetNameList>FactoryDefaults</CurrentPresetNameList>",
        ),
        "SelectPreset" => SoapResponse::ok(RENDERING_CONTROL_SERVICE_TYPE, "SelectPreset", ""),
        "GetMute" => SoapResponse::ok(
            RENDERING_CONTROL_SERVICE_TYPE,
            "GetMute",
            "<CurrentMute>0</CurrentMute>",
        ),
        "SetMute" => {
            state.note_volume_control_ignored();
            SoapResponse::ok(RENDERING_CONTROL_SERVICE_TYPE, "SetMute", "")
        }
        "GetVolume" => SoapResponse::ok(
            RENDERING_CONTROL_SERVICE_TYPE,
            "GetVolume",
            "<CurrentVolume>100</CurrentVolume>",
        ),
        "SetVolume" => {
            state.note_volume_control_ignored();
            SoapResponse::ok(RENDERING_CONTROL_SERVICE_TYPE, "SetVolume", "")
        }
        "GetVolumeDB" => SoapResponse::ok(
            RENDERING_CONTROL_SERVICE_TYPE,
            "GetVolumeDB",
            "<CurrentVolume>0</CurrentVolume>",
        ),
        "GetVolumeDBRange" => SoapResponse::ok(
            RENDERING_CONTROL_SERVICE_TYPE,
            "GetVolumeDBRange",
            "<MinValue>0</MinValue><MaxValue>0</MaxValue>",
        ),
        _ => SoapResponse::fault(401, "Invalid Action"),
    }
}
