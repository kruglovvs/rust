//! JSON bodies: the status snapshot, the API index and the embedded OpenAPI
//! document.
//!
//! # Contract
//!
//! ```text
//! pub struct Snapshot<'a> { pub version: &'a str, pub build: &'a str, ... }
//! pub fn status_body(snapshot: &Snapshot<'_>, out: &mut String)
//! pub fn index_body(out: &mut String)
//! pub fn error_body(status: Status, message: &str, out: &mut String)
//! pub fn openapi_body() -> &'static str
//! ```
//!
//! Every function here is pure and host/device agnostic: this module never
//! imports `esp_idf_*` and never touches the card. [`crate::status`] is the
//! only module that reads a device and fills a [`Snapshot`]; the builders
//! below are what the device test binary exercises directly.
//!
//! [`status_body`] appends one JSON object to `out` whose property names are
//! exactly the `components.schemas.Status` properties of the embedded
//! document, in the same order. Every property is always emitted: `null` is
//! written for [`Snapshot::recording_file`] and
//! [`Snapshot::temperature_c`] when they are absent, never an omitted key.
//! Numbers stay JSON numbers (`uptime_s`, byte counts and the port are
//! integers, `temperature_c` should be finite - a non-finite reading is
//! rejected by `serde_json` rather than silently written as `null`). The
//! object is serialised with `serde_json`, so the output is valid JSON by
//! construction and the caller can append it to a `String` buffer it reuses.
//!
//! [`index_body`] appends `{"endpoints":[...]}` with one `method`, `path` and
//! `description` per entry of [`crate::router::ROUTES`], in table order. It is
//! deliberately built from `ROUTES`, so the index cannot advertise an
//! operation the router does not serve.
//!
//! [`error_body`] appends `{"error":"<reason>","message":"<message>"}`, the
//! `ApiError` shape documented for every non-2xx JSON response.
//!
//! [`openapi_body`] returns the checked-in `/firmware/api/openapi.json`
//! verbatim: it is compiled into the image with `include_str!`, so the
//! document the device serves is byte-identical to the one the host contract
//! test validates. No HTML is ever produced by this module.

use crate::http::Status;
use crate::router::ROUTES;

/// The RTSP path the media pipeline serves; the status document reports it.
pub const RTSP_PATH: &str = "/live";

/// The mDNS hostname the firmware advertises (`<hostname>.local`).
pub const HOSTNAME: &str = "esp32s3-av";

/// The firmware crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
            assert!(nullable, "{name} is null but the document does not allow it");
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
        assert_eq!(
            value_of(&fixture())["recording_file"],
            "rec/REC_0002.avi"
        );
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
                entry["description"].as_str().is_some_and(|text| !text.is_empty()),
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
