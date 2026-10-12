//! Todo 12 host-executed proofs for `firmware/src/rtsp/payload/`.
//!
//! The payload modules' own `#[cfg(test)]` suites (µ-law 256-code round-trip,
//! G.711 bounds, half-band swept-sine, PT-0 header byte) run here too via the
//! `#[path]` include — the proven audio_quality / task11_rtsp pattern. What
//! lives in THIS file are the fixture-driven packetizer proofs:
//!
//! 1. **Gate-(a) contract**: fixture JPEG → `jfif.rs` → `JpegTrack` RTP
//!    fragments → parse the packets back (RFC 2435 offset fields) →
//!    reassemble → byte-identical scan (marker last-only, seq/rtptime/PT
//!    contract).
//!
//! This target deliberately does NOT include `rtsp/mod.rs`: including it also
//! compiles `session.rs` + `service.rs`'s `#[cfg(test)]` suites, which share
//! the module-global `session::SESSION` unsafely (service's loopback teardown
//! vs session's `before == Idle` assertion — a pre-existing Todo 11/14 defect
//! that makes `task11_rtsp` non-deterministic; see the Todo 12 notepad entry).
//! The RTSP-path proofs (Todo 11's `interleaved_frame`, real `RtpSender`)
//! live in the `task12_report` bin modes instead — a non-test build compiles
//! no `#[cfg(test)]` suites, so no shared static is ever raced there.

#[path = "../../firmware/src/jfif.rs"]
mod jfif;
#[path = "../../firmware/src/rtsp/payload/mod.rs"]
mod payload;

use jfif::Frame;
use payload::jpeg_track::JpegTrack;

const FIXTURE: &[u8] = include_bytes!("../fixtures/qvga_422_dyn.jpg");
const FIXTURE_RESTART: &[u8] = include_bytes!("../fixtures/qvga_422_restart.jpg");

/// Receiver-side RFC 2435 header length, derived from the packet's own bytes
/// (this is what a real depacketizer does — see gate6's `expected_header_len`).
/// Layout after the 12-byte RTP header: Type-specific(1) | Offset(3) | Type(1)
/// | Q(1) | Width(1) | Height(1) [+ Restart(4)] [+ Quant(4+128)].
fn rfc2435_header_len(pkt: &[u8], first_of_frame: bool) -> usize {
    let rtp = 12;
    let r#type = pkt[rtp + 4];
    let q = pkt[rtp + 5];
    let mut n = 8usize;
    if r#type >= 64 {
        n += 4; // restart header
    }
    if first_of_frame && q == 255 {
        n += 4 + 128; // Q=255 in-band quantisation header
    }
    n
}

/// Reassemble a sequence of RTP/JPEG datagrams (one frame) into the scan.
/// Returns (scan_bytes, marker_last_only, seqs, timestamps, pts).
fn reassemble(packets: &[Vec<u8>]) -> (Vec<u8>, bool, Vec<u16>, Vec<u32>, Vec<u8>) {
    let mut scan: Vec<u8> = Vec::new();
    let mut markers = Vec::new();
    let mut seqs = Vec::new();
    let mut tss = Vec::new();
    let mut pts = Vec::new();
    for (i, pkt) in packets.iter().enumerate() {
        assert!(pkt.len() >= 12 + 8, "packet {i} too short: {}", pkt.len());
        assert_eq!(pkt[0], 0x80, "packet {i}: V=2");
        let pt = pkt[1] & 0x7F;
        markers.push(pkt[1] & 0x80 != 0);
        seqs.push(u16::from_be_bytes([pkt[2], pkt[3]]));
        tss.push(u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]));
        pts.push(pt);

        let off = ((pkt[13] as usize) << 16) | ((pkt[14] as usize) << 8) | (pkt[15] as usize);
        let hlen = rfc2435_header_len(pkt, i == 0);
        if i == 0 {
            scan.clear();
        }
        assert_eq!(off, scan.len(), "fragment offsets must be contiguous");
        scan.extend_from_slice(&pkt[12 + hlen..]);
    }
    let last_only = markers.last().copied().unwrap_or(false)
        && markers[..markers.len() - 1].iter().all(|&m| !m);
    (scan, last_only, seqs, tss, pts)
}

