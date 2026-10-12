//! Todo 13 host-executed proofs for `firmware/src/rtcp.rs`.
//!
//! The plan QA-failure row (quoted in the module header) demands four
//! host-provable things: the explicit wrap at `0xFFFF_FFFF → 0` with
//! **continuity of the reported time across the boundary**, suppression when
//! unsynced (`unsynced, SR suppressed`, zero bytes), the ≥ 5 s scheduler
//! firing at boundaries and never early, and the exact RFC 3550 §6.4.1 byte
//! layout — plus the NTP 64-bit field mapping from fixed `SystemTime`
//! instants. `rtcp.rs` references no `crate::` paths, so the `#[path]`
//! include (the proven `audio_quality` / `task14_net` pattern) compiles it
//! standalone; its own in-file suite runs here too.

#[path = "../../firmware/src/rtcp.rs"]
mod rtcp;

use rtcp::*;
use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

/// Build a minimal RTP packet (12-byte header + `payload_len` octets) the
/// way a receiver would parse it: V=2, given PT, seq, rtptime, ssrc.
fn rtp_packet(pt: u8, seq: u16, rtptime: u32, ssrc: u32, payload_len: usize) -> Vec<u8> {
    let mut p = vec![0u8; 12 + payload_len];
    p[0] = 0x80;
    p[1] = pt;
    p[2..4].copy_from_slice(&seq.to_be_bytes());
    p[4..8].copy_from_slice(&rtptime.to_be_bytes());
    p[8..12].copy_from_slice(&ssrc.to_be_bytes());
    p
}

// ---------------------------------------------------------------------------
// 1. SR byte layout — RFC 3550 §6.4.1 against a hand-computed vector
// ---------------------------------------------------------------------------

#[test]
fn sr_byte_layout_matches_hand_computed_rfc3550_vector() {
    // Hand-computed expectation, field by field (RFC 3550 §6.4.1):
    //   byte 0      : V=2 (0b10) | P=0 | RC=0        -> 0x80
    //   byte 1      : PT                              -> 200 = 0xC8
    //   bytes 2..4  : length (32-bit words - 1)       -> 6 = 0x0006
    //   bytes 4..8  : SSRC of sender                  -> 0x11223344
    //   bytes 8..12 : NTP ms word (seconds 1900+)     -> 0x80000000 (2147483648)
    //   bytes 12..16: NTP ls word (fraction)          -> 0x40000000 (1/4 second)
    //   bytes 16..20: RTP timestamp                    -> 0x55667788
    //   bytes 20..24: sender's packet count            -> 0x00000BB8 (3000)
    //   bytes 24..28: sender's octet count             -> 0x00010000 (65536)
    let sr = SenderReport {
        ssrc: 0x1122_3344,
        ntp_seconds: 0x8000_0000,
        ntp_fraction: 0x4000_0000,
        rtp_timestamp: 0x5566_7788,
        packet_count: 0x0000_0BB8,
        octet_count: 0x0001_0000,
    };
    let expected: [u8; 28] = [
        0x80, 0xC8, 0x00, 0x06, // V/P/RC, PT=200, length=6
        0x11, 0x22, 0x33, 0x44, // SSRC
        0x80, 0x00, 0x00, 0x00, // NTP seconds
        0x40, 0x00, 0x00, 0x00, // NTP fraction
        0x55, 0x66, 0x77, 0x88, // RTP timestamp
        0x00, 0x00, 0x0B, 0xB8, // packet count
        0x00, 0x01, 0x00, 0x00, // octet count
    ];
    let got = sr.to_bytes();
    println!("SR layout hex: {:02x?}", got);
    assert_eq!(got, expected, "RFC 3550 §6.4.1 byte-exact SR");
    assert_eq!(got.len(), 28);
    assert_eq!(got[0] >> 6, 2, "V=2");
    assert_eq!(got[0] & 0x20, 0, "P=0");
    assert_eq!(got[0] & 0x1F, 0, "RC=0");
    assert_eq!(got[1], PT_SR);
    assert_eq!(u16::from_be_bytes([got[2], got[3]]), SR_LENGTH_FIELD);
    assert_eq!(SR_LENGTH_FIELD, 6, "7 words total minus one");
}

