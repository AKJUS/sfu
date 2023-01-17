use async_trait::async_trait;
use std::sync::Arc;

use crate::rtc::server::server_states::ServerStates;

use retty::channel::handler::{
    Handler, InboundHandler, InboundHandlerContext, InboundHandlerInternal, OutboundHandler,
    OutboundHandlerInternal,
};
use retty::runtime::sync::Mutex;
use retty::transport::async_transport_udp::TaggedBytesMut;

/// MatchFunc allows custom logic for mapping packets to an Endpoint
type MatchFunc = Box<dyn (Fn(&[u8]) -> bool) + Send + Sync>;

/// match_all always returns true
fn match_all(_b: &[u8]) -> bool {
    true
}

/// match_range is a MatchFunc that accepts packets with the first byte in [lower..upper]
fn match_range(lower: u8, upper: u8) -> MatchFunc {
    Box::new(move |buf: &[u8]| -> bool {
        if buf.is_empty() {
            return false;
        }
        let b = buf[0];
        b >= lower && b <= upper
    })
}

/// MatchFuncs as described in RFC7983
/// <https://tools.ietf.org/html/rfc7983>
///              +----------------+
///              |        [0..3] -+--> forward to STUN
///              |                |
///              |      [16..19] -+--> forward to ZRTP
///              |                |
///  packet -->  |      [20..63] -+--> forward to DTLS
///              |                |
///              |      [64..79] -+--> forward to TURN Channel
///              |                |
///              |    [128..191] -+--> forward to RTP/RTCP
///              +----------------+
/// match_dtls is a MatchFunc that accepts packets with the first byte in [20..63]
/// as defied in RFC7983
fn match_dtls(b: &[u8]) -> bool {
    match_range(20, 63)(b)
}

/// match_srtp_or_srtcp is a MatchFunc that accepts packets with the first byte in [128..191]
/// as defied in RFC7983
fn match_srtp_or_srtcp(b: &[u8]) -> bool {
    match_range(128, 191)(b)
}

pub(crate) fn is_rtcp(buf: &[u8]) -> bool {
    // Not long enough to determine RTP/RTCP
    if buf.len() < 4 {
        return false;
    }

    let rtcp_packet_type = buf[1];
    (192..=223).contains(&rtcp_packet_type)
}

/// match_srtp is a MatchFunc that only matches SRTP and not SRTCP
fn match_srtp(buf: &[u8]) -> bool {
    match_srtp_or_srtcp(buf) && !is_rtcp(buf)
}

/// match_srtcp is a MatchFunc that only matches SRTCP and not SRTP
fn match_srtcp(buf: &[u8]) -> bool {
    match_srtp_or_srtcp(buf) && is_rtcp(buf)
}

struct UDPDemuxerDecoder {
    server_states: Arc<Mutex<ServerStates>>,
}
struct UDPDemuxerEncoder;

pub struct UDPDemuxer {
    decoder: UDPDemuxerDecoder,
    encoder: UDPDemuxerEncoder,
}

impl UDPDemuxer {
    pub fn new(server_states: Arc<Mutex<ServerStates>>) -> Self {
        UDPDemuxer {
            decoder: UDPDemuxerDecoder { server_states },
            encoder: UDPDemuxerEncoder {},
        }
    }
}

#[async_trait]
impl InboundHandler<TaggedBytesMut> for UDPDemuxerDecoder {
    async fn read(&mut self, ctx: &mut InboundHandlerContext, msg: &mut TaggedBytesMut) {
        if match_srtp_or_srtcp(&msg.message) {
            //TODO: dispatch the packet to Media Pipeline
        } else {
            ctx.fire_read(msg).await;
        }
    }
}

#[async_trait]
impl OutboundHandler<TaggedBytesMut> for UDPDemuxerEncoder {}

impl Handler for UDPDemuxer {
    fn id(&self) -> String {
        "UDPDemuxer Handler".to_string()
    }

    fn split(
        self,
    ) -> (
        Arc<Mutex<dyn InboundHandlerInternal>>,
        Arc<Mutex<dyn OutboundHandlerInternal>>,
    ) {
        let decoder: Box<dyn InboundHandler<TaggedBytesMut>> = Box::new(self.decoder);
        let encoder: Box<dyn OutboundHandler<TaggedBytesMut>> = Box::new(self.encoder);
        (Arc::new(Mutex::new(decoder)), Arc::new(Mutex::new(encoder)))
    }
}
