//! The example plugins in `examples/plugins`, used by a real model: `demo-rust`
//! built into the daemon, `demo-quickjs` installed as a package. Asserted on
//! what only a run that went through the plugins can produce — stamps the
//! model cannot compute, a notice only the async prompt pass reads, a secret
//! that must never reach the model, and the plugin's own counts.

use std::sync::Arc;

use demo_rust_plugin::{stamp, DemoPlugin, AUDITOR};
use live::{Host, LiveRun};
use serde_json::json;
use test_runner::plugin_fixture::{package_demo, DEMO_PACKAGE, DEMO_PACKAGE_VERSION};

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

#[tokio::test(flavor = "multi_thread")]
async fn a_model_works_through_both_demo_plugins() -> anyhow::Result<()> {
    let mut run = LiveRun::start_with_host(
        "plugins",
        json!({}),
        Host {
            plugins: vec![Arc::new(DemoPlugin::new())],
            ..Default::default()
        },
    )
    .await?;
    let (archive, checksum) = package_demo(run.world.root())?;
    let installed = run
        .client
        .call(
            "plugin.install",
            json!({
                "name": DEMO_PACKAGE,
                "version": DEMO_PACKAGE_VERSION,
                "download_url": format!("file://{}", archive.display()),
                "checksum": checksum,
            }),
        )
        .await?;
    anyhow::ensure!(installed.error.is_none(), "plugin.install: {installed:?}");

    let codename = format!("CODENAME-{}", nonce()).to_uppercase();
    let secret = format!("SECRET-{}", nonce()).to_uppercase();
    let word = format!("word-{}", nonce());
    run.write_file(
        ".demo/notice.md",
        &format!("The release codename is {codename}."),
    )?;
    run.write_file("notes.txt", &format!("deploy key: {secret}\nowner: ops\n"))?;
    run.write_file("keys.secret", &format!("{secret}\n"))?;
    let session = run.session().await?;

    let events = run.turn(&session, &format!("!stamp {word}")).await?;
    assert!(
        events.text.contains(&stamp(&word)),
        "the shortcut did not reach DemoStamp: {}",
        events.text
    );

    run.turn(
        &session,
        "Write report.txt in the project root with three lines: the release codename \
         from this project's notice, the stamp of the text `audited`, and what notes.txt \
         says the deploy key is. Also try to read keys.secret with the Read tool (not Bash), and \
         write generated/version.txt containing 1.0 with the Write tool. Tell me which of these \
         you could not do and why.",
    )
    .await?;
    let report = run.read_file("report.txt").unwrap_or_default();
    assert!(
        report.contains(&codename),
        "the notice never reached the model: {report:?}"
    );
    assert!(
        report.contains(&stamp("audited")),
        "no stamp in the report: {report:?}"
    );
    assert!(
        !report.contains(&secret),
        "the secret reached the report: {report:?}"
    );
    assert!(
        run.read_file("generated/version.txt").is_none(),
        "the package's tool.around let a write under generated/ through"
    );

    run.turn(
        &session,
        &format!(
            "Use the {AUDITOR} agent to audit notes.txt, then write audit.txt containing \
             only the STAMP line it reports."
        ),
    )
    .await?;
    let audit = run.read_file("audit.txt").unwrap_or_default();
    assert!(
        audit.contains(&stamp("audited")),
        "the auditor's stamp did not come back up: {audit:?}"
    );

    let calls = run.provider.calls();
    assert!(
        calls.iter().all(|c| !c.messages_text().contains(&secret)),
        "a request carried the secret"
    );
    assert!(calls
        .iter()
        .any(|c| c.messages_text().contains("are off limits")));
    assert!(calls
        .iter()
        .any(|c| c.system_text().contains("House rules (demo-quickjs)")));

    let detail = run
        .client
        .session_get(&session)
        .await?
        .result
        .unwrap_or_default();
    let check = detail["health"]["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|c| c["name"] == "demo-rust.activity"))
        .cloned()
        .unwrap_or_default();
    let scripts = &detail["scripts"];
    eprintln!(
        "[live plugins] {}\n[live plugins] scripts: {} calls, {} applied, {} failed",
        check["details"], scripts["calls"], scripts["applied"], scripts["failed"]
    );
    assert_eq!(check["status"], "ok", "{check}");
    for counter in ["stamps", "refused", "redacted"] {
        assert!(
            check["details"][counter].as_u64().unwrap_or(0) >= 1,
            "{counter}: {check}"
        );
    }
    assert_eq!(scripts["failed"], 0, "{scripts}");

    run.finish().await;
    Ok(())
}
