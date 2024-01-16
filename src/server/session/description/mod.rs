pub(crate) mod fmtp;
pub(crate) mod rtp_codec;
pub(crate) mod rtp_receiver;
pub(crate) mod rtp_sender;
pub(crate) mod rtp_transceiver;
pub(crate) mod rtp_transceiver_direction;
pub(crate) mod sdp_type;

use crate::server::certificate::RTCDtlsFingerprint;
use crate::server::endpoint::candidate::RTCIceParameters;
use crate::server::session::description::rtp_codec::RTCRtpParameters;
use crate::server::session::description::rtp_transceiver::RTCRtpTransceiver;
use crate::server::session::description::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use crate::server::session::description::sdp_type::RTCSdpType;
use sdp::description::common::{Address, ConnectionInformation};
use sdp::description::media::{MediaName, RangedPort};
use sdp::description::session::{
    Origin, ATTR_KEY_CONNECTION_SETUP, ATTR_KEY_GROUP, ATTR_KEY_ICELITE, ATTR_KEY_MID,
    ATTR_KEY_RTCPMUX, ATTR_KEY_RTCPRSIZE,
};
use sdp::util::ConnectionRole;
use sdp::{MediaDescription, SessionDescription};
use serde::{Deserialize, Serialize};
use shared::error::{Error, Result};
use std::collections::HashMap;
use std::io::Cursor;
use std::net::SocketAddr;
use url::Url;

pub(crate) const UNSPECIFIED_STR: &str = "Unspecified";
pub(crate) const SDP_ATTRIBUTE_RID: &str = "rid";

/// SessionDescription is used to expose local and remote session descriptions.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct RTCSessionDescription {
    #[serde(rename = "type")]
    pub sdp_type: RTCSdpType,

    pub sdp: String,

    /// This will never be initialized by callers, internal use only
    #[serde(skip)]
    pub(crate) parsed: Option<SessionDescription>,
}

impl RTCSessionDescription {
    /// Given SDP representing an answer, wrap it in an RTCSessionDescription
    /// that can be given to an RTCPeerConnection.
    pub fn answer(sdp: String) -> Result<RTCSessionDescription> {
        let mut desc = RTCSessionDescription {
            sdp,
            sdp_type: RTCSdpType::Answer,
            parsed: None,
        };

        let parsed = desc.unmarshal()?;
        desc.parsed = Some(parsed);

        Ok(desc)
    }

    /// Given SDP representing an offer, wrap it in an RTCSessionDescription
    /// that can be given to an RTCPeerConnection.
    pub fn offer(sdp: String) -> Result<RTCSessionDescription> {
        let mut desc = RTCSessionDescription {
            sdp,
            sdp_type: RTCSdpType::Offer,
            parsed: None,
        };

        let parsed = desc.unmarshal()?;
        desc.parsed = Some(parsed);

        Ok(desc)
    }

    /// Given SDP representing an answer, wrap it in an RTCSessionDescription
    /// that can be given to an RTCPeerConnection. `pranswer` is used when the
    /// answer may not be final, or when updating a previously sent pranswer.
    pub fn pranswer(sdp: String) -> Result<RTCSessionDescription> {
        let mut desc = RTCSessionDescription {
            sdp,
            sdp_type: RTCSdpType::Pranswer,
            parsed: None,
        };

        let parsed = desc.unmarshal()?;
        desc.parsed = Some(parsed);

        Ok(desc)
    }

    /// Unmarshal is a helper to deserialize the sdp
    pub fn unmarshal(&self) -> Result<SessionDescription> {
        let mut reader = Cursor::new(self.sdp.as_bytes());
        let parsed = SessionDescription::unmarshal(&mut reader)
            .map_err(|err| Error::Other(err.to_string()))?;
        Ok(parsed)
    }
}

pub(crate) const MEDIA_SECTION_APPLICATION: &str = "application";

