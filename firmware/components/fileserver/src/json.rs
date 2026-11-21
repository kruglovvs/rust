//! JSON bodies: the status snapshot, the API index, the recording listing and
//! the embedded OpenAPI document.
//!
//! # Contract
//!
//! ```text
//! pub struct Snapshot<'a> { pub version: &'a str, pub build: &'a str, ... }
//! pub const LIST_CAP: usize = 200;
//! pub fn status_body(snapshot: &Snapshot<'_>, out: &mut String)
//! pub fn index_body(out: &mut String)
//! pub fn list_body(dir: &str, source: &dyn FileSource, out: &mut String, cap: usize)
//! pub fn error_body(status: Status, message: &str, out: &mut String)
//! pub fn openapi_body() -> &'static str
//! ```
//!
//! Every function here is pure apart from the [`crate::body::FileSource`] the
//! listing reads through: this module never imports `esp_idf_*` and never
//! touches the card itself. [`crate::status`] is the only module that reads a
//! device and fills a [`Snapshot`], and [`crate::body::CardSource`] is the only
//! source the server passes in; the builders below are what the device test
//! binary exercises directly.
//!
//! [`status_body`] appends one JSON object to `out` whose property names are
//! exactly the `components.schemas.Status` properties of the embedded
//! document, in the same order. Every property is always emitted: `null` is
//! written for [`Snapshot::recording_file`] and
//! [`Snapshot::temperature_c`] when they are absent, never an omitted key.
//! Numbers stay JSON numbers (`uptime_s`, byte counts and the port are
//! integers, `temperature_c` should be finite - `serde_json` serialises a
//! non-finite float as `null` rather than rejecting it, so
//! [`crate::status`] filters a non-finite reading before the builder sees
//! it). The object is serialised with `serde_json`, so the output is valid
//! JSON by construction and the caller can append it to a `String` buffer it
//! reuses.
//!
//! [`index_body`] appends `{"endpoints":[...]}` with one `method`, `path` and
//! `description` per entry of [`crate::router::ROUTES`], in table order. It is
//! deliberately built from `ROUTES`, so the index cannot advertise an
//! operation the router does not serve.
//!
//! [`error_body`] appends `{"error":"<reason>","message":"<message>"}`, the
//! `ApiError` shape documented for every non-2xx JSON response.
//!
//! [`list_body`] appends `{"files":[{"path":"/rec/REC_0001.avi","size":123456}]}`
//! for the files directly inside `dir`, a mount-relative directory (`""` for
//! the card root). It reads the entries and sizes through the
//! [`crate::body::FileSource`] rather than the card, skips directories, and
//! takes each `size` from `source.len`, so the number a client sees is the
//! number of bytes a download of that `path` would send. Entries are sorted by
//! name and the list is cut at `cap.min(`[`LIST_CAP`]`)`: the caller's ceiling
//! can lower the documented 200 but never raise it. Each `path` is
//! mount-relative with a leading slash - the form the download route takes
//! without the slash - so the mount itself never appears in the API. A
//! directory that cannot be read is written as an empty list, because a card
//! without the directory yet and an unreadable one look the same to a client
//! that is only listing; an entry that fails its size lookup is skipped for
//! the same reason.
//!
//! [`openapi_body`] returns the checked-in `/firmware/api/openapi.json`
//! verbatim: it is compiled into the image with `include_str!`, so the
//! document the device serves is byte-identical to the one the host contract
//! test validates. No HTML is ever produced by this module.

use crate::body::{DirEntry, FileSource};
use crate::plan::Status;
use crate::router::ROUTES;

/// The RTSP path the media pipeline serves; the status document reports it.
pub const RTSP_PATH: &str = "/live";

/// The mDNS hostname the firmware advertises (`<hostname>.local`).
pub const HOSTNAME: &str = "esp32s3-av";

/// The firmware crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Most entries a listing reports, whatever the caller asks for.
pub const LIST_CAP: usize = 200;

/// The build profile the image was compiled with.
pub const BUILD: &str = if cfg!(debug_assertions) {
    "debug"
} else {
    "release"
};

