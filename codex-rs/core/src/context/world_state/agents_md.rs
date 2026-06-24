use super::WorldStateSection;
use crate::agents_md::LoadedAgentsMd;
use crate::context::ContextualUserFragment;
use crate::context::UserInstructions;

const REPLACEMENT_NOTICE: &str =
    "These AGENTS.md instructions replace all previously provided AGENTS.md instructions.";
const REMOVAL_NOTICE: &str = "The previously provided AGENTS.md instructions no longer apply.";

/// The AGENTS.md instructions currently visible to the model.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AgentsMdState {
    instructions: Option<UserInstructions>,
    // TODO(sayan): remove this fallback once rollouts persist and restore AGENTS.md state.
    historical_state_unknown: bool,
}

impl AgentsMdState {
    pub(crate) fn new(loaded: Option<&LoadedAgentsMd>) -> Self {
        Self {
            instructions: loaded.map(LoadedAgentsMd::contextual_user_fragment),
            historical_state_unknown: false,
        }
    }

    pub(crate) fn unknown() -> Self {
        Self {
            instructions: None,
            historical_state_unknown: true,
        }
    }
}

impl WorldStateSection for AgentsMdState {
    fn render_diff(&self, previous: Option<&Self>) -> Option<Box<dyn ContextualUserFragment>> {
        if self.historical_state_unknown {
            return None;
        }

        let previous_unknown = previous.is_some_and(|state| state.historical_state_unknown);
        let previous = previous.and_then(|state| state.instructions.as_ref());
        if !previous_unknown && self.instructions.as_ref() == previous {
            return None;
        }

        let instructions = match (&self.instructions, previous) {
            (Some(instructions), Some(_)) => UserInstructions {
                directory: instructions.directory.clone(),
                text: format!("{REPLACEMENT_NOTICE}\n\n{}", instructions.text),
            },
            (Some(instructions), None) if previous_unknown => UserInstructions {
                directory: instructions.directory.clone(),
                text: format!("{REPLACEMENT_NOTICE}\n\n{}", instructions.text),
            },
            (Some(instructions), None) => instructions.clone(),
            (None, Some(_)) => UserInstructions {
                directory: None,
                text: REMOVAL_NOTICE.to_string(),
            },
            (None, None) if previous_unknown => UserInstructions {
                directory: None,
                text: REMOVAL_NOTICE.to_string(),
            },
            (None, None) => return None,
        };
        Some(Box::new(instructions))
    }
}

#[cfg(test)]
#[path = "agents_md_tests.rs"]
mod tests;