pub(crate) fn get_mid_value(media: &MediaDescription) -> Option<&String> {
    for attr in &media.attributes {
        if attr.key == "mid" {
            return attr.value.as_ref();
        }
    }
    None
}

pub(crate) fn get_peer_direction(media: &MediaDescription) -> RTCRtpTransceiverDirection {
    for a in &media.attributes {
        let direction = RTCRtpTransceiverDirection::from(a.key.as_str());
        if direction != RTCRtpTransceiverDirection::Unspecified {
            return direction;
        }
    }
    RTCRtpTransceiverDirection::Unspecified
}

pub(crate) fn get_rids(media: &MediaDescription) -> HashMap<String, String> {
    let mut rids = HashMap::new();
    for attr in &media.attributes {
        if attr.key.as_str() == SDP_ATTRIBUTE_RID {
            if let Some(value) = &attr.value {
                let split: Vec<&str> = value.split(' ').collect();
                rids.insert(split[0].to_owned(), value.to_owned());
            }
        }
    }
    rids
}

#[derive(Default)]
pub(crate) struct MediaSection<'a> {
    pub(crate) id: String,
    pub(crate) transceiver: Option<&'a RTCRtpTransceiver>,
    pub(crate) data: bool,
    pub(crate) rid_map: HashMap<String, String>,
    pub(crate) offered_direction: Option<RTCRtpTransceiverDirection>,
}

/// ICEGatheringState describes the state of the candidate gathering process.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum RTCIceGatheringState {
    #[default]
    Unspecified,

    /// ICEGatheringStateNew indicates that any of the ICETransports are
    /// in the "new" gathering state and none of the transports are in the
    /// "gathering" state, or there are no transports.
    New,

    /// ICEGatheringStateGathering indicates that any of the ICETransports
    /// are in the "gathering" state.
    Gathering,

    /// ICEGatheringStateComplete indicates that at least one ICETransport
    /// exists, and all ICETransports are in the "completed" gathering state.
    Complete,
}

pub(crate) fn add_candidate_to_media_descriptions(
    candidate: &SocketAddr,
    mut m: MediaDescription,
    ice_gathering_state: RTCIceGatheringState,
) -> Result<MediaDescription> {
    let append_candidate_if_new =
        |c: &SocketAddr, component: u16, m: MediaDescription| -> MediaDescription {
            let marshaled = format!("1 {} UDP 1 {} {} typ host", component, c.ip(), c.port());
            for a in &m.attributes {
                if let Some(value) = &a.value {
                    if &marshaled == value {
                        return m;
                    }
                }
            }

            m.with_value_attribute("candidate".to_owned(), marshaled)
        };

    m = append_candidate_if_new(candidate, 1, m); // 1: RTP
    m = append_candidate_if_new(candidate, 2, m); // 2: RTCP

    if ice_gathering_state != RTCIceGatheringState::Complete {
        return Ok(m);
    }
    for a in &m.attributes {
        if &a.key == "end-of-candidates" {
            return Ok(m);
        }
    }

    Ok(m.with_property_attribute("end-of-candidates".to_owned()))
}

pub(crate) struct AddDataMediaSectionParams {
    should_add_candidates: bool,
    mid_value: String,
    ice_params: RTCIceParameters,
    dtls_role: ConnectionRole,
    ice_gathering_state: RTCIceGatheringState,
}

