//! Forward the ESP-IDF linker arguments to this crate's own test binaries.
//!
//! `esp-idf-sys` publishes them through its `links` metadata, but
//! `cargo:rustc-link-arg` only applies to the package whose build script
//! emits it, so a component that builds an on-device test binary has to
//! repeat the forwarding or `ldproxy` panics.

use std::env;

fn main() {
    let link_args = env::vars().find(|(key, value)| {
        key.starts_with("DEP_") && key.ends_with("EMBUILD_LINK_ARGS") && !value.is_empty()
    });
    match link_args {
        Some((key, value)) => {
            for arg in shlex_split(&value) {
                println!("cargo::rustc-link-arg={arg}");
            }
            println!("cargo:rerun-if-env-changed={key}");
        }
        None => println!(
            "cargo:warning={}: no EMBUILD_LINK_ARGS; on-device tests may fail to link",
            env::var("CARGO_PKG_NAME").unwrap_or_default()
        ),
    }
}

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
