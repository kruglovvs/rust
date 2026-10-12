//! Todo 6 acceptance-gate runner (Wave 1, host-only).
//!
//! Commands:
//!   gate6 roundtrip <file> <mtu>   Gate (a): fragmentation accounting, byte-exact
//!                                  reassembly, header-size equality, EOI exclusion.
//!   gate6 restart <file>           Restart hard counts: Type 64/65, RSTn in payload,
//!                                  F/L/Count word, ceil(mcus/Ri)-1.
//!   gate6 send <file> <ip:port>    Emit real RTP (PT 26) datagrams for an external
//!                                  depacketizer (ffmpeg SDP / gstreamer udpsrc).
//!   gate6 retina-probe             Build-probe retina 0.4.21 (MSRV 1.88 <= 1.89).
//!   gate6 all [fixtures-dir]       Run roundtrip (two MTUs) + restart over the set.

#[path = "../../firmware/src/jfif.rs"]
mod jfif;

use jfif::Frame;
use std::net::UdpSocket;
use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let outcome = match args.get(1).map(String::as_str) {
        Some("roundtrip") => roundtrip(
            args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3)
                .ok_or_else(|| "roundtrip requires <mtu>".to_string())
                .and_then(|s| s.parse::<usize>().map_err(|e| e.to_string())),
        ),
        Some("restart") => restart(args.get(2).map(String::as_str).unwrap_or("")),
        Some("send") => send(
            args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3).map(String::as_str).unwrap_or(""),
        ),
        Some("retina-probe") => retina_probe(),
        Some("all") => all(args.get(2).map(String::as_str).unwrap_or("fixtures")),
        _ => Err(
            "usage: gate6 roundtrip <file> <mtu> | restart <file> | send <file> <ip:port> | retina-probe | all [dir]"
                .into(),
        ),
    };
    match outcome {
        Ok(()) => println!("GATE: PASS"),
        Err(e) => {
            eprintln!("GATE: FAIL — {e}");
            exit(1);
        }
    }
}

fn read(path: &str) -> Result<Vec<u8>, String> {
    if path.is_empty() {
        return Err("missing file argument".into());
    }
    std::fs::read(path).map_err(|e| format!("read {path}: {e}"))
}

fn with_frame<T>(
    path: &str,
    f: impl FnOnce(&[u8], &Frame<'_>) -> Result<T, String>,
) -> Result<T, String> {
    let bytes = read(path)?;
    let frame = Frame::parse(&bytes).map_err(|e| format!("parse {path}: {e}"))?;
    f(&bytes, &frame)
}

fn expected_header_len(frame: &Frame<'_>, first: bool) -> usize {
    let mut n = 8usize;
    if frame.r#type >= 64 {
        n += 4;
    }
    if first && frame.static_q.is_none() {
        n += 4 + 128;
    }
    n
}

