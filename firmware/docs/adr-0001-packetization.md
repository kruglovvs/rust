# ADR-0001: RFC 2435 RTP/JPEG packetization — implement in-house, reject `rusty_esp_video`

> **Superseded:** The in-house RFC 2435 packetizer was replaced by the
> Espressif GMF capture/RTSP pipeline. The rejection of `rusty_esp_video`
> still stands. See `firmware/docs/architecture.md` for the current design.

- **Status:** Accepted, 2026-10-11 (plan Todo 6)
- **Decision:** **REJECT `rusty_esp_video` 0.1.2.** Implement `firmware/src/jfif.rs`
  (no_std, zero-dependency) as the RFC 2435 extractor. Default posture of the audit
  was REJECT; the burden of proof was on adoption; two items failed outright and one
  could not be cleared.
- **Consequences:** MSRV floor reverts from **1.85 to 1.82** — the 1.85 requirement
  came from `rusty_esp_video` and nothing else. Propagating `rust-version = "1.85"` →
  `"1.82"` in `firmware/Cargo.toml` is owned by Todo 19 (this worker's lane is
  `jfif.rs`, this ADR, and `harness/`; the one-line change is recorded here so it is
  not lost). `rusty_esp_video` is **not** added to any workspace; `esp_video`,
  `esp_webrtc`, `esp_jpeg`, `rusty_h264` and friends stay out of the tree.

## Context

The OV3660 (through esp32-camera) hands the firmware a **complete JFIF file** —
SOI … SOS, entropy-coded scan, trailing `FF D9` (confirmed in `esp-cpp/camera-streamer`
which validates the trailing `FF D9` before packetizing). RFC 2435 instead defines the
RTP payload as **the entropy-coded scan only**: the receiver synthesizes
SOI/DQT/DRT-adjacent/SOF0/DHT/SOS from the RTP/JPEG header (Appendix B) and appends
EOI. Packetizing the JFIF verbatim would make every conforming receiver
(ffmpeg, VLC, retina) wrap an already-wrapped file.

Two candidate implementations existed: adopt `rusty_esp_video` 0.1.2 (crates.io,
created 2026-09-16, 23 downloads, 0 stars, 0 forks, 2 versions — which is why the plan
raised MSRV to 1.85), or write ~150 lines the RFC fully specifies. The plan requires a
10-item supply-chain audit with **adoption only if all 10 items are clean**; otherwise
the fallback `firmware/src/jfif.rs` is written and exercised against two non-circular
acceptance gates.

### Audit method (reproducible)

Sources were fetched as published `.crate` archives from `static.crates.io` into a
scratch dir and read **in full** (including every `build.rs` — there are none), plus a
`cargo tree` run through the harness workspace with the crates added as temporary
`=`-pinned dependencies, then removed (the plan forbids keeping `rusty_esp_video`
unless adopted; the dep does not remain). crates.io API and the GitHub API supplied
provenance fields. Audited artifacts:

| crate | version | published | package SHA (`.cargo_vcs_info.json`) |
|---|---|---|---|
| `rusty_esp_video` | 0.1.2 | 2026-10-03 (first release 0.1.0, 2026-09-16) | `577f71f83cb8b9dae6abf46a85171dbbf28bbe6a` |
| `rusty_esp_video-core` | 0.1.2 | 2026-10-03 | `577f71f83cb8b9dae6abf46a85171dbbf28bbe6a` (same commit, `crates/rusty_esp_video-core`) |
| `rusty_esp_video-esp` | 0.1.2 | 2026-10-03 | `577f71f83cb8b9dae6abf46a85171dbbf28bbe6a` (same commit, `crates/rusty_esp_video-esp`) |
| `oxideav-avi` | 0.0.10 | — | `e4d1e2ed90c63582f1d33a4be210ff7bf8deff2d` |
| `oxideav-core` (transitive) | 0.1.39 (resolved) | — | — |

`rusty_esp_video` is a three-crate workspace facade (facade → `-core` + `-esp`), so
all three published packages were audited as one unit.

---

## Part A — `rusty_esp_video` 0.1.2: the 10-item audit

**Item 1 — `build.rs` in full: CLEAN.**
No `build.rs` exists in any of the three published packages; the normalized
`Cargo.toml` even carries `build = false` on the facade. `find` over all three
package trees returns no `build.rs`. Nothing shells out, downloads, or mutates the
ESP-IDF build.

**Item 2 — `remote_component` additions: CLEAN.**
Zero occurrences of `remote_component` in any `.toml`, `.rs`, or `.md` of the three
packages. The crate adds no ESP component of any kind, pinned or floating.