#[test]
fn build_from_track_state_reports_wire_truth_counts_and_ts() {
    let mut track = RtState::with_ssrc(0xCAFE_BABE);
    assert!(track.observe_rtp_packet(&rtp_packet(26, 0, 900, 0xCAFE_BABE, 1400)));
    assert!(track.observe_rtp_packet(&rtp_packet(26, 1, 900, 0xCAFE_BABE, 1400)));
    let wallclock = UNIX_EPOCH + Duration::from_secs(1_000_000);
    let sr = build_sender_report(&track, wallclock);
    assert_eq!(sr.ssrc, 0xCAFE_BABE);
    assert_eq!(sr.rtp_timestamp, 900, "latest on-wire RTP timestamp");
    assert_eq!(sr.packet_count, 2);
    assert_eq!(sr.octet_count, 2800, "payload octets only (header excluded)");
    let b = sr.to_bytes();
    assert_eq!(&b[16..20], &900u32.to_be_bytes());
    assert_eq!(&b[20..24], &2u32.to_be_bytes());
    assert_eq!(&b[24..28], &2800u32.to_be_bytes());
}

// ---------------------------------------------------------------------------
// 2. NTP 64-bit encoding from fixed SystemTime instants
// ---------------------------------------------------------------------------

#[test]
fn ntp_field_maps_fixed_systemtime_with_era2000_offset() {
    // The documented constant: NTP epoch 1900-01-01, UNIX epoch 1970-01-01.
    assert_eq!(
        NTP_UNIX_EPOCH_OFFSET_SECS,
        2_208_988_800,
        "70 years incl. 17 leap days = 25 567 days x 86 400 s"
    );

    // Era-2000 anchor instant: 2000-01-01T00:00:00Z = 946 684 800 unix s.
    let y2k = UNIX_EPOCH + Duration::from_secs(946_684_800);
    let (secs, frac) = system_time_to_ntp(y2k);
    assert_eq!(
        secs, 3_155_673_600,
        "946684800 + 2208988800"
    );
    assert_eq!(secs, 0xBC17_C200, "same value in hex");
    assert_eq!(frac, 0, "exactly on the second");

    // Half a second is exactly 2^31 in the 32-bit fraction word.
    let half = UNIX_EPOCH + Duration::new(1_000_000, 500_000_000);
    assert_eq!(
        system_time_to_ntp(half),
        (1_000_000 + 2_208_988_800, 0x8000_0000)
    );

    // A quarter second: floor(0.25 * 2^32) = 0x40000000.
    let quarter = UNIX_EPOCH + Duration::new(0, 250_000_000);
    assert_eq!(system_time_to_ntp(quarter), (2_208_988_800, 0x4000_0000));

    // A 2026 instant (well inside NTP era 0, far from the 2036 wrap).
    let t2026 = UNIX_EPOCH + Duration::from_secs(1_791_000_000);
    assert_eq!(
        system_time_to_ntp(t2026).0,
        (1_791_000_000u64 + 2_208_988_800) as u32
    );
}

// ---------------------------------------------------------------------------
// 3. Explicit 32-bit wrap + continuity of the reported time
// ---------------------------------------------------------------------------

#[test]
fn wrap_delta_is_explicit_at_ffffffff_to_zero() {
    // The exact boundary the plan names in its QA-failure row.
    assert_eq!(rtp_delta_units(0xFFFF_FFFF, 0), 1);
    assert_eq!(rtp_delta_units(0xFFFF_FFFE, 0), 2);
    assert_eq!(rtp_delta_units(u32::MAX - 1, 0), 2);
    assert_eq!(rtp_delta_units(0, 0xFFFF_FFFF), u32::MAX, "backward wrap distance");
    // Microsecond form at the video clock: 1 unit = 1/90000 s -> 11 us floor.
    assert_eq!(rtp_delta_us(0xFFFF_FFFF, 0, CLOCK_RATE_VIDEO_HZ), 11);
    // 900 units (one 100 ms video frame) across the boundary -> exactly 10 ms.
    assert_eq!(
        rtp_delta_us(0xFFFF_FFFF - 899, 0, CLOCK_RATE_VIDEO_HZ),
        10_000
    );
    // Audio clock: 160 units (one 20 ms frame) across the wrap -> exactly
    // 20 ms (0x1_0000_0000 - 160 = 0xFFFF_FF60 is the boundary-crossing start).
    assert_eq!(rtp_delta_units(0xFFFF_FF60, 0), 160);
    assert_eq!(rtp_delta_us(0xFFFF_FF60, 0, CLOCK_RATE_AUDIO_HZ), 20_000);
}

