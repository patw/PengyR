//! Shared About info — edition name, links, and blurb for all three frontends.
//!
//! Keep the description/links here in sync with the Python and C++ editions'
//! own About screens so all editions present the same facts.

use chrono::Datelike;
use serde::Serialize;

pub const GITHUB_URL: &str = "https://github.com/patw/PengyR";
pub const WEBSITE_URL: &str = "https://pengy.catbee.ca";
pub const LICENSE_NAME: &str = "MIT License";

pub const CATBEE_URL: &str = "https://catbee.ca";
pub const CATBEE_BLURB: &str = "Pengy is part of Catbee — a collection of open-source, self-hosted AI tools for hyper-personal computing, designed to be self-hosted, fully controllable, and yours to own.";

pub const DESCRIPTION: &str = "Pengy is a local-first AI agent that connects to any OpenAI-compatible API (OpenAI, Ollama, vLLM, Groq, OpenRouter, or a local endpoint) and gives the model tools to operate on your filesystem, run code, search the web, and more — all with your approval.";

/// The year Pengy was first published — kept in sync with LICENSE's copyright year.
const FOUNDING_YEAR: i32 = 2026;

pub fn license_url() -> String {
    format!("{GITHUB_URL}/blob/main/LICENSE")
}

/// e.g. edition_line("Rust") -> "Pengy Rust - 1.8.1"
pub fn edition_line(edition: &str) -> String {
    format!("Pengy {edition} - {}", env!("CARGO_PKG_VERSION"))
}

/// e.g. "Copyright © 2026 Pat Wendorf (dungeons@gmail.com)", ranged once the year rolls over.
pub fn copyright_line() -> String {
    let year = chrono::Local::now().year();
    let year_str = if year <= FOUNDING_YEAR {
        FOUNDING_YEAR.to_string()
    } else {
        format!("{FOUNDING_YEAR}–{year}")
    };
    format!("Copyright © {year_str} Pat Wendorf (dungeons@gmail.com)")
}

#[derive(Serialize)]
pub struct AboutInfo {
    pub edition_line: String,
    pub github_url: String,
    pub website_url: String,
    pub description: String,
    pub catbee_blurb: String,
    pub catbee_url: String,
    pub copyright: String,
    pub license_name: String,
    pub license_url: String,
}

/// Build the full About payload for a given edition name (e.g. "Rust").
pub fn about_info(edition: &str) -> AboutInfo {
    AboutInfo {
        edition_line: edition_line(edition),
        github_url: GITHUB_URL.to_string(),
        website_url: WEBSITE_URL.to_string(),
        description: DESCRIPTION.to_string(),
        catbee_blurb: CATBEE_BLURB.to_string(),
        catbee_url: CATBEE_URL.to_string(),
        copyright: copyright_line(),
        license_name: LICENSE_NAME.to_string(),
        license_url: license_url(),
    }
}
