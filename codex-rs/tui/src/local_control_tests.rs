use super::*;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;
use tokio::io::AsyncBufReadExt;

fn request() -> EffortRequest {
    EffortRequest {
        request_id: Uuid::new_v4(),
        expected_revision: 1,
        expected_thread_id: Uuid::new_v4().to_string(),
        effort: ReasoningEffort::High,
    }
}

async fn exchange(stream: &mut BufReader<UnixStream>, value: Value) -> Value {
    stream
        .get_mut()
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(/*secs*/ 2), stream.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str::<Value>(&line).unwrap()["result"].clone()
}

#[tokio::test]
async fn reconnect_recovers_same_request_without_dispatching_twice() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let mut control = LocalControl::start(home.path(), "test".into()).unwrap();
    control.publish(json!({"threadId":"visible","focused":true}));
    let path = control.directory.join("control.sock");
    let mut first = BufReader::new(UnixStream::connect(&path).await.unwrap());
    let request = request();
    let mut command = serde_json::to_value(&request).unwrap();
    command["method"] = "effort/set".into();
    let pending = exchange(&mut first, command.clone()).await;
    assert_eq!(
        pending,
        json!({"requestId":request.request_id,"status":"pending"})
    );
    assert_eq!(
        control.commands.recv().await.unwrap(),
        request.clone().into()
    );
    drop(first);
    control.finish(request.request_id, json!({"effort":"high"}));
    let mut second = BufReader::new(UnixStream::connect(path).await.unwrap());
    let applied =
        json!({"requestId":request.request_id,"status":"applied","outcome":{"effort":"high"}});
    assert_eq!(
        exchange(
            &mut second,
            json!({"method":"request/read","requestId":request.request_id})
        )
        .await,
        applied
    );
    assert_eq!(exchange(&mut second, command.clone()).await, applied);
    command["effort"] = "low".into();
    assert_eq!(
        exchange(&mut second, command).await,
        json!({"error":"request ID reused with different parameters"})
    );
    assert!(control.commands.try_recv().is_err());
}

#[tokio::test]
async fn subscription_reports_focus_loss_and_selection_changes() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let mut control = LocalControl::start(home.path(), "test".into()).unwrap();
    control.publish(json!({"threadId":"first","focused":true}));
    let mut stream = BufReader::new(
        UnixStream::connect(control.directory.join("control.sock"))
            .await
            .unwrap(),
    );
    let mut expected = control.metadata.clone();
    expected["threadId"] = "first".into();
    expected["focused"] = true.into();
    expected["revision"] = 1.into();
    assert_eq!(
        exchange(&mut stream, json!({"method":"status/subscribe"})).await,
        expected
    );
    control.publish(json!({"threadId":"second","focused":false}));
    expected["threadId"] = "second".into();
    expected["focused"] = false.into();
    expected["revision"] = 2.into();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(/*secs*/ 2), stream.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap(),
        json!({"result":expected})
    );
    let directory = control.directory.clone();
    drop(control);
    assert!(!directory.exists());
}

#[tokio::test]
async fn live_endpoints_are_distinct_and_insecure_discovery_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let first = LocalControl::start(home.path(), "test".into()).unwrap();
    let second = LocalControl::start(home.path(), "test".into()).unwrap();
    assert_ne!(first.directory, second.directory);
    assert!(first.directory.join("control.sock").exists());
    std::fs::set_permissions(
        home.path().join("tui-control"),
        std::fs::Permissions::from_mode(/*mode*/ 0o755),
    )
    .unwrap();
    assert!(LocalControl::start(home.path(), "test".into()).is_err());
}

#[tokio::test]
async fn oversized_input_does_not_reach_tui() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let mut control = LocalControl::start(home.path(), "test".into()).unwrap();
    let mut stream = UnixStream::connect(control.directory.join("control.sock"))
        .await
        .unwrap();
    stream.write_all(&vec![b'x'; 4097]).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(/*secs*/ 2), stream.read_u8())
        .await
        .unwrap();
    assert!(result.is_err());
    assert!(control.commands.try_recv().is_err());
}

