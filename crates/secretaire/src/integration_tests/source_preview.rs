use super::{mcp_app, mcp_data, mcp_request_for, TempProject};
use anyhow::Result;
use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, Request, StatusCode},
    Extension,
};
use fuzukue_i18n::Locale;
use fuzukue_proto::ServerEvent;
use fuzukue_view::{encode_file_path, source_file};
use image::{DynamicImage, ImageFormat};
use serde_json::json;
use std::{fs, io::Cursor, net::SocketAddr, os::unix::fs::symlink, sync::Arc};
use tower::ServiceExt;

const IMAGE: &str = "target/themes/sheet 100%.png";

#[tokio::test]
async fn editor_route_and_open_file_agree_on_workspace_previews() -> Result<()> {
    let project = TempProject::new()?;
    let app = mcp_app(&project);
    let root = project.path();
    fs::write(root.join(".gitignore"), "/target/\n")?;
    fs::create_dir_all(root.join("target/themes"))?;
    let mut png = Cursor::new(Vec::new());
    DynamicImage::new_rgba16(2464, 2255).write_to(&mut png, ImageFormat::Png)?;
    let png = png.into_inner();
    fs::write(root.join(IMAGE), &png)?;
    fs::write(root.join("source.txt"), "one\ntwo\n")?;
    fs::write(root.join("binary.dat"), [0xff, 0x00])?;
    fs::write(root.join("corrupt.png"), &png[..16])?;
    fs::File::create(root.join("oversized.png"))?.set_len(source_file::MAX_BYTES + 1)?;
    let outside = TempProject::new()?;
    fs::write(outside.path().join("outside.txt"), "outside")?;
    symlink(outside.path().join("outside.txt"), root.join("outside.txt"))?;
    symlink(root.join(IMAGE), root.join("inside.png"))?;
    fs::hard_link(root.join("source.txt"), root.join("linked.txt"))?;

    let access = crate::remote::Access::new(&app.state_dir, Default::default())?;
    let router = crate::router(crate::App::fixture(app.clone()), Arc::new(access)).layer(
        Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234)))),
    );
    let get = |path: &str| {
        router.clone().oneshot(
            Request::get(format!(
                "/editor/{}?workspace={}",
                encode_file_path(path),
                app.id
            ))
            .body(Body::empty())
            .unwrap(),
        )
    };
    let session = app.hub.edit_threads(|store| store.create(None))?;
    let (_, mut live) = app.hub.subscribe_client(u64::MAX, true);
    let open = |arguments| {
        mcp_request_for(
            &app,
            &session,
            "tools/call",
            json!({
                "name": "open_file", "arguments": arguments,
            }),
        )
    };

    for (path, ty, bytes) in [
        (IMAGE, "image/png", png.as_slice()),
        ("inside.png", "image/png", png.as_slice()),
        (
            "linked.txt",
            "text/plain; charset=utf-8",
            &b"one\ntwo\n"[..],
        ),
    ] {
        let response = get(path).await?;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(response.headers()[header::CONTENT_TYPE], ty);
        assert_eq!(to_bytes(response.into_body(), bytes.len()).await?, bytes);
    }
    for (path, status, error) in [
        (
            "binary.dat",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            source_file::PreviewError::Unsupported,
        ),
        (
            "corrupt.png",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            source_file::PreviewError::Unsupported,
        ),
        (
            "oversized.png",
            StatusCode::PAYLOAD_TOO_LARGE,
            source_file::PreviewError::TooLarge,
        ),
        (
            "outside.txt",
            StatusCode::NOT_FOUND,
            source_file::PreviewError::Unavailable,
        ),
        (
            "missing.png",
            StatusCode::NOT_FOUND,
            source_file::PreviewError::Unavailable,
        ),
        (
            "target",
            StatusCode::NOT_FOUND,
            source_file::PreviewError::Unavailable,
        ),
    ] {
        let is_localized = |message: &str| {
            Locale::available().any(|locale| message.contains(locale.text(error.message_key())))
        };
        let response = get(path).await?;
        assert_eq!(response.status(), status, "{path}");
        let message = String::from_utf8(to_bytes(response.into_body(), 2048).await?.to_vec())?;
        assert!(is_localized(&message), "{message}");

        let result = open(json!({"path": path})).await;
        assert_eq!(result["isError"], true, "{result}");
        assert!(
            is_localized(result["content"][0]["text"].as_str().unwrap()),
            "{result}"
        );
        assert!(live.try_recv().is_err(), "{path} produced a UI event");
    }
    for (path, lines) in [(IMAGE, None), ("source.txt", Some((2, 2)))] {
        let args = if lines.is_some() {
            json!({"path": path, "line": 2})
        } else {
            json!({"path": path})
        };
        assert_eq!(
            mcp_data(&open(args).await),
            &json!({"path": path, "lines": lines})
        );
        assert!(matches!(live.try_recv()?.event,
            ServerEvent::OpenFile { session: id, path: file, lines: range }
            if id == session && file == path && range == lines));
    }
    Ok(())
}
