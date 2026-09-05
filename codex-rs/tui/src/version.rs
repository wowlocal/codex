/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = if cfg!(test) {
    // Keep UI snapshots stable when testing a release-qualified workspace version.
    "0.0.0"
} else {
    env!("CARGO_PKG_VERSION")
};