**Item 3 — full transitive tree / esp-idf pins: CLEAN (vacuously).**
`cargo tree -e normal -p rusty_esp_video` (run twice: default features and
`features = ["esp-idf"]` — identical):

```
rusty_esp_video v0.1.2
├── rusty_esp_video-core v0.1.2
│   ├── rusty_esp_core v0.1.2
│   └── rusty_esp_image-core v0.1.2
│       ├── embedded-hal v1.0.0
│       ├── rusty_esp_core v0.1.2
│       └── rusty_esp_dsp v0.1.1
│           ├── libm v0.2.16
│           └── rusty_esp_core v0.1.2
└── rusty_esp_video-esp v0.1.2
    └── rusty_esp_video-core v0.1.2 (*)
```

The tree contains **zero `esp-idf-*` nodes** — no `esp-idf-hal`, `esp-idf-sys`,
`esp-idf-svc`, no `embuild`. The crate's `esp-idf` feature only forwards to
`std` (`esp-idf = ["std"]`); it uses `std::net` and never links chip crates. Todo 19
pins `esp-idf-hal =0.47.0`, `esp-idf-svc =0.53.0`, `esp-idf-sys =0.38.1`; a second,
different `esp-idf-hal`/`esp-idf-sys` in the graph is therefore impossible — there is
nothing to pin and nothing to conflict with. (The `h264` feature would additionally
pull `rusty_h264` 0.14.0; it does not pull esp-idf either.)

**Item 4 — `esp_video` in any form: CLEAN.**
No dependency, feature, or remote component named `esp_video` (or `esp_webrtc`,
`esp_jpeg`). The string appears only in prose (“Espressif's video stack (`esp_video`,
`esp_h264`, …) remade in Rust”) describing what the crate replaces. The large C
component never enters the tree.

**Item 5 — licenses (self + transitive): CLEAN.**
`rusty_esp_video`/-core/-esp: `MIT OR Apache-2.0`. Transitive (from the shipped
`Cargo.lock` + crates.io): `rusty_esp_core` 0.1.2 MIT OR Apache-2.0,
`rusty_esp_image-core` 0.1.1 MIT OR Apache-2.0, `rusty_esp_dsp` 0.1.1 MIT OR
Apache-2.0, `embedded-hal` 1.0.0 MIT OR Apache-2.0, `libm` 0.2.16 MIT, `once_cell`
1.21.4 MIT OR Apache-2.0, `portable-atomic` 1.15.0 Apache-2.0 OR MIT, `bytemuck`
1.25.2 Zlib OR Apache-2.0 OR MIT, `safe_arch` 0.7.4 and `wide` 0.7.33 Zlib OR
Apache-2.0 OR MIT; feature-`h264`/dev-only: `rusty_h264` 0.14.0 (+ `-common`,
`-decoder`, `-encoder`) BSD-2-Clause. All permissive; no GPL/AGPL/unknown.
(Note: the GitHub repo page reports “Apache-2.0” only while the crates declare
dual-licensing — a minor provenance inconsistency, recorded, not a license failure.)

**Item 6 — Arduino-shaped HAL coupling: CLEAN.**
Grep across all three packages’ `src/`, `tests/`, `examples/` for
`digitalWrite|pinMode|Serial.begin|analogRead|Wire.|delay(` finds no Arduino API —
the single hit is `set_nodelay(true)` (TCP_NODELAY), a false positive. The sibling
ecosystem package `rusty_esp_arduino` (“the sketch facade”) exists but is **not** a
dependency of this crate.

**Item 7 — PCM sender: L16/PT 97 vs G.711: FAIL (capability absent, claim not
substantiated).**
The facade README advertises Track A as “the page, the **RTP and PCM senders**, the
sockets”, but `grep -ri 'pcm|g711|ulaw|mu-law|L16'` over `rusty_esp_video-esp`’s
sources finds **no PCM sender at all** — no L16/PT 97, no G.711, no PCMU/PCPA. The
advertised component is not present in the published package (it may live in the
unaudited sibling `rusty_esp_audio`). We require µ-law/PCMU (PT 0) for Todo 11; this
crate neither provides it nor documents honestly what it provides.