#[test]
fn reported_time_is_continuous_across_the_wrap_boundary() {
    let mut elapsed = RtpElapsed::new();
    // Approach the boundary at 90 kHz in 900-unit (100 ms) steps.
    let base = 0xFFFF_F000u32;
    assert_eq!(elapsed.push(base), 0);
    let mut expected_units = 0u64;
    let mut ts = base;
    for _ in 0..4 {
        ts = ts.wrapping_add(900);
        expected_units += 900;
        assert_eq!(elapsed.push(ts), expected_units, "before the wrap");
    }
    // ts is now 0xFFFF_F000 + 3600 = 0xFFFF_FE10. Walk across 0xFFFFFFFF -> 0
    // explicitly: 0x1F0 units later we land at 0x0000_0000, then keep going.
    ts = 0xFFFF_FFFF;
    expected_units += u64::from(rtp_delta_units(0xFFFF_FE10, ts));
    assert_eq!(elapsed.push(ts), expected_units, "last unit before the wrap");
    ts = 0;
    expected_units += 1; // THE boundary: 0xFFFF_FFFF -> 0 is +1, never a jump
    assert_eq!(elapsed.push(ts), expected_units, "continuity at 0xFFFF_FFFF -> 0");
    ts = 900;
    expected_units += 900;
    assert_eq!(elapsed.push(ts), expected_units, "after the wrap");

    // The reported time never regresses and matches naive-free arithmetic.
    assert_eq!(elapsed.total_units(), expected_units);
    assert_eq!(
        elapsed.elapsed_us(CLOCK_RATE_VIDEO_HZ),
        expected_units * 1_000_000 / 90_000
    );
    // Proof that plain arithmetic would have failed here: naive signed
    // subtraction at the boundary is negative, wrapping_sub is +1.
    assert!(
        i64::from(0) - i64::from(0xFFFF_FFFFu32) < 0,
        "naive goes negative"
    );
    assert_eq!(rtp_delta_units(0xFFFF_FFFF, 0), 1, "explicit wrap does not");
}

#[test]
fn counter_wraps_at_u32_max_without_panic_rfc3550_permits_it() {
    let mut track = RtState {
        packets: u32::MAX,
        octets: u32::MAX,
        ..RtState::with_ssrc(1)
    };
    assert!(track.observe_rtp_packet(&rtp_packet(0, 0, 0, 1, 160)));
    assert_eq!(track.packets, 0, "wrapping_add, RFC 3550 allows the u32 wrap");
    assert_eq!(track.octets, 159, "MAX + 160 wraps to 159");
    // Sub-header packets are refused, state untouched (never chokes).
    assert!(!track.observe_rtp_packet(&[0u8; 11]));
}

// ---------------------------------------------------------------------------
// 4. The 5 s scheduler decision fn
// ---------------------------------------------------------------------------

#[test]
fn scheduler_fires_at_five_second_boundaries_and_never_early() {
    assert!(sr_due(0, None), "first SR of a session: always due");
    assert!(sr_due(u64::MAX, None), "teardown force (last=None): always due");
    // Not early: 0..=4999 ms after the last SR.
    for now in [1u64, 1_000, 4_998, 4_999] {
        assert!(!sr_due(now, Some(0)), "{now} ms is early");
    }
    // Boundary: exactly 5000 ms and everything after.
    for now in [5_000u64, 5_001, 9_999, 10_000] {
        assert!(sr_due(now, Some(0)), "{now} ms is due");
    }
    // Offset window: last sent at t=1000.
    assert!(!sr_due(5_999, Some(1_000)), "4999 ms elapsed: not early");
    assert!(sr_due(6_000, Some(1_000)), "5000 ms elapsed: due");
    // Monotonic anomaly (now < last) can only delay, never send early.
    assert!(!sr_due(100, Some(1_000)), "clock anomaly must not fire early");
    assert!(sr_due(u64::MAX, Some(0)), "saturation can only delay");
}

// ---------------------------------------------------------------------------
// 5. Suppression predicate — unsynced => no bytes, exact log line
// ---------------------------------------------------------------------------

