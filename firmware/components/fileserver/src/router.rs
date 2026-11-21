//! Method and path to [`Route`] mapping, plus the single route table.
//!
//! # Contract
//!
//! ```text
//! pub enum Route { Index, Status, Files, OpenApi, Download }
//! pub const ROUTES: &[(&str, &str)]  // (method, path pattern) per operation
//! pub const DOWNLOAD_PREFIX: &str
//! pub fn route(method: Method, path: &str) -> Result<Route, Status>
//! pub fn download_relative(route: Route, path: &str) -> Option<&str>
//! ```
//!
//! [`ROUTES`] is the **single parity source** for the shipped API: one entry
//! per documented method/path pair and nothing else. The path patterns are the
//! OpenAPI `paths` keys verbatim, with `{path}` as the only template. `GET` and
//! `HEAD` are both listed for the download because the server answers both;
//! every other route answers `GET` only.
//!
//! [`route`] is pure: it looks at the method and the request path only, never
//! at the filesystem, the query string or the headers. Its result is:
//!
//! - `Ok(Route)` when the method/path pair matches an entry of [`ROUTES`].
//!   Only `GET` and `HEAD` can match; `HEAD` matches the parameterised
//!   download pattern and nothing else, because a `HEAD` of a JSON route has
//!   no entry in the shipped OpenAPI document. [`crate::plan`] still answers a
//!   `HEAD` of those routes by selecting the `GET` entry, then dropping the
//!   body.
//! - `Err(Status::METHOD_NOT_ALLOWED)` for a method that is not `GET` or
//!   `HEAD` (`POST`, `PUT`, `DELETE`, `OPTIONS`, and [`Method::Other`], which
//!   includes a lowercase `get`). The method is
//!   checked first, so a rejected verb is a `405` for every path and cannot be
//!   used to probe which paths exist.
//! - `Err(Status::NOT_FOUND)` for a `GET`/`HEAD` path that matches no pattern.
//!   Matching is exact: no trailing slash is tolerated
//!   (`/api/v1/status/` is a `404`), no segment is case-folded, and a
//!   parameterised route requires at least one non-empty path segment after
//!   its prefix. The segment is *not* decoded here: [`crate::path`] owns
//!   validation and traversal rejection.
//!
//! The JSON error body for a failure is [`crate::json::error_body`]; this
//! module only names the status.

use crate::plan::{Method, Status};

/// The operation a request selects. One variant per documented operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Route {
    /// `GET /` - the JSON index of the API.
    Index,
    /// `GET /api/v1/status` - the device status document.
    Status,
    /// `GET /api/v1/files` - the recording listing.
    Files,
    /// `GET /api/v1/openapi.json` - the embedded OpenAPI document.
    OpenApi,
    /// `GET`/`HEAD /api/v1/files/{path}` - one file from the card.
    Download,
}

impl Route {
    /// Stable name used by logs and tests.
    pub const fn name(self) -> &'static str {
        match self {
            Route::Index => "Index",
            Route::Status => "Status",
            Route::Files => "Files",
            Route::OpenApi => "OpenApi",
            Route::Download => "Download",
        }
    }
}

/// The route table: `(method, path pattern)` for every documented operation.
///
/// The order is the documented order (`/`, then the three collection
/// endpoints, then the file route with its two methods). Adding a route here
/// without adding it to `api/openapi.json` fails the parity tests in this
/// module and in [`crate::json`].
pub const ROUTES: &[(&str, &str)] = &[
    ("GET", "/"),
    ("GET", "/api/v1/status"),
    ("GET", "/api/v1/files"),
    ("GET", "/api/v1/openapi.json"),
    ("GET", "/api/v1/files/{path}"),
    ("HEAD", "/api/v1/files/{path}"),
];

/// The fixed prefix of the download route, i.e. what [`route`] strips before
/// the remainder reaches [`crate::path::map`].
pub const DOWNLOAD_PREFIX: &str = "/api/v1/files/";

