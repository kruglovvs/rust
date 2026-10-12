//! SDP generation (plan Todo 11) — its own unit-testable unit, per the plan:
//!
//! > **The audio codec is settled here: µ-law, payload type 0, clock rate
//! > 8000, declared as `a=rtpmap:0 PCMU/8000`.** [...] µ-law is chosen over
//! > A-law because payload type 0 is the universally-defaulted static RTP
//! > payload, so clients that ignore the SDP still decode audio correctly;
//! > PCMA is static type 8 and requires the SDP to be honoured. The video
//! > track is `a=rtpmap:26 JPEG/90000`.
//!
//! **These two strings are locked** (plan round-1 fix). The exact lines are
//! asserted here and again in Todo 15's on-device suite. No PCMA, no L16, no
//! dynamic payload types ever appear in this SDP.

/// Parameters for one SDP instance.
///
/// `control_base` is the absolute (or client-relative) base URL the `a=control:`
/// attributes are appended to; it must end with `/`. The server derives it from
/// the DESCRIBE request URI so `trackID=0` resolves to
/// `<presentation-uri>/trackID=0` — with the trailing slash on purpose, because
/// RFC 3986 relative resolution against `/mjpeg/1` (no trailing slash) would
/// replace the last segment and yield `/mjpeg/trackID=0`.
pub struct SdpParams<'a> {
    /// `o=` session id (non-negative integer as a string).
    pub session_id: &'a str,
    /// `o=` version — increments when the description changes (SDP only; this
    /// server's description is fixed per boot, so the caller passes 1).
    pub version: u64,
    /// Base for `a=control:` attributes; must end with `/`.
    pub control_base: &'a str,
}

/// Generate the presentation SDP: two media lines (video track 0, audio
/// track 1), plan-locked rtpmap lines, absolute control URLs.
pub fn generate(params: &SdpParams<'_>) -> String {
    let SdpParams {
        session_id,
        version,
        control_base,
    } = params;
    format!(
        "v=0\r\n\
         o=- {session_id} {version} IN IP4 0.0.0.0\r\n\
         s=ESP32-S3 AV Live Stream\r\n\
         c=IN IP4 0.0.0.0\r\n\
         t=0 0\r\n\
         a=control:*\r\n\
         m=video 0 RTP/AVP 26\r\n\
         a=rtpmap:26 JPEG/90000\r\n\
         a=control:{control_base}trackID=0\r\n\
         m=audio 0 RTP/AVP 0\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=control:{control_base}trackID=1\r\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdp() -> String {
        generate(&SdpParams {
            session_id: "1",
            version: 1,
            control_base: "rtsp://192.168.1.5:554/mjpeg/1/",
        })
    }

    #[test]
    fn locked_audio_rtpmap_string_is_exact() {
        // Plan round-1 fix — the exact string, byte for byte, as its own SDP line.
        let s = sdp();
        assert!(s.lines().any(|l| l == "a=rtpmap:0 PCMU/8000"));
        assert!(s.contains("\r\na=rtpmap:0 PCMU/8000\r\n"));
    }

    #[test]
    fn locked_video_rtpmap_string_is_exact() {
        assert!(sdp().lines().any(|l| l == "a=rtpmap:26 JPEG/90000"));
    }

    #[test]
    fn exactly_two_media_lines_video_first() {
        let s = sdp();
        let mlines: Vec<&str> = s.lines().filter(|l| l.starts_with("m=")).collect();
        assert_eq!(mlines, vec!["m=video 0 RTP/AVP 26", "m=audio 0 RTP/AVP 0"]);
    }

    #[test]
    fn session_id_and_version_appear_in_the_origin_line() {
        assert!(sdp().lines().any(|l| l == "o=- 1 1 IN IP4 0.0.0.0"));
        let other = generate(&SdpParams {
            session_id: "255",
            version: 3,
            control_base: "rtsp://h:554/mjpeg/1/",
        });
        assert!(other.lines().any(|l| l == "o=- 255 3 IN IP4 0.0.0.0"));
    }

    #[test]
    fn forbidden_codecs_never_appear() {
        // Plan Must-NOT-Have for this SDP: no PCMA, no L16, no dynamic PTs.
        let s = sdp();
        assert!(!s.contains("PCMA"));
        assert!(!s.contains("L16"));
        assert!(!s.contains("H264") && !s.contains("H.264"));
        // Dynamic payload types live at PT >= 96; only static 26 and 0 here.
        assert!(!s.lines().any(|l| l.starts_with("a=rtpmap:") && {
            l["a=rtpmap:".len()..]
                .split_whitespace()
                .next()
                .and_then(|pt| pt.parse::<u16>().ok())
                .map(|pt| pt >= 96)
                .unwrap_or(true)
        }));
    }

    #[test]
    fn control_urls_are_absolute_and_carry_the_trailing_slash_base() {
        let s = sdp();
        assert!(s.lines().any(|l| l == "a=control:rtsp://192.168.1.5:554/mjpeg/1/trackID=0"));
        assert!(s.lines().any(|l| l == "a=control:rtsp://192.168.1.5:554/mjpeg/1/trackID=1"));
        assert!(s.lines().any(|l| l == "a=control:*"));
    }

    #[test]
    fn body_is_crlf_terminated_on_every_line() {
        // RTSP message bodies must be octet-aligned CRLF; a stray bare LF
        // would let a client desync on the Content-Length boundary.
        let s = sdp();
        assert!(s.ends_with("\r\n"));
        let stripped = s.replace("\r\n", "");
        assert!(!stripped.contains('\n'), "bare LF found in SDP body");
    }
}
