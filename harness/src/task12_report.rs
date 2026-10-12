//! Todo 12 evidence driver (plan: `.omo/evidence/task-12/`).
//!
//! Commands:
//!   task12_report compander          256-code µ-law round-trip table with
//!                                    per-code G.711 bounds + MIN/MAX rails
//!                                    → compander-tests.txt
//!   task12_report filter             swept-sine half-band response numbers
//!   task12_report pt0                real audio packet header hex + PT-0 read
//!   task12_report jpeg-loop <file>   fixture → JpegTrack → $-interleaved →
//!                                    reassemble → byte-identity result
//!   task12_report audio-send <ip:port> <seconds>
//!                                    1 kHz sine → 16 kHz→µ-law pipeline → UDP
//!                                    (for the ffmpeg SDP depacketizer run)
//!   task12_report video-send <file> <ip:port>
//!                                    fixture → JpegTrack → UDP
//!   task12_report wav-check <file>   assert 8000 Hz + measure tone frequency
//!                                    with audio_quality::goertzel_mag
//!
//! Exit code is non-zero on any assertion failure, so `task12_e2e.sh` fails
//! the task on a regression.

#[path = "../../firmware/src/audio_quality.rs"]
mod audio_quality;
#[path = "../../firmware/src/jfif.rs"]
mod jfif;
#[path = "../../firmware/src/rtsp/payload/mod.rs"]
mod payload;
#[path = "../../firmware/src/rtsp/mod.rs"]
mod rtsp;
#[path = "../../firmware/src/session.rs"]
mod session;

use audio_codec_algorithms::{decode_ulaw, encode_ulaw};
use jfif::Frame;
use payload::audio_track::AudioTrack;
use payload::halfband;
use payload::jpeg_track::JpegTrack;
use std::net::UdpSocket;
use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let outcome = match args.get(1).map(String::as_str) {
        Some("compander") => compander(),
        Some("filter") => filter(),
        Some("pt0") => pt0(),
        Some("jpeg-loop") => jpeg_loop(args.get(2).map(String::as_str).unwrap_or("")),
        Some("rtsp-loop") => rtsp_loop(),
        Some("audio-send") => audio_send(
            args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3).and_then(|s| s.parse::<f64>().ok()),
        ),
        Some("video-send") => video_send(
            args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3).map(String::as_str).unwrap_or(""),
        ),
        Some("wav-check") => wav_check(args.get(2).map(String::as_str).unwrap_or("")),
        _ => Err(
            "usage: task12_report compander|filter|pt0|jpeg-loop <file>|rtsp-loop|audio-send <ip:port> <sec>|video-send <file> <ip:port>|wav-check <file>"
                .into(),
        ),
    };
    match outcome {
        Ok(()) => {
            println!("TASK12: PASS");
        }
        Err(e) => {
            eprintln!("TASK12: FAIL — {e}");
            exit(1);
        }
    }
}

/// Per-code G.711 bound: half the quantisation segment step (steps double per
/// segment: 8·2^segment, verified against the G.711 decode table), or the
/// clipping distance 644 = 32768 − 32124 for the two saturation codes.
fn g711_bound(code: u8) -> (i32, u8, bool) {
    let word = (code ^ 0xFF) & 0x7F;
    let segment = word >> 4;
    if word == 0x7F {
        (644, segment, true)
    } else {
        (1 << (segment + 2), segment, false)
    }
}