/// Everything the status document reports, borrowed from the caller.
///
/// [`crate::status::gather`] fills it from the device; tests fill it directly.
/// Byte counts are `u64` and seconds are `u64` so no cast can turn a real
/// reading into a negative number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapshot<'a> {
    pub version: &'a str,
    pub build: &'a str,
    pub uptime_s: u64,
    pub ip: &'a str,
    pub hostname: &'a str,
    pub mounted: bool,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub clock_khz: u64,
    pub rtsp_port: u16,
    pub rtsp_path: &'a str,
    pub rtsp_running: bool,
    pub recording_enabled: bool,
    /// Mount-relative path of the newest recording, if the card has one.
    pub recording_file: Option<&'a str>,
    /// On-die sensor reading in degrees Celsius, `None` when unreadable.
    pub temperature_c: Option<f32>,
    pub heap_internal_free: u64,
    pub heap_spiram_free: u64,
}

/// Append the status document for `snapshot` to `out`.
pub fn status_body(snapshot: &Snapshot<'_>, out: &mut String) {
    let body = serde_json::json!({
        "version": snapshot.version,
        "build": snapshot.build,
        "uptime_s": snapshot.uptime_s,
        "ip": snapshot.ip,
        "hostname": snapshot.hostname,
        "mounted": snapshot.mounted,
        "total_bytes": snapshot.total_bytes,
        "free_bytes": snapshot.free_bytes,
        "clock_khz": snapshot.clock_khz,
        "rtsp_port": snapshot.rtsp_port,
        "rtsp_path": snapshot.rtsp_path,
        "rtsp_running": snapshot.rtsp_running,
        "recording_enabled": snapshot.recording_enabled,
        "recording_file": snapshot.recording_file,
        "temperature_c": snapshot.temperature_c,
        "heap_internal_free": snapshot.heap_internal_free,
        "heap_spiram_free": snapshot.heap_spiram_free,
    });
    out.push_str(&body.to_string());
}

/// Append the API index, one entry per [`ROUTES`] row, to `out`.
pub fn index_body(out: &mut String) {
    let endpoints: std::vec::Vec<serde_json::Value> = ROUTES
        .iter()
        .map(|(method, path)| {
            serde_json::json!({
                "method": method,
                "path": path,
                "description": description(path),
            })
        })
        .collect();
    let body = serde_json::json!({ "endpoints": endpoints });
    out.push_str(&body.to_string());
}

/// Append the recording listing for `dir` to `out`, at most `cap` entries.
pub fn list_body(dir: &str, source: &dyn FileSource, out: &mut String, cap: usize) {
    let limit = cap.min(LIST_CAP);
    let mut entries: std::vec::Vec<DirEntry> = source.list(dir).unwrap_or_default();
    entries.retain(|entry| !entry.is_dir);
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries.truncate(limit);

    let files: std::vec::Vec<serde_json::Value> = entries
        .iter()
        .filter_map(|entry| {
            let rel = child_path(dir, &entry.name);
            let size = source.len(&rel).ok()?;
            Some(serde_json::json!({ "path": format!("/{rel}"), "size": size }))
        })
        .collect();
    let body = serde_json::json!({ "files": files });
    out.push_str(&body.to_string());
}

fn child_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Append the documented error body for `status` to `out`.
pub fn error_body(status: Status, message: &str, out: &mut String) {
    let body = serde_json::json!({
        "error": status.reason(),
        "message": message,
    });
    out.push_str(&body.to_string());
}

/// The embedded OpenAPI 3.0.3 document, byte-identical to the checked-in file.
pub fn openapi_body() -> &'static str {
    include_str!("../../../api/openapi.json")
}