**Item 8 — JPEG payloader: complete JFIF vs pre-stripped scan — CLEAN (the favourable
answer).**
`JpegScan::parse` (rtp.rs) accepts a **complete JFIF**: it requires SOI (`FF D8`),
walks every marker segment (APP0/COM skipped), reads DQT (8-bit tables, ids 0–1),
SOF0 (8-bit, 3 components, Y sampling `0x21`→Type 0 / `0x22`→Type 1, dimensions
multiple of 8 and ≤ 2040), DHT (must match Annex K.3 tables exactly), DRI, locates
SOS, and returns the scan **excluding the trailing `FF D9`**. `JpegPayloader::packetize`
then emits: main header with **byte** fragment offsets, `Q = 255`, quant header
(`MBZ=0 | Precision=0 | Length=64·qcount`, i.e. 128 for two tables) in the first
packet only, restart header with the word `0xFFFF` (F=1, L=1, count=0x3FFF), and the
RTP marker bit on the final fragment only. So the answer to the decisive question is:
**it accepts a complete JFIF and strips the header itself** — exactly the behaviour we
need. This item alone would have supported adoption.

**Item 9 — H.264 payloader in the tree: FAIL (hard stop).**
`H264Payloader` (RFC 6184: single NAL units + FU-A fragmentation) is defined
**unconditionally** in `rusty_esp_video-core/src/rtp.rs` (lines 126 ff.); the only
`cfg` in that entire file is `#[cfg(test)]`. `pub mod rtp;` and `pub mod annexb;`
(H.264 NAL scanning, 398 lines) are unconditional in `lib.rs`, and `mpegts.rs` muxes
`Codec::H264` unconditionally (`STREAM_TYPE_H264 = 0x1B`). The optional `h264`
*feature* gates only the software **encoder** (`src/h264.rs` + `dep:rusty_h264`), not
the payloader. There is therefore **no feature that removes the H.264 path** — it is
compiled into every consumer of the crate, feature flags aside. The plan’s
Must-NOT-Have forbids any H.264 path anywhere in the tree and F1 enforces it; the
plan states explicitly that an H.264 path compiled in “even behind a disabled feature”
breaches the compliance gate. This item is a hard stop on its own.

**Item 10 — provenance and claim verification: NOT CLEAN (claims remain
`CLAIMED, UNVERIFIED`).**
Recorded facts: author “Remade With Rust” / Mata Network (homepage `mata.network`);
repository `github.com/Remade-With-Rust/rusty_esp_video`, created 2026-09-01,
**0 stars, 0 forks**, 2 open issues, default branch `main`, last push 2026-10-09;
crate created 2026-09-16, two versions (0.1.0: 11 downloads, 0.1.2: 11 + misc = 23
total); package SHA `577f71f83cb8b9dae6abf46a85171dbbf28bbe6a`. The two specific
claims — “byte-identical round-trip against FFmpeg’s own RTP/JPEG output” and
“14.3 fps / 0.51 Mbit/s on a Seeed XIAO ESP32-S3 Sense — this exact board” — are
backed by a `docs/LEDGER.md` that exists upstream (HTTP 200) with stated reproduction
steps, **but were not reproduced by this audit** and remain **`CLAIMED, UNVERIFIED`**
per the plan; they must not appear in any project document as established fact.
Additional red flag: the published README opens with an injected third-party
advertising line (“FREE RAG Converter Online — RAGconverter.com”), i.e. the project’s
public-facing content control is weak enough for ad spam to land in the crate
README — a supply-chain hygiene signal for a three-week-old crate on the critical
path.

**Verdict:** items 1–6, 8 clean; **item 7 FAIL; item 9 FAIL; item 10 not clean.**
Adoption requires 10/10 — **REJECT** (two independent hard failures). Even without
item 9, items 7 and 10 alone defeat the burden of proof for a 23-download crate on
the central critical path of ~150 lines that RFC 2435 fully specifies.

**Rejected alternative (recorded for fairness):** adopt `rusty_esp_video` and fork the
payloader to excise H.264. Rejected because (a) patching a three-week-old 0-star
crate on our critical path creates a permanent rebase liability, (b) the fork inherits
an unaudited moving target (`main` moves; versions float), and (c) the plan’s gate is
adoption-as-published, and item 9 alone forbids that.

---

## Part B — `oxideav-avi` 0.0.10: items 1, 2, 5, 9

(Plan-approved at `=0.0.10` for Todo 9 / IS-3; it is the sole mechanism behind the
headline AVI deliverable. Same gate, four items.)

**Item 1 — `build.rs`: CLEAN.** No `build.rs`; `build = false`; nothing runs at
compile time.

**Item 2 — `remote_component`: CLEAN.** No `remote_component` anywhere; pure-Rust
package. One runtime dependency: `oxideav-core = "0.1"` — a **floating caret range**,
not `=`. That is an adopter-side pinning obligation (Todo 9 must pin
`oxideav-avi =0.0.10` directly and lock `oxideav-core` in `Cargo.lock`; the crate’s
range does not “float at build time” once the lock exists). Recorded, not a
remote-component violation.