fn compander() -> Result<(), String> {
    println!("task-12 compander evidence — audio-codec-algorithms 0.8.1 encode_ulaw/decode_ulaw");
    println!("bound model: |decode(encode(x)) - x| <= half segment step = 1<<(segment+2);");
    println!("saturation codes (word 0x7F) bounded by clipping distance 32768-32124 = 644");
    println!();
    println!("code  decode      reencode  roundtrip  max_err  bound  sat  segment");

    // Exhaustive i16 sweep: per-code worst error + reachability.
    let mut max_err = [0i32; 256];
    let mut hit = [false; 256];
    for x in i32::from(i16::MIN)..=i32::from(i16::MAX) {
        let x = x as i16;
        let c = encode_ulaw(x);
        hit[c as usize] = true;
        let err = (decode_ulaw(c) as i32 - x as i32).abs();
        if err > max_err[c as usize] {
            max_err[c as usize] = err;
        }
    }

    let mut violations = 0;
    let mut rt_bad = 0;
    for c in 0u16..256 {
        let c = c as u8;
        let d = decode_ulaw(c);
        let c2 = encode_ulaw(d);
        let rt = c2 == c;
        // 0x7F is µ-law's duplicate digital zero: both 0x7F and 0xFF decode
        // to 0, so 0x7F canonicalises to 0xFF — the ONLY non-identity pair.
        let rt_ok = rt || (c == 0x7F && c2 == 0xFF);
        let stab = decode_ulaw(c2) == d;
        if !rt_ok || !stab {
            rt_bad += 1;
        }
        let (bound, segment, sat) = g711_bound(c);
        let err = max_err[c as usize];
        if err > bound {
            violations += 1;
        }
        let mark = if rt { "ident" } else { "0x7F->FF" };
        println!(
            "{c:#04x}  {d:>10}  {c2:#08}  {mark:>8}  {err:>7}  {bound:>5}  {:>3}  {}",
            if sat { "yes" } else { " - " },
            segment
        );
    }

    println!();
    println!("codes reached by encode over all 65536 i16 inputs: {}", hit.iter().filter(|&&h| h).count());
    println!("G.711 bound violations: {violations}");
    println!("round-trip/stability failures: {rt_bad}");
    println!();
    println!("MIN/MAX rails (explicit no-panic proof):");
    for (label, v) in [("i16::MIN", i16::MIN), ("i16::MAX", i16::MAX)] {
        let c = encode_ulaw(v);
        let d = decode_ulaw(c);
        let (bound, _, _) = g711_bound(c);
        let err = (d as i32 - v as i32).abs();
        println!(
            "  {label} = {v} -> encode {c:#04x} -> decode {d} (err {err} <= bound {bound})"
        );
        if err > bound {
            violations += 1;
        }
    }

    if violations == 0 && rt_bad == 0 && hit.iter().all(|&h| h) {
        println!("COMPANDER: 256/256 codes within G.711 bounds, all reachable, no panics");
        Ok(())
    } else {
        Err(format!("violations={violations} rt_bad={rt_bad}"))
    }
}

fn filter() -> Result<(), String> {
    println!("task-12 half-band swept-sine (fs=16000 in, 8000 out; 33-tap Kaiser sinc, beta=3.66)");
    let r = halfband::swept_sine_report();
    println!();
    println!("passband (gain vs unity dB):");
    let mut line = String::new();
    for (i, (f, g)) in r.passband.iter().enumerate() {
        line.push_str(&format!("{f:>5} Hz: {g:+.4}   "));
        if i % 6 == 5 {
            println!("{line}");
            line.clear();
        }
    }
    if !line.is_empty() {
        println!("{line}");
    }
    let (pf, pd) = r.worst_passband();
    println!(
        "worst passband deviation: {:.4} dB @ {} Hz (point value {pd:+.4} dB)  (plan bound: <= 0.5 dB)  {}",
        r.max_passband_abs_dev_db(),
        pf,
        if r.max_passband_abs_dev_db() <= 0.5 { "PASS" } else { "FAIL" }
    );

    println!();
    println!("stopband (attenuation dB, positive = rejection):");
    let mut line = String::new();
    for (i, (f, a)) in r.stopband.iter().enumerate() {
        line.push_str(&format!("{f:>5} Hz: {a:.2}   "));
        if i % 6 == 5 {
            println!("{line}");
            line.clear();
        }
    }
    if !line.is_empty() {
        println!("{line}");
    }
    let (sf, sa) = r.worst_stopband();
    println!(
        "worst stopband attenuation: {sa:.2} dB @ {sf} Hz  (plan bound: >= 40 dB from 4.6 kHz)  {}",
        if sa >= 40.0 { "PASS" } else { "FAIL" }
    );

    if r.max_passband_abs_dev_db() > 0.5 {
        return Err(format!("passband ripple {:.4} dB > 0.5", r.max_passband_abs_dev_db()));
    }
    if sa < 40.0 {
        return Err(format!("stopband {sa:.2} dB < 40"));
    }
    Ok(())
}

