//! RFC 2435 (RTP Payload Format for JPEG-compressed Video) extractor.
//!
//! `#![no_std]`, zero dependencies — compiles under the firmware toolchain, under
//! the harness toolchain (1.89), and as its own crate root (see the `harness` lib
//! target, which is the no_std proof). Included from the firmware as
//! `mod jfif;` and from the harness via `#[path]`.
//!
//! The camera hands us a **complete JFIF file** (SOI … SOS, entropy-coded scan,
//! trailing `FF D9`). RFC 2435 payload is the **entropy-coded scan only**: the
//! receiver synthesises SOI/DQT/SOF0/DHT/SOS from the RTP/JPEG header (Appendix B)
//! and appends EOI. This module parses the JFIF, extracts everything the header
//! must declare, and emits RFC 2435 fragments.
//!
//! # Decisions (recorded in `firmware/docs/adr-0001-packetization.md`)
//!
//! * **Fragment Offset is in BYTES** (§3.1.2) — not the abandoned RFC 2035
//!   8-octet blocks.
//! * **Type 0 = 4:2:2** (Y sampling 2x1), **Type 1 = 4:2:0** (Y sampling 2x2),
//!   derived from SOF0; **+64 when a DRI segment is present** → 64/65.
//! * **Q = 255** (dynamic tables) with the 4-byte quant header
//!   (`MBZ=0 | Precision=0 | Length=128` — two 8-bit tables in zig-zag order) in
//!   the **first packet only**, unless both tables exactly equal the static
//!   Appendix-A tables for some `q` in `1..=99`, in which case `Q = q` and the
//!   quant header is omitted. Q=0 and 100..=127 are never emitted.
//! * **Width/Height in 8-pixel units**, straight from SOF0; non-multiples of 8
//!   or > 2040 are typed errors, never silently rounded.
//! * **RTP marker bit on the final fragment of the frame only** (§3/§4.3).
//!   There is no `P` bit: `P` is the JPEG arithmetic-coding selector.
//! * **EOI policy: the trailing `FF D9` is EXCLUDED from the payload.** The
//!   payload is the pure scan; the receiver appends EOI (§3.1.9 permits both
//!   choices; excluding it makes a double-EOI at the receiver impossible by
//!   construction and is asserted by gate (a)'s `Σ payload == scan length`).
//! * **Restart header layout: `Restart Interval(16) | F(1) | L(1) | Restart
//!   Count(14)`** — F = bit 16, L = bit 17, count = bits 18..31. (The plan body
//!   renders `L(2)|Count(11)`, which sums to 30 bits and cannot hold the plan's
//!   own `0x3FFF`; verified against the RFC diagram's character widths and
//!   against `rusty_esp_video`'s independent encoder, whose `0xFFFF` word equals
//!   `F=1, L=1, count=0x3FFF`.) **Chunking policy: every restart-bearing packet
//!   carries F=1, L=1, Count=0x3FFF** — the §3.1.7/§4.4 "reassemble the entire
//!   frame" mode, fully RFC-sanctioned.
//! * **RSTn markers remain in the payload** (§3.1.9 — never stripped). A frame
//!   without DRI must contain none (typed error otherwise); a frame with DRI
//!   must contain exactly `ceil(total_mcus / Ri) - 1` of them (checked here —
//!   this is what catches a spliced-DRI fixture whose scan has no markers).
//! * **Huffman tables:** the receiver synthesises the Annex K.3 set from the
//!   Type, so the source DHT is verified to be exactly those four tables; a
//!   scan coded with custom tables would decode as garbage at every receiver
//!   and is rejected with a typed error.

#![allow(unused_attributes)] // the next attribute is inert when this file is a module
#![no_std]

/// RFC 2435 main JPEG header: Type-specific | Fragment Offset | Type | Q | Width | Height.
pub const BASE_HEADER_LEN: usize = 8;
/// Restart Marker header (types 64-127): Restart Interval | F | L | Restart Count.
pub const RESTART_HEADER_LEN: usize = 4;
/// Quantisation Table header prefix: MBZ | Precision | Length.
pub const QUANT_HEADER_LEN: usize = 4;
/// Two 8-bit quantisation tables, 64 bytes each, zig-zag order.
pub const QUANT_DATA_LEN: usize = 128;
/// Largest JPEG header any single fragment can carry (first packet, Q=255, restart).
pub const MAX_HEADER_LEN: usize = BASE_HEADER_LEN + RESTART_HEADER_LEN + QUANT_HEADER_LEN + QUANT_DATA_LEN;

/// Type 0 base: 4:2:2, no restart markers.
pub const TYPE_422: u8 = 0;
/// Type 1 base: 4:2:0, no restart markers.
pub const TYPE_420: u8 = 1;
/// Added to the base type when restart markers are present (64 = 4:2:2+restart, 65 = 4:2:0+restart).
pub const RESTART_TYPE_OFFSET: u8 = 64;
/// Q value meaning "quantisation tables travel in-band in every first packet".
pub const Q_DYNAMIC: u8 = 255;