#[test]
fn unsynced_suppression_emits_no_bytes_and_the_exact_log_line() {
    let sample = SyncSample::unsynced();
    let track = RtState::with_ssrc(0xDEAD_0001);

    // Overdue by hours — suppression must win over "due".
    let action = sr_action(9_999_999, Some(0), &sample, None, &track);
    match &action {
        SrAction::Suppress(line) => assert_eq!(*line, "unsynced, SR suppressed"),
        other => panic!("expected Suppress, got {other:?}"),
    }
    assert_eq!(SUPPRESSED_LOG, "unsynced, SR suppressed", "plan-grepped line");

    // Defensive: may_send_rtcp()=true but no wallclock still suppresses —
    // there is no branch that invents a time.
    let odd = SyncSample::new(true, None);
    assert_eq!(
        sr_action(9_999_999, Some(0), &odd, None, &track),
        SrAction::Suppress(SUPPRESSED_LOG)
    );

    // Belt and braces: a wallclock passed alongside may_send_rtcp()=false
    // does NOT unlock sending.
    let liar = SyncSample::new(false, Some(UNIX_EPOCH));
    assert_eq!(
        sr_action(9_999_999, None, &liar, Some(UNIX_EPOCH), &track),
        SrAction::Suppress(SUPPRESSED_LOG)
    );
}

#[test]
fn sr_clock_never_fabricates_when_unsynced() {
    let mut clock = SrClock::new();
    assert_eq!(clock.poll(SyncSample::unsynced(), 0), None);
    assert_eq!(clock.poll(SyncSample::unsynced(), 60_000), None);

    // Synced: anchor = the SNTP value itself, then monotonic advancement.
    let sync = UNIX_EPOCH + Duration::from_secs(1_791_000_000);
    let synced = SyncSample::new(true, Some(sync));
    assert_eq!(clock.poll(synced.clone(), 1_000), Some(sync));
    assert_eq!(
        clock.poll(synced.clone(), 6_000),
        Some(sync + Duration::from_millis(5_000)),
        "monotonic advances the SNTP anchor — never replaces it"
    );

    // Resync (60-minute poll) changes last_sync_utc -> re-anchor at the new
    // value: keeps reported NTP within one poll interval of last_sync_utc
    // (the plan acceptance) and corrects accumulated drift.
    let sync2 = sync + Duration::from_secs(3_600);
    let resynced = SyncSample::new(true, Some(sync2));
    assert_eq!(clock.poll(resynced, 7_000), Some(sync2));

    // Losing sync clears the anchor: back to None, and re-observation of the
    // SAME sync value re-anchors fresh (no stale elapsed carried across).
    assert_eq!(clock.poll(SyncSample::unsynced(), 8_000), None);
    assert_eq!(clock.poll(synced.clone(), 9_000), Some(sync));
}

#[test]
fn synced_and_due_produces_the_exact_sr_bytes() {
    let sample = SyncSample::new(true, Some(UNIX_EPOCH + Duration::from_secs(1_000_000)));
    let mut clock = SrClock::new();
    let wallclock = clock.poll(sample.clone(), 0);
    let mut track = RtState::with_ssrc(0x1234_5678);
    track.observe_rtp_packet(&rtp_packet(0, 7, 160, 0x1234_5678, 160));

    // Not due -> Wait, no bytes.
    assert_eq!(
        sr_action(4_999, Some(0), &sample, wallclock, &track),
        SrAction::Wait
    );
    // Due -> Send: exactly the RFC 3550 layout built from this state.
    let action = sr_action(5_000, Some(0), &sample, wallclock, &track);
    let bytes = match action {
        SrAction::Send(b) => b,
        other => panic!("expected Send, got {other:?}"),
    };
    assert_eq!(bytes.len(), 28);
    assert_eq!(bytes[0], 0x80);
    assert_eq!(bytes[1], 200);
    assert_eq!(u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]), 0x1234_5678);
    // NTP seconds = 1_000_000 + 2_208_988_800 = 2_209_988_800 = 0x83B9_C0C0.
    assert_eq!(
        u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        2_209_988_800
    );
    assert_eq!(u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]), 160);
    assert_eq!(u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]), 1);
    assert_eq!(u32::from_be_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]), 160);
}

// ---------------------------------------------------------------------------
// 6. counting_sink — per-track counters from the wire
// ---------------------------------------------------------------------------

