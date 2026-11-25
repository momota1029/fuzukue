use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use anyhow::Result;
use fuzukue_proto::{
    AgentBackend, AgentSelection, FolderGrantExpiry, FullApprovalChange, FullApprovalDuration, ServerEvent,
};

use crate::hub::folder_grants::AccessRequest;

use crate::{Hub, Workspace};

#[test]
fn pending_folder_grant_is_recovered_once_after_a_storage_failure() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let project = fixture.path().join("project");
    let state = fixture.path().join("state");
    let outside = fixture.path().join("outside");
    fs::create_dir(&project)?;
    fs::create_dir(&state)?;
    fs::create_dir(&outside)?;
    let outside = outside.canonicalize()?;
    let note = outside.join("note.txt");
    fs::write(&note, "outside the workspace")?;

    let open = || Workspace::new(project.clone(), String::new(), state.clone(), Arc::new(Hub::new()));
    let app = open();
    let session = app.hub.edit_threads(|threads| threads.create(None))?;
    let AccessRequest::Pending(id) = app.hub.request_folder_access(&session, &outside)? else {
        anyhow::bail!("Expected a pending folder grant");
    };
    let mut events = app.hub.subscribe(u64::MAX).1;
    let obstructed = state.join("folder-grants.json");
    fs::create_dir(&obstructed)?;
    app.hub.set_full_approval(
        FullApprovalChange::Full { duration: FullApprovalDuration::UntilDisabled },
        &crate::remote::SettingsPrincipal::fixture(),
    )?;
    assert!(app.hub.decide_folder_access(&id, Some(FolderGrantExpiry::NoExpiry), "desktop").is_err());
    assert!(!app.hub.folder_read(&session, &note, None, false)?);
    assert!(!std::iter::from_fn(|| events.try_recv().ok()).any(|env|
        matches!(env.event, ServerEvent::ApprovalResolved { request_id, .. } if request_id == id)));
    drop(app);
    fs::remove_dir(&obstructed)?;

    let app = open();
    assert!(app.hub.full_approval_settings(None)?.active.is_some());
    assert!(app.hub.folder_read(&session, &note, None, false)?);
    let (grants, _, _) = app.hub.list_folder_grants(&session, None)?;
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].id, id);
    assert_eq!(grants[0].expiry, FolderGrantExpiry::OneHour);
    assert_eq!(grants[0].granted_by, "full_mode");
    let audit = app.hub.full_approval_settings(None)?.audit;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].approval_id, id);
    app.hub.set_full_approval(FullApprovalChange::Off, &crate::remote::SettingsPrincipal::fixture())?;
    assert!(app.hub.folder_read(&session, &note, None, false)?);
    app.hub.revoke_folder_grant(&session, &id)?;
    assert!(!open().hub.folder_read(&session, &note, None, false)?);
    Ok(())
}

#[tokio::test]
async fn full_approval_launches_claude_with_native_shell_and_audits_its_grant() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let project = fixture.path().join("project");
    let state = fixture.path().join("state");
    fs::create_dir(&project)?;
    let program = fixture.path().join("claude");
    fs::write(&program, r#"#!/usr/bin/python3
import json, subprocess, sys
from pathlib import Path
args = sys.argv[1:]
assert args[args.index("--permission-mode") + 1] == "bypassPermissions"
assert "hooks" not in json.loads(args[args.index("--settings") + 1])
json.loads(sys.stdin.readline())

command = "printf full > ../native-write"
request = {"type": "control_request", "request_id": "native-rpc", "request": {
    "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": command}
}}
print(json.dumps(request), flush=True)
response = json.loads(sys.stdin.readline())["response"]
assert response["request_id"] == "native-rpc"
assert response["response"]["behavior"] == "allow"
subprocess.run(["/bin/sh", "-ec", response["response"]["updatedInput"]["command"]], check=True)
assert Path("../native-write").read_text() == "full"
print(json.dumps({"type": "result", "result": "native shell completed"}), flush=True)
"#)?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
    let mut app = Workspace::new(project, String::new(), state, Arc::new(Hub::new()));
    app.claude_program = Some(program);
    let app = Arc::new(app);
    let session = app.hub.edit_threads(|threads| threads.create(None))?;
    app.hub.set_full_approval(
        FullApprovalChange::Full { duration: FullApprovalDuration::OneHour },
        &crate::remote::SettingsPrincipal::fixture(),
    )?;
    let selection = AgentSelection { backend: AgentBackend::Claude, model: None, effort: None };
    let report = tokio::time::timeout(Duration::from_secs(10), crate::delegate::run_in(
        &app, &session, "Run the fixture shell command.".into(), None, Some(selection),
    )).await??;
    assert_eq!(report.lines().next(), Some("native shell completed"));
    assert_eq!(fs::read_to_string(fixture.path().join("native-write"))?, "full");
    assert!(app.hub.live_approval_threads().is_empty());
    let audit = app.hub.full_approval_settings(None)?.audit;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].granted_by, "full_mode");
    assert_eq!(audit[0].action.tool, "Bash");
    assert_eq!(audit[0].action.args, "[redacted]");
    Ok(())
}
