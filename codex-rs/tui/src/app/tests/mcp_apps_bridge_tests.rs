use super::*;
use crate::backend_id::BackendId;
use codex_app_server_client::AppServerClient;
use codex_app_server_client::CloudClient;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn mcp_app_message_rejects_a_different_backend_with_the_same_thread_id() -> Result<()> {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    app.active_thread_id = Some(thread_id);
    app.primary_thread_id = Some(thread_id);
    app.chat_widget
        .handle_thread_session_quiet(test_thread_session(thread_id, app.config.cwd.to_path_buf()));
    assert_eq!(app.current_displayed_thread_id(), Some(thread_id));
    let (client, _commands, _events) = CloudClient::channel(/*capacity*/ 1);
    let mut session = AppServerSession::new(
        BackendId::new(),
        AppServerClient::Cloud(client),
        crate::ThreadParamsMode::Embedded,
    );
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let (response_tx, response) = tokio::sync::oneshot::channel();
    app.handle_event(
        &mut tui,
        &mut session,
        AppEvent::McpAppMessage {
            backend_id: BackendId::new(),
            thread_id,
            text: "do not send this to the other backend".to_string(),
            response_tx,
        },
    )
    .await?;
    assert_eq!(
        response.await?,
        Err("Open the MCP App's thread in Codex before sending a message.".to_string())
    );
    Ok(())
}