fn pt0() -> Result<(), String> {
    let mut t = AudioTrack::new(0xC0FF_EE00);
    let mut phase = 0.0f64;
    let mut frame = [0i16; 320];
    for s in frame.iter_mut() {
        *s = (12000.0 * phase.sin()).round() as i16;
        phase += 2.0 * std::f64::consts::PI * 1000.0 / 16_000.0;
    }
    let pkt = t.encode_frame(&frame);
    let hex: Vec<String> = pkt[..12].iter().map(|b| format!("{b:02x}")).collect();
    println!("task-12 PT=0 proof — first 12 bytes of a real audio RTP packet:");
    println!("  header hex: {}", hex.join(" "));
    let v = pkt[0];
    let pt_byte = pkt[1];
    let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
    let ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
    let ssrc = u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);
    println!("  V/P/X/CC byte = {v:#04x} (must be 0x80)");
    println!("  M|PT byte     = {pt_byte:#04x} (must be 0x00: M=0, PT field literally 0)");
    println!("  seq = {seq}, rtptime = {ts} (expect 0/0 first packet), ssrc = {ssrc:#010x}");
    println!("  packet length = {} bytes (12 header + 160 payload)", pkt.len());
    if v != 0x80 || pt_byte != 0 || seq != 0 || ts != 0 || pkt.len() != 172 {
        return Err("audio RTP header violates the plan contract".into());
    }
    println!("PT=0 READ FROM THE ACTUAL PACKET HEADER: PASS");
    Ok(())
}

fn split_interleaved(stream: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < stream.len() {
        assert_eq!(stream[i], 0x24, "interleaved frame must start with '$'");
        let ch = stream[i + 1];
        let len = u16::from_be_bytes([stream[i + 2], stream[i + 3]]) as usize;
        out.push((ch, stream[i + 4..i + 4 + len].to_vec()));
        i += 4 + len;
    }
    out
}

fn jpeg_loop(path: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let frame = Frame::parse(&bytes).map_err(|e| format!("parse: {e}"))?;
    let scan = frame.scan.to_vec();

    let mut track = JpegTrack::new(0xDEAD_BEEF);
    let mut wire = Vec::new();
    let n = track
        .packetize(&bytes, &mut |pkt| {
            wire.extend_from_slice(&rtsp::interleaved_frame(0, pkt));
            Ok(())
        })
        .map_err(|e| format!("packetize: {e}"))?;
    let frames = split_interleaved(&wire);
    let pkts: Vec<Vec<u8>> = frames.into_iter().map(|(_, p)| p).collect();
    let (rebuilt, r) = reassemble_video(&pkts)?;
    let identical = rebuilt == scan;
    println!("task-12 video integration — fixture {path}");
    println!(
        "  jfif scan = {} bytes, {} RTP packets via JpegTrack -> $-interleaved",
        scan.len(),
        n
    );
    println!(
        "  seq range = {}..={} (starts 0), pt26_all = {}, marker_last_only = {}",
        r.seqs.first().unwrap_or(&0),
        r.seqs.last().unwrap_or(&0),
        r.pt_all_26,
        r.marker_last_only
    );
    println!(
        "  reassembled == scan byte-for-byte: {identical} (Todo 6 gate-(a) contract through the RTSP path)"
    );
    if !identical || !r.pt_all_26 || !r.marker_last_only || r.seqs.first() != Some(&0)
        || track.rtptime() != 900
    {
        return Err("jpeg loopback contract violated".into());
    }
    Ok(())
}