pub(crate) fn add_data_media_section(
    d: SessionDescription,
    dtls_fingerprints: &[RTCDtlsFingerprint],
    candidate: &SocketAddr,
    params: AddDataMediaSectionParams,
) -> Result<SessionDescription> {
    let mut media = MediaDescription {
        media_name: MediaName {
            media: MEDIA_SECTION_APPLICATION.to_owned(),
            port: RangedPort {
                value: 9,
                range: None,
            },
            protos: vec!["UDP".to_owned(), "DTLS".to_owned(), "SCTP".to_owned()],
            formats: vec!["webrtc-datachannel".to_owned()],
        },
        media_title: None,
        connection_information: Some(ConnectionInformation {
            network_type: "IN".to_owned(),
            address_type: "IP4".to_owned(),
            address: Some(Address {
                address: "0.0.0.0".to_owned(),
                ttl: None,
                range: None,
            }),
        }),
        bandwidth: vec![],
        encryption_key: None,
        attributes: vec![],
    }
    .with_value_attribute(
        ATTR_KEY_CONNECTION_SETUP.to_owned(),
        params.dtls_role.to_string(),
    )
    .with_value_attribute(ATTR_KEY_MID.to_owned(), params.mid_value)
    .with_property_attribute(RTCRtpTransceiverDirection::Sendrecv.to_string())
    .with_value_attribute("sctp-port".to_owned(), "5000".to_owned()) //TODO: configurable
    .with_value_attribute("max-message-size".to_owned(), "262144".to_owned()) //TODO: configurable
    .with_ice_credentials(
        params.ice_params.username_fragment,
        params.ice_params.password,
    );

    for f in dtls_fingerprints {
        media = media.with_fingerprint(f.algorithm.clone(), f.value.to_uppercase());
    }

    if params.should_add_candidates {
        media = add_candidate_to_media_descriptions(candidate, media, params.ice_gathering_state)?;
    }

    Ok(d.with_media(media))
}

pub(crate) struct AddTransceiverSdpParams {
    should_add_candidates: bool,
    mid_value: String,
    dtls_role: ConnectionRole,
    ice_gathering_state: RTCIceGatheringState,
    offered_direction: Option<RTCRtpTransceiverDirection>,
}

