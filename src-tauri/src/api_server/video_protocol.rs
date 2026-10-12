//! Seedance wire-adapter identity, independent of desktop login identity.
//! Existing continuation evidence certifies this unchanged adapter (0.1.50),
//! not the installed TRAE CN application's display/product version.
pub(super) const CLIENT_VERSION: &str = "0.1.50";
pub(super) const VERSION_CODE: &str = "20260811";

pub(super) fn post(agent: &ureq::Agent, url: &str) -> ureq::Request {
    agent.post(url)
        .set("x-app-version-code", VERSION_CODE)
        .set("x-ide-version", CLIENT_VERSION)
        .set("x-ide-version-code", VERSION_CODE)
        .set("app-version", CLIENT_VERSION)
}