/// Receiver-side reassembly of one frame's RTP/JPEG packets (RFC 2435 offset
/// fields after the 12-byte RTP header — the same derivation as gate6).
struct ReasmStats {
    pt_all_26: bool,
    marker_last_only: bool,
    seqs: Vec<u16>,
    tss: Vec<u32>,
}

fn reassemble_video(pkts: &[Vec<u8>]) -> Result<(Vec<u8>, ReasmStats), String> {
    let mut rebuilt = Vec::new();
    let mut marker_flags = Vec::new();
    let mut seqs = Vec::new();
    let mut tss = Vec::new();
    let mut pt_all_26 = true;
    for (i, pkt) in pkts.iter().enumerate() {
        if pkt.len() < 12 + 8 {
            return Err(format!("packet {i} too short: {}", pkt.len()));
        }
        if pkt[0] != 0x80 {
            return Err(format!("packet {i}: V != 2"));
        }
        pt_all_26 &= pkt[1] & 0x7F == 26;
        marker_flags.push(pkt[1] & 0x80 != 0);
        seqs.push(u16::from_be_bytes([pkt[2], pkt[3]]));
        tss.push(u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]));
        // Header after RTP: Type-specific(1) | Offset(3) | Type(1) | Q(1) | …
        let off =
            ((pkt[13] as usize) << 16) | ((pkt[14] as usize) << 8) | (pkt[15] as usize);
        let r#type = pkt[16];
        let q = pkt[17];
        let mut hlen = 8usize;
        if r#type >= 64 {
            hlen += 4;
        }
        if i == 0 && q == 255 {
            hlen += 4 + 128;
        }
        if off != rebuilt.len() {
            return Err(format!(
                "packet {i}: offset {off} != contiguous {}",
                rebuilt.len()
            ));
        }
        rebuilt.extend_from_slice(&pkt[12 + hlen..]);
    }
    let marker_last_only = marker_flags.last().copied().unwrap_or(false)
        && marker_flags[..marker_flags.len().saturating_sub(1)]
            .iter()
            .all(|&m| !m);
    Ok((
        rebuilt,
        ReasmStats {
            pt_all_26,
            marker_last_only,
            seqs,
            tss,
        },
    ))
}