#[test]
fn counting_sink_tracks_packets_and_octets_then_forwards() {
    let state = shared_state(0);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    let mut sink = counting_sink(Arc::clone(&state), move |p| {
        seen2.lock().unwrap().push(p.len());
        Ok(())
    });

    sink(&rtp_packet(26, 0, 0, 0xABCD, 100)).unwrap();
    sink(&rtp_packet(26, 1, 900, 0xABCD, 50)).unwrap();
    // Forward failure must not update counts.
    let mut failing = counting_sink(Arc::clone(&state), |_p: &[u8]| -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    });
    assert!(failing(&rtp_packet(26, 2, 1800, 0xABCD, 10)).is_err());

    let guard = state.lock().unwrap();
    assert_eq!(guard.packets, 2, "counted only successfully sent packets");
    assert_eq!(guard.octets, 150);
    assert_eq!(guard.seq, 1, "latest on-wire seq");
    assert_eq!(guard.rtptime, 900);
    assert_eq!(guard.ssrc, 0xABCD);
    drop(guard);
    assert_eq!(*seen.lock().unwrap(), vec![112, 62], "both forwarded");
}

// ---------------------------------------------------------------------------
// 7. Incoming Receiver Reports — parse, describe, never choke
// ---------------------------------------------------------------------------

fn compound_sr_rr() -> Vec<u8> {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(
        &SenderReport {
            ssrc: 0xCAFE_0001,
            ntp_seconds: 1,
            ntp_fraction: 2,
            rtp_timestamp: 3,
            packet_count: 4,
            octet_count: 5,
        }
        .to_bytes(),
    );
    // RR: V=2, RC=1, PT=201, length=7 -> 32 bytes; SSRC + one 24-byte block.
    pkt.extend_from_slice(&[0x81, PT_RR, 0x00, 0x07]);
    pkt.extend_from_slice(&0xCAFE_0002u32.to_be_bytes());
    pkt.extend_from_slice(&[0u8; 24]);
    pkt
}

#[test]
fn receiver_report_parses_and_describes_with_documented_scope() {
    let parsed = parse_rtcp(&compound_sr_rr()).expect("compound parses");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0], RtcpSummary::SenderReport { ssrc: 0xCAFE_0001 });
    assert_eq!(
        parsed[1],
        RtcpSummary::ReceiverReport { ssrc: 0xCAFE_0002, blocks: 1 }
    );
    let line = describe_rtcp(&parsed);
    assert!(line.contains("RR ssrc=0xcafe0002 blocks=1"), "{line}");
    assert!(
        line.contains("logged only"),
        "scope limit must be visible in the log: {line}"
    );

    // SDES/BYE/unknown PT are walked past without choking.
    let mut with_bye = compound_sr_rr();
    with_bye.extend_from_slice(&[0x81, 203, 0x00, 0x01, 0, 0, 0, 0]); // BYE, len=1
    let parsed = parse_rtcp(&with_bye).expect("compound with BYE parses");
    assert_eq!(parsed.len(), 3);
    assert!(matches!(parsed[2], RtcpSummary::Other { pt: 203, .. }));
}

#[test]
fn malformed_rtcp_is_a_typed_error_never_a_panic() {
    assert_eq!(parse_rtcp(&[]), Err(RtcpParseError::Empty));
    assert_eq!(
        parse_rtcp(&[0x40, PT_RR, 0x00, 0x00]), // V=1
        Err(RtcpParseError::BadVersion { version: 1 })
    );
    // length claims 60 bytes (14 words + 1), datagram has 4.
    assert_eq!(
        parse_rtcp(&[0x80, PT_RR, 0x00, 0x0E]),
        Err(RtcpParseError::Truncated { needed: 60, have: 4 })
    );
    // Valid header followed by a dangling byte.
    let mut pkt = compound_sr_rr();
    pkt.push(0x00);
    assert!(matches!(
        parse_rtcp(&pkt),
        Err(RtcpParseError::Truncated { .. })
    ));
    assert!(!format!("{}", RtcpParseError::Empty).is_empty());
}

// ---------------------------------------------------------------------------
// 8. spawn_sender_reports — device loop, teardown SR, unsynced silence
// ---------------------------------------------------------------------------