/// Typed error for every rejection path — no panics, no malformed headers emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JfifError {
    /// Fewer than 4 bytes, or a marker/header cut short by end-of-input.
    TooShort,
    /// No SOI (`FF D8`) at the start: not a JPEG interchange file.
    NotJfif,
    /// A marker segment length is inconsistent with the input (includes corrupt
    /// DQT lengths and stray bytes where a marker should be).
    CorruptSegment,
    /// SOF0 missing or declares something RFC 2435 cannot carry (progressive,
    /// 12-bit, not exactly 3 components, SOF1..SOF15).
    UnsupportedSof,
    /// SOF0 sampling factors or component table mapping are not the Type 0/1
    /// shapes (Y 2x1 or 2x2, chroma 1x1, Tq 0/1/1, ids 1/2/3).
    UnsupportedSampling,
    /// Width/height 0, not a multiple of 8, or above the RFC's 2040 maximum.
    UnsupportedDimensions,
    /// Quantisation table 0 or 1 missing, or present only at 16-bit precision.
    MissingQuantTable,
    /// DHT missing or not exactly the four Annex K.3 (RFC 2435 Appendix B) tables.
    NonStandardHuffman,
    /// SOS does not declare a single baseline interleaved scan of components 1,2,3.
    UnsupportedScanHeader,
    /// Input ends before SOS (header region) — subsumed by TooShort for < 4 bytes.
    MissingSos,
    /// Entropy-coded data ends without an EOI marker (file truncated mid-scan).
    TruncatedScan,
    /// Entropy-coded data contains an illegal marker: un-stuffed `FF`, RSTn where
    /// DRI forbids them, a marker count that contradicts the declared restart
    /// interval, or an empty scan.
    CorruptScan,
    /// The requested payload MTU cannot fit the first packet's headers plus data.
    MtuTooSmall,
}

impl core::fmt::Display for JfifError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            JfifError::TooShort => "jfif: input shorter than 4 bytes or truncated marker",
            JfifError::NotJfif => "jfif: missing SOI (FF D8)",
            JfifError::CorruptSegment => "jfif: corrupt marker segment length",
            JfifError::UnsupportedSof => "jfif: unsupported frame header (need baseline SOF0, 8-bit, 3 components)",
            JfifError::UnsupportedSampling => "jfif: sampling/table layout is not RFC 2435 type 0 or 1",
            JfifError::UnsupportedDimensions => "jfif: dimensions must be 1..=2040 and a multiple of 8",
            JfifError::MissingQuantTable => "jfif: quantisation tables 0 and 1 required (8-bit)",
            JfifError::NonStandardHuffman => "jfif: DHT is not the RFC 2435 Annex K.3 standard set",
            JfifError::UnsupportedScanHeader => "jfif: SOS is not a single baseline interleaved scan",
            JfifError::MissingSos => "jfif: no SOS marker before end of input",
            JfifError::TruncatedScan => "jfif: entropy-coded scan truncated (no EOI)",
            JfifError::CorruptScan => "jfif: corrupt entropy-coded scan",
            JfifError::MtuTooSmall => "jfif: MTU too small for header + payload",
        })
    }
}

impl core::error::Error for JfifError {}

/// A parsed JPEG frame, stripped to what RFC 2435 must transmit.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// Width in pixels (multiple of 8, 8..=2040).
    pub width: u16,
    /// Height in pixels (multiple of 8, 8..=2040).
    pub height: u16,
    /// RFC 2435 type: 0/1 base, 64/65 with restart markers.
    pub r#type: u8,
    /// DRI value in MCUs, when the frame carries restart markers.
    pub restart_interval: Option<u16>,
    /// Quantisation table 0 (luma), 64 bytes, zig-zag order, as stored in the file.
    pub qtable0: &'a [u8],
    /// Quantisation table 1 (chroma), 64 bytes, zig-zag order, as stored in the file.
    pub qtable1: &'a [u8],
    /// Static Q (1..=99) when both tables equal RFC 2435 Appendix A for some Q;
    /// `None` → transmit Q=255 with in-band tables every frame.
    pub static_q: Option<u8>,
    /// The entropy-coded scan: after SOS, **excluding** the trailing EOI.
    pub scan: &'a [u8],
}

/// One emitted RTP/JPEG fragment: fixed-size header bytes + borrowed payload.
#[derive(Debug, Clone, Copy)]
pub struct Fragment<'a> {
    /// The RFC 2435 JPEG header bytes (main [+ restart [+ quant]]).
    pub header: [u8; MAX_HEADER_LEN],
    /// Number of valid bytes in `header`.
    pub header_len: usize,
    /// Payload slice: the fragment's share of the entropy-coded scan.
    pub payload: &'a [u8],
    /// Byte offset of this payload within the scan (the Fragment Offset field).
    pub offset: usize,
    /// RTP marker bit: true on the final fragment of the frame only.
    pub marker: bool,
}

/// Iterator over the fragments of a frame. No allocation.
#[derive(Debug, Clone, Copy)]
pub struct Fragments<'a> {
    frame: Frame<'a>,
    max_payload: usize,
    offset: usize,
}

