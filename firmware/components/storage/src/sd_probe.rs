//! Pure SD-card probe classification (plan Todo 7, extracted from
//! `storage.rs` so the decision logic is host-testable).
//!
//! This module owns **only** the hardware-free decisions: command framing,
//! the all-ones "not selected" signal, R1 location, `CMD8`/R7 and
//! `ACMD41`/`CMD58` identification, and the aggregate "first detected pin
//! wins / absent only when every pin is silent" fold. It imports nothing from
//! `esp-idf`, so `harness/tests/task18_negative.rs` compiles and runs it on
//! the host.
//!
//! The public re-export path is unchanged: `storage::{CardClass,
//! ProbeOutcome, R7}` and the pure functions remain nameable through
//! `crate::storage`.

use core::fmt;

/// Which generation the responding card announced itself as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardClass {
    /// `CMD8` returned a valid R7 → SD Physical Layer spec 2.x (SDv2 / SDHC /
    /// SDXC).
    Sdv2,
    /// `CMD8` answered `ILLEGAL_COMMAND`, so identification fell through to
    /// `ACMD41`/`CMD58` → a legacy card.
    Legacy,
}

impl fmt::Display for CardClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CardClass::Sdv2 => f.write_str("SDv2"),
            CardClass::Legacy => f.write_str("legacy"),
        }
    }
}

/// Per-candidate CS pin verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The card completed identification on this CS pin.
    Detected { class: CardClass },
    /// MISO stayed all-ones (`0xFF`) for the whole exchange — this CS selects
    /// nothing: wrong pin, or no card seated.
    NotSelected,
    /// Something toggled MISO off all-ones but identification never completed.
    Unidentified,
}

impl ProbeOutcome {
    /// Short token for the per-pin log line.
    pub const fn tag(&self) -> &'static str {
        match self {
            ProbeOutcome::Detected { .. } => "detected",
            ProbeOutcome::NotSelected => "not-selected",
            ProbeOutcome::Unidentified => "unidentified",
        }
    }
}

/// The card's answer to one command (R1 plus, for `CMD8`, the R7 payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct R7 {
    /// First byte with its MSB clear — the R1 status byte.
    pub r1: u8,
    /// The four R7 payload bytes that follow R1.
    pub payload: [u8; 4],
}

/// Encode one 6-byte SD SPI command frame:
/// `[0x40|index, arg>>24, arg>>16, arg>>8, arg, crc]`.
///
/// Bit 6 is the start bit, bit 5 the transmission bit; the remaining six bits
/// are the command index. CRC checking is off by default in SPI mode **except
/// for `CMD0` and `CMD8`**, which therefore need their real CRCs (`0x95`,
/// `0x87`); every other frame only needs the end bit, `0x01`.
pub fn encode_command(index: u8, arg: u32, crc: u8) -> [u8; 6] {
    [
        0x40 | (index & 0x3F),
        (arg >> 24) as u8,
        (arg >> 16) as u8,
        (arg >> 8) as u8,
        arg as u8,
        crc,
    ]
}

/// `true` when every byte is `0xFF`.
///
/// This is plan (b)'s one reliable no-response signal: with the card's CS
/// deasserted its MISO is pulled high, so all-ones means *not selected* —
/// wrong CS or no card.
pub fn is_all_ones(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|&b| b == 0xFF)
}

/// Locate R1 inside a post-command response window.
///
/// The card may insert up to 8 `0xFF` (NCR) bytes before answering; R1 is
/// defined by its clear MSB, and `0xFF` filler has that bit set, so the first
/// byte with `b & 0x80 == 0` is R1. `None` = the card never answered
/// (all-ones ⇒ not selected).
pub fn find_r1(window: &[u8]) -> Option<(usize, u8)> {
    window
        .iter()
        .enumerate()
        .find(|(_, &b)| b & 0x80 == 0)
        .map(|(i, &b)| (i, b))
}

/// `true` when a command response window carries an R1 with the **idle** bit
/// set — the answer `CMD0` (`GO_IDLE_STATE`) must produce before the card is
/// in SPI mode.
///
/// SD Physical Layer §7.2.1: the host repeats `CMD0` until the card answers
/// R1 = `0x01`. Some cards swallow the first frames while their power-up state
/// machine settles, so a single unanswered `CMD0` proves nothing; the caller
/// retries a bounded number of times and uses this predicate to stop on the
/// first real idle answer.
///
/// `false` when no R1 appeared in the window (all-ones ⇒ not selected) or when
/// the card answered with an error flag set (e.g. `0x08`, `COM CRC ERROR`).
pub fn r1_is_idle(window: &[u8]) -> bool {
    find_r1(window).is_some_and(|(_, r1)| r1 & 0x01 != 0)
}

/// Classify an `ACMD41`/`CMD58` exchange on a card that rejected `CMD8`.
///
/// Returns `true` when the legacy identification sequence responded sanely.
pub fn legacy_identifies(r1_acmd41: u8, r1_cmd58: u8) -> bool {
    // A real answer has R1's MSB clear on both; idle (0x01) on ACMD41 is
    // expected, and CMD58 must not report ILLEGAL_COMMAND (0x04).
    r1_acmd41 & 0x80 == 0 && r1_cmd58 & 0x80 == 0 && r1_cmd58 & 0x04 == 0
}

/// Classify a `CMD8`/`SEND_IF_COND` R7 exchange — plan (b).
///
/// - `Sdv2` — R1 = `0x01` (idle, no error flags), voltage accepted `0x01`
///   (2.7–3.6 V) and the `0xAA` check pattern echoed. This is the "expect
///   `0x01` on SDv2" the plan asks for.
///
/// The 32-bit R7 word is `[31:12] reserved | [11:8] voltage | [7:0] echo`,
/// transmitted MSB first, so the four payload bytes after R1 are
/// `[0x00, 0x00, voltage, echo]`.
/// - `Legacy` — R1 has `ILLEGAL_COMMAND` (`0x04`) set: a pre-SDv2 card that
///   predates `CMD8`; the caller falls back to `ACMD41`/`CMD58`.
/// - `None` — the card answered but with a combination that identifies
///   nothing (caller reports [`ProbeOutcome::Unidentified`]).
pub fn classify_cmd8(reply: &R7) -> Option<CardClass> {
    if reply.r1 & 0x04 != 0 {
        return Some(CardClass::Legacy);
    }
    if reply.r1 == 0x01 && reply.payload[2] == 0x01 && reply.payload[3] == 0xAA {
        return Some(CardClass::Sdv2);
    }
    None
}

/// Return the pin of the **first** candidate that identified a card.
///
/// This is the generic half of `storage::aggregate_probe`: order decides, so
/// a card answering on both candidates reports the first (GPIO21 on this
/// board). `None` means every candidate was `NotSelected`/`Unidentified`,
/// which `storage` maps to `CardProbe::Absent`.
pub fn aggregate_detected<P: Copy>(results: &[(P, ProbeOutcome)]) -> Option<P> {
    results
        .iter()
        .find(|(_, outcome)| matches!(outcome, ProbeOutcome::Detected { .. }))
        .map(|(pin, _)| *pin)
}