/// Map a parsed method and path to the operation it selects.
///
/// See the module contract for the full rule set. Never panics and never
/// allocates: the path is compared in place.
pub fn route(method: Method, path: &str) -> Result<Route, Status> {
    match method {
        Method::Get | Method::Head => {}
        Method::Options | Method::Other => return Err(Status::METHOD_NOT_ALLOWED),
    }
    ROUTES
        .iter()
        .find(|(declared, pattern)| method_matches(method, declared) && path_matches(pattern, path))
        .and_then(|(declared, pattern)| operation_for(pattern, declared))
        .ok_or(Status::NOT_FOUND)
}

/// The request path relative to [`DOWNLOAD_PREFIX`] for a [`Route::Download`],
/// or `None` for any other route.
///
/// The result is still percent-encoded and unvalidated; [`crate::path::map`]
/// is the only thing that may turn it into a VFS path.
pub fn download_relative(route: Route, path: &str) -> Option<&str> {
    match route {
        Route::Download => path.strip_prefix(DOWNLOAD_PREFIX),
        _ => None,
    }
}

fn method_matches(method: Method, declared: &str) -> bool {
    match method {
        Method::Get => declared == "GET",
        Method::Head => declared == "HEAD",
        Method::Options => declared == "OPTIONS",
        Method::Other => false,
    }
}

/// `pattern` is either a literal (compared exactly) or ends in `/{path}`,
/// which matches any non-empty remainder after its fixed prefix.
fn path_matches(pattern: &str, path: &str) -> bool {
    match pattern.strip_suffix("/{path}") {
        Some(prefix) => {
            path.starts_with(prefix)
                && path.as_bytes().get(prefix.len()) == Some(&b'/')
                && path.len() > prefix.len() + 1
        }
        None => pattern == path,
    }
}