impl<'a> Frame<'a> {
    /// Parse a complete JFIF file into an RFC 2435 frame.
    ///
    /// Every failure is a typed [`JfifError`]; this function never panics on
    /// malformed input.
    pub fn parse(jpeg: &'a [u8]) -> Result<Frame<'a>, JfifError> {
        if jpeg.len() < 4 {
            return Err(JfifError::TooShort);
        }
        if jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
            return Err(JfifError::NotJfif);
        }

        let mut i = 2usize;
        let mut width = 0u16;
        let mut height = 0u16;
        let mut base_type = None;
        let mut restart_interval: Option<u16> = None;
        let mut qtables: [Option<&'a [u8]>; 2] = [None, None];
        let mut dht_bits = 0u8;

        loop {
            // Marker: FF fill bytes, then the marker code.
            while jpeg.get(i) == Some(&0xFF) {
                i += 1;
            }
            let &marker = jpeg.get(i).ok_or(JfifError::MissingSos)?;
            i += 1;

            // Standalone codes (TEM, RSTn before SOS — tolerated like all
            // reference parsers) carry no length.
            if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                continue;
            }
            if marker == 0xD9 {
                return Err(JfifError::MissingSos); // EOI before SOS
            }

            let len_hi = *jpeg.get(i).ok_or(JfifError::TooShort)?;
            let len_lo = *jpeg.get(i + 1).ok_or(JfifError::TooShort)?;
            let seg_len = u16::from_be_bytes([len_hi, len_lo]);
            if seg_len < 2 {
                return Err(JfifError::CorruptSegment);
            }
            let body_len = usize::from(seg_len) - 2;
            let body_start = i + 2;
            let body_end = body_start
                .checked_add(body_len)
                .ok_or(JfifError::CorruptSegment)?;
            if body_end > jpeg.len() {
                return Err(JfifError::CorruptSegment); // corrupt (e.g. DQT) length
            }
            let body = &jpeg[body_start..body_end];

            match marker {
                0xDB => {
                    // DQT: one or more (Pq|Tq, 64 bytes) tables.
                    let mut s = body;
                    while !s.is_empty() {
                        let pq = s[0] >> 4;
                        let tq = usize::from(s[0] & 0x0F);
                        if pq != 0 {
                            return Err(JfifError::MissingQuantTable); // 16-bit tables unsupported
                        }
                        if tq > 1 || s.len() < 65 {
                            return Err(JfifError::CorruptSegment);
                        }
                        qtables[tq] = Some(&s[1..65]);
                        s = &s[65..];
                    }
                }
                0xC0 => {
                    // SOF0: baseline DCT, the only frame header RFC 2435 carries.
                    if body.len() < 6 {
                        return Err(JfifError::UnsupportedSof);
                    }
                    if body[0] != 8 {
                        return Err(JfifError::UnsupportedSof); // not 8-bit
                    }
                    height = u16::from_be_bytes([body[1], body[2]]);
                    width = u16::from_be_bytes([body[3], body[4]]);
                    let nf = body[5];
                    if nf != 3 || body.len() < 6 + 3 * 3 {
                        return Err(JfifError::UnsupportedSof);
                    }
                    if width == 0
                        || height == 0
                        || width % 8 != 0
                        || height % 8 != 0
                        || width > 2040
                        || height > 2040
                    {
                        return Err(JfifError::UnsupportedDimensions);
                    }
                    // Component 1: id 1, Y sampling 2x1 (type 0) or 2x2 (type 1), Tq 0.
                    let id0 = body[6];
                    let hv0 = body[7];
                    let tq0 = body[8];
                    let base = match hv0 {
                        0x21 => TYPE_422,
                        0x22 => TYPE_420,
                        _ => return Err(JfifError::UnsupportedSampling),
                    };
                    if id0 != 1 || tq0 != 0 {
                        return Err(JfifError::UnsupportedSampling);
                    }
                    // Components 2,3: ids 2,3, sampling 1x1, Tq 1 (RFC 2435 §4.1).
                    for k in 1..3usize {
                        let id = body[6 + 3 * k];
                        let hv = body[7 + 3 * k];
                        let tq = body[8 + 3 * k];
                        if id != k as u8 + 1 || hv != 0x11 || tq != 1 {
                            return Err(JfifError::UnsupportedSampling);
                        }
                    }
                    base_type = Some(base);
                }
                0xC4 => {
                    // DHT: must be exactly the four Annex K.3 tables the
                    // receiver will synthesise from the Type field.
                    let mut s = body;
                    while !s.is_empty() {
                        if s.len() < 17 {
                            return Err(JfifError::NonStandardHuffman);
                        }
                        let class_id = s[0];
                        let codelens = &s[1..17];
                        let nsym: usize = codelens.iter().map(|&c| usize::from(c)).sum();
                        let symbols_end = 17usize
                            .checked_add(nsym)
                            .ok_or(JfifError::NonStandardHuffman)?;
                        if symbols_end > s.len() {
                            return Err(JfifError::NonStandardHuffman);
                        }
                        let symbols = &s[17..symbols_end];
                        let expected = standard_table(class_id).ok_or(JfifError::NonStandardHuffman)?;
                        if codelens != expected.0 || symbols != expected.1 {
                            return Err(JfifError::NonStandardHuffman);
                        }
                        dht_bits |= table_bit(class_id);
                        s = &s[symbols_end..];
                    }
                }
                0xDD => {
                    // DRI: restart interval in MCUs. DRI 0 means "no restarts".
                    if body.len() != 2 {
                        return Err(JfifError::CorruptSegment);
                    }
                    let ri = u16::from_be_bytes([body[0], body[1]]);
                    restart_interval = if ri == 0 { None } else { Some(ri) };
                }
                0xDA => {
                    // SOS: everything RFC 2435 needs must be known by now.
                    let base = base_type.ok_or(JfifError::UnsupportedSof)?;
                    if qtables[0].is_none() || qtables[1].is_none() {
                        return Err(JfifError::MissingQuantTable);
                    }
                    if dht_bits != 0b1111 {
                        return Err(JfifError::NonStandardHuffman);
                    }
                    if body.len() < 10 {
                        return Err(JfifError::UnsupportedScanHeader);
                    }
                    let ns = body[0];
                    // Ns=3, Cs=1,2,3 with TdTa = 0x00, 0x11, 0x11; Ss=0, Se=63, AhAl=0.
                    if ns != 3
                        || body[1] != 1
                        || body[2] != 0x00
                        || body[3] != 2
                        || body[4] != 0x11
                        || body[5] != 3
                        || body[6] != 0x11
                        || body[7] != 0
                        || body[8] != 63
                        || body[9] != 0
                    {
                        return Err(JfifError::UnsupportedScanHeader);
                    }

                    let scan_start = body_end;
                    let (scan, rst_count) = read_scan(&jpeg[scan_start..])?;
                    if scan.is_empty() {
                        return Err(JfifError::CorruptScan);
                    }

                    let mut r#type = base;
                    match restart_interval {
                        Some(ri) => {
                            // DRI declared: markers MUST be present at exactly
                            // the declared interval (a spliced DRI with no
                            // markers in the scan proves nothing and would
                            // green-light a broken path).
                            let mcus = mcu_count(width, height, base)?;
                            let expected = markers_for(mcus, ri);
                            if rst_count != expected {
                                return Err(JfifError::CorruptScan);
                            }
                            r#type = base + RESTART_TYPE_OFFSET;
                        }
                        None => {
                            // Types without restart support MUST NOT contain
                            // restart markers anywhere in the payload (§3.1.9).
                            if rst_count != 0 {
                                return Err(JfifError::CorruptScan);
                            }
                        }
                    }

                    let qtable0 = qtables[0].unwrap();
                    let qtable1 = qtables[1].unwrap();
                    let static_q = detect_static_q(qtable0, qtable1);
                    return Ok(Frame {
                        width,
                        height,
                        r#type,
                        restart_interval,
                        qtable0,
                        qtable1,
                        static_q,
                        scan,
                    });
                }
                // APP0..APPn, COM, JPG* — skipped.
                _ => {}
            }
            i = body_end;
        }
    }

