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
use std::process::Command;

fn main() {
    warn_missing_wifi_credentials();
    // The knob is a C-source patch input, not a Rust constant: `settings.rs`
    // resolves the same variable with `option_env!` for the app.
    println!("cargo:rerun-if-env-changed=VIDEO_JPEG_QUALITY");
    let jpeg_quality = env::var("VIDEO_JPEG_QUALITY")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(12)
        .min(63);
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
            let args = shlex_split(&value);
            patch_esp32_camera_eoi_scan(&args);
            patch_esp32_camera_soi_dump(&args);
            patch_esp_capture_jpeg_quality(&args, &jpeg_quality);
            for arg in args {
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

/// Replace esp32-camera's tail-only EOI search with a full-frame scan.
///
/// The component finalizes `camera_fb_t.len` from the DMA EOF interrupt count
/// at VSYNC. That count can lag the bytes actually written into the PSRAM
/// framebuffer (or overrun the real JPEG end), so the EOI marker is often
/// outside the last `dma_node_buffer_size + 1` bytes. The driver then reports
/// `NO-EOI` and drops a frame whose bytes are actually complete. Scanning the
/// whole framebuffer forward from the validated SOI finds the real EOI and
/// trims `len` to it. This runs the component's own ninja rule so the Rust
/// link in the same build picks up the rebuilt archive.
fn patch_esp32_camera_eoi_scan(link_args: &[String]) {
    let Some(cwd_index) = link_args.iter().position(|arg| arg == "--ldproxy-cwd") else {
        return;
    };
    let Some(build_dir) = link_args.get(cwd_index + 1).map(PathBuf::from) else {
        return;
    };
    let cam_hal = build_dir
        .join("../managed_components/espressif__esp32-camera/driver/cam_hal.c");
    let Ok(contents) = fs::read_to_string(&cam_hal) else {
        println!(
            "cargo:warning=firmware build.rs: could not read {}; skipping camera EOI patch",
            cam_hal.display()
        );
        return;
    };
    if contents.contains("size_t probe_len = cam_obj->fb_size;") {
        return;
    }
    let Some(patched) = rewrite_camera_eoi_scan(&contents) else {
        println!(
            "cargo:warning=firmware build.rs: camera EOI scan block not found in {}",
            cam_hal.display()
        );
        return;
    };
    if let Err(error) = fs::write(&cam_hal, patched) {
        println!(
            "cargo:warning=firmware build.rs: could not write {} ({error}); camera EOI patch skipped",
            cam_hal.display()
        );
        return;
    }

    match run_component_ninja(link_args, &build_dir, "esp-idf/espressif__esp32-camera/libespressif__esp32-camera.a") {
        Ok(status) if status.success() => println!(
            "cargo:warning=firmware build.rs: patched esp32-camera EOI scan to search the full frame buffer"
        ),
        Ok(status) => println!(
            "cargo:warning=firmware build.rs: esp32-camera rebuild failed ({status}); \
             the EOI patch may not be linked"
        ),
        Err(error) => println!(
            "cargo:warning=firmware build.rs: could not run ninja ({error}); \
             the EOI patch may not be linked"
        ),
    }
}

/// Log the length and first bytes of frames that fail the SOI check.
///
/// `CAM_WARN_THROTTLE` only accepts a literal format string, so the existing
/// `NO-SOI` branch cannot report the frame contents. Replace that branch with
/// a direct `ESP_CAMERA_ETS_PRINTF` call that dumps `len` and the first eight
/// bytes, once on the first reject and then every 100 rejects. Idempotent via
/// the `NO-SOI len=` marker.
fn patch_esp32_camera_soi_dump(link_args: &[String]) {
    let Some(cwd_index) = link_args.iter().position(|arg| arg == "--ldproxy-cwd") else {
        return;
    };
    let Some(build_dir) = link_args.get(cwd_index + 1).map(PathBuf::from) else {
        return;
    };
    let cam_hal = build_dir
        .join("../managed_components/espressif__esp32-camera/driver/cam_hal.c");
    let Ok(contents) = fs::read_to_string(&cam_hal) else {
        println!(
            "cargo:warning=firmware build.rs: could not read {}; skipping camera SOI dump patch",
            cam_hal.display()
        );
        return;
    };
    if contents.contains("NO-SOI len=") {
        return;
    }
    let original = "\
    CAM_WARN_THROTTLE(warn_soi_miss_cnt,
                      \"NO-SOI - JPEG start marker missing\");
    return -1;";
    if !contents.contains(original) {
        println!(
            "cargo:warning=firmware build.rs: camera NO-SOI branch not found in {}",
            cam_hal.display()
        );
        return;
    }
    let patched = "\
    if (++warn_soi_miss_cnt == 1 || (warn_soi_miss_cnt % 100) == 0) {
        ESP_CAMERA_ETS_PRINTF(DRAM_STR(\"cam_hal: NO-SOI len=%u head=%02x%02x%02x%02x %02x%02x%02x%02x\\r\\n\"),
                              (unsigned)length,
                              inbuf[0], inbuf[1], inbuf[2], inbuf[3],
                              inbuf[4], inbuf[5], inbuf[6], inbuf[7]);
    }
    return -1;";
    let patched = contents.replace(original, patched);
    if let Err(error) = fs::write(&cam_hal, patched) {
        println!(
            "cargo:warning=firmware build.rs: could not write {} ({error}); camera SOI dump patch skipped",
            cam_hal.display()
        );
        return;
    }

    match run_component_ninja(link_args, &build_dir, "esp-idf/espressif__esp32-camera/libespressif__esp32-camera.a") {
        Ok(status) if status.success() => println!(
            "cargo:warning=firmware build.rs: patched esp32-camera SOI dump"
        ),
        Ok(status) => println!(
            "cargo:warning=firmware build.rs: esp32-camera rebuild failed ({status}); \
             the SOI dump patch may not be linked"
        ),
        Err(error) => println!(
            "cargo:warning=firmware build.rs: could not run ninja ({error}); \
             the SOI dump patch may not be linked"
        ),
    }
}

/// Warn when the build has no Wi-Fi credentials to embed.
///
/// Credentials are secrets: `scripts/build-firmware.sh` sources the gitignored
/// `.env` and forwards them, and they are deliberately absent from
/// `.cargo/config.toml`. A build without them is valid - the firmware boots
/// without a network - so this is a warning, never an error.
fn warn_missing_wifi_credentials() {
    println!("cargo:rerun-if-env-changed=WIFI_SSID");
    println!("cargo:rerun-if-env-changed=WIFI_PASSWORD");
    if env::var_os("WIFI_SSID").is_none() || env::var_os("WIFI_PASSWORD").is_none() {
        println!(
            "cargo:warning=firmware: WIFI_SSID/WIFI_PASSWORD not set - the build will boot \
             without Wi-Fi"
        );
    }
}

/// Replace esp_capture's hard-coded JPEG quality with the configured value.
///
/// The DVP source builds `camera_config_t` inline with `.jpeg_quality = 12`,
/// which is a very high quality (and therefore large) JPEG. The knob lets a
/// build trade size for bandwidth without touching the component.
fn patch_esp_capture_jpeg_quality(link_args: &[String], quality: &u32) {
    let Some(cwd_index) = link_args.iter().position(|arg| arg == "--ldproxy-cwd") else {
        return;
    };
    let Some(build_dir) = link_args.get(cwd_index + 1).map(PathBuf::from) else {
        return;
    };
    let src = build_dir.join(
        "../managed_components/espressif__esp_capture/impl/capture_video_src/capture_video_dvp_src.c",
    );
    let Ok(contents) = fs::read_to_string(&src) else {
        println!(
            "cargo:warning=firmware build.rs: could not read {}; jpeg quality left at its default",
            src.display()
        );
        return;
    };
    let current = contents
        .lines()
        .find(|line| line.trim_start().starts_with(".jpeg_quality = "))
        .and_then(|line| line.split('=').nth(1))
        .and_then(|value| value.trim().trim_end_matches(',').parse::<u32>().ok());
    if current == Some(*quality) {
        return;
    }
    let patched: String = contents
        .lines()
        .map(|line| {
            if line.trim_start().starts_with(".jpeg_quality = ") {
                let indent = &line[..line.len() - line.trim_start().len()];
                format!("{indent}.jpeg_quality = {quality},")
            } else {
                line.to_string()
            }
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    if patched == contents {
        return;
    }
    if let Err(error) = fs::write(&src, patched + "\n") {
        println!(
            "cargo:warning=firmware build.rs: could not write {} ({error}); jpeg quality unchanged",
            src.display()
        );
        return;
    }
    match run_component_ninja(
        link_args,
        &build_dir,
        "esp-idf/espressif__esp_capture/libespressif__esp_capture.a",
    ) {
        Ok(status) if status.success() => {
            println!("cargo:warning=firmware build.rs: patched esp_capture jpeg quality to {quality}")
        }
        Ok(status) => println!(
            "cargo:warning=firmware build.rs: esp_capture rebuild failed ({status}); jpeg quality may be stale"
        ),
        Err(error) => println!(
            "cargo:warning=firmware build.rs: could not run ninja ({error}); jpeg quality may be stale"
        ),
    }
}

/// Rebuild the esp32-camera component archive with the component's own ninja
/// rule so the Rust link in the same build picks up the patched sources.
fn run_component_ninja(
    link_args: &[String],
    build_dir: &Path,
    target: &str,
) -> std::io::Result<std::process::ExitStatus> {
    let mut path = env::var_os("PATH").unwrap_or_default();
    if let Some(linker_dir) = link_args
        .iter()
        .position(|arg| arg == "--ldproxy-linker")
        .and_then(|index| link_args.get(index + 1))
        .and_then(|linker| Path::new(linker).parent())
    {
        let mut prefixed = linker_dir.as_os_str().to_owned();
        prefixed.push(":");
        prefixed.push(env::var_os("PATH").unwrap_or_default());
        path = prefixed;
    }
    Command::new("ninja")
        .arg("-C")
        .arg(build_dir)
        .arg(target)
        .env("PATH", path)
        .status()
}

fn rewrite_camera_eoi_scan(contents: &str) -> Option<String> {
    let original = "\
                /* Search forward from (JPEG_EOI_MARKER_LEN - 1) bytes before the final
                 * DMA block. We prefer forward search to pick the earliest EOI in the
                 * last DMA node, avoiding stale markers from a larger prior frame. */
                size_t probe_len = eoi_probe_window(cam_obj->dma_node_buffer_size,
                                                   dma_buffer->len);
                if (probe_len < JPEG_EOI_MARKER_LEN) {
                    goto skip_eoi_check;
                }
                uint8_t *probe_start = dma_buffer->buf + dma_buffer->len - probe_len;
                cam_drop_psram_cache(probe_start, probe_len);
                int off = cam_verify_jpeg_eoi(probe_start, probe_len, true);
                if (off >= 0) {
                    offset_e = dma_buffer->len - probe_len + off;
                }";
    let patched = "\
                /* `len` is derived from the DMA EOF interrupt count at VSYNC and
                 * can lag or overrun the bytes actually written, so the real EOI
                 * may sit outside the last DMA node. Scan the whole framebuffer
                 * forward from the validated SOI instead. */
                size_t probe_len = cam_obj->fb_size;
                if (probe_len < JPEG_EOI_MARKER_LEN) {
                    goto skip_eoi_check;
                }
                cam_drop_psram_cache(dma_buffer->buf, probe_len);
                int off = cam_verify_jpeg_eoi(dma_buffer->buf, probe_len, true);
                if (off >= 0) {
                    offset_e = off;
                }";
    let patched = contents.replace(original, patched);
    patched
        .contains("size_t probe_len = cam_obj->fb_size;")
        .then_some(patched)
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
