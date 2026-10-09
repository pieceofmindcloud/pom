//! Wire contract against the real license server.
//!
//! The fixtures in `tests/fixtures/` are response bodies captured from a real
//! run of the `license-server` (pom-website), not written by hand. If the
//! server stops emitting a field these structs require, POM breaks in
//! production with `missing field ...` at activation - that is exactly how
//! plugin activation broke when the server became a plugin catalog and
//! renamed `entitlements` to `licensed_plugins`. This test makes that breakage
//! show up here, and not on the machine of whoever bought it.
//!
//! To re-record the fixtures, run the generator described in
//! `license-server/test/wire-contract.test.ts` in the pom-website repository.

use enterprise_protocol::{
    decode_response, ActivationResponse, LicenseStatusResponse, ValidateResponse,
};

const VALIDATE_DISCOVERY: &str = include_str!("fixtures/validate-discovery.json");
const VALIDATE_PLUGIN: &str = include_str!("fixtures/validate-plugin.json");
const ACTIVATE: &str = include_str!("fixtures/activate.json");
const STATUS: &str = include_str!("fixtures/status.json");

#[test]
fn validate_discovery_lists_the_plugin_codes_the_license_grants() {
    let response =
        decode_response::<ValidateResponse>(VALIDATE_DISCOVERY.as_bytes()).expect("validate body");

    assert_eq!(response.protocol, enterprise_protocol::LICENSE_PROTOCOL);
    assert_eq!(response.payload.status, "active");
    // Discovery is what the client does first: without plugin_code, the server
    // answers only with the list of codes the key grants.
    assert_eq!(response.payload.licensed_plugins, vec!["logs"]);
    assert!(response.payload.activation_required);
    assert_eq!(response.payload.activation_id, None);
}

#[test]
fn validate_with_a_plugin_code_also_previews_the_release() {
    let response =
        decode_response::<ValidateResponse>(VALIDATE_PLUGIN.as_bytes()).expect("validate body");

    assert_eq!(response.payload.licensed_plugins, vec!["logs"]);
    let release = response.payload.eligible_release.expect("eligible release");
    assert_eq!(release.release_id, "logs-0.8.0-linux-x86_64");
    assert_eq!(release.version, "0.8.0");
}

#[test]
fn activation_carries_the_lease_the_grant_and_the_plugin_manifest() {
    let response =
        decode_response::<ActivationResponse>(ACTIVATE.as_bytes()).expect("activation body");
    let payload = response.payload;

    assert_eq!(payload.plugin_code, "logs");
    assert_eq!(payload.licensed_plugins, vec!["logs"]);
    assert_eq!(payload.plugin_release_id, "logs-0.8.0-linux-x86_64");
    assert_eq!(
        payload.plugin_manifest.plugin_abi,
        enterprise_protocol::PLUGIN_ABI_VERSION
    );
    // A plugin's features come from the installed release, not from the license: it
    // is the manifest that says what that version delivers.
    assert_eq!(
        payload.plugin_manifest.feature_set,
        vec!["logs.request_logs", "logs.statistics"]
    );
    // Existing server responses omit the optional preference schema, and
    // serializing them preserves the old signed manifest shape.
    assert_eq!(payload.plugin_manifest.preferences, None);
    assert!(!serde_json::to_value(&payload.plugin_manifest)
        .expect("manifest JSON")
        .as_object()
        .expect("manifest object")
        .contains_key("preferences"));
    assert_eq!(payload.grant.alg, "Ed25519");
    assert!(!payload.grant.body.is_empty());
}

#[test]
fn plugin_manifests_accept_typed_optional_preferences_without_changing_the_contract_id() {
    let mut value: serde_json::Value = serde_json::from_str(ACTIVATE).expect("activation fixture");
    value["payload"]["plugin_manifest"]["preferences"] = serde_json::json!({
        "features": [{
            "key": "include_headers",
            "label": "Include request headers",
            "default": false,
            "description": "Capture approved request metadata"
        }],
        "storage_directory": {
            "label": "Storage directory",
            "default": "/var/lib/pom/plugin-data"
        }
    });
    let extended = serde_json::to_vec(&value).expect("extended activation JSON");
    let response = decode_response::<ActivationResponse>(&extended).expect("extended manifest");
    let schema = response
        .payload
        .plugin_manifest
        .preferences
        .expect("preferences");
    assert_eq!(schema.features[0].key, "include_headers");
    assert!(!schema.features[0].default);
    assert_eq!(
        schema.storage_directory.unwrap().default.as_deref(),
        Some("/var/lib/pom/plugin-data")
    );
    assert_eq!(enterprise_protocol::LICENSE_PROTOCOL, "pom-license/v1");

    let values = enterprise_protocol::PluginPreferences {
        features: [("include_headers".to_owned(), true)].into(),
        storage_directory: Some("/tmp/plugin-data".into()),
    };
    assert_eq!(
        serde_json::from_value::<enterprise_protocol::PluginPreferences>(
            serde_json::to_value(values).expect("preferences JSON")
        )
        .expect("typed local values")
        .storage_directory
        .as_deref(),
        Some("/tmp/plugin-data")
    );
}

#[test]
fn the_signed_grant_body_is_scoped_to_one_plugin_code() {
    let response =
        decode_response::<ActivationResponse>(ACTIVATE.as_bytes()).expect("activation body");
    let raw = base64_url_decode(&response.payload.grant.body);
    let grant: enterprise_protocol::GrantBody =
        serde_json::from_slice(&raw).expect("signed grant body");

    assert_eq!(grant.v, 1);
    assert_eq!(grant.iss, "license.pieceofmind.cloud");
    assert_eq!(grant.product, "pom");
    assert_eq!(grant.plugin_code, response.payload.plugin_code);
    assert_eq!(grant.activation_id, response.payload.activation_id);
    assert_eq!(grant.lease_id, response.payload.lease_id);
    assert_eq!(grant.release_id, response.payload.plugin_release_id);
    assert_eq!(
        grant.fingerprint_scheme,
        enterprise_protocol::FINGERPRINT_SCHEME
    );
}

#[test]
fn status_reports_the_lease_and_what_the_license_still_grants() {
    let response =
        decode_response::<LicenseStatusResponse>(STATUS.as_bytes()).expect("status body");

    assert_eq!(response.payload.state, "active");
    assert_eq!(response.payload.licensed_plugins, vec!["logs"]);
    assert_eq!(response.payload.plugin_release_id, "logs-0.8.0-linux-x86_64");
}

/// base64url without padding, the same the server uses for the grant body.
fn base64_url_decode(value: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(value.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in value.bytes() {
        let index = ALPHABET
            .iter()
            .position(|candidate| *candidate == byte)
            .unwrap_or_else(|| panic!("invalid base64url character: {byte}"));
        buffer = (buffer << 6) | index as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}