    /// Minimum payload size (RTP payload = JPEG header + data) that can carry
    /// this frame's fragments.
    pub fn min_payload(&self) -> usize {
        1 + first_header_len(self)
    }

    /// Iterate the RFC 2435 fragments. `max_payload` is the largest RTP
    /// **payload** allowed per packet (MTU minus the 12-byte RTP header).
    pub fn fragments(&self, max_payload: usize) -> Result<Fragments<'a>, JfifError> {
        if max_payload < self.min_payload() {
            return Err(JfifError::MtuTooSmall);
        }
        Ok(Fragments {
            frame: *self,
            max_payload,
            offset: 0,
        })
    }

}

impl<'a> Iterator for Fragments<'a> {
    type Item = Fragment<'a>;

    fn next(&mut self) -> Option<Fragment<'a>> {
        let frame = self.frame;
        let scan: &'a [u8] = frame.scan;
        if self.offset >= scan.len() {
            return None;
        }
        let first = self.offset == 0;
        let hlen = if first {
            first_header_len(&frame)
        } else {
            later_header_len(&frame)
        };
        let room = self.max_payload - hlen; // guaranteed >= 1 by fragments()
        let take = (scan.len() - self.offset).min(room);
        let payload = &scan[self.offset..self.offset + take];
        let marker = self.offset + take == scan.len();

        let mut header = [0u8; MAX_HEADER_LEN];
        let header_len = write_header(&frame, self.offset, &mut header);

        let fragment = Fragment {
            header,
            header_len,
            payload,
            offset: self.offset,
            marker,
        };
        self.offset += take;
        Some(fragment)
    }
}

/// Header length for the fragment at `offset` (restart/quant presence).
fn header_len_at(frame: &Frame<'_>, offset: usize) -> usize {
    let mut n = BASE_HEADER_LEN;
    if frame.r#type >= RESTART_TYPE_OFFSET {
        n += RESTART_HEADER_LEN;
    }
    if offset == 0 {
        n += quant_header_len(frame);
    }
    n
}

fn first_header_len(frame: &Frame<'_>) -> usize {
    header_len_at(frame, 0)
}

fn later_header_len(frame: &Frame<'_>) -> usize {
    header_len_at(frame, 1) // offset != 0 → no quant header
}

fn quant_header_len(frame: &Frame<'_>) -> usize {
    if frame.static_q.is_some() {
        0
    } else {
        QUANT_HEADER_LEN + QUANT_DATA_LEN
    }
}

