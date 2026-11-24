//! Language selection for server-authored agent instructions.

pub(crate) fn japanese() -> bool {
    fuzukue_proto::compat::env_os("FUZUKUE_HARNESS_LANG").is_some_and(|lang| lang == "ja")
}

macro_rules! harness_text {
    ($en:expr, $ja:expr $(,)?) => {
        if crate::harness::japanese() { $ja } else { $en }
    };
}

macro_rules! harness_format {
    ($en:expr, $ja:expr $(, $($args:tt)*)?) => {
        harness_text!(format!($en $(, $($args)*)?), format!($ja $(, $($args)*)?))
    };
}

/// Diagnostics used by tools/harness-ab. Live turns use the normal partner launch and MCP route.
pub(crate) async fn run(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    use std::{io::Read, path::PathBuf, sync::Arc};

    use anyhow::{bail, Context};
    use fuzukue_proto::{Actor, ServerEvent};

    let mode = args.next().context("usage: secretaire harness-ab <snapshot|turn> PROJECT")?;
    let project = PathBuf::from(args.next().context("missing project path")?).canonicalize()?;
    anyhow::ensure!(args.next().is_none(), "unexpected harness-ab argument");
    if mode == "snapshot" {
        let system = crate::claude::partner_system(&project, &Default::default(), false)?;
        println!("{}", serde_json::json!({
            "system": system,
            "toolList": crate::mcp::partner_tool_list(false),
        }));
        return Ok(());
    }
    anyhow::ensure!(mode == "turn", "unknown harness-ab mode: {mode}");
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt)?;

    let state = tempfile::tempdir()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unset")?);
    let registry = crate::App::new(home, project, state.path().into(), addr.to_string(), None)?;
    let app = registry.clone().route(None).await?;
    let access = Arc::new(crate::remote::Access::new(state.path(), Default::default())?);
    let service = crate::router(registry, access)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let server = tokio::spawn(async move { axum::serve(listener, service).await });
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let (_, mut events) = app.hub.subscribe_client(u64::MAX, false);
    let event = crate::prompt_event(&session, Actor::User, &prompt);
    crate::prompt_claude(&app, &session, prompt, Some(event)).await?;
    loop {
        match events.recv().await?.event {
            ServerEvent::TurnEnded { session: id, actor: Actor::Claude } if id == session => break,
            ServerEvent::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
    server.abort();
    Ok(())
}
