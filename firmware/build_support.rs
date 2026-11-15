// Shared esp-idf link-argument forwarding for every build script in the
// firmware workspace.
//
// `esp-idf-sys` declares `links = "esp_idf"` and emits
// `cargo:EMBUILD_LINK_ARGS=<shlex-joined args>` from its build script. Cargo
// only hands that metadata to a dependent that has a build script, and
// `cargo::rustc-link-arg` only applies to the package that emits it, so each
// crate that links a final ESP-IDF image re-emits every argument itself.
// Without them the link runs a bare `ldproxy` with no `--ldproxy-linker` and
// panics ("Cannot locate argument '--ldproxy-linker <linker>'").
//
// The four component crates `include!` this file from their two-line build
// scripts. `firmware/build.rs` includes it too, but calls `forward_link_args`
// so it can patch the C sources named by the argument list before it emits.
//
// Deliberately dependency-free: `embuild` could do the parse, but a
// `[build-dependencies]` entry is unnecessary here. The value is
// `shlex::join`-encoded, so it is un-split by `shlex_split` below.

// Scan the environment for the propagated link arguments.
//
// Cargo uppercases metadata keys into `DEP_<LINKS>_<KEY>`, so match on the
// suffix. `None` means `esp-idf-sys` has not run its build script yet; every
// caller warns rather than letting the link fail downstream.
pub fn forward_link_args() -> Option<(String, Vec<String>)> {
    let (key, value) = std::env::vars().find(|(key, value)| {
        key.starts_with("DEP_") && key.ends_with("EMBUILD_LINK_ARGS") && !value.is_empty()
    })?;
    Some((key, shlex_split(&value)))
}

// Emit one directive per argument: each value is a single argv item, and a
// single joined directive silently breaks `ldproxy`. Also re-runs the build
// script when the propagated set changes (an `esp-idf-sys` rebuild).
pub fn emit_link_args(key: &str, args: &[String]) {
    for arg in args {
        println!("cargo::rustc-link-arg={arg}");
    }
    println!("cargo:rerun-if-env-changed={key}");
}

// Entry point for a component build script: forward or warn when absent.
//
// The `rerun-if-changed` target is this shared file resolved from the
// component crate (`components/<name>/build.rs` -> `firmware/`).
#[allow(dead_code)] // the firmware crate only calls forward_link_args/emit
pub fn forward_and_emit_link_args() {
    println!("cargo:rerun-if-changed=../../build_support.rs");
    match forward_link_args() {
        Some((key, args)) => emit_link_args(&key, &args),
        None => println!(
            "cargo:warning={}: no EMBUILD_LINK_ARGS; on-device tests may fail to link",
            std::env::var("CARGO_PKG_NAME").unwrap_or_default()
        ),
    }
}

// Minimal `shlex` splitter: whitespace separates tokens, single quotes are
// literal, double quotes allow backslash escapes, backslash escapes outside
// quotes. Mirrors what `shlex::join` (embuild) produces.
pub fn shlex_split(input: &str) -> Vec<String> {
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