/// End-to-end RTSP-path proof against the REAL `service::Service` (Todo 11's
/// production wiring): full OPTIONS/DESCRIBE/SETUP(×2)/PLAY handshake over
/// TCP, both payloaders pushed through `RtpSender::send_rtp`, then every
/// asserted byte read back off the socket — PT 0 read from the wire, video
/// scan byte-identical after `$`-frame splitting.
///
/// Runs as a BIN mode on purpose: a non-test build compiles no
/// `#[cfg(test)]` suites, so the session/service shared-static race that
/// affects test targets (see task12_payload's doc header) cannot occur here.
fn rtsp_loop() -> Result<(), String> {
    use rtsp::server::{ConnIo, Track};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const HOST: &str = "rtsp://127.0.0.1/mjpeg/1";

    let mut service = rtsp::service::Service::spawn(0).map_err(|e| e.to_string())?;
    let addr = service.local_addr();
    let mut client = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut leftover: Vec<u8> = Vec::new();

    fn find_header_end(b: &[u8]) -> Option<usize> {
        b.windows(4).position(|w| w == b"\r\n\r\n")
    }

    fn read_response(c: &mut TcpStream, leftover: &mut Vec<u8>) -> Result<String, String> {
        loop {
            if let Some(pos) = find_header_end(leftover) {
                let head = String::from_utf8_lossy(&leftover[..pos]).to_string();
                let mut body_len = 0usize;
                for line in head.split("\r\n") {
                    if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:")
                    {
                        body_len = rest.trim().parse().unwrap_or(0);
                    }
                }
                let total = pos + 4 + body_len;
                if leftover.len() >= total {
                    let resp = String::from_utf8_lossy(&leftover[..total]).to_string();
                    leftover.drain(..total);
                    return Ok(resp);
                }
            }
            let mut buf = [0u8; 4096];
            let n = c.read(&mut buf).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                return Err("connection closed while reading response".into());
            }
            leftover.extend_from_slice(&buf[..n]);
        }
    }

    fn status_of(resp: &str) -> u16 {
        resp.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    fn header_of<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
        let want = name.as_bytes();
        resp.split("\r\n").find_map(|line| {
            let line = line.trim_start();
            if line.len() > want.len()
                && line.as_bytes()[..want.len()].eq_ignore_ascii_case(want)
                && line.as_bytes()[want.len()] == b':'
            {
                Some(line[want.len() + 1..].trim())
            } else {
                None
            }
        })
    }

    fn exchange(
        c: &mut TcpStream,
        leftover: &mut Vec<u8>,
        req: &str,
    ) -> Result<String, String> {
        c.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
        read_response(c, leftover)
    }

    fn read_interleaved(
        c: &mut TcpStream,
        leftover: &mut Vec<u8>,
    ) -> Result<(u8, Vec<u8>), String> {
        loop {
            if leftover.len() >= 4 {
                if leftover[0] != 0x24 {
                    return Err(format!("expected '$' frame, got {:#04x}", leftover[0]));
                }
                let len = u16::from_be_bytes([leftover[2], leftover[3]]) as usize;
                if leftover.len() >= 4 + len {
                    let ch = leftover[1];
                    let pkt = leftover[4..4 + len].to_vec();
                    leftover.drain(..4 + len);
                    return Ok((ch, pkt));
                }
            }
            let mut buf = [0u8; 4096];
            let n = c.read(&mut buf).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                return Err("connection closed while reading interleaved frame".into());
            }
            leftover.extend_from_slice(&buf[..n]);
        }
    }

    let want = |resp: &str, status: u16, what: &str| -> Result<(), String> {
        let got = status_of(resp);
        if got != status {
            return Err(format!("{what}: status {got} != {status}"));
        }
        Ok(())
    };

    let resp = exchange(&mut client, &mut leftover,
        &format!("OPTIONS {HOST} RTSP/1.0\r\nCSeq: 1\r\n\r\n"))?;
    want(&resp, 200, "OPTIONS")?;

    let resp = exchange(&mut client, &mut leftover,
        &format!("DESCRIBE {HOST} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n"))?;
    want(&resp, 200, "DESCRIBE")?;
    if !resp.contains("a=rtpmap:0 PCMU/8000") {
        return Err("SDP missing a=rtpmap:0 PCMU/8000".into());
    }
    if !resp.contains("a=rtpmap:26 JPEG/90000") {
        return Err("SDP missing a=rtpmap:26 JPEG/90000".into());
    }

    let resp = exchange(&mut client, &mut leftover,
        &format!("SETUP {HOST}/trackID=0 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"))?;
    want(&resp, 200, "SETUP video")?;
    let session = header_of(&resp, "Session")
        .ok_or("no Session header")?
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // Audio re-offers interleaved=0-1 (the ffmpeg behaviour) — the server
    // must reassign 2-3 so the tracks cannot collide.
    let resp = exchange(&mut client, &mut leftover,
        &format!("SETUP {HOST}/trackID=1 RTSP/1.0\r\nCSeq: 4\r\nSession: {session}\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"))?;
    want(&resp, 200, "SETUP audio")?;
    let audio_transport = header_of(&resp, "Transport").ok_or("no Transport echo")?;
    if !audio_transport.contains("interleaved=2-3") {
        return Err(format!("audio not reassigned to 2-3: {audio_transport}"));
    }

    let resp = exchange(&mut client, &mut leftover,
        &format!("PLAY {HOST} RTSP/1.0\r\nCSeq: 5\r\nSession: {session}\r\n\r\n"))?;
    want(&resp, 200, "PLAY")?;
    let rtp_info = header_of(&resp, "RTP-Info").ok_or("no RTP-Info")?;
    if !rtp_info.contains("seq=0") || !rtp_info.contains("rtptime=0") {
        return Err(format!("RTP-Info must open at seq=0/rtptime=0: {rtp_info}"));
    }

    let (video_sender, audio_sender) = {
        let srv = service.server();
        let srv = srv.lock().unwrap_or_else(|p| p.into_inner());
        (
            srv.sender(Track::Video).ok_or("no video sender")?,
            srv.sender(Track::Audio).ok_or("no audio sender")?,
        )
    };

    // --- video: fixture JPEG through JpegTrack → real RtpSender ---
    let jpeg_path = "fixtures/qvga_422_dyn.jpg";
    let jpeg = std::fs::read(jpeg_path).map_err(|e| format!("read {jpeg_path}: {e}"))?;
    let frame = Frame::parse(&jpeg).map_err(|e| format!("parse: {e}"))?;
    let mut vtrack = JpegTrack::new(0x1111_2222);
    let n_video = vtrack
        .packetize(&jpeg, &mut |p| video_sender.send_rtp(p))
        .map_err(|e| format!("video packetize: {e}"))?;

    // --- audio: one 1 kHz mic frame through AudioTrack → real RtpSender ---
    let mut atrack = AudioTrack::new(0x3333_4444);
    let mut phase = 0.0f64;
    let mut mic = [0i16; 320];
    for s in mic.iter_mut() {
        *s = (12000.0 * phase.sin()).round() as i16;
        phase += 2.0 * std::f64::consts::PI * 1000.0 / 16_000.0;
    }
    // Encode ONCE — the first packet of the session carries seq 0 / rtptime 0
    // (RTP-Info contract); copying detaches the borrow before send_rtp.
    let pkt_copy: Vec<u8> = atrack.encode_frame(&mic).to_vec();
    let audio_pkt_len = pkt_copy.len();
    audio_sender
        .send_rtp(&pkt_copy)
        .map_err(|e| format!("audio send: {e}"))?;
    if atrack.seq() != 1 || atrack.rtptime() != 160 {
        return Err(format!(
            "track bookkeeping: seq {} rtptime {} (want 1/160 after one packet)",
            atrack.seq(),
            atrack.rtptime()
        ));
    }

    // --- read back what actually crossed the socket ---
    let mut video_bytes = Vec::new();
    let mut got = 0usize;
    while got < n_video {
        let (ch, pkt) = read_interleaved(&mut client, &mut leftover)?;
        if ch != 0 {
            return Err(format!("video RTP on channel {ch}, expected 0"));
        }
        video_bytes.extend_from_slice(&rtsp::interleaved_frame(ch, &pkt));
        got += 1;
    }
    let (ch, pkt) = read_interleaved(&mut client, &mut leftover)?;
    if ch != 2 {
        return Err(format!("audio RTP on channel {ch}, expected 2 (reassigned)"));
    }

    // THE acceptance assertion: PT byte read from the wire.
    if pkt.len() != audio_pkt_len || pkt.len() != 172 {
        return Err(format!("audio packet len {} != 172", pkt.len()));
    }
    if pkt[0] != 0x80 {
        return Err(format!("audio V byte {:#04x} != 0x80", pkt[0]));
    }
    if pkt[1] != 0 {
        return Err(format!(
            "audio M|PT byte = {:#04x}, must be 0x00 (payload type literally 0)",
            pkt[1]
        ));
    }
    let a_seq = u16::from_be_bytes([pkt[2], pkt[3]]);
    let a_ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
    let a_ssrc = u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);
    if a_seq != 0 || a_ts != 0 {
        return Err(format!(
            "first audio packet must carry seq 0 / rtptime 0 (RTP-Info), got {a_seq}/{a_ts}"
        ));
    }
    if a_ssrc != 0x3333_4444 {
        return Err(format!("audio SSRC {a_ssrc:#010x} != 0x33334444"));
    }
    println!("audio over RtpSender: wire header = 80 00 seq=0 rtptime=0 ssrc=0x33334444 len=172 — PT 0 CONFIRMED ON THE WIRE");

    // Video: split the $-frames, reassemble, compare byte-for-byte.
    let frames = split_interleaved(&video_bytes);
    let pkts: Vec<Vec<u8>> = frames.into_iter().map(|(_, p)| p).collect();
    if pkts.len() != n_video {
        return Err(format!("{} video packets on wire != {n_video} sent", pkts.len()));
    }
    let (rebuilt, r) = reassemble_video(&pkts)?;
    if rebuilt != frame.scan {
        return Err(format!(
            "video scan mismatch after RTSP round trip: {} != {} bytes",
            rebuilt.len(),
            frame.scan.len()
        ));
    }
    if !r.pt_all_26 || !r.marker_last_only || r.seqs.first() != Some(&0) {
        return Err(format!(
            "video contract: pt26={} marker_last={} first_seq={:?}",
            r.pt_all_26, r.marker_last_only, r.seqs.first()
        ));
    }
    println!(
        "video over RtpSender: {} packets, scan byte-identical ({} bytes), marker last-only, PT 26, seq from 0",
        pkts.len(),
        rebuilt.len()
    );

    let resp = exchange(&mut client, &mut leftover,
        &format!("TEARDOWN {HOST} RTSP/1.0\r\nCSeq: 6\r\nSession: {session}\r\n\r\n"))?;
    want(&resp, 200, "TEARDOWN")?;
    drop(client);
    service.shutdown();
    println!("RTSP-PATH LOOPBACK (Service + RtpSender + interleaved): PASS");
    Ok(())
}

