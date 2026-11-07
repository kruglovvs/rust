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
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=../.env");
    write_wifi_credentials();
    patch_fatfs_exfat();

    // ESP-IDF 5.5.2 `sdmmc_card_init` always issues the SDIO CMD5 probe
    // (`sdmmc_init.c` `sdmmc_init_io`); the inserted card answers it late and
    // the stale R1 is then read as CMD59's response, aborting init with
    // ESP_ERR_NOT_SUPPORTED. `storage::ffi::__wrap_sdspi_host_do_transaction`
    // reports CMD5 unsupported instead (upstream's
    // CONFIG_SD_ENABLE_SDIO_SUPPORT=n path). ldproxy wraps xtensa GCC, so the
    // flag needs GCC's `-Wl,` passthrough.
    println!("cargo:rustc-link-arg=-Wl,--wrap=sdspi_host_do_transaction");

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

/// Enable exFAT in the ESP-IDF FatFs component.
///
/// ESP-IDF's `components/fatfs/src/ffconf.h` hard-codes `FF_FS_EXFAT 0` and
/// there is no Kconfig symbol for it, so an exFAT-formatted card fails to
/// mount with `ENODEV`. This rewrites only the `FF_FS_EXFAT` line to `1` and
/// writes the file back if it changed. A missing or unwritable IDF tree is
/// reported as a warning but never fails the build, so hosts without a local
/// IDF checkout (or with a read-only one) still compile.
fn patch_fatfs_exfat() {
    // Re-evaluate this patch when the IDF location changes. Deliberately no
    // `rerun-if-changed` on the header itself: writing it would retrigger the
    // build script forever.
    println!("cargo:rerun-if-env-changed=IDF_PATH");

    let idf_path = env::var_os("IDF_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/esp/idf"));
    let ffconf = idf_path.join("components/fatfs/src/ffconf.h");

    let contents = match fs::read_to_string(&ffconf) {
        Ok(contents) => contents,
        Err(error) => {
            println!(
                "cargo:warning=firmware build.rs: could not read {} ({error}); \
                 skipping ESP-IDF FatFs exFAT patch",
                ffconf.display()
            );
            return;
        }
    };

    let mut patched = String::with_capacity(contents.len() + 1);
    let mut changed = false;
    for line in contents.split_inclusive('\n') {
        if !changed {
            let (body, newline) = match line.strip_suffix('\n') {
                Some(body) => match body.strip_suffix('\r') {
                    Some(body) => (body, "\r\n"),
                    None => (body, "\n"),
                },
                None => (line, ""),
            };
            if let Some(replacement) = rewrite_exfat_line(body) {
                patched.push_str(&replacement);
                patched.push_str(newline);
                changed = true;
                continue;
            }
        }
        patched.push_str(line);
    }

    if !changed {
        return;
    }

    match fs::write(&ffconf, patched) {
        Ok(()) => println!(
            "cargo:warning=firmware build.rs: patched ESP-IDF FatFs for exFAT \
             (FF_FS_EXFAT=1) in {}",
            ffconf.display()
        ),
        Err(error) => println!(
            "cargo:warning=firmware build.rs: could not write {} ({error}); \
             ESP-IDF FatFs exFAT support may be unavailable",
            ffconf.display()
        ),
    }
}

/// Rewrite a `#define FF_FS_EXFAT <0>` line to `1`, preserving indentation.
/// Returns `None` for any other line, so unrelated macros are never touched.
fn rewrite_exfat_line(line: &str) -> Option<String> {
    let trimmed_start = line.trim_start();
    let indent = &line[..line.len() - trimmed_start.len()];
    let mut parts = trimmed_start.split_whitespace();
    if parts.next()? != "#define"
        || parts.next()? != "FF_FS_EXFAT"
        || parts.next()? != "0"
        || parts.next().is_some()
    {
        return None;
    }
    Some(format!("{indent}#define FF_FS_EXFAT\t1"))
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

fn write_wifi_credentials() {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let env_path = Path::new(&manifest_dir).join("../.env");
    let values = fs::read_to_string(env_path)
        .ok()
        .map(|contents| parse_wifi_credentials(&contents));
    let (ssid, password) = values.unwrap_or_default();
    let generated = format!(
        "pub const WIFI_SSID: Option<&str> = {}\n;pub const WIFI_PASSWORD: Option<&str> = {}\n;",
        rust_option(ssid.as_deref()),
        rust_option(password.as_deref())
    );
    let out_dir = env::var_os("OUT_DIR").unwrap();
    fs::write(Path::new(&out_dir).join("wifi_credentials.rs"), generated).unwrap();
}

fn parse_wifi_credentials(contents: &str) -> (Option<String>, Option<String>) {
    let mut ssid = None;
    let mut password = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value)
            .to_owned();
        match key.trim() {
            "WIFI_SSID" => ssid = Some(value),
            "WIFI_PASSWORD" => password = Some(value),
            _ => {}
        }
    }
    (ssid, password)
}

fn rust_option(value: Option<&str>) -> String {
    value.map_or_else(|| "None".to_owned(), |value| format!("Some({value:?})"))
}