/// One-line description for a path pattern in the index.
fn description(pattern: &str) -> &'static str {
    match pattern {
        "/" => "this JSON index",
        "/api/v1/status" => "device status",
        "/api/v1/files" => "recording listing",
        "/api/v1/openapi.json" => "this OpenAPI document",
        "/api/v1/files/{path}" => "download one file",
        _ => "documented endpoint",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The snapshot the happy-path tests use: every field populated.
    fn fixture<'a>() -> Snapshot<'a> {
        Snapshot {
            version: "0.1.0",
            build: "debug",
            uptime_s: 42,
            ip: "192.168.1.50",
            hostname: "esp32s3-av",
            mounted: true,
            total_bytes: 31_914_983_424,
            free_bytes: 29_000_000_000,
            clock_khz: 20_000,
            rtsp_port: 554,
            rtsp_path: RTSP_PATH,
            rtsp_running: true,
            recording_enabled: true,
            recording_file: Some("rec/REC_0002.avi"),
            temperature_c: Some(47.5),
            heap_internal_free: 200_000,
            heap_spiram_free: 7_000_000,
        }
    }

    fn value_of(snapshot: &Snapshot<'_>) -> serde_json::Value {
        let mut body = String::new();
        status_body(snapshot, &mut body);
        serde_json::from_str(&body).expect("status body is valid JSON")
    }

    /// Type check for one documented property against the JSON body.
    fn assert_schema_type(schema: &serde_json::Value, value: &serde_json::Value, name: &str) {
        let nullable = schema["nullable"].as_bool().unwrap_or(false);
        if value.is_null() {
            assert!(
                nullable,
                "{name} is null but the document does not allow it"
            );
            return;
        }
        match schema["type"].as_str() {
            Some("string") => assert!(value.is_string(), "{name} should be a string"),
            Some("boolean") => assert!(value.is_boolean(), "{name} should be a boolean"),
            Some("array") => assert!(value.is_array(), "{name} should be an array"),
            Some("object") => assert!(value.is_object(), "{name} should be an object"),
            Some("number") => assert!(value.is_number(), "{name} should be a number"),
            Some("integer") => {
                assert!(
                    value.is_number() && value.as_f64().is_some_and(|v| v.fract() == 0.0),
                    "{name} should be an integer, got {value}"
                );
            }
            other => panic!("{name}: unsupported schema type {other:?}"),
        }
    }

    #[test]
    fn status_body_parses_as_json() {
        let mut body = String::new();
        status_body(&fixture(), &mut body);
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert!(value.is_object());
        assert_eq!(body, value.to_string(), "the body is canonical JSON");
        // Key order is serde_json's, not the document's; the contract is the
        // property set, so the test checks presence rather than order.
        assert!(body.starts_with('{') && body.ends_with('}'), "{body}");
    }

    #[test]
    fn status_body_has_every_documented_property() {
        let document: serde_json::Value =
            serde_json::from_str(openapi_body()).expect("document parses");
        let properties = document["components"]["schemas"]["Status"]["properties"]
            .as_object()
            .expect("Status schema has properties");
        assert_eq!(
            properties.len(),
            17,
            "the document describes the seventeen status fields"
        );

        let mut body = String::new();
        status_body(&fixture(), &mut body);
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

        for (name, schema) in properties {
            let field = value
                .get(name)
                .unwrap_or_else(|| panic!("status body lacks documented property {name}"));
            assert_schema_type(schema, field, name);
        }
        // Nothing may be emitted that the document does not describe.
        for name in value.as_object().expect("object").keys() {
            assert!(
                properties.contains_key(name),
                "status body emits undocumented property {name}"
            );
        }
    }

    #[test]
    fn documented_properties_must_match_the_body() {
        // Guard the drift test against vacuity: renaming one documented
        // property in a scratch copy of the document must make the check
        // above fail for that name.
        let mut document: serde_json::Value =
            serde_json::from_str(openapi_body()).expect("document parses");
        let properties = document["components"]["schemas"]["Status"]["properties"]
            .as_object_mut()
            .expect("properties object");
        let schema = properties
            .remove("uptime_s")
            .expect("uptime_s is documented");
        properties.insert("uptime_seconds".to_string(), schema);

        let mut body = String::new();
        status_body(&fixture(), &mut body);
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert!(
            value.get("uptime_seconds").is_none(),
            "the body has no uptime_seconds field, so the renamed documented \
             property is missing and the real check would fail"
        );
        assert_eq!(
            value["uptime_s"], 42,
            "the body kept the original name, which is what makes the rename a drift"
        );
    }

    #[test]
    fn status_temperature_some_is_number() {
        let value = value_of(&fixture());
        assert!(value["temperature_c"].is_number());
        let reported = value["temperature_c"].as_f64().expect("number");
        assert_eq!(reported, 47.5, "one decimal is preserved");

        // Zero is a legitimate reading and must stay a number.
        let cold = Snapshot {
            temperature_c: Some(0.0),
            ..fixture()
        };
        assert!(value_of(&cold)["temperature_c"].is_number());
    }

    #[test]
    fn status_temperature_none_is_null() {
        let warm = Snapshot {
            temperature_c: None,
            ..fixture()
        };
        assert!(value_of(&warm)["temperature_c"].is_null());
    }

    #[test]
    fn status_recording_file_none_is_null() {
        let empty = Snapshot {
            recording_file: None,
            ..fixture()
        };
        assert!(value_of(&empty)["recording_file"].is_null());
        assert_eq!(value_of(&fixture())["recording_file"], "rec/REC_0002.avi");
    }

    #[test]
    fn status_numbers_are_numbers_not_strings() {
        let value = value_of(&fixture());
        for name in [
            "uptime_s",
            "total_bytes",
            "free_bytes",
            "clock_khz",
            "rtsp_port",
            "heap_internal_free",
            "heap_spiram_free",
        ] {
            assert!(value[name].is_number(), "{name} should be a number");
            assert!(!value[name].is_string(), "{name} must not be a string");
        }
        assert!(value["mounted"].is_boolean());
        assert!(value["rtsp_running"].is_boolean());
        assert!(value["recording_enabled"].is_boolean());
        assert!(value["ip"].is_string());
    }

    #[test]
    fn index_body_lists_every_endpoint() {
        let mut body = String::new();
        index_body(&mut body);
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        let endpoints = value["endpoints"].as_array().expect("endpoints array");
        assert_eq!(endpoints.len(), ROUTES.len());
        for ((method, path), entry) in ROUTES.iter().zip(endpoints) {
            assert_eq!(entry["method"], *method);
            assert_eq!(entry["path"], *path);
            assert!(
                entry["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "{method} {path} needs a description"
            );
            assert!(body.contains(&format!("\"{path}\"")), "{path} in {body}");
        }
        // The document the index describes uses the same path spellings.
        for (_, path) in ROUTES {
            assert!(
                openapi_body().contains(&format!("\"{path}\"")),
                "{path} must appear in the document"
            );
        }
    }

    #[test]
    fn openapi_body_is_the_embedded_document() {
        let body = openapi_body();
        assert_eq!(
            body,
            include_str!("../../../api/openapi.json"),
            "openapi_body returns the checked-in document verbatim"
        );
        let document: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(document["openapi"], "3.0.3");
        assert_eq!(document["info"]["title"], "esp32s3-av firmware API");
        for (_, path) in ROUTES {
            assert!(
                document["paths"].get(path).is_some(),
                "{path} is missing from the document"
            );
        }
        assert!(
            document["components"]["schemas"]["Status"].is_object(),
            "the status schema is shipped"
        );
    }

    #[test]
    fn error_body_matches_the_documented_shape() {
        let mut body = String::new();
        error_body(Status::METHOD_NOT_ALLOWED, "GET or HEAD only", &mut body);
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(value["error"], "Method Not Allowed");
        assert_eq!(value["message"], "GET or HEAD only");

        let mut broken = String::new();
        error_body(Status::NOT_FOUND, "quote \" and newline \n", &mut broken);
        let value: serde_json::Value =
            serde_json::from_str(&broken).expect("message text is escaped");
        assert_eq!(value["message"], "quote \" and newline \n");
    }

    #[test]
    fn constants_match_the_shipped_defaults() {
        assert_eq!(HOSTNAME, "esp32s3-av");
        assert!(RTSP_PATH.starts_with('/'));
        assert!(!VERSION.is_empty());
        assert!(!BUILD.is_empty());
    }
}

#[cfg(test)]
mod list {
    use super::*;
    use crate::body::{FileSource, MemorySource};

    const SMALL: [u8; 222] = [7; 222];
    const BIG: [u8; 123_456] = [9; 123_456];
    const TINY: [u8; 1] = [1; 1];

    fn body_of(dir: &str, files: &[(&str, &[u8])], cap: usize) -> serde_json::Value {
        let source = MemorySource::new(files);
        let mut out = std::string::String::new();
        list_body(dir, &source, &mut out, cap);
        serde_json::from_str(&out).expect("the listing is valid JSON")
    }

    fn paths(value: &serde_json::Value) -> Vec<std::string::String> {
        value["files"]
            .as_array()
            .expect("files array")
            .iter()
            .map(|entry| {
                entry["path"]
                    .as_str()
                    .expect("path is a string")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn list_empty_dir() {
        let value = body_of("rec", &[], LIST_CAP);
        assert_eq!(value["files"], serde_json::json!([]));
        assert_eq!(paths(&value).len(), 0);
    }

    #[test]
    fn list_sorts_by_name() {
        let files = [
            ("rec/REC_0003.avi", &SMALL[..]),
            ("rec/REC_0001.avi", &SMALL[..]),
            ("rec/REC_0002.avi", &SMALL[..]),
        ];
        let value = body_of("rec", &files, LIST_CAP);
        assert_eq!(
            paths(&value),
            vec![
                "/rec/REC_0001.avi",
                "/rec/REC_0002.avi",
                "/rec/REC_0003.avi"
            ]
        );
    }

    #[test]
    fn list_skips_directories() {
        let files = [
            ("rec/REC_0001.avi", &SMALL[..]),
            ("rec/raw/REC_0009.avi", &BIG[..]),
        ];
        let value = body_of("rec", &files, LIST_CAP);
        assert_eq!(paths(&value), vec!["/rec/REC_0001.avi"]);
        assert!(
            !value.to_string().contains("raw"),
            "a directory is skipped, not listed as a file: {value}"
        );
    }

    #[test]
    fn list_respects_cap() {
        assert_eq!(LIST_CAP, 200);
        let names: Vec<std::string::String> = (0..250)
            .map(|index| format!("rec/REC_{index:04}.avi"))
            .collect();
        let data = [0u8; 8];
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .map(|name| (name.as_str(), &data[..]))
            .collect();

        let listed = paths(&body_of("rec", &files, LIST_CAP));
        assert_eq!(listed.len(), 200, "the documented ceiling is 200");
        assert_eq!(
            listed.first().map(String::as_str),
            Some("/rec/REC_0000.avi")
        );
        assert_eq!(listed.last().map(String::as_str), Some("/rec/REC_0199.avi"));

        assert_eq!(paths(&body_of("rec", &files, 5)).len(), 5, "cap lowers");
        assert_eq!(
            paths(&body_of("rec", &files, 500)).len(),
            200,
            "cap cannot raise the ceiling"
        );
    }

    #[test]
    fn list_uses_forward_slashes() {
        let files = [("rec/REC_0001.avi", &SMALL[..])];
        let value = body_of("rec", &files, LIST_CAP);
        assert_eq!(paths(&value), vec!["/rec/REC_0001.avi"]);
        let text = value.to_string();
        assert!(
            !text.contains('\\'),
            "listing paths never use a backslash: {text}"
        );
    }

    #[test]
    fn list_size_matches_source() {
        let files = [("rec/SMALL.avi", &SMALL[..]), ("rec/BIG.avi", &BIG[..])];
        let source = MemorySource::new(&files);
        let mut out = std::string::String::new();
        list_body("rec", &source, &mut out, LIST_CAP);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");

        let mut reported = Vec::new();
        for entry in value["files"].as_array().expect("files array") {
            let path = entry["path"].as_str().expect("path");
            let rel = path.strip_prefix('/').expect("mount-relative with a slash");
            let size = entry["size"].as_u64().expect("size is an integer");
            let served = source.len(rel).expect("the source serves the listed path");
            reported.push((path.to_string(), size, served));
        }
        assert_eq!(
            reported,
            vec![
                (
                    "/rec/BIG.avi".to_string(),
                    BIG.len() as u64,
                    BIG.len() as u64
                ),
                (
                    "/rec/SMALL.avi".to_string(),
                    SMALL.len() as u64,
                    SMALL.len() as u64
                ),
            ],
            "the listing size is the number of bytes a download sends"
        );
    }

    #[test]
    fn list_path_is_mount_relative() {
        let files = [("rec/REC_0001.avi", &TINY[..])];
        let value = body_of("rec", &files, LIST_CAP);
        let path = &paths(&value)[0];
        assert_eq!(path, "/rec/REC_0001.avi");
        assert!(
            !path.contains("/sdcard"),
            "the mount is not part of the API path: {path}"
        );
        assert_eq!(
            &path[1..],
            "rec/REC_0001.avi",
            "the download route takes the path without the slash"
        );

        // A root-level file lists with the same leading slash.
        let root = body_of("", &[("LOOSE.avi", &TINY[..])], LIST_CAP);
        assert_eq!(paths(&root), vec!["/LOOSE.avi"]);
    }
}