fn count_rstn(data: &[u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i + 1 < data.len() {
        if data[i] == 0xFF {
            if (0xD0..=0xD7).contains(&data[i + 1]) {
                n += 1;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    n
}

fn roundtrip(path: &str, mtu: Result<usize, String>) -> Result<(), String> {
    let mtu = mtu?;
    with_frame(path, |file, frame| {
        println!(
            "fixture={path} bytes={} width={} height={} type={} static_q={:?} restart={:?}",
            file.len(),
            frame.width,
            frame.height,
            frame.r#type,
            frame.static_q,
            frame.restart_interval
        );
        let scan_len = frame.scan.len();
        println!("scan_len={scan_len}");

        let fragments: Vec<_> = frame
            .fragments(mtu)
            .map_err(|e| e.to_string())?
            .collect();
        let n = fragments.len();
        println!("mtu={mtu} fragments={n}");
        if n == 0 {
            return Err("zero fragments".into());
        }

        let mut rebuilt = vec![0u8; scan_len];
        let mut covered = vec![false; scan_len];
        let mut sum_payload = 0usize;
        let mut sum_header = 0usize;
        let mut running = 0usize;
        let mut header_lens = Vec::new();
        for (i, fr) in fragments.iter().enumerate() {
            let hdr_off = ((fr.header[1] as usize) << 16)
                | ((fr.header[2] as usize) << 8)
                | (fr.header[3] as usize);
            assert_eq!(hdr_off, fr.offset, "fragment {i}: header offset field (BYTES)");
            assert_eq!(fr.offset, running, "fragment {i}: offsets must be contiguous");
            assert!(fr.offset + fr.payload.len() <= scan_len, "fragment {i}: bounds");
            assert!(
                !covered[fr.offset..fr.offset + fr.payload.len()]
                    .iter()
                    .any(|&c| c),
                "fragment {i}: overlapping payload"
            );
            for (k, &b) in fr.payload.iter().enumerate() {
                rebuilt[fr.offset + k] = b;
                covered[fr.offset + k] = true;
            }
            assert_eq!(
                fr.header_len,
                expected_header_len(frame, i == 0),
                "fragment {i}: header_len must be the exact fixed size (catches a missing/misplaced 4-byte quant header)"
            );
            assert_eq!(
                fr.marker,
                i + 1 == n,
                "fragment {i}: RTP marker bit on final fragment only"
            );
            running += fr.payload.len();
            sum_payload += fr.payload.len();
            sum_header += fr.header_len;
            header_lens.push(fr.header_len);
        }
        assert!(
            covered.iter().all(|&c| c),
            "every scan byte must be covered exactly once"
        );
        assert_eq!(
            rebuilt.as_slice(),
            frame.scan,
            "reassembled == extracted scan byte-for-byte (no header re-prepend)"
        );
        assert_eq!(
            sum_payload, scan_len,
            "Σ payload == extracted scan length (catches double-EOI / quant bytes in payload)"
        );
        assert!(file.ends_with(&[0xFF, 0xD9]), "source file must end with EOI");
        assert!(
            !frame.scan.windows(2).any(|w| w == [0xFF, 0xD9]),
            "EOI policy: FF D9 must be excluded from the payload entirely"
        );
        assert!(scan_len < file.len(), "scan is a strict subset of the file");

        let sum_packet = sum_payload + sum_header;
        let mut dedup = header_lens.clone();
        dedup.sort_unstable();
        dedup.dedup();
        println!("sum_payload={sum_payload} sum_header={sum_header} sum_packet={sum_packet}");
        println!(
            "header_lens_present={dedup:?} first_header_len={} scan_bytes_covered={}",
            fragments[0].header_len,
            covered.iter().filter(|&&c| c).count()
        );
        println!("A1 reassembled==scan byte-for-byte, no re-prepend: PASS");
        println!("A2 sum_payload==scan_len ({sum_payload}=={scan_len}): PASS");
        println!(
            "A3 sum_packet==sum_payload+sum_header ({sum_packet}=={sum_payload}+{sum_header}); per-packet header_len exact: PASS"
        );
        println!("EOI=EXCLUDED; file_tail_eoi=true; scan_contains_eoi=false; marker=last-only; offset=BYTES: PASS");
        Ok(())
    })
}

fn restart(path: &str) -> Result<(), String> {
    with_frame(path, |_file, frame| {
        if frame.r#type < 64 {
            return Err(format!("expected restart type 64/65, got {}", frame.r#type));
        }
        let ri = frame
            .restart_interval
            .ok_or("no restart interval despite restart type")?;
        // Independent MCU math, written locally on purpose (not library-calling).
        let base = frame.r#type - 64;
        let mcus = match base {
            0 => ((u32::from(frame.width) + 15) / 16) * ((u32::from(frame.height) + 7) / 8),
            1 => ((u32::from(frame.width) + 15) / 16) * ((u32::from(frame.height) + 15) / 16),
            other => return Err(format!("unexpected base type {other}")),
        };
        let expected_rstn = (mcus.div_ceil(u32::from(ri)) - 1) as usize;
        let rstn_scan = count_rstn(frame.scan);
        println!(
            "fixture={path} type={} base={base} dri={ri} mcus={mcus} rstn_scan={rstn_scan} expected_rstn=ceil({mcus}/{ri})-1={expected_rstn}",
            frame.r#type
        );
        assert_eq!(
            rstn_scan, expected_rstn,
            "rstn_count(scan) == ceil(total_mcus/Ri) - 1"
        );
        assert!(expected_rstn > 0, "fixture must really contain RSTn markers");

        let fragments: Vec<_> = frame.fragments(1400).map_err(|e| e.to_string())?.collect();
        let mut rstn_payload = 0usize;
        for (i, fr) in fragments.iter().enumerate() {
            assert_eq!(fr.header[4], frame.r#type, "fragment {i}: type field");
            assert_eq!(
                u16::from_be_bytes([fr.header[10], fr.header[11]]),
                0xFFFF,
                "fragment {i}: F=1 | L=1 | Restart Count=0x3FFF"
            );
            assert_eq!(
                u16::from_be_bytes([fr.header[8], fr.header[9]]),
                ri,
                "fragment {i}: restart interval field"
            );
            rstn_payload += count_rstn(fr.payload);
        }
        assert_eq!(
            rstn_payload, rstn_scan,
            "every RSTn marker remains in the payload (RFC 2435 3.1.9)"
        );
        println!(
            "type_field={} on all {} fragments; F_word=0xFFFF on all; rstn_in_payload={rstn_payload}: PASS",
            frame.r#type,
            fragments.len()
        );
        println!("chunking_policy=FULL_FRAME (F=1,L=1,Count=0x3FFF, RFC 2435 3.1.7/4.4): PASS");
        Ok(())
    })
}

fn send(path: &str, addr: &str) -> Result<(), String> {
    if addr.is_empty() {
        return Err("send requires <ip:port>".into());
    }
    with_frame(path, |_file, frame| {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
        socket
            .connect(addr)
            .map_err(|e| format!("connect {addr}: {e}"))?;
        let mut seq: u16 = 0x1000;
        let ts: u32 = 90_000;
        let ssrc: u32 = 0x0C_0D_EA_11;
        let mut sent = 0usize;
        let mut bytes = 0usize;
        let mtu = 1400usize;
        let mut pkt = vec![0u8; 12 + mtu];
        for fr in frame.fragments(mtu).map_err(|e| e.to_string())? {
            let total = 12 + fr.header_len + fr.payload.len();
            pkt[0] = 0x80; // V=2, no CSRC
            pkt[1] = if fr.marker { 0x80 | 26 } else { 26 }; // M + PT 26 (JPEG)
            pkt[2..4].copy_from_slice(&seq.to_be_bytes());
            pkt[4..8].copy_from_slice(&ts.to_be_bytes());
            pkt[8..12].copy_from_slice(&ssrc.to_be_bytes());
            pkt[12..12 + fr.header_len].copy_from_slice(&fr.header[..fr.header_len]);
            pkt[12 + fr.header_len..total].copy_from_slice(fr.payload);
            socket
                .send(&pkt[..total])
                .map_err(|e| format!("send: {e}"))?;
            bytes += total;
            sent += 1;
            seq = seq.wrapping_add(1);
        }
        println!(
            "sent={sent} rtp_bytes={bytes} pt=26 first_seq=4096 ts={ts} marker=last-only addr={addr}"
        );
        Ok(())
    })
}

fn retina_probe() -> Result<(), String> {
    let mut d = retina::codec::Depacketizer::new("video", "jpeg", 90_000, None, None)
        .map_err(|e| format!("retina Depacketizer::new(video,jpeg): {e}"))?;
    assert!(d.pull().is_none(), "no frame without packets");
    println!(
        "retina 0.4.21 jpeg depacketizer constructible; push() is pub(crate) so packets cannot be fed externally (see evidence CONFORMANCE_NOTE)"
    );
    Ok(())
}

fn all(dir: &str) -> Result<(), String> {
    let fixtures = [
        "qvga_422.jpg",
        "qvga_420.jpg",
        "qvga_422_dyn.jpg",
        "qvga_422_restart.jpg",
    ];
    for f in fixtures {
        let path = format!("{dir}/{f}");
        println!("==== roundtrip {path} mtu=1400 ====");
        roundtrip(&path, Ok(1400))?;
        println!("==== roundtrip {path} mtu=96 ====");
        match roundtrip(&path, Ok(96)) {
            Err(e) if e.contains("MTU too small") => println!(
                "mtu=96 rejected with typed MtuTooSmall (Q=255 first packet needs 141 B): PASS (expected)"
            ),
            other => other?,
        }
    }
    let p = format!("{dir}/qvga_422_restart.jpg");
    println!("==== restart {p} ====");
    restart(&p)?;
    Ok(())
}