fn audio_send(addr: &str, seconds: Option<f64>) -> Result<(), String> {
    let seconds = seconds.ok_or("audio-send requires <ip:port> <seconds>")?;
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;

    let mut track = AudioTrack::new(0x0A0B_0C0D);
    let mut phase = 0.0f64;
    let frames = (seconds * 50.0).round() as u32;
    let mut sent = 0usize;
    let mut bytes = 0usize;
    for _ in 0..frames {
        let mut frame = [0i16; 320];
        for s in frame.iter_mut() {
            *s = (12000.0 * phase.sin()).round() as i16;
            phase += 2.0 * std::f64::consts::PI * 1000.0 / 16_000.0;
        }
        let pkt = track.encode_frame(&frame);
        let n = sock.send(pkt).map_err(|e| format!("send: {e}"))?;
        assert_eq!(n, pkt.len());
        sent += 1;
        bytes += n;
    }
    println!(
        "audio-send: sent={sent} packets, {bytes} bytes, {seconds}s of 1 kHz sine @16 kHz -> decimate -> PCMU PT0 -> {addr}"
    );
    Ok(())
}

fn video_send(path: &str, addr: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    let mut track = JpegTrack::new(0x0A0B_0C0D);
    let n = track
        .packetize(&bytes, &mut |pkt| {
            sock.send(pkt)?;
            Ok(())
        })
        .map_err(|e| format!("packetize: {e}"))?;
    println!("video-send: {n} RTP packets (PT 26, rtptime {track_rtptime} = 0 first frame) -> {addr}",
        track_rtptime = track.rtptime() - 900);
    Ok(())
}

