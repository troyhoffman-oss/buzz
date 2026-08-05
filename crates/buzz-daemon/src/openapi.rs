//! OpenAPI 3.1 emission and TypeScript client generation.
//!
//! Implements Wave-1 daemon deliverable 14 (`DESIGN.md` §4.1.1) and the
//! `daemon-spec-check` gate of §6.2.
//!
//! # Why the generated client is committed
//!
//! §6.2: `daemon-spec-check` regenerates the OpenAPI document *and* the TS
//! client and fails if either differs from what is committed. That is what
//! makes "adding an endpoint without adding it to the spec is a build failure"
//! real, and it catches TS-client drift in the same step.
//!
//! # Additive-only is a contract, not politeness
//!
//! §2.3 [D-1]: the version rule is a **floor** —
//! `daemon.api_version >= client.min_api_version` — and what makes a floor safe
//! is that API changes are additive-only, enforced here. A removed or narrowed
//! endpoint fails the build. A genuine breaking change is an
//! [`crate::API_VERSION`] bump, which is precisely what the floor is for.
//!
//! # §6.4 boundary properties the spec must preserve
//!
//! The generated client is the front end's *entire* protocol surface, so two
//! §6.4 gates are really assertions about this document:
//! - **no `{nsec}` field** on `POST /session/identity` — §2.5 deleted that form,
//!   and a regenerated client that grows one is a spec regression;
//! - **no bare event-kind integers** in any response schema the TUI reads —
//!   kinds are daemon vocabulary, and the TUI receives `type: "message.new"`,
//!   never `kind: 40002`.

/// Emit the OpenAPI 3.1 document for the Wave-1 surface.
///
/// The `paths` object enumerates [`crate::api::MOUNTED_ENDPOINTS`] — what the
/// router actually serves, not what the spec aspires to. An empty object, which
/// is what shipped before the routes existed, tells a generated client that the
/// daemon has no API at all; listing the mounted set means the document is
/// *incomplete* rather than *wrong*, and a client can at least discover the
/// surface.
///
/// TODO(wave1, §4.1.1 deliverable 14): derive the operation objects — request
/// bodies, response schemas, parameters — from the handler types (utoipa or
/// aide, per `tui-research.md`'s recommended shape) so the document cannot
/// drift from the routes, and wire `just daemon-spec-check` to regenerate and
/// diff. An **endpoint-not-in-spec build failure** is the required outcome, not
/// a warning. Until then the paths are present and their operations are not,
/// which the `x-incomplete` marker below states rather than implies.
pub fn document() -> serde_json::Value {
    let paths: serde_json::Map<String, serde_json::Value> = crate::api::MOUNTED_ENDPOINTS
        .iter()
        .map(|path| {
            (
                (*path).to_string(),
                serde_json::json!({"x-incomplete": "operations are not yet derived from the handlers"}),
            )
        })
        .collect();
    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "buzz-daemon",
            "version": crate::VERSION,
            "x-api-version": crate::API_VERSION,
        },
        "paths": paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_declares_openapi_31_and_the_api_version() {
        let doc = document();
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["info"]["x-api-version"], crate::API_VERSION);
    }

    /// §2.5/§6.4: the `{nsec}` form does not exist, so it must never appear in
    /// the emitted document.
    #[test]
    fn document_never_mentions_a_raw_nsec_field() {
        let text = serde_json::to_string(&document()).unwrap();
        assert!(!text.contains("\"nsec\""), "{text}");
    }
}
