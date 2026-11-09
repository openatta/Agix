//! A declarative scene registered at runtime with `scene.register`: its tool
//! list is all the model is offered, its prompt is in the system prompt, a
//! session in it gets work done; `scene.describe` / `scene.list` report it,
//! and neither `scene.deactivate` nor `scene.unregister` goes through while
//! a session uses it.

use live::LiveRun;
use serde_json::{json, Value};

fn nonce() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 0xffff_ffff_ffff
    )
}

fn listed(resp: &Value, scene: &str) -> Option<Value> {
    resp["scenes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["scene"] == scene)
        .cloned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scene_registered_at_runtime_shapes_its_sessions_and_holds_while_used(
) -> anyhow::Result<()> {
    let mut run = LiveRun::builder("scene-runtime").start().await?;
    let (scene, mark) = ("copyist", format!("SCENE-PROMPT-{}", nonce()));
    let registered = run
        .client
        .call(
            "scene.register",
            json!({
                "scene": {
                    "id": scene,
                    "name": "Copyist",
                    "description": "Copies files and nothing else",
                    "tools": ["Read", "Write"],
                    "requires_project": true,
                },
                "prompt": format!("You copy files for the user. Session mark: {mark}."),
            }),
        )
        .await?;
    assert_eq!(
        registered.result.unwrap_or_default()["registered"],
        true,
        "{:?}",
        registered.error
    );
    let again = run
        .client
        .call(
            "scene.register",
            json!({ "scene": { "id": scene, "name": "Again" }, "prompt": "x" }),
        )
        .await?;
    assert_eq!(
        again.error.map(|e| e.code),
        Some(-32602),
        "the id was taken twice"
    );

    let described = run
        .client
        .call("scene.describe", json!({ "scene": scene }))
        .await?
        .result
        .unwrap_or_default();
    assert_eq!(described["name"], "Copyist", "{described}");
    assert_eq!(described["active"], false);
    assert_eq!(described["tools"]["allowed"], json!(["Read", "Write"]));

    let token = format!("COPY-{}", nonce());
    run.write_file("in.txt", &format!("{token}\n"))?;
    let session = run.scene_session(scene, json!({})).await?;
    run.turn(
        &session,
        "Read in.txt and write exactly its content to out.txt, then reply DONE.",
    )
    .await?;
    assert_eq!(
        run.read_file("out.txt").map(|s| s.trim().to_string()),
        Some(token)
    );
    for call in run.provider.calls() {
        let tools = call.tool_names();
        assert!(
            tools.iter().any(|t| t == "Read") && tools.iter().any(|t| t == "Write"),
            "{tools:?}"
        );
        for absent in ["Bash", "Edit", "Grep", "Agent", "WebFetch"] {
            assert!(
                !tools.iter().any(|t| t == absent),
                "{absent} offered: {tools:?}"
            );
        }
        assert!(call.system_contains(&mark), "the scene's prompt is missing");
    }

    let scenes = run
        .client
        .call("scene.list", Value::Null)
        .await?
        .result
        .unwrap_or_default();
    let entry = listed(&scenes, scene).expect("the scene is listed");
    assert_eq!(entry["active"], true, "{entry}");
    assert_eq!(entry["sessions"], 1);

    for method in ["scene.deactivate", "scene.unregister"] {
        let refused = run.client.call(method, json!({ "scene": scene })).await?;
        let error = refused.error.expect("refused while a session uses it");
        assert_eq!(error.code, -32009, "{method}: {error:?}");
        assert!(
            error.message.contains(&session),
            "{method}: {}",
            error.message
        );
    }

    run.client.session_close(&session).await?;
    let deactivated = run
        .client
        .call("scene.deactivate", json!({ "scene": scene }))
        .await?;
    assert!(deactivated.error.is_none(), "{deactivated:?}");
    let unregistered = run
        .client
        .call("scene.unregister", json!({ "scene": scene }))
        .await?;
    assert_eq!(unregistered.result.unwrap_or_default()["registered"], false);
    let scenes = run
        .client
        .call("scene.list", Value::Null)
        .await?
        .result
        .unwrap_or_default();
    assert!(listed(&scenes, scene).is_none(), "{scenes}");
    let resumed = run
        .client
        .call("session.resume", json!({ "session_id": session }))
        .await?;
    assert!(
        resumed.error.is_some(),
        "a session of an unregistered scene resumed: {resumed:?}"
    );
    run.finish().await;
    Ok(())
}