/// Minimal RIFF/WAVE reader (plan MUST NOT: no `hound` crate) + tone check
/// with Todo 8's `goertzel_mag` (read-only use).
fn read_wav(path: &str) -> Result<(u32, Vec<i16>), String> {
    let b = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    if b.len() < 44 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut pos = 12usize;
    let mut fmt = None;
    let mut data = None;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let len = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + len).min(b.len());
        if id == b"fmt " && len >= 16 {
            let format = u16::from_le_bytes([b[body_start], b[body_start + 1]]);
            let channels = u16::from_le_bytes([b[body_start + 2], b[body_start + 3]]);
            let rate =
                u32::from_le_bytes([b[body_start + 4], b[body_start + 5], b[body_start + 6], b[body_start + 7]]);
            let bits = u16::from_le_bytes([b[body_start + 14], b[body_start + 15]]);
            if format != 1 {
                return Err(format!("WAV format {format} != PCM(1)"));
            }
            if channels != 1 {
                return Err(format!("WAV channels {channels} != 1"));
            }
            if bits != 16 {
                return Err(format!("WAV bits {bits} != 16"));
            }
            fmt = Some(rate);
        } else if id == b"data" {
            data = Some(&b[body_start..body_end]);
        }
        pos = body_start + len + (len & 1); // chunks are word-aligned
    }
    let rate = fmt.ok_or("no fmt chunk")?;
    let data = data.ok_or("no data chunk")?;
    if data.len() < 2 {
        return Err("empty data chunk".into());
    }
    let samples: Vec<i16> = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok((rate, samples))
}