#[test]
fn spawn_emits_layout_valid_srs_including_the_teardown_report() {
    let video = shared_state(0xDEAD_BEEF);
    let audio = shared_state(0x0BAD_F00D);
    video
        .lock()
        .unwrap()
        .observe_rtp_packet(&rtp_packet(26, 0, 900, 0xDEAD_BEEF, 1400));
    audio
        .lock()
        .unwrap()
        .observe_rtp_packet(&rtp_packet(0, 0, 160, 0x0BAD_F00D, 160));

    let sent_v: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sent_a: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sv = Arc::clone(&sent_v);
    let sa = Arc::clone(&sent_a);
    let sync_value = SyncSample::new(true, Some(UNIX_EPOCH + Duration::from_secs(1_000_000)));

    let (sr_loop, join) = spawn_sender_reports(
        Arc::clone(&video),
        Arc::clone(&audio),
        move |p| {
            sv.lock().unwrap().push(p.to_vec());
            Ok(())
        },
        move |p| {
            sa.lock().unwrap().push(p.to_vec());
            Ok(())
        },
        move || sync_value.clone(),
    )
    .expect("spawn");

    // Let the immediate first tick run, then request teardown.
    thread::sleep(Duration::from_millis(120));
    sr_loop.stop();
    assert!(sr_loop.is_stopped());
    join.join().expect("loop thread exits cleanly");

    for (label, sent, ssrc, rtp_ts, pkts, octets) in [
        ("video", &sent_v, 0xDEAD_BEEF, 900u32, 1u32, 1400u32),
        ("audio", &sent_a, 0x0BAD_F00D, 160u32, 1u32, 160u32),
    ] {
        let guard = sent.lock().unwrap();
        // First tick (session start) + forced teardown report; a stop that
        // raced the very first tick degrades to exactly one — both valid.
        assert!(
            (1..=2).contains(&guard.len()),
            "{label}: expected the immediate and teardown SRs, got {}",
            guard.len()
        );
        for sr in guard.iter() {
            assert_eq!(sr.len(), 28, "{label} SR must be RFC 3550 §6.4.1 size");
            assert_eq!(sr[0], 0x80, "{label}: V=2 P=0 RC=0");
            assert_eq!(sr[1], 200, "{label}: PT=200");
            assert_eq!(&sr[2..4], &[0x00, 0x06], "{label}: length=6");
            assert_eq!(
                u32::from_be_bytes([sr[4], sr[5], sr[6], sr[7]]),
                ssrc,
                "{label} SSRC"
            );
            // NTP seconds: 1_000_000 + 2_208_988_800 (anchor = sync value;
            // the loop's first poll anchors at mono≈0, so elapsed is 0 s and
            // the seconds word is exact — sub-second drift cannot move it).
            assert_eq!(
                u32::from_be_bytes([sr[8], sr[9], sr[10], sr[11]]),
                2_209_988_800,
                "{label} NTP = SNTP anchor, never synthesised"
            );
            assert_eq!(
                u32::from_be_bytes([sr[16], sr[17], sr[18], sr[19]]),
                rtp_ts,
                "{label} RTP ts"
            );
            assert_eq!(u32::from_be_bytes([sr[20], sr[21], sr[22], sr[23]]), pkts);
            assert_eq!(u32::from_be_bytes([sr[24], sr[25], sr[26], sr[27]]), octets);
        }
    }
}

#[test]
fn spawn_sends_zero_bytes_when_unsynced() {
    let video = shared_state(1);
    let audio = shared_state(2);
    video
        .lock()
        .unwrap()
        .observe_rtp_packet(&rtp_packet(26, 0, 0, 1, 100));

    let sent_v: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sent_a: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sv = Arc::clone(&sent_v);
    let sa = Arc::clone(&sent_a);

    let (sr_loop, join) = spawn_sender_reports(
        Arc::clone(&video),
        Arc::clone(&audio),
        move |p| {
            sv.lock().unwrap().push(p.to_vec());
            Ok(())
        },
        move |p| {
            sa.lock().unwrap().push(p.to_vec());
            Ok(())
        },
        || SyncSample::unsynced(),
    )
    .expect("spawn");

    thread::sleep(Duration::from_millis(120));
    sr_loop.stop();
    join.join().expect("loop thread exits cleanly");

    // Cardinal rule: unsynced means ZERO SR bytes on both tracks — even at
    // the forced teardown tick. The "unsynced, SR suppressed" line is printed
    // by the loop (visible with --nocapture) and asserted verbatim at the
    // decision level in unsynced_suppression_emits_no_bytes_and_the_exact_log_line.
    assert!(
        sent_v.lock().unwrap().is_empty(),
        "video track must emit nothing while unsynced"
    );
    assert!(
        sent_a.lock().unwrap().is_empty(),
        "audio track must emit nothing while unsynced"
    );
}