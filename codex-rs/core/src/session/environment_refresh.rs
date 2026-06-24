use codex_protocol::protocol::TurnEnvironmentSelection;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

/// Serializes environment-derived refreshes and remembers the last completed revision.
pub(super) struct EnvironmentRefresh {
    pub(super) gate: Semaphore,
    pub(super) last_refreshed_selections: Mutex<Vec<TurnEnvironmentSelection>>,
}

impl EnvironmentRefresh {
    pub(super) fn new(last_refreshed_selections: Vec<TurnEnvironmentSelection>) -> Self {
        Self {
            gate: Semaphore::new(/*permits*/ 1),
            last_refreshed_selections: Mutex::new(last_refreshed_selections),
        }
    }
}