/// Write the RFC 2435 JPEG header for the fragment at `offset`; returns its length.
fn write_header(frame: &Frame<'_>, offset: usize, out: &mut [u8; MAX_HEADER_LEN]) -> usize {
    let mut p = 0usize;
    // Main header: Type-specific | Fragment Offset (BYTES) | Type | Q | W | H
    out[p] = 0; // Type-specific: progressive=0, not a field (§4.1 value 0)
    out[p + 1] = ((offset >> 16) & 0xFF) as u8;
    out[p + 2] = ((offset >> 8) & 0xFF) as u8;
    out[p + 3] = (offset & 0xFF) as u8;
    out[p + 4] = frame.r#type;
    out[p + 5] = frame.static_q.unwrap_or(Q_DYNAMIC);
    out[p + 6] = (frame.width / 8) as u8;
    out[p + 7] = (frame.height / 8) as u8;
    p += BASE_HEADER_LEN;

    if frame.r#type >= RESTART_TYPE_OFFSET {
        let ri = frame.restart_interval.unwrap_or(1);
        out[p..p + 2].copy_from_slice(&ri.to_be_bytes());
        // F=1 (bit 16), L=1 (bit 17), Restart Count=0x3FFF (bits 18..31) → 0xFFFF.
        out[p + 2] = 0xFF;
        out[p + 3] = 0xFF;
        p += RESTART_HEADER_LEN;
    }

    if offset == 0 && frame.static_q.is_none() {
        out[p] = 0; // MBZ
        out[p + 1] = 0; // Precision: all tables 8-bit (a set bit would mean 16-bit)
        let len: u16 = QUANT_DATA_LEN as u16;
        out[p + 2..p + 4].copy_from_slice(&len.to_be_bytes());
        p += QUANT_HEADER_LEN;
        out[p..p + 64].copy_from_slice(frame.qtable0);
        p += 64;
        out[p..p + 64].copy_from_slice(frame.qtable1);
        p += 64;
    }
    p
}

/// Walk the entropy-coded region: validate byte-stuffing, count RSTn markers,
/// stop at EOI. Returns `(scan_without_eoi, rstn_count)`.
fn read_scan(data: &[u8]) -> Result<(&[u8], usize), JfifError> {
    let mut i = 0usize;
    let mut rst_count = 0usize;
    loop {
        let b = *data.get(i).ok_or(JfifError::TruncatedScan)?; // EOF before EOI
        if b != 0xFF {
            i += 1;
            continue;
        }
        let n = *data.get(i + 1).ok_or(JfifError::TruncatedScan)?;
        match n {
            0x00 => i += 2, // stuffed FF
            0xD0..=0xD7 => {
                rst_count += 1;
                i += 2;
            }
            0xD9 => return Ok((&data[..i], rst_count)),
            _ => return Err(JfifError::CorruptScan), // un-stuffed 0xFF / illegal marker
        }
    }
}

/// MCUs per frame for base type 0 (MCU 16x8) or 1 (MCU 16x16).
pub fn mcu_count(width: u16, height: u16, base_type: u8) -> Result<u32, JfifError> {
    let vmax = match base_type {
        TYPE_422 => 1u32,
        TYPE_420 => 2u32,
        _ => return Err(JfifError::UnsupportedSampling),
    };
    let hmax = 2u32;
    let mcus_x = u32::from(width).div_ceil(8 * hmax);
    let mcus_y = u32::from(height).div_ceil(8 * vmax);
    Ok(mcus_x * mcus_y)
}

/// Expected RSTn markers: `ceil(total_mcus / Ri) - 1` (§1: the first restart
/// interval omits its marker).
pub fn markers_for(mcus: u32, restart_interval: u16) -> usize {
    let ri = u32::from(restart_interval.max(1));
    (mcus.div_ceil(ri)).saturating_sub(1) as usize
}

/// T.81 Annex K.3 / RFC 2435 Appendix B tables: (class_id, codelens, symbols).
fn standard_table(class_id: u8) -> Option<(&'static [u8; 16], &'static [u8])> {
    match class_id {
        0x00 => Some((&LUM_DC_CODELENS, &LUM_DC_SYMBOLS)),
        0x10 => Some((&LUM_AC_CODELENS, &LUM_AC_SYMBOLS)),
        0x01 => Some((&CHM_DC_CODELENS, &CHM_DC_SYMBOLS)),
        0x11 => Some((&CHM_AC_CODELENS, &CHM_AC_SYMBOLS)),
        _ => None,
    }
}

fn table_bit(class_id: u8) -> u8 {
    match class_id {
        0x00 => 0b0001,
        0x10 => 0b0010,
        0x01 => 0b0100,
        0x11 => 0b1000,
        _ => 0,
    }
}

static LUM_DC_CODELENS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
static LUM_DC_SYMBOLS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
static LUM_AC_CODELENS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
static LUM_AC_SYMBOLS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
static CHM_DC_CODELENS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
static CHM_DC_SYMBOLS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
static CHM_AC_CODELENS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
static CHM_AC_SYMBOLS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// Zig-zag scan order: natural (row-major) index of the i-th coefficient in scan order.
static ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// T.81 Table K.1 (luma), natural order — RFC 2435 Appendix A.
static K1: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// T.81 Table K.2 (chroma), natural order — RFC 2435 Appendix A.
static K2: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// RFC 2435 Appendix A `MakeTables(q)`: the two static tables a Q in 1..=99
/// stands for, in **zig-zag order** (as a DQT / quant header carries them).
pub fn make_tables(q: u8) -> [[u8; 64]; 2] {
    let q = u32::from(q.clamp(1, 99));
    let factor = if q < 50 { 5000 / q } else { 200 - q * 2 };
    let scale = |base: &[u8; 64]| {
        let mut out = [0u8; 64];
        for (i, slot) in out.iter_mut().enumerate() {
            let v = (u32::from(base[usize::from(ZIGZAG[i])]) * factor + 50) / 100;
            *slot = v.clamp(1, 255) as u8;
        }
        out
    };
    [scale(&K1), scale(&K2)]
}