**Item 5 — licenses: CLEAN.** `oxideav-avi` MIT; `oxideav-core` 0.1.39 MIT;
transitive (`cargo tree`): `bytemuck` Zlib OR Apache-2.0 OR MIT, `serde_json` /
`serde_core` / `itoa` / `syn` / `quote` / `proc-macro2` / `thiserror` MIT OR
Apache-2.0, `memchr` Unlicense OR MIT, `zmij` MIT, `unicode-ident` (MIT OR Apache-2.0)
AND Unicode-3.0. Dev-only `criterion` 0.5 Apache-2.0 OR MIT. All permissive.

**Item 9 — H.264 path: CLEAN.** Zero references to `h264`/`avc1`/`x264` in any
`.rs` file of the published package (20 276 lines of `src/`); `oxideav-core`’s
`src/` references `"h264"` only as an **opaque codec-name string** (`CodecId::new`)
and doc comments — a container carries fourcc bytes, it does not encode or packetize
video. No encoder, decoder, payloader, or feature flag for H.264 exists in either
crate; our firmware only ever hands it MJPEG (`MJPG`) bytes, so no H.264 path enters
the tree through this dependency. (The README’s codec table that *lists* `h264` among
AVI fourccs is documentation of the demuxer’s name vocabulary, not a code path —
this distinction is recorded so F1’s grep-based check is interpreted correctly.)

**Exemption argument — stated, not omitted (per plan).** The plan previously exempted
`oxideav-avi` for no stated reason. The stated reason is test evidence, measured from
the published package: **97 integration-test binaries carrying 629 `#[test]`
functions** (including `avi_ffmpeg` and `round394_ffmpeg_blackbox` — black-box
verification against ffmpeg itself — plus `round415_golden_mux_hash` byte-golden
tests), a **cargo-fuzz** harness referenced by `.github/workflows/fuzz.yml` (daily
scheduled fuzzing, structure-aware writer/reader targets, README §Fuzzing), a
criterion benchmark suite, and 12 000 lines of demuxer alone. That is the concrete,
checkable reason to trust a 1-star single-maintainer crate more than a 23-download
one: its correctness claim is machine-checked and continuously re-checked. It is an
argument, not an exemption from scrutiny — the four items above were still run and
their verdicts stand on their own.

**Known caveat carried forward (not an audit item):** the plan notes the crate’s docs
contradict its own OpenDML implementation somewhere in the large README support
matrix; this audit did not isolate the specific row within its timebox. Recorded as
an open observation for Todo 9 to pin down while writing the IS-3 integration
(behavioural tests, not README prose, are the acceptance oracle).

---

## Part C — Decision and its consequences

1. **REJECT `rusty_esp_video`** (items 7, 9 fail; item 10 unclean). It is not added
   to any manifest; the temporary `=`-pinned dependency used for `cargo tree` was
   removed again.
2. **Write `firmware/src/jfif.rs`** — no_std, zero-dependency, host-includable —
   implementing RFC 2435 packetization per the field spec below. This file is the
   plan’s real answer and is exercised by the two acceptance gates in `harness/`.
3. **MSRV floor reverts to 1.82** (Todo 19 changes `firmware/Cargo.toml`
   `rust-version = "1.85"` → `"1.82"`; the raised floor existed only for this crate).
4. `oxideav-avi` proceeds as planned at `=0.0.10` (items 1/2/5/9 clean; exemption
   argument stated above), becoming a firmware dependency in Todo 9, not now.

## RFC 2435 field spec as implemented (decisions and where the plan and the RFC differed)

- **Main header (8 bytes):** `Type-specific(8) | Fragment Offset(24, BYTES) |
  Type(8) | Q(8) | Width(8) | Height(8)`. Offset is in **bytes** (RFC 2435 §3.1.2 —
  the RFC-2035 8-octet-block unit was abandoned; this is the most common
  implementation bug and is asserted by gate (a)).
- **Type:** 0 = 4:2:2 (Y sampling 2×1), 1 = 4:2:0 (Y sampling 2×2), derived from SOF0
  sampling factors; **+64** when a DRI segment is present → Type **64/65** as the
  plan requires. Sampling other than 2×1/2×2 (e.g. 4:4:4) is a typed error
  (types 2–63 reserved).
