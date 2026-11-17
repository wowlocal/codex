use super::*;
use crate::backend_id::BackendId;
use codex_app_server_client::AppServerClient;
use codex_app_server_client::CloudClient;
use codex_app_server_client::CloudOperation;
use codex_app_server_protocol::AdditionalContextEntry;
use codex_app_server_protocol::AdditionalContextKind;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

#[tokio::test]
async fn mcp_app_context_reaches_start_steer_and_cloud_admission() {
    for (mode, mut active) in [
        (ThreadParamsMode::Embedded, None),
        (ThreadParamsMode::Embedded, Some("active-turn".to_string())),
        (ThreadParamsMode::Cloud, None),
    ] {
        let steering = active.is_some();
        let (client, mut commands, _events) = CloudClient::channel(/*capacity*/ 1);
        let mut session =
            AppServerSession::new(BackendId::new(), AppServerClient::Cloud(client), mode);
        let context = HashMap::from([(
            "mcp_app_selected_rows".to_string(),
            AdditionalContextEntry {
                value: "Selected rows: 2, 5".to_string(),
                kind: AdditionalContextKind::Untrusted,
                name: None,
            },
        )]);
        let op = AppCommand::UserTurn {
            turn_trigger: "user",
            cloud_submission: CloudSubmissionIntent::StartOrSteer,
            client_user_message_id: "message".to_string(),
            items: vec![UserInput::Text {
                text: "Summarize my selection".to_string(),
                text_elements: Vec::new(),
            }],
            cwd: std::env::current_dir().expect("working directory"),
            approval_policy: AskForApproval::Never,
            approvals_reviewer: None,
            active_permission_profile: None,
            model: "test-model".to_string(),
            effort: None,
            summary: None,
            service_tier: None,
            final_output_json_schema: None,
            collaboration_mode: None,
        };
        let (result, (method, sent_context)) = tokio::join!(
            session.submit_user_message(
                ThreadId::new(),
                &op,
                CloudSubmissionIntent::StartOrSteer,
                &mut active,
                |_| Ok(None),
                || TurnStartSettings {
                    approval_policy: None,
                    approvals_reviewer: None,
                    permissions: TurnPermissionsOverride::Preserve,
                    workspace_roots: &[],
                },
                context.clone(),
            ),
            async {
                let command = commands.recv().await.expect("outbound submission");
                let recorded = match command.operation {
                    CloudOperation::Request(request) => match *request {
                        ClientRequest::TurnStart { params, .. } => {
                            ("start", params.additional_context)
                        }
                        ClientRequest::TurnSteer { params, .. } => {
                            ("steer", params.additional_context)
                        }
                        _ => panic!("unexpected request"),
                    },
                    CloudOperation::SubmitUserMessage(params) => {
                        ("cloud", params.additional_context)
                    }
                    _ => panic!("unexpected operation"),
                };
                command
                    .response
                    .send(Ok(Err(JSONRPCErrorError {
                        code: JSONRPC_INVALID_PARAMS,
                        data: None,
                        message: "stop after recording".to_string(),
                    })))
                    .expect("submission waiting for response");
                recorded
            }
        );
        assert!(result.is_err());
        assert_eq!(
            method,
            if mode == ThreadParamsMode::Cloud {
                "cloud"
            } else if steering {
                "steer"
            } else {
                "start"
            }
        );
        assert_eq!(
            serde_json::to_value(sent_context).unwrap(),
            serde_json::to_value(Some(context)).unwrap()
        );
    }
}
