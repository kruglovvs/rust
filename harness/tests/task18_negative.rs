//! Host-executed negative-path tests for the SD probe classifier
//! (`firmware/src/sd_probe.rs`, Task 18).
//!
//! These functions were previously inside `storage.rs`, whose `esp-idf`
//! imports keep that file off the host. The pure classification logic now
//! lives in a module with no firmware imports, so this target compiles it
//! unchanged and RUNS the matrix here (the device-only copy in
//! `storage.rs::tests` still needs a board).
//!
//! Scope is deliberately negative: wrong chip-select (all-ones), a card that
//! answers but identifies nothing, a legacy card that rejected `CMD8`, and
//! "absent only when every candidate pin is silent". No happy-path
//! filesystem or hardware behaviour is asserted.

#[path = "../../firmware/src/sd_probe.rs"]
mod sd_probe;

use sd_probe::{
    aggregate_detected, classify_cmd8, encode_command, find_r1, is_all_ones,
    legacy_identifies, CardClass, ProbeOutcome, R7,
};

#[test]
fn command_frames_use_the_real_crcs_and_six_bit_index() {
    assert_eq!(
        encode_command(0, 0x0000_0000, 0x95),
        [0x40, 0x00, 0x00, 0x00, 0x00, 0x95]
    );
    assert_eq!(
        encode_command(8, 0x0000_01AA, 0x87),
        [0x48, 0x00, 0x00, 0x01, 0xAA, 0x87]
    );
    assert_eq!(encode_command(0xFF, 0, 0x01)[0], 0x40 | 0x3F);
    assert_eq!(encode_command(41, 0, 0x01)[0], 0x40 | 41);
}

#[test]
fn all_ones_means_not_selected_and_empty_proves_nothing() {
    assert!(is_all_ones(&[0xFF; 16]));
    assert!(is_all_ones(&[0xFF]));
    assert!(!is_all_ones(&[0xFF, 0x01]));
    assert!(!is_all_ones(&[0x01, 0xFF]));
    assert!(!is_all_ones(&[]));
}

#[test]
fn r1_is_found_after_ncr_filler_and_never_in_an_all_ones_window() {
    let mut win = [0xFFu8; 12];
    win[3] = 0x01;
    assert_eq!(find_r1(&win), Some((3, 0x01)));
    assert_eq!(find_r1(&[0xFFu8; 32]), None);
    assert_eq!(find_r1(&[0xFF, 0x00]), Some((1, 0x00)));
    assert_eq!(find_r1(&[0xFF, 0x7F]), Some((1, 0x7F)));
}

#[test]
fn cmd8_classification_is_positive_only_for_a_full_valid_r7() {
    let sdv2 = R7 {
        r1: 0x01,
        payload: [0x01, 0x00, 0x00, 0xAA],
    };
    assert_eq!(classify_cmd8(&sdv2), Some(CardClass::Sdv2));

    let legacy = R7 {
        r1: 0x05,
        payload: [0xFF; 4],
    };
    assert_eq!(classify_cmd8(&legacy), Some(CardClass::Legacy));

    for garbage in [
        R7 {
            r1: 0x00,
            payload: [0x01, 0x00, 0x00, 0xAA],
        },
        R7 {
            r1: 0x01,
            payload: [0x01, 0x00, 0x00, 0x55],
        },
        R7 {
            r1: 0x09,
            payload: [0x01, 0x00, 0x00, 0xAA],
        },
    ] {
        assert_eq!(classify_cmd8(&garbage), None, "{garbage:?}");
    }

    let bare_r1_with_illegal = R7 {
        r1: 0x04,
        payload: [0xFF; 4],
    };
    assert_eq!(
        classify_cmd8(&bare_r1_with_illegal),
        Some(CardClass::Legacy),
        "ILLEGAL_COMMAND routes to the verified ACMD41/CMD58 fallback"
    );
}

#[test]
fn legacy_fallback_requires_two_clean_answers() {
    assert!(legacy_identifies(0x01, 0x00));
    assert!(legacy_identifies(0x00, 0x00));
    assert!(!legacy_identifies(0x01, 0x04));
    assert!(!legacy_identifies(0xFF, 0x00));
    assert!(!legacy_identifies(0x01, 0xFF));
}

#[test]
fn aggregate_reports_the_first_detected_pin_only() {
    let both = [
        (
            "GPIO21",
            ProbeOutcome::Detected {
                class: CardClass::Sdv2,
            },
        ),
        (
            "GPIO3",
            ProbeOutcome::Detected {
                class: CardClass::Legacy,
            },
        ),
    ];
    assert_eq!(aggregate_detected(&both), Some("GPIO21"));

    let alternate_only = [
        ("GPIO21", ProbeOutcome::NotSelected),
        (
            "GPIO3",
            ProbeOutcome::Detected {
                class: CardClass::Sdv2,
            },
        ),
    ];
    assert_eq!(aggregate_detected(&alternate_only), Some("GPIO3"));
}

#[test]
fn aggregate_is_absent_when_every_candidate_is_silent() {
    for second in [ProbeOutcome::NotSelected, ProbeOutcome::Unidentified] {
        let results = [("GPIO21", ProbeOutcome::NotSelected), ("GPIO3", second)];
        assert_eq!(aggregate_detected(&results), None, "{second:?}");
    }
    let empty: [(&str, ProbeOutcome); 0] = [];
    assert_eq!(aggregate_detected(&empty), None);
}

#[test]
fn probe_tokens_match_the_logged_words() {
    assert_eq!(
        ProbeOutcome::Detected {
            class: CardClass::Sdv2
        }
        .tag(),
        "detected"
    );
    assert_eq!(ProbeOutcome::NotSelected.tag(), "not-selected");
    assert_eq!(ProbeOutcome::Unidentified.tag(), "unidentified");
    assert_eq!(CardClass::Sdv2.to_string(), "SDv2");
    assert_eq!(CardClass::Legacy.to_string(), "legacy");
}