/// The operation a table entry stands for; the table drives the match order,
/// so this stays in lockstep with [`ROUTES`].
fn operation_for(pattern: &str, declared: &str) -> Option<Route> {
    match (declared, pattern) {
        ("GET", "/") => Some(Route::Index),
        ("GET", "/api/v1/status") => Some(Route::Status),
        ("GET", "/api/v1/files") => Some(Route::Files),
        ("GET", "/api/v1/openapi.json") => Some(Route::OpenApi),
        ("GET" | "HEAD", "/api/v1/files/{path}") => Some(Route::Download),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A concrete path for every pattern, so the table is exercised as a
    /// client would exercise it.
    fn sample_path(pattern: &str) -> &'static str {
        match pattern {
            "/api/v1/files/{path}" => "/api/v1/files/rec/REC_0001.avi",
            "/" => "/",
            "/api/v1/status" => "/api/v1/status",
            "/api/v1/files" => "/api/v1/files",
            "/api/v1/openapi.json" => "/api/v1/openapi.json",
            other => panic!("test needs a concrete path for {other}"),
        }
    }

    #[test]
    fn route_index() {
        assert_eq!(route(Method::Get, "/"), Ok(Route::Index));
        assert_eq!(route(Method::Get, "/").unwrap().name(), "Index");
    }

    #[test]
    fn route_status() {
        assert_eq!(route(Method::Get, "/api/v1/status"), Ok(Route::Status));
        assert_eq!(
            route(Method::Get, "/api/v1/status").unwrap().name(),
            "Status"
        );
    }

    #[test]
    fn route_files() {
        assert_eq!(route(Method::Get, "/api/v1/files"), Ok(Route::Files));
        assert_eq!(route(Method::Get, "/api/v1/files").unwrap().name(), "Files");
    }

    #[test]
    fn route_openapi() {
        assert_eq!(
            route(Method::Get, "/api/v1/openapi.json"),
            Ok(Route::OpenApi)
        );
    }

    #[test]
    fn route_download() {
        assert_eq!(
            route(Method::Get, "/api/v1/files/rec/REC_0001.avi"),
            Ok(Route::Download)
        );
        assert_eq!(
            route(Method::Head, "/api/v1/files/rec/REC_0001.avi"),
            Ok(Route::Download)
        );
        assert_eq!(
            route(Method::Get, "/api/v1/files/rec/REC_0001.avi")
                .unwrap()
                .name(),
            "Download"
        );
        // The parameterised route needs at least one segment.
        assert_eq!(route(Method::Get, "/api/v1/files/"), Err(Status::NOT_FOUND));
        assert_eq!(
            route(Method::Head, "/api/v1/files"),
            Err(Status::NOT_FOUND),
            "HEAD of the listing has no documented equivalent"
        );
        assert_eq!(
            download_relative(Route::Download, "/api/v1/files/rec/REC_0001.avi"),
            Some("rec/REC_0001.avi")
        );
        assert_eq!(download_relative(Route::Status, "/api/v1/status"), None);
    }

    #[test]
    fn route_rejects_post_with_405() {
        for path in [
            "/",
            "/api/v1/status",
            "/api/v1/files",
            "/api/v1/openapi.json",
            "/api/v1/files/rec/REC_0001.avi",
            "/does/not/exist",
        ] {
            assert_eq!(
                route(Method::Other, path),
                Err(Status::METHOD_NOT_ALLOWED),
                "POST {path}"
            );
        }
        assert_eq!(
            route(Method::Options, "/api/v1/status"),
            Err(Status::METHOD_NOT_ALLOWED)
        );
    }

    #[test]
    fn route_rejects_unknown_with_404() {
        for path in [
            "/api/v1",
            "/api/v1/",
            "/api/v2/status",
            "/status",
            "/index.html",
            "",
        ] {
            assert_eq!(route(Method::Get, path), Err(Status::NOT_FOUND), "{path}");
        }
    }

    #[test]
    fn route_trailing_slash_is_404() {
        for path in ["/api/v1/status/", "/api/v1/files/", "/api/v1/openapi.json/"] {
            assert_eq!(route(Method::Get, path), Err(Status::NOT_FOUND), "{path}");
        }
        // The same path without the trailing slash still routes, so the 404 is
        // caused by the separator and not by an unrelated table change.
        assert_eq!(route(Method::Get, "/api/v1/status"), Ok(Route::Status));
    }

    #[test]
    fn routes_have_no_duplicates() {
        for (index, first) in ROUTES.iter().enumerate() {
            for second in &ROUTES[index + 1..] {
                assert_ne!(first, second, "duplicate table entry {first:?}");
            }
        }
        assert_eq!(ROUTES.len(), 6, "six documented operations");
    }

    #[test]
    fn routes_match_openapi_paths() {
        let document: serde_json::Value =
            serde_json::from_str(crate::json::openapi_body()).expect("embedded document parses");
        let paths = document["paths"].as_object().expect("paths is an object");
        let mut documented = std::vec::Vec::new();
        for (path, operations) in paths {
            let operations = operations.as_object().expect("path item is an object");
            for (method, operation) in operations {
                if operation.is_object() && matches!(method.as_str(), "get" | "head") {
                    documented.push((method.to_ascii_uppercase(), path.clone()));
                }
            }
        }
        assert_eq!(documented.len(), ROUTES.len(), "operations: {documented:?}");
        for (method, path) in &documented {
            assert!(
                ROUTES.contains(&(method.as_str(), path.as_str())),
                "{method} {path} is documented but absent from ROUTES"
            );
        }
        for (method, path) in ROUTES {
            assert!(
                documented.iter().any(|(m, p)| m == method && p == path),
                "{method} {path} is in ROUTES but absent from the document"
            );
        }
    }

    #[test]
    fn every_table_entry_routes_to_its_operation() {
        for (method, pattern) in ROUTES {
            let method = match *method {
                "GET" => Method::Get,
                "HEAD" => Method::Head,
                other => panic!("unexpected table method {other}"),
            };
            let path = sample_path(pattern);
            let selected = route(method, path)
                .unwrap_or_else(|status| panic!("{method} {path}: {}", status.reason()));
            assert_eq!(operation_for(pattern, method.as_str()), Some(selected));
        }
    }

    #[test]
    fn download_route_takes_precedence_over_the_listing() {
        // `/api/v1/files` is the listing; a trailing segment is a download.
        assert_eq!(route(Method::Get, "/api/v1/files"), Ok(Route::Files));
        assert_eq!(route(Method::Get, "/api/v1/files/x"), Ok(Route::Download));
        assert_eq!(
            download_relative(Route::Download, "/api/v1/files/x"),
            Some("x")
        );
    }
}
