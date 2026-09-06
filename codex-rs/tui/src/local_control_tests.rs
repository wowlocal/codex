use super::*;
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
    assert_eq!(control.commands.recv().await.unwrap(), request);
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
