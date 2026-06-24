use super::session::Session;
use super::step_context::StepContext;
use crate::context::world_state::AgentsMdState;
use crate::context::world_state::EnvironmentsState;
use crate::context::world_state::WorldState;
use codex_features::Feature;
use codex_protocol::protocol::TurnContextItem;

impl Session {
    pub(crate) async fn build_world_state_for_step(
        &self,
        step_context: &StepContext,
    ) -> WorldState {
        let turn_context = step_context.turn.as_ref();
        let environment_subagents = if turn_context.config.include_environment_context {
            self.services
                .agent_control
                .format_environment_context_subagents(self.thread_id)
                .await
        } else {
            String::new()
        };

        let mut world_state = WorldState::default();
        if turn_context
            .config
            .features
            .enabled(Feature::DeferredExecutor)
        {
            world_state.add_section(AgentsMdState::new(step_context.loaded_agents_md.as_deref()));
        }
        if turn_context.config.include_environment_context {
            world_state.add_section(
                EnvironmentsState::from_turn_context_with_environments(
                    turn_context,
                    &step_context.environments,
                )
                .with_subagents(environment_subagents),
            );
        }
        world_state
    }
}

pub(super) fn build_world_state_from_turn_context_item(
    turn_context_item: &TurnContextItem,
) -> WorldState {
    let mut world_state = WorldState::default();
    // TODO(sayan): rollouts don't persist AGENTS.md state yet, so conservatively supersede history on resume.
    world_state.add_section(AgentsMdState::unknown());
    world_state.add_section(EnvironmentsState::from_turn_context_item(turn_context_item));
    world_state
}