#[tokio::test]
async fn only_matching_native_settings_confirm_request() {
    use codex_app_server_protocol::ThreadSettings;
    use codex_app_server_protocol::ThreadSettingsUpdatedNotification;
    use codex_protocol::config_types::CollaborationMode;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::config_types::Settings;
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let mut control = LocalControl::start(home.path(), "test".into()).unwrap();
    let request = request();
    let (tx, mut rx) = mpsc::channel(/*buffer*/ 1);
    control
        .requests
        .lock()
        .unwrap()
        .enqueue(request.clone().into(), &tx);
    rx.recv().await.unwrap();
    control.pending = Some((
        request.clone().into(),
        json!({"model":"test-model","effort":"high"}),
    ));
    let mut updated = ThreadSettingsUpdatedNotification {
        thread_id: "background".into(),
        thread_settings: ThreadSettings {
            cwd: codex_utils_absolute_path::AbsolutePathBuf::try_from(home.path()).unwrap(),
            approval_policy: codex_app_server_protocol::AskForApproval::Never,
            approvals_reviewer: codex_app_server_protocol::ApprovalsReviewer::User,
            sandbox_policy: codex_app_server_protocol::SandboxPolicy::ReadOnly {
                network_access: false,
            },
            active_permission_profile: None,
            model: "test-model".into(),
            model_provider: "openai".into(),
            service_tier: None,
            effort: Some(ReasoningEffort::High),
            summary: None,
            collaboration_mode: CollaborationMode {
                mode: ModeKind::Plan,
                settings: Settings {
                    model: "test-model".into(),
                    reasoning_effort: Some(ReasoningEffort::High),
                    developer_instructions: None,
                },
            },
            multi_agent_mode: Default::default(),
            personality: None,
        },
    };
    control.observe(&AppServerEvent::ServerNotification(Box::new(
        ServerNotification::ThreadSettingsUpdated(updated.clone()),
    )));
    assert!(control.pending.is_some());
    updated.thread_id.clone_from(&request.expected_thread_id);
    control.observe(&AppServerEvent::ServerNotification(Box::new(
        ServerNotification::ThreadSettingsUpdated(updated),
    )));
    assert_eq!(
        control.requests.lock().unwrap().0[0].1,
        json!({"requestId":request.request_id,"status":"applied",
        "outcome":{"threadId":request.expected_thread_id,"model":"test-model","effort":"high"}})
    );
    assert!(control.pending.is_none());
}

#[tokio::test]
async fn fast_write_reconnect_is_idempotent_and_shares_effort_queue() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let mut control = LocalControl::start(home.path(), "test".into()).unwrap();
    let path = control.directory.join("control.sock");
    let mut stream = BufReader::new(UnixStream::connect(&path).await.unwrap());
    let id = Uuid::new_v4();
    let command = json!({"method":"fast/set","requestId":id,"expectedRevision":1,
        "expectedThreadId":"visible","enabled":true});
    assert_eq!(
        exchange(&mut stream, command.clone()).await,
        json!({"requestId":id,"status":"pending"})
    );
    let received = control.commands.recv().await.unwrap();
    assert_eq!(received.change, SettingsChange::Fast { enabled: true });
    let mut effort = serde_json::to_value(request()).unwrap();
    effort["method"] = "effort/set".into();
    assert_eq!(
        exchange(&mut stream, effort).await,
        json!({"error":"another settings request is pending"})
    );
    drop(stream);
    control.finish(id, json!({"serviceTier":"priority"}));
    let mut stream = BufReader::new(UnixStream::connect(&path).await.unwrap());
    assert_eq!(
        exchange(&mut stream, command).await,
        json!({"requestId":id,"status":"applied","outcome":{"serviceTier":"priority"}})
    );
    assert!(control.commands.try_recv().is_err());
    let mut invalid = json!({"method":"fast/set","requestId":id,"expectedRevision":1,
        "expectedThreadId":"visible","enabled":false});
    assert_eq!(
        exchange(&mut stream, invalid.clone()).await,
        json!({"error":"request ID reused with different parameters"})
    );
    invalid["effort"] = "high".into();
    assert_eq!(
        exchange(&mut stream, invalid).await,
        json!({"error":"invalid control request"})
    );
}