fn wav_check(path: &str) -> Result<(), String> {
    let (rate, samples) = read_wav(path)?;
    println!("task-12 wav-check — {path}");
    println!("  sample_rate = {rate} Hz, channels = 1 (PCM s16), samples = {}", samples.len());
    if rate != 8000 {
        return Err(format!("sample_rate {rate} != 8000 (G.711 clock)"));
    }
    if samples.is_empty() {
        return Err("no samples".into());
    }
    // Skip 0.25 s of start-up transient for the measurement window.
    let win = &samples[samples.len().min(2000)..];
    if win.len() < 8000 {
        return Err(format!("window too short: {} samples", win.len()));
    }

    // Coarse sweep to find the dominant tone, then fine sweep for the estimate.
    let mut best = (0.0f32, 0u32);
    let mut f = 100.0f32;
    while f <= 3900.0 {
        let m = audio_quality::goertzel_mag(win, f, 8000.0);
        if m > best.0 {
            best = (m, f as u32);
        }
        f += 25.0;
    }
    let mut fine = (0.0f32, 0u32);
    let lo = (best.1 as f32 - 25.0).max(20.0);
    let hi = best.1 as f32 + 25.0;
    let mut f = lo;
    while f <= hi {
        let m = audio_quality::goertzel_mag(win, f, 8000.0);
        if m > fine.0 {
            fine = (m, f as u32);
        }
        f += 1.0;
    }
    let mag1k = audio_quality::goertzel_mag(win, 1000.0, 8000.0);
    let mag3k = audio_quality::goertzel_mag(win, 3000.0, 8000.0);
    let rms = {
        let s: f64 = win.iter().map(|&x| (x as f64 / 32768.0).powi(2)).sum();
        (s / win.len() as f64).sqrt()
    };
    println!(
        "  dominant tone = {} Hz (mag {mag1k:.4} full-scale), fine peak {} Hz, mag@3kHz = {mag3k:.4}, rms = {rms:.4} ({:.1} dBFS)",
        best.1,
        fine.1,
        20.0 * rms.log10()
    );
    let est = fine.1 as i64;
    if (est - 1000).abs() > 5 {
        return Err(format!("tone {est} Hz not within ±5 Hz of 1000"));
    }
    if mag1k < mag3k * 5.0 {
        return Err(format!("1 kHz not dominant: mag1k={mag1k:.4} mag3k={mag3k:.4}"));
    }
    if rms <= 0.0 {
        return Err("silence".into());
    }
    println!("  sample_rate==8000, non-empty, tone ≈1 kHz: PASS");
    Ok(())
}