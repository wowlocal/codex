use super::*;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ServerNotification;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn native_effort_waits_for_settings_event_and_preserves_plan_and_config() -> Result<()> {
    let mut app = make_test_app().await;
    let home = tempfile::tempdir_in("/tmp")?;
    app.config.codex_home = home.path().to_path_buf().abs();
    app.config.sqlite = codex_state::SqliteConfig::new_for_testing(home.path().abs());
    let config_path = home.path().join("config.toml");
    std::fs::write(&config_path, "model_reasoning_effort = \"high\"\n")?;
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let started = server.start_thread(&app.config).await?;
    let thread = started.session.thread_id;
    app.chat_widget
        .handle_thread_session_quiet(started.session.clone());
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    // A fresh idle TUI keeps this boundary until its first keypress. It must
    // not prevent a settings-only dial request before the user starts typing.
    app.startup_protected_input_boundary = true;
    let config_before = std::fs::read_to_string(&config_path)?;
    app.chat_widget
        .set_collaboration_mask(CollaborationModeMask {
            name: "Plan".into(),
            mode: Some(ModeKind::Plan),
            model: None,
            reasoning_effort: Some(Some(ReasoningEffortConfig::High)),
            developer_instructions: None,
        });
    let tui = crate::tui::test_support::make_test_tui()?;
    let mut control = crate::local_control::LocalControl::start(home.path(), "test".into())?;
    let before = app.local_control_snapshot(&tui);
    assert_eq!(before["ready"], serde_json::json!(true));
    control.publish(before);
    let request = crate::local_control::EffortRequest {
        request_id: uuid::Uuid::new_v4(),
        expected_revision: control.revision,
        expected_thread_id: thread.to_string(),
        effort: ReasoningEffortConfig::Low,
    };
    app.apply_local_settings(&tui, &mut server, &mut control, request.into())
        .await;
    assert!(
        control.pending.is_some(),
        "write acceptance must not be treated as confirmation"
    );
    let updated = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
        loop {
            if let Some(event @ AppServerEvent::ServerNotification(_)) = server.next_event().await {
                control.observe(&event);
                if let AppServerEvent::ServerNotification(notification) = event
                    && let ServerNotification::ThreadSettingsUpdated(updated) = *notification
                    && updated.thread_id == thread.to_string()
                    && updated.thread_settings.effort == Some(ReasoningEffortConfig::Low)
                {
                    break updated;
                }
            }
        }
    })
    .await?;
    assert!(control.pending.is_none());
    app.chat_widget.on_thread_settings_updated(updated);
    assert_eq!(
        app.chat_widget.current_reasoning_effort(),
        Some(ReasoningEffortConfig::Low)
    );
    assert_eq!(
        app.chat_widget.effective_collaboration_mode().mode,
        ModeKind::Plan
    );
    assert_eq!(std::fs::read_to_string(config_path)?, config_before);
    // Reuse the native composer renderer; the endpoint adds no alternate UI.
    insta::assert_snapshot!(
        "native_effort_plan_composer",
        render_bottom_popup(&app.chat_widget, /*width*/ 80)
    );
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_fast_roundtrip_preserves_effort_plan_and_config() -> Result<()> {
    let mut app = make_test_app().await;
    let home = tempfile::tempdir_in("/tmp")?;
    app.config.codex_home = home.path().to_path_buf().abs();
    app.config.sqlite = codex_state::SqliteConfig::new_for_testing(home.path().abs());
    app.config.model = Some("gpt-5.4".into());
    app.config.service_tier = Some("default".into());
    let config_path = home.path().join("config.toml");
    let config_before = "model_reasoning_effort = \"high\"\nservice_tier = \"default\"\n";
    std::fs::write(&config_path, config_before)?;
    let mut server = crate::start_embedded_app_server_for_picker(&app.config).await?;
    let started = server.start_thread(&app.config).await?;
    let thread = started.session.thread_id;
    app.chat_widget
        .handle_thread_session_quiet(started.session.clone());
    app.enqueue_primary_thread_session(started.session, started.turns)
        .await?;
    set_chatgpt_auth(&mut app.chat_widget);
    set_fast_mode_test_catalog(&mut app.chat_widget);
    app.chat_widget
        .set_collaboration_mask(CollaborationModeMask {
            name: "Plan".into(),
            mode: Some(ModeKind::Plan),
            model: None,
            reasoning_effort: Some(Some(ReasoningEffortConfig::High)),
            developer_instructions: None,
        });
    let tui = crate::tui::test_support::make_test_tui()?;
    let mut control = crate::local_control::LocalControl::start(home.path(), "test".into())?;
    for (enabled, tier, snapshot) in [
        (true, "priority", "native_fast_plan_composer"),
        (false, "default", "native_standard_plan_composer"),
    ] {
        control.publish(app.local_control_snapshot(&tui));
        let request = crate::local_control::FastRequest {
            request_id: uuid::Uuid::new_v4(),
            expected_revision: control.revision,
            expected_thread_id: thread.to_string(),
            enabled,
        };
        app.apply_local_settings(&tui, &mut server, &mut control, request.into())
            .await;
        assert!(control.pending.is_some(), "await the native settings event");
        let updated = tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            loop {
                if let Some(event @ AppServerEvent::ServerNotification(_)) =
                    server.next_event().await
                {
                    control.observe(&event);
                    if let AppServerEvent::ServerNotification(notification) = event
                        && let ServerNotification::ThreadSettingsUpdated(updated) = *notification
                        && updated.thread_id == thread.to_string()
                        && updated.thread_settings.service_tier.as_deref() == Some(tier)
                    {
                        break updated;
                    }
                }
            }
        })
        .await?;
        assert!(control.pending.is_none());
        app.chat_widget.on_thread_settings_updated(updated);
        assert_eq!(app.chat_widget.current_service_tier(), Some(tier));
        assert_eq!(
            app.chat_widget.effective_collaboration_mode().mode,
            ModeKind::Plan
        );
        assert_eq!(
            app.chat_widget.current_reasoning_effort(),
            Some(ReasoningEffortConfig::High)
        );
        assert_eq!(std::fs::read_to_string(&config_path)?, config_before);
        insta::assert_snapshot!(
            snapshot,
            render_bottom_popup(&app.chat_widget, /*width*/ 80)
        );
    }
    server.shutdown().await?;
    Ok(())
}