/// Packetize one fixture `frames` times and return every emitted RTP packet.
fn packets_for(jpeg: &[u8], frames: usize) -> Vec<Vec<u8>> {
    let mut track = JpegTrack::new(0x0A0B_0C0D);
    let mut out: Vec<Vec<u8>> = Vec::new();
    for _ in 0..frames {
        track
            .packetize(jpeg, &mut |pkt| {
                out.push(pkt.to_vec());
                Ok(())
            })
            .expect("packetize fixture");
    }
    out
}

#[test]
fn jpeg_track_fragments_reassemble_byte_identical_scan_two_frames() {
    let frame = Frame::parse(FIXTURE).expect("fixture parses");
    let expected_scan = frame.scan.to_vec();

    let pkts = packets_for(FIXTURE, 2);
    assert!(pkts.len() >= 4, "two QVGA frames ⇒ multiple RTP packets");

    // Frame boundary: marker bit lands on the LAST packet of each frame.
    let mut start = 0usize;
    for frame_idx in 0..2 {
        let mut end = start;
        while end < pkts.len() && (pkts[end][1] & 0x80) == 0 {
            end += 1;
        }
        assert!(end < pkts.len(), "frame {frame_idx}: marker packet exists");
        let (scan, last_only, seqs, tss, pts) = reassemble(&pkts[start..=end]);
        assert!(
            scan == expected_scan,
            "frame {frame_idx}: reassembled scan must be BYTE-IDENTICAL to jfif's extract \
             (gate-(a) contract) — {} vs {} bytes",
            scan.len(),
            expected_scan.len()
        );
        assert!(last_only, "frame {frame_idx}: marker bit on final fragment only");
        assert!(pts.iter().all(|&p| p == 26), "PT 26 in every packet byte");
        // seq +1 per packet across both frames, starting at 0 (RTP-Info).
        assert_eq!(seqs[0], start as u16, "seq is global across frames");
        let expected_ts: u32 = frame_idx * payload::jpeg_track::FRAME_TIMESTAMP_STEP;
        assert!(
            tss.iter().all(|&t| t == expected_ts),
            "frame {frame_idx}: one timestamp per frame, +900 between frames"
        );
        start = end + 1;
    }
}

#[test]
fn restart_fixture_type_64_65_flows_through_the_track() {
    let frame = Frame::parse(FIXTURE_RESTART).expect("restart fixture parses");
    assert!(frame.r#type >= 64, "fixture must exercise the restart header");
    let pkts = packets_for(FIXTURE_RESTART, 1);
    for (i, pkt) in pkts.iter().enumerate() {
        assert_eq!(pkt[16], frame.r#type, "packet {i}: type field carried (header[4])");
        assert!(
            pkt[17] != 255 || i == 0,
            "packet {i}: Q=255 (in-band quant tables) may appear only in the first packet"
        );
    }
    let (scan, last_only, ..) = reassemble(&pkts);
    assert_eq!(scan, frame.scan, "restart scan byte-identical after reassembly");
    assert!(last_only, "marker bit on final fragment");
}

#[test]
fn jpeg_track_packet_sizes_stay_within_mtu_budget() {
    let pkts = packets_for(FIXTURE, 1);
    let max = payload::jpeg_track::RTP_HEADER_LEN + payload::jpeg_track::MAX_PAYLOAD;
    for (i, p) in pkts.iter().enumerate() {
        assert!(p.len() <= max, "packet {i} exceeds MTU budget: {}", p.len());
        assert!(p.len() > payload::jpeg_track::RTP_HEADER_LEN, "packet {i} carries payload");
    }
}