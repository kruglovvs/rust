//! `Transport:` header parsing (plan Todo 11).
//!
//! > plus `Transport:` header parsing for both TCP-interleaved and UDP
//!
//! Must accept (plan MUST DO #2):
//! - `RTP/AVP/TCP;unicast;interleaved=0-1` — TCP interleaved;
//! - `RTP/AVP;unicast;client_port=NNNN-NNNN+1` — UDP;
//!
//! and reject everything the server does not offer with **461 Unsupported
//! Transport**. Clients (ffmpeg in particular) may offer several alternatives
//! in one header, comma-separated; the first supported alternative wins, and
//! only when *no* alternative is supported is this an error. Pure `std`.

/// One accepted transport alternative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportReq {
    /// `RTP/AVP/TCP;…;interleaved=a-b` — RTP on channel `a`, RTCP on `b`.
    Tcp { rtp: u8, rtcp: u8 },
    /// `RTP/AVP;…;client_port=x-y` — client's UDP RTP/RTCP ports.
    Udp { client_rtp: u16, client_rtcp: u16 },
}

/// No offered alternative is supported → **461 Unsupported Transport**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    Unsupported,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no supported Transport alternative offered")
    }
}

impl std::error::Error for TransportError {}

fn parse_u16_pair(value: &str) -> Option<(u16, u16)> {
    let (a, b) = value.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn parse_u8_pair(value: &str) -> Option<(u8, u8)> {
    let (a, b) = value.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn parse_alternative(alt: &str) -> Option<TransportReq> {
    let mut parts = alt.split(';');
    let spec = parts.next()?.trim().to_ascii_uppercase();

    let mut interleaved: Option<(u8, u8)> = None;
    let mut interleaved_malformed = false;
    let mut client_port: Option<(u16, u16)> = None;
    let mut multicast = false;
    let mut mode_play = true;

    for part in parts {
        let part = part.trim();
        let (key, value) = match part.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), Some(v.trim())),
            None => (part.to_ascii_lowercase(), None),
        };
        match key.as_str() {
            "interleaved" => match value.and_then(parse_u8_pair) {
                Some(pair) => interleaved = Some(pair),
                None => interleaved_malformed = true,
            },
            "client_port" => client_port = value.and_then(parse_u16_pair),
            "multicast" => multicast = true,
            "mode" => mode_play = value.map(|v| v.eq_ignore_ascii_case("play")).unwrap_or(false),
            _ => {}
        }
    }

    if !mode_play {
        return None;
    }

    match spec.as_str() {
        // RFC 2326 §10.12 / A.1: RTP over RTSP interleaved on the control
        // connection. `interleaved` may legally be absent → default 0-1;
        // present-but-unparseable is a rejection, not a silent default.
        "RTP/AVP/TCP" => {
            if multicast || interleaved_malformed {
                return None;
            }
            Some(TransportReq::Tcp {
                rtp: interleaved.map(|(a, _)| a).unwrap_or(0),
                rtcp: interleaved.map(|(_, b)| b).unwrap_or(1),
            })
        }
        // Plain RTP/AVP is UDP by definition; `RTP/AVP/UDP` is the explicit form.
        "RTP/AVP" | "RTP/AVP/UDP" => {
            if multicast {
                return None;
            }
            let (client_rtp, client_rtcp) = client_port?;
            Some(TransportReq::Udp {
                client_rtp,
                client_rtcp,
            })
        }
        _ => None,
    }
}

/// Parse a full `Transport:` header value. Every comma-separated alternative
/// is tried in order; `Err(Unsupported)` only when none is implementable.
pub fn parse(header: &str) -> Result<TransportReq, TransportError> {
    for alt in header.split(',') {
        if let Some(found) = parse_alternative(alt) {
            return Ok(found);
        }
    }
    Err(TransportError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_tcp_interleaved_the_plan_must_handle_form() {
        assert_eq!(
            parse("RTP/AVP/TCP;unicast;interleaved=0-1").unwrap(),
            TransportReq::Tcp { rtp: 0, rtcp: 1 }
        );
        assert_eq!(
            parse("RTP/AVP/TCP;unicast;interleaved=2-3;mode=play").unwrap(),
            TransportReq::Tcp { rtp: 2, rtcp: 3 }
        );
    }

    #[test]
    fn tcp_with_missing_interleaved_defaults_to_0_1() {
        assert_eq!(
            parse("RTP/AVP/TCP;unicast").unwrap(),
            TransportReq::Tcp { rtp: 0, rtcp: 1 }
        );
    }

    #[test]
    fn accepts_udp_client_port_the_plan_must_handle_form() {
        assert_eq!(
            parse("RTP/AVP;unicast;client_port=6970-6971").unwrap(),
            TransportReq::Udp {
                client_rtp: 6970,
                client_rtcp: 6971
            }
        );
        assert_eq!(
            parse("RTP/AVP/UDP;unicast;client_port=5000-5001;mode=play").unwrap(),
            TransportReq::Udp {
                client_rtp: 5000,
                client_rtcp: 5001
            }
        );
    }

    #[test]
    fn comma_alternatives_pick_the_first_supported_one() {
        // ffmpeg offers UDP first, TCP second (and vice versa for -rtsp_tcp).
        assert_eq!(
            parse("RTP/AVP;unicast;client_port=10000-10001,RTP/AVP/TCP;unicast;interleaved=0-1")
                .unwrap(),
            TransportReq::Udp {
                client_rtp: 10000,
                client_rtcp: 10001
            }
        );
        assert_eq!(
            parse("RTP/AVP/TCP;unicast;interleaved=0-1,RTP/AVP;unicast;client_port=10000-10001")
                .unwrap(),
            TransportReq::Tcp { rtp: 0, rtcp: 1 }
        );
        // First alternative unsupported, second supported → still accepted.
        assert_eq!(
            parse("raw/RAW;unicast,RTP/AVP/TCP;unicast;interleaved=0-1").unwrap(),
            TransportReq::Tcp { rtp: 0, rtcp: 1 }
        );
    }

    #[test]
    fn rejects_what_the_server_does_not_offer_with_461_cases() {
        // Multicast is not offered.
        assert_eq!(
            parse("RTP/AVP;multicast;port=0-1"),
            Err(TransportError::Unsupported)
        );
        // Non-RTP transport (RFC 2326 §10.2 raw/RAW example).
        assert_eq!(
            parse("raw/RAW;unicast"),
            Err(TransportError::Unsupported)
        );
        // UDP without client_port cannot be answered.
        assert_eq!(parse("RTP/AVP;unicast"), Err(TransportError::Unsupported));
        // TCP without usable interleaved spec.
        assert_eq!(
            parse("RTP/AVP/TCP;interleaved=a-b"),
            Err(TransportError::Unsupported)
        );
        // A non-play mode is not supported.
        assert_eq!(
            parse("RTP/AVP;unicast;client_port=1-2;mode=record"),
            Err(TransportError::Unsupported)
        );
        assert_eq!(parse(""), Err(TransportError::Unsupported));
        // Every alternative unsupported → 461 even when many are offered.
        assert_eq!(
            parse("raw/RAW;unicast,RTP/AVP;multicast;port=0-1"),
            Err(TransportError::Unsupported)
        );
    }

    #[test]
    fn case_insensitive_spec_and_params() {
        assert_eq!(
            parse("rtp/avp/tcp;UNICAST;Interleaved=4-5").unwrap(),
            TransportReq::Tcp { rtp: 4, rtcp: 5 }
        );
    }
}