fn detect_static_q(t0: &[u8], t1: &[u8]) -> Option<u8> {
    let a: &[u8; 64] = t0.try_into().ok()?;
    let b: &[u8; 64] = t1.try_into().ok()?;
    (1u8..=99)
        .find(|&q| {
            let tables = make_tables(q);
            *a == tables[0] && *b == tables[1]
        })
        .or(None)
}

#[cfg(test)]
mod tests {
    // Test builds: the harness crate is std; the no_std lib target is only
    // *built* (its proof is compilation), never tested as a no_std crate.
    extern crate std;

    use super::*;
    use std::vec;
    use std::vec::Vec;

    const FIX_422: &[u8] = include_bytes!("../../harness/fixtures/qvga_422.jpg");
    const FIX_420: &[u8] = include_bytes!("../../harness/fixtures/qvga_420.jpg");
    const FIX_RESTART: &[u8] = include_bytes!("../../harness/fixtures/qvga_422_restart.jpg");

    #[test]
    fn parses_422_fixture_as_type0() {
        let f = Frame::parse(FIX_422).expect("4:2:2 fixture parses");
        assert_eq!((f.width, f.height), (320, 240));
        assert_eq!(f.r#type, TYPE_422);
        assert_eq!(f.restart_interval, None);
        assert!(!f.scan.is_empty());
        // The scan is exactly the bytes between SOS and the trailing EOI.
        let tail = &FIX_422[FIX_422.len() - 2 - f.scan.len()..FIX_422.len() - 2];
        assert_eq!(tail, f.scan);
        assert_eq!(&FIX_422[FIX_422.len() - 2..], &[0xFF, 0xD9]);
    }

    #[test]
    fn parses_420_fixture_as_type1() {
        let f = Frame::parse(FIX_420).expect("4:2:0 fixture parses");
        assert_eq!((f.width, f.height), (320, 240));
        assert_eq!(f.r#type, TYPE_420);
    }

    #[test]
    fn parses_restart_fixture_as_type64_with_hard_counts() {
        let f = Frame::parse(FIX_RESTART).expect("restart fixture parses");
        assert_eq!((f.width, f.height), (320, 240));
        assert_eq!(f.r#type, TYPE_422 + RESTART_TYPE_OFFSET, "4:2:2 + restart = Type 64");
        let ri = f.restart_interval.expect("DRI present");
        assert!(ri > 0);
        // Hard count: rstn_count(scan) == ceil(total_mcus / Ri) - 1, counted
        // independently here from raw bytes.
        let mcus = mcu_count(f.width, f.height, TYPE_422).unwrap();
        let expected = markers_for(mcus, ri);
        assert_eq!(count_rstn(f.scan), expected, "RSTn count == ceil(mcus/Ri)-1");
        assert_eq!(count_rstn(f.scan), count_rstn(FIX_RESTART), "no markers lost in extraction");
        assert!(expected > 0, "fixture must actually contain RSTn");
    }

    #[test]
    fn no_restart_frame_contains_no_rstn() {
        let f = Frame::parse(FIX_422).unwrap();
        assert_eq!(count_rstn(f.scan), 0);
    }

    #[test]
    fn negative_input_shorter_than_4_bytes() {
        assert_eq!(Frame::parse(&[0xFF, 0xD8]).unwrap_err(), JfifError::TooShort);
        assert_eq!(Frame::parse(&[0x00, 0x01, 0x02]).unwrap_err(), JfifError::TooShort);
        assert_eq!(Frame::parse(&[]).unwrap_err(), JfifError::TooShort);
    }

    #[test]
    fn negative_truncated_mid_scan() {
        // Drop the last 16 bytes (EOI + end of scan) → no EOI findable.
        let cut = &FIX_422[..FIX_422.len() - 16];
        assert_eq!(Frame::parse(cut).unwrap_err(), JfifError::TruncatedScan);
    }

    #[test]
    fn negative_corrupt_dqt_length() {
        // Locate the DQT segment (FF DB) and inflate its length beyond EOF.
        let dqt = find_marker(FIX_422, 0xDB).expect("fixture has a DQT");
        let mut buf = std::vec::Vec::from(FIX_422);
        buf[dqt + 2] = 0xFF;
        buf[dqt + 3] = 0xFF; // length 65535 > file → corrupt
        assert_eq!(Frame::parse(&buf).unwrap_err(), JfifError::CorruptSegment);
    }

    #[test]
    fn negative_corrupt_dqt_truncated_table() {
        // Length that ends mid-table (segment says < 65 body bytes).
        let dqt = find_marker(FIX_422, 0xDB).expect("fixture has a DQT");
        let mut buf = std::vec::Vec::from(FIX_422);
        buf[dqt + 2] = 0x00;
        buf[dqt + 3] = 0x0A; // len 10 → body 8 < 65
        assert_eq!(Frame::parse(&buf).unwrap_err(), JfifError::CorruptSegment);
    }

    #[test]
    fn negative_not_jfif() {
        assert_eq!(Frame::parse(&[0x00, 0x01, 0x02, 0x03, 0x04]).unwrap_err(), JfifError::NotJfif);
    }

    #[test]
    fn negative_rstn_without_dri_is_rejected() {
        // Hand-built: SOI + SOF0(422) + DQTs + DHTs + SOS + scan with RSTn but no DRI.
        let mut jpeg = std::vec::Vec::new();
        jpeg.extend_from_slice(&[0xFF, 0xD8]); // SOI
        append_sof0(&mut jpeg, 16, 8, 0x21);
        append_dqts(&mut jpeg);
        append_dhts(&mut jpeg);
        append_sos(&mut jpeg);
        // scan: some bytes, an RSTn marker, then EOI — no DRI anywhere.
        jpeg.extend_from_slice(&[0x12, 0x34, 0xFF, 0xD0, 0x56, 0xFF, 0xD9]);
        assert_eq!(Frame::parse(&jpeg).unwrap_err(), JfifError::CorruptScan);
    }

    #[test]
    fn negative_dri_without_markers_is_rejected() {
        // DRI declared but the scan holds fewer markers than the interval
        // implies — the spliced-DRI trap the plan calls out.
        let mut jpeg = std::vec::Vec::new();
        jpeg.extend_from_slice(&[0xFF, 0xD8]);
        append_dri(&mut jpeg, 1);
        append_sof0(&mut jpeg, 32, 16, 0x21); // 4 MCUs → DRI 1 demands 3 RSTn
        append_dqts(&mut jpeg);
        append_dhts(&mut jpeg);
        append_sos(&mut jpeg);
        jpeg.extend_from_slice(&[0x12, 0x34, 0x56, 0xFF, 0xD9]);
        assert_eq!(Frame::parse(&jpeg).unwrap_err(), JfifError::CorruptScan);
    }

    #[test]
    fn negative_unstuffed_ff_in_scan() {
        let mut jpeg = std::vec::Vec::new();
        jpeg.extend_from_slice(&[0xFF, 0xD8]);
        append_sof0(&mut jpeg, 16, 8, 0x21);
        append_dqts(&mut jpeg);
        append_dhts(&mut jpeg);
        append_sos(&mut jpeg);
        jpeg.extend_from_slice(&[0x12, 0xFF, 0xE0, 0x34, 0xFF, 0xD9]); // 0xE0 un-stuffed
        assert_eq!(Frame::parse(&jpeg).unwrap_err(), JfifError::CorruptScan);
    }

    #[test]
    fn fragment_headers_are_byte_offset_based() {
        let f = Frame::parse(FIX_422).unwrap();
        let frags: Vec<_> = f.fragments(1400).unwrap().collect();
        assert!(frags.len() > 1, "320x240 must fragment at 1400 B");
        let mut expected_offset = 0usize;
        for (n, fr) in frags.iter().enumerate() {
            // Byte offset field (RFC 2435 §3.1.2 — BYTES, not 8-octet blocks).
            let off = ((fr.header[1] as usize) << 16)
                | ((fr.header[2] as usize) << 8)
                | (fr.header[3] as usize);
            assert_eq!(off, expected_offset, "fragment {n}: offset in bytes");
            assert_eq!(off, fr.offset);
            // Marker bit on final fragment only.
            assert_eq!(fr.marker, n + 1 == frags.len(), "fragment {n} marker");
            // Offset unit sanity: consecutive offsets advance by payload length.
            expected_offset += fr.payload.len();
        }
        assert_eq!(expected_offset, f.scan.len());
    }

    #[test]
    fn fragmentation_accounting_is_exact() {
        for fix in [FIX_422, FIX_420, FIX_RESTART] {
            let f = Frame::parse(fix).unwrap();
            for mtu in [f.min_payload(), 200, 1400] {
                let mut payload_sum = 0usize;
                let mut header_sum = 0usize;
                let mut placed = vec![false; f.scan.len()];
                let mut rebuilt = vec![0u8; f.scan.len()];
                let mut n = 0usize;
                for fr in f.fragments(mtu).unwrap() {
                    payload_sum += fr.payload.len();
                    header_sum += fr.header_len;
                    // Place by byte offset: catches byte-vs-block confusion.
                    assert!(fr.offset + fr.payload.len() <= f.scan.len());
                    assert!(
                        !placed[fr.offset..fr.offset + fr.payload.len()]
                            .iter()
                            .any(|&p| p),
                        "overlap at {}",
                        fr.offset
                    );
                    for (k, &b) in fr.payload.iter().enumerate() {
                        placed[fr.offset + k] = true;
                        rebuilt[fr.offset + k] = b;
                    }
                    // header_len must equal the declared fixed sizes (a missing
                    // or misplaced 4-byte quant header fails this immediately).
                    let mut expect = BASE_HEADER_LEN;
                    if f.r#type >= RESTART_TYPE_OFFSET {
                        expect += RESTART_HEADER_LEN;
                    }
                    if n == 0 && f.static_q.is_none() {
                        expect += QUANT_HEADER_LEN + QUANT_DATA_LEN;
                    }
                    assert_eq!(fr.header_len, expect, "packet {n} header_len");
                    n += 1;
                }
                assert!(placed.iter().all(|&p| p), "every scan byte covered exactly once");
                assert_eq!(
                    rebuilt.as_slice(),
                    f.scan,
                    "reassembled == scan byte-for-byte (no re-prepend)"
                );
                assert_eq!(payload_sum, f.scan.len(), "Σ payload == extracted scan length");
                assert!(!f.scan.windows(2).any(|w| w == [0xFF, 0xD9]), "no EOI in payload");
                assert!(header_sum > 0);
            }
        }
    }

    #[test]
    fn q_and_quant_header_rules() {
        // Fixtures use non-static tables → Q=255 + 128-byte quant data.
        let f = Frame::parse(FIX_422).unwrap();
        let q = f.static_q;
        let frags: Vec<_> = f.fragments(1400).unwrap().collect();
        let first = &frags[0];
        match q {
            Some(qv) => {
                assert!((1..=99).contains(&qv));
                assert_eq!(first.header[5], qv);
                assert_eq!(first.header_len, BASE_HEADER_LEN);
            }
            None => {
                assert_eq!(first.header[5], Q_DYNAMIC, "Q=255");
                assert_eq!(first.header_len, MAX_HEADER_LEN);
                assert_eq!(first.header[8], 0, "MBZ");
                assert_eq!(first.header[9], 0, "Precision");
                let len = u16::from_be_bytes([first.header[10], first.header[11]]);
                assert_eq!(len, 128, "Length == 128 for two 8-bit tables");
                assert_eq!(&first.header[12..76], f.qtable0, "table 0 zig-zag data");
                assert_eq!(&first.header[76..140], f.qtable1, "table 1 zig-zag data");
                // Later packets carry no quant header.
                for fr in &frags[1..] {
                    assert!(fr.header_len <= BASE_HEADER_LEN + RESTART_HEADER_LEN);
                }
            }
        }
    }

    #[test]
    fn restart_header_word_is_f_l_count() {
        let f = Frame::parse(FIX_RESTART).unwrap();
        let ri = f.restart_interval.unwrap();
        for fr in f.fragments(1400).unwrap() {
            assert_eq!(fr.header[4], 64, "Type 64");
            assert_eq!(u16::from_be_bytes([fr.header[8], fr.header[9]]), ri);
            assert_eq!(
                u16::from_be_bytes([fr.header[10], fr.header[11]]),
                0xFFFF,
                "F=1 | L=1 | Count=0x3FFF"
            );
        }
    }

    #[test]
    fn mtu_too_small_is_typed_error() {
        let f = Frame::parse(FIX_422).unwrap();
        assert_eq!(f.fragments(f.min_payload() - 1).unwrap_err(), JfifError::MtuTooSmall);
    }

    #[test]
    fn width_height_fields_are_8_pixel_units() {
        let f = Frame::parse(FIX_422).unwrap();
        let first = f.fragments(1400).unwrap().next().unwrap();
        assert_eq!(usize::from(first.header[6]) * 8, 320);
        assert_eq!(usize::from(first.header[7]) * 8, 240);
    }

    // ---- helpers ----

    fn count_rstn(data: &[u8]) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i + 1 < data.len() {
            if data[i] == 0xFF && (0xD0..=0xD7).contains(&data[i + 1]) {
                n += 1;
                i += 2;
            } else {
                i += 1;
            }
        }
        n
    }

    fn find_marker(j: &[u8], m: u8) -> Option<usize> {
        j.windows(2).position(|w| w == [0xFF, m])
    }

    fn append_seg(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
        out.push(0xFF);
        out.push(marker);
        let len = (body.len() + 2) as u16;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(body);
    }

    fn append_sof0(out: &mut Vec<u8>, w: u16, h: u16, y_hv: u8) {
        let mut body = std::vec::Vec::new();
        body.push(8); // precision
        body.extend_from_slice(&h.to_be_bytes());
        body.extend_from_slice(&w.to_be_bytes());
        body.push(3); // components
        body.extend_from_slice(&[1, y_hv, 0]); // Y: id 1, sampling, Tq 0
        body.extend_from_slice(&[2, 0x11, 1]); // Cb
        body.extend_from_slice(&[3, 0x11, 1]); // Cr
        append_seg(out, 0xC0, &body);
    }

    fn append_dqts(out: &mut Vec<u8>) {
        for tq in 0..2u8 {
            let mut body = std::vec::Vec::new();
            body.push(tq);
            body.extend_from_slice(&[1u8; 64]); // constant table
            append_seg(out, 0xDB, &body);
        }
    }

    fn append_dhts(out: &mut Vec<u8>) {
        for (class_id, codelens, symbols) in [
            (0x00u8, &LUM_DC_CODELENS[..], &LUM_DC_SYMBOLS[..]),
            (0x10, &LUM_AC_CODELENS[..], &LUM_AC_SYMBOLS[..]),
            (0x01, &CHM_DC_CODELENS[..], &CHM_DC_SYMBOLS[..]),
            (0x11, &CHM_AC_CODELENS[..], &CHM_AC_SYMBOLS[..]),
        ] {
            let mut body = std::vec::Vec::new();
            body.push(class_id);
            body.extend_from_slice(codelens);
            body.extend_from_slice(symbols);
            append_seg(out, 0xC4, &body);
        }
    }

    fn append_dri(out: &mut Vec<u8>, ri: u16) {
        append_seg(out, 0xDD, &ri.to_be_bytes());
    }

    fn append_sos(out: &mut Vec<u8>) {
        let body = [3u8, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0];
        append_seg(out, 0xDA, &body);
    }
}