pub(crate) fn add_transceiver_sdp(
    mut d: SessionDescription,
    dtls_fingerprints: &[RTCDtlsFingerprint],
    ice_params: &RTCIceParameters,
    candidate: &SocketAddr,
    media_section: &MediaSection<'_>,
    params: AddTransceiverSdpParams,
) -> Result<(SessionDescription, bool)> {
    if media_section.transceiver.is_none() {
        return Err(Error::Other("ErrSDPZeroTransceivers".to_string()));
    }
    let (should_add_candidates, mid_value, dtls_role, ice_gathering_state) = (
        params.should_add_candidates,
        params.mid_value,
        params.dtls_role,
        params.ice_gathering_state,
    );

    // Use the first transceiver to generate the section attributes
    let t = &media_section.transceiver.as_ref().unwrap();
    let mut media = MediaDescription::new_jsep_media_description(t.kind.to_string(), vec![])
        .with_value_attribute(ATTR_KEY_CONNECTION_SETUP.to_owned(), dtls_role.to_string())
        .with_value_attribute(ATTR_KEY_MID.to_owned(), mid_value.clone())
        .with_ice_credentials(
            ice_params.username_fragment.clone(),
            ice_params.password.clone(),
        )
        .with_property_attribute(ATTR_KEY_RTCPMUX.to_owned())
        .with_property_attribute(ATTR_KEY_RTCPRSIZE.to_owned());

    let codecs = &t.codecs;
    for codec in codecs {
        let name = codec
            .capability
            .mime_type
            .trim_start_matches("audio/")
            .trim_start_matches("video/")
            .to_owned();
        media = media.with_codec(
            codec.payload_type,
            name,
            codec.capability.clock_rate,
            codec.capability.channels,
            codec.capability.sdp_fmtp_line.clone(),
        );

        for feedback in &codec.capability.rtcp_feedback {
            media = media.with_value_attribute(
                "rtcp-fb".to_owned(),
                format!(
                    "{} {} {}",
                    codec.payload_type, feedback.typ, feedback.parameter
                ),
            );
        }
    }
    if codecs.is_empty() {
        // If we are sender and we have no codecs throw an error early
        if t.sender.track.is_some() {
            return Err(Error::Other("ErrSenderWithNoCodecs".to_string()));
        }

        // Explicitly reject track if we don't have the codec
        d = d.with_media(MediaDescription {
            media_name: sdp::description::media::MediaName {
                media: t.kind.to_string(),
                port: RangedPort {
                    value: 0,
                    range: None,
                },
                protos: vec![
                    "UDP".to_owned(),
                    "TLS".to_owned(),
                    "RTP".to_owned(),
                    "SAVPF".to_owned(),
                ],
                formats: vec!["0".to_owned()],
            },
            media_title: None,
            // We need to include connection information even if we're rejecting a track, otherwise Firefox will fail to
            // parse the SDP with an error like:
            // SIPCC Failed to parse SDP: SDP Parse Error on line 50:  c= connection line not specified for every media level, validation failed.
            // In addition this makes our SDP compliant with RFC 4566 Section 5.7: https://datatracker.ietf.org/doc/html/rfc4566#section-5.7
            connection_information: Some(ConnectionInformation {
                network_type: "IN".to_owned(),
                address_type: "IP4".to_owned(),
                address: Some(Address {
                    address: "0.0.0.0".to_owned(),
                    ttl: None,
                    range: None,
                }),
            }),
            bandwidth: vec![],
            encryption_key: None,
            attributes: vec![],
        });
        return Ok((d, false));
    }

    let parameters = RTCRtpParameters::default(); //TODO: media_engine.get_rtp_parameters_by_kind(t.kind, t.direction());
    for rtp_extension in &parameters.header_extensions {
        let ext_url = Url::parse(rtp_extension.uri.as_str())?;
        media = media.with_extmap(sdp::extmap::ExtMap {
            value: rtp_extension.id,
            uri: Some(ext_url),
            ..Default::default()
        });
    }

    if !media_section.rid_map.is_empty() {
        let mut recv_rids: Vec<String> = vec![];

        for rid in media_section.rid_map.keys() {
            media =
                media.with_value_attribute(SDP_ATTRIBUTE_RID.to_owned(), rid.to_owned() + " recv");
            recv_rids.push(rid.to_owned());
        }
        // Simulcast
        media = media.with_value_attribute(
            "simulcast".to_owned(),
            "recv ".to_owned() + recv_rids.join(";").as_str(),
        );
    }

    let sender = &t.sender;
    if let Some(track) = &sender.track {
        media = media.with_media_source(
            sender.ssrc,
            track.stream_id.to_owned(), /* cname */
            track.stream_id.to_owned(), /* streamLabel */
            track.id.to_owned(),
        );

        // Send msid based on the configured track if we haven't already
        // sent on this sender. If we have sent we must keep the msid line consistent, this
        // is handled below.
        if sender.initial_track_id.is_none() {
            for stream_id in &sender.associated_media_stream_ids {
                media = media.with_property_attribute(format!("msid:{} {}", stream_id, track.id));
            }

            //TODO: sender.initial_track_id = Some(track.id.to_string());
        }
    }

    if let Some(track_id) = &sender.initial_track_id {
        // After we have include an msid attribute in an offer it must stay the same for
        // all subsequent offer even if the track or transceiver direction changes.
        //
        // [RFC 8829 Section 5.2.2](https://datatracker.ietf.org/doc/html/rfc8829#section-5.2.2)
        //
        // For RtpTransceivers that are not stopped, the "a=msid" line or
        // lines MUST stay the same if they are present in the current
        // description, regardless of changes to the transceiver's direction
        // or track.  If no "a=msid" line is present in the current
        // description, "a=msid" line(s) MUST be generated according to the
        // same rules as for an initial offer.
        for stream_id in &sender.associated_media_stream_ids {
            media = media.with_property_attribute(format!("msid:{stream_id} {track_id}"));
        }
    }

    let direction = match params.offered_direction {
        Some(offered_direction) => {
            use RTCRtpTransceiverDirection::*;
            let transceiver_direction = t.direction;

            match offered_direction {
                Sendonly | Recvonly => {
                    // If a stream is offered as sendonly, the corresponding stream MUST be
                    // marked as recvonly or inactive in the answer.

                    // If a media stream is
                    // listed as recvonly in the offer, the answer MUST be marked as
                    // sendonly or inactive in the answer.
                    offered_direction.reverse().intersect(transceiver_direction)
                }
                // If an offered media stream is
                // listed as sendrecv (or if there is no direction attribute at the
                // media or session level, in which case the stream is sendrecv by
                // default), the corresponding stream in the answer MAY be marked as
                // sendonly, recvonly, sendrecv, or inactive
                Sendrecv | Unspecified => t.direction,
                // If an offered media
                // stream is listed as inactive, it MUST be marked as inactive in the
                // answer.
                Inactive => Inactive,
            }
        }
        None => {
            // If don't have an offered direction to intersect with just use the transceivers
            // current direction.
            //
            // https://datatracker.ietf.org/doc/html/rfc8829#section-4.2.3
            //
            //    When creating offers, the transceiver direction is directly reflected
            //    in the output, even for re-offers.
            t.direction
        }
    };
    media = media.with_property_attribute(direction.to_string());

    for fingerprint in dtls_fingerprints {
        media = media.with_fingerprint(
            fingerprint.algorithm.to_owned(),
            fingerprint.value.to_uppercase(),
        );
    }

    if should_add_candidates {
        media = add_candidate_to_media_descriptions(candidate, media, ice_gathering_state)?;
    }

    Ok((d.with_media(media), true))
}

