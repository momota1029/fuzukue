use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use anyhow::Result;
use fuzukue_proto::{AgentBackend, AgentSelection, FullApprovalChange, FullApprovalDuration};

use crate::{Hub, Workspace};

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
