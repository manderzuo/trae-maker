//! Compatibility identity for the existing text-generation/catalog protocol.
//! The installed desktop client's product version code belongs to native login
//! and video paths; it is not the model-catalog revision accepted by this lane.
//! Verified with GLM-5.3-Flash on 2026-10-12: desktop version 3.4.2 works with
//! 20260811, while its product.json value 20250325 produces upstream error 4001.

pub(super) const VERSION_CODE: &str = "20260811";

pub(super) fn post(agent: &ureq::Agent, url: &str) -> ureq::Request {
    // Start a fresh request: ureq appends X-* headers instead of replacing them.
    // Owning construction avoids sending both native and protocol version codes.
    agent.post(url)
        .set("x-app-version-code", VERSION_CODE)
        .set("x-ide-version-code", VERSION_CODE)
}

#[cfg(test)]
mod tests {
    #[test]
    fn text_headers_set_one_protocol_code_and_preserve_native_identity() {
        let agent = ureq::Agent::new();
        for version in ["0.1.50", "3.4.2"] {
            let request = super::post(&agent, "http://127.0.0.1:9/text-fixture")
                .set("x-ide-version", version)
                .set("x-ide-token", "fixture-token")
                .set("x-device-id", "fixture-device");
            assert_eq!(request.all("x-app-version-code"), vec!["20260811"]);
            assert_eq!(request.all("x-ide-version-code"), vec!["20260811"]);
            assert_eq!(request.header("x-ide-version"), Some(version));
            assert_eq!(request.header("x-ide-token"), Some("fixture-token"));
            assert_eq!(request.header("x-device-id"), Some("fixture-device"));
        }
    }
}