- **Q = 255** with in-band tables: `MBZ=0 | Precision=0 | Length=128` (two 8-bit
  tables, 64 bytes each, zig-zag order) in the **first packet only**. Optional
  static-Q path: if both tables exactly equal RFC 2435 Appendix A `MakeTables(q)`
  for some `q ∈ 1..=99` (zig-zag mapped), emit `Q = q` and omit the quant header.
  Q=0 and Q∈100..127 never emitted.
- **Width/Height:** units of 8 pixels, derived from SOF0; non-multiples of 8 or
  > 2040 are typed errors (never silently rounded).
- **Marker bit:** set on the **final fragment of the frame only** (no P bit exists —
  `P` is a JPEG arithmetic-coding selector, not an RTP/JPEG field).
- **EOI policy (explicit):** the trailing `FF D9` is **excluded from the payload** —
  the payload is the pure entropy-coded scan; the receiver appends EOI per
  §3.1.9/Appendix B. This makes double-EOI impossible by construction and is asserted
  in gate (a) (`Σ payload == scan length` would fail if EOI crept in).
- **Restart header (4 bytes, types 64–127):** `Restart Interval(16) | F(1) | L(1) |
  Restart Count(14)` — interval is the DRI value in MCUs. **Plan-typo note:** the
  plan body renders this `F(1)|L(2)|Restart Count(11)`, which sums to 30 bits and
  cannot hold the plan’s own `0x3FFF` sentinel. Verified against the RFC diagram’s
  character widths (interval segment 30 chars ↔ 16 bits, count segment 27 chars ↔
  14 bits at the drawing’s ~1.875 chars/bit) and against `rusty_esp_video`’s
  independent encoder (`0xFFFF == F|L|0x3FFF`): **F = bit 16, L = bit 17, count =
  bits 18–31**. **Chunking policy:** every restart-bearing packet carries
  `F=1, L=1, Count=0x3FFF` (§3.1.7/§4.4 “reassemble the entire frame” mode) — a
  deliberate simplification that is fully RFC-sanctioned and avoids partial-decode
  chunk alignment on a lossy Wi-Fi link.
- **RSTn markers stay in the payload** (§3.1.9 — RFC 2435 does not strip them; the
  header only locates intervals); conversely, a frame **without** DRI must contain no
  RSTn anywhere (typed error if it does), and a frame **with** DRI must contain
  exactly `ceil(total_mcus / Ri) − 1` markers (hard-count assertion on the synthetic
  fixture).
- **Huffman tables:** the receiver synthesizes Annex K.3 tables from the Type, so
  `jfif.rs` verifies the source DHT is exactly the four standard tables; a scan
  coded with optimized/custom tables would decode as garbage at every receiver and is
  rejected with a typed error instead.

**Acceptance gates (both must pass; run in `harness/`, host-only):**

- **(a) Fragmentation accounting:** reassembling our emitted fragments by
  `fragment_offset` reproduces the extracted scan **byte-for-byte with no header
  re-prepend**; `Σ payload_len == extracted scan length`; and
  `Σ packet_len == Σ payload_len + Σ header_len` with `Σ header_len` equal to the
  exact expected per-packet header sizes (8 base, +4 restart, +4+128 first-packet
  quant). *(Interpretation note: the plan writes a single combined equation
  `Σ payload + Σ header == scan`, which is dimensionally impossible once headers are
  positive; the two equalities above are what catch the three faults the plan names —
  byte-vs-block offsets fail the byte-for-byte reassembly, a missing/misplaced
  4-byte quant header fails the header-sum equality, and a double-EOI fails
  `Σ payload == scan`.)*
- **(b) Conformance via an independent depacketizer:** fragments are wrapped in RTP
  and streamed over loopback UDP to **ffmpeg’s own RFC 2435 depacketizer**
  (`libavformat/rtpdec_jpeg.c`) via an SDP session; ffmpeg must decode error-free,
  `ffprobe` must report `codec_name=mjpeg` at 320×240, and decoded pixels must match
  the source frame’s decode (PSNR). `retina` 0.4.21 was fetched for the harness as a
  second candidate oracle; if it does not run as an in-process depacketizer, that is
  recorded honestly in the evidence rather than glossed over.

## Evidence

`.omo/evidence/task-6/adr.md` (this audit, mirrored with raw command transcripts),
`.omo/evidence/task-6/roundtrip.txt` (gate (a) numbers, gate (b) results, restart
hard counts), `.omo/evidence/task-6/ffprobe-streams.json` (or
`CONFORMANCE_NOTE.md` if ffmpeg were unavailable — it was installed, ffmpeg 5.1.9).