/// populate_sdp serializes a PeerConnections state into an SDP
pub(crate) fn populate_sdp(
    mut d: SessionDescription,
    dtls_fingerprints: &[RTCDtlsFingerprint],
    candidate: &SocketAddr,
    ice_params: &RTCIceParameters,
    connection_role: ConnectionRole,
    media_sections: &[MediaSection<'_>],
    media_description_fingerprint: bool,
) -> Result<SessionDescription> {
    let media_dtls_fingerprints = if media_description_fingerprint {
        dtls_fingerprints.to_vec()
    } else {
        vec![]
    };

    let mut bundle_value = "BUNDLE".to_owned();
    let mut bundle_count = 0;
    let append_bundle = |mid_value: &str, value: &mut String, count: &mut i32| {
        *value = value.clone() + " " + mid_value;
        *count += 1;
    };

    for (i, m) in media_sections.iter().enumerate() {
        if m.data && m.transceiver.is_some() {
            return Err(Error::Other(
                "ErrSDPMediaSectionMediaDataChanInvalid".to_string(),
            ));
        }

        let should_add_candidates = i == 0;

        let should_add_id = if m.data {
            let params = AddDataMediaSectionParams {
                should_add_candidates,
                mid_value: m.id.clone(),
                ice_params: ice_params.clone(),
                dtls_role: connection_role,
                ice_gathering_state: RTCIceGatheringState::Complete,
            };
            d = add_data_media_section(d, &media_dtls_fingerprints, candidate, params)?;
            true
        } else {
            let params = AddTransceiverSdpParams {
                should_add_candidates,
                mid_value: m.id.clone(),
                dtls_role: connection_role,
                ice_gathering_state: RTCIceGatheringState::Complete,
                offered_direction: m.offered_direction,
            };
            let (d1, should_add_id) = add_transceiver_sdp(
                d,
                &media_dtls_fingerprints,
                ice_params,
                candidate,
                m,
                params,
            )?;
            d = d1;
            should_add_id
        };

        if should_add_id {
            append_bundle(&m.id, &mut bundle_value, &mut bundle_count);
        }
    }

    if !media_description_fingerprint {
        for fingerprint in dtls_fingerprints {
            d = d.with_fingerprint(
                fingerprint.algorithm.clone(),
                fingerprint.value.to_uppercase(),
            );
        }
    }

    // is_ice_lite for SFU
    // RFC 5245 S15.3
    d = d.with_property_attribute(ATTR_KEY_ICELITE.to_owned());

    Ok(d.with_value_attribute(ATTR_KEY_GROUP.to_owned(), bundle_value))
}

/// update_sdp_origin saves sdp.Origin in PeerConnection when creating 1st local SDP;
/// for subsequent calling, it updates Origin for SessionDescription from saved one
/// and increments session version by one.
/// <https://tools.ietf.org/html/draft-ietf-rtcweb-jsep-25#section-5.2.2>
pub(crate) fn update_sdp_origin(origin: &mut Origin, d: &mut SessionDescription) {
    //TODO: if atomic.CompareAndSwapUint64(&origin.SessionVersion, 0, d.Origin.SessionVersion)
    if origin.session_version == 0 {
        // store
        origin.session_version = d.origin.session_version;
        //atomic.StoreUint64(&origin.SessionID, d.Origin.SessionID)
        origin.session_id = d.origin.session_id;
    } else {
        // load
        /*for { // awaiting for saving session id
            d.Origin.SessionID = atomic.LoadUint64(&origin.SessionID)
            if d.Origin.SessionID != 0 {
                break
            }
        }*/
        d.origin.session_id = origin.session_id;

        //d.Origin.SessionVersion = atomic.AddUint64(&origin.SessionVersion, 1)
        origin.session_version += 1;
        d.origin.session_version += 1;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_new_sdp_type() {
        let tests = vec![
            ("Unspecified", RTCSdpType::Unspecified),
            ("offer", RTCSdpType::Offer),
            ("pranswer", RTCSdpType::Pranswer),
            ("answer", RTCSdpType::Answer),
            ("rollback", RTCSdpType::Rollback),
        ];

        for (sdp_type_string, expected_sdp_type) in tests {
            assert_eq!(RTCSdpType::from(sdp_type_string), expected_sdp_type);
        }
    }

    #[test]
    fn test_sdp_type_string() {
        let tests = vec![
            (RTCSdpType::Unspecified, "Unspecified"),
            (RTCSdpType::Offer, "offer"),
            (RTCSdpType::Pranswer, "pranswer"),
            (RTCSdpType::Answer, "answer"),
            (RTCSdpType::Rollback, "rollback"),
        ];

        for (sdp_type, expected_string) in tests {
            assert_eq!(sdp_type.to_string(), expected_string);
        }
    }

    #[test]
    fn test_session_description_json() {
        let tests = vec![
            (
                RTCSessionDescription {
                    sdp_type: RTCSdpType::Offer,
                    sdp: "sdp".to_owned(),
                    parsed: None,
                },
                r#"{"type":"offer","sdp":"sdp"}"#,
            ),
            (
                RTCSessionDescription {
                    sdp_type: RTCSdpType::Pranswer,
                    sdp: "sdp".to_owned(),
                    parsed: None,
                },
                r#"{"type":"pranswer","sdp":"sdp"}"#,
            ),
            (
                RTCSessionDescription {
                    sdp_type: RTCSdpType::Answer,
                    sdp: "sdp".to_owned(),
                    parsed: None,
                },
                r#"{"type":"answer","sdp":"sdp"}"#,
            ),
            (
                RTCSessionDescription {
                    sdp_type: RTCSdpType::Rollback,
                    sdp: "sdp".to_owned(),
                    parsed: None,
                },
                r#"{"type":"rollback","sdp":"sdp"}"#,
            ),
            (
                RTCSessionDescription {
                    sdp_type: RTCSdpType::Unspecified,
                    sdp: "sdp".to_owned(),
                    parsed: None,
                },
                r#"{"type":"Unspecified","sdp":"sdp"}"#,
            ),
        ];

        for (desc, expected_string) in tests {
            let result = serde_json::to_string(&desc);
            assert!(result.is_ok(), "testCase: marshal err: {result:?}");
            let desc_data = result.unwrap();
            assert_eq!(desc_data, expected_string, "string is not expected");

            let result = serde_json::from_str::<RTCSessionDescription>(&desc_data);
            assert!(result.is_ok(), "testCase: unmarshal err: {result:?}");
            if let Ok(sd) = result {
                assert!(sd.sdp == desc.sdp && sd.sdp_type == desc.sdp_type);
            }
        }
    }

    #[test]
    fn test_new_rtp_transceiver_direction() {
        let tests = vec![
            ("Unspecified", RTCRtpTransceiverDirection::Unspecified),
            ("sendrecv", RTCRtpTransceiverDirection::Sendrecv),
            ("sendonly", RTCRtpTransceiverDirection::Sendonly),
            ("recvonly", RTCRtpTransceiverDirection::Recvonly),
            ("inactive", RTCRtpTransceiverDirection::Inactive),
        ];

        for (ct_str, expected_type) in tests {
            assert_eq!(RTCRtpTransceiverDirection::from(ct_str), expected_type);
        }
    }

    #[test]
    fn test_rtp_transceiver_direction_string() {
        let tests = vec![
            (RTCRtpTransceiverDirection::Unspecified, "Unspecified"),
            (RTCRtpTransceiverDirection::Sendrecv, "sendrecv"),
            (RTCRtpTransceiverDirection::Sendonly, "sendonly"),
            (RTCRtpTransceiverDirection::Recvonly, "recvonly"),
            (RTCRtpTransceiverDirection::Inactive, "inactive"),
        ];

        for (d, expected_string) in tests {
            assert_eq!(d.to_string(), expected_string);
        }
    }

    #[test]
    fn test_rtp_transceiver_has_send() {
        let tests = vec![
            (RTCRtpTransceiverDirection::Unspecified, false),
            (RTCRtpTransceiverDirection::Sendrecv, true),
            (RTCRtpTransceiverDirection::Sendonly, true),
            (RTCRtpTransceiverDirection::Recvonly, false),
            (RTCRtpTransceiverDirection::Inactive, false),
        ];

        for (d, expected_value) in tests {
            assert_eq!(d.has_send(), expected_value);
        }
    }

    #[test]
    fn test_rtp_transceiver_has_recv() {
        let tests = vec![
            (RTCRtpTransceiverDirection::Unspecified, false),
            (RTCRtpTransceiverDirection::Sendrecv, true),
            (RTCRtpTransceiverDirection::Sendonly, false),
            (RTCRtpTransceiverDirection::Recvonly, true),
            (RTCRtpTransceiverDirection::Inactive, false),
        ];

        for (d, expected_value) in tests {
            assert_eq!(d.has_recv(), expected_value);
        }
    }

    #[test]
    fn test_rtp_transceiver_from_send_recv() {
        let tests = vec![
            (RTCRtpTransceiverDirection::Sendrecv, (true, true)),
            (RTCRtpTransceiverDirection::Sendonly, (true, false)),
            (RTCRtpTransceiverDirection::Recvonly, (false, true)),
            (RTCRtpTransceiverDirection::Inactive, (false, false)),
        ];

        for (expected_value, (send, recv)) in tests {
            assert_eq!(
                RTCRtpTransceiverDirection::from_send_recv(send, recv),
                expected_value
            );
        }
    }

    #[test]
    fn test_rtp_transceiver_intersect() {
        use RTCRtpTransceiverDirection::*;

        let tests = vec![
            ((Sendrecv, Recvonly), Recvonly),
            ((Sendrecv, Sendonly), Sendonly),
            ((Sendrecv, Inactive), Inactive),
            ((Sendonly, Inactive), Inactive),
            ((Recvonly, Inactive), Inactive),
            ((Recvonly, Sendrecv), Recvonly),
            ((Sendonly, Sendrecv), Sendonly),
            ((Sendonly, Recvonly), Inactive),
            ((Recvonly, Recvonly), Recvonly),
        ];

        for ((a, b), expected_direction) in tests {
            assert_eq!(a.intersect(b), expected_direction);
        }
    }
}
