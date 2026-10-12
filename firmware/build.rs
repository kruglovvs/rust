//! Forward the ESP-IDF linker arguments propagated by `esp-idf-sys`.
//!
//! `esp-idf-sys` declares `links = "esp_idf"` and emits
//! `cargo:EMBUILD_LINK_ARGS=<shlex-joined args>` from its build script
//! (embuild `LinkArgs::propagate()`), including `--ldproxy-linker`,
//! `--ldproxy-cwd`, the IDF linker scripts, component archives (notably
//! `espressif__esp32-camera`) and `main/libmain.a`.
//!
//! Cargo only hands that metadata to a dependent that has a build script, and
//! `cargo::rustc-link-arg` only applies to the package that emits it — so this
//! crate must re-emit each argument itself, otherwise the final link runs
//! bare `ldproxy` with no `--ldproxy-linker` and panics
//! ("Cannot locate argument '--ldproxy-linker <linker>'").
//!
//! Deliberately dependency-free: embuild could do the parse, but adding a
//! `[build-dependencies]` entry to `firmware/Cargo.toml` is out of scope for
//! this task. The value is `shlex::join`-encoded, so we un-split it here.

use std::env;

fn main() {
    // cargo uppercases metadata keys for the DEP_<LINKS>_<KEY> env var; match
    // on the suffix so casing differences do not matter.
    let link_args = env::vars().find(|(key, value)| {
        key.starts_with("DEP_") && key.ends_with("EMBUILD_LINK_ARGS") && !value.is_empty()
    });

    match link_args {
        Some((key, value)) => {
            for arg in shlex_split(&value) {
                // One directive per argument: each value is a single argv item.
                println!("cargo::rustc-link-arg={arg}");
            }
            // Re-run if the propagated set changes (esp-idf-sys rebuild).
            println!("cargo:rerun-if-env-changed={key}");
        }
        None => {
            // Surface the failure instead of letting ldproxy panic downstream.
            println!(
                "cargo:warning=firmware build.rs: no DEP_*_EMBUILD_LINK_ARGS metadata found; \
                 esp-idf-sys has not run its build script yet"
            );
        }
    }
}

/// Minimal `shlex` splitter: whitespace separates tokens, single quotes are
/// literal, double quotes allow backslash escapes, backslash escapes outside
/// quotes. Mirrors what `shlex::join` (embuild) produces.
fn shlex_split(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('\'') | None => break,
                        Some(ch) => current.push(ch),
                    }
                }
            }
            '"' => {
                in_token = true;
                while let Some(&ch) = chars.peek() {
                    match ch {
                        '"' => {
                            chars.next();
                            break;
                        }
                        '\\' => {
                            chars.next();
                            if let Some(esc) = chars.next() {
                                current.push(esc);
                            }
                        }
                        other => {
                            chars.next();
                            current.push(other);
                        }
                    }
                }
            }
            '\\' => {
                in_token = true;
                if let Some(esc) = chars.next() {
                    current.push(esc);
                }
            }
            c if c.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            other => {
                in_token = true;
                current.push(other);
            }
        }
    }

    if in_token {
        tokens.push(current);
    }
    tokens
}