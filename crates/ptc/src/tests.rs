use super::*;
use base::interface::tool_dispatch::SubCallAnswer;
use serde_json::json;
use std::sync::atomic::AtomicUsize;

/// `Echo` answers its input's `text` after `delay_ms`; `Data` answers with
/// structured content; `Write` is not concurrency-safe; `Fail` reports a
/// failure. Records every call and the most that ran at once.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, Value)>>,
    running: AtomicUsize,
    peak: AtomicUsize,
    write_peak: AtomicUsize,
    writes: AtomicUsize,
}

#[async_trait]
impl ToolDispatch for Fake {
    fn callable(&self) -> Vec<CallableTool> {
        vec![]
    }
    fn concurrency_safe(&self, name: &str, _: &Value) -> bool {
        name != "Write"
    }
    async fn call(
        &self,
        name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> Result<SubCallAnswer, String> {
        self.calls
            .lock()
            .unwrap()
            .push((name.to_string(), input.clone()));
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        if name == "Write" {
            let w = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
            self.write_peak.fetch_max(w, Ordering::SeqCst);
        }
        let delay = input["delay_ms"].as_u64().unwrap_or(0);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
            _ = cancel.cancelled() => {}
        }
        if name == "Write" {
            self.writes.fetch_sub(1, Ordering::SeqCst);
        }
        self.running.fetch_sub(1, Ordering::SeqCst);
        match name {
            "Echo" | "Write" => Ok(SubCallAnswer {
                text: input["text"].as_str().unwrap_or_default().to_string(),
                structured: None,
                reported_failure: false,
            }),
            "Data" | "Agent" => Ok(SubCallAnswer {
                text: "ignored".into(),
                structured: Some(json!({"n": 7, "input": input})),
                reported_failure: false,
            }),
            "Fail" => Ok(SubCallAnswer {
                text: input["text"].as_str().unwrap_or("it broke").to_string(),
                structured: None,
                reported_failure: true,
            }),
            other => Err(format!(
                "{other} cannot be called from code in this session"
            )),
        }
    }
}

fn limits() -> Limits {
    Limits {
        time: Duration::from_secs(10),
        ..Limits::default()
    }
}

async fn run_with(code: &str, args: Value, limits: Limits) -> (ToolResult, Arc<Fake>, ToolContext) {
    let fake = Arc::new(Fake::default());
    let mut ctx = ToolContext::for_test("/tmp".into());
    ctx.tool_use_id = "toolu_code".into();
    ctx.dispatch = Some(fake.clone());
    let tool = RunCodeTool::new(&[], limits);
    let result = tool
        .call(
            json!({"code": code, "args": args, "description": "test"}),
            ctx.clone(),
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
    (result, fake, ctx)
}

async fn run(code: &str) -> (ToolResult, Arc<Fake>) {
    let (r, f, _) = run_with(code, json!({}), limits()).await;
    (r, f)
}

fn text(r: &ToolResult) -> String {
    match &r.content {
        base::tool::ToolResultContent::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_return_value_comes_back_and_calls_run_in_parallel() {
    let (r, fake) = run("const parts = await Promise.all([1,2,3,4].map(i =>
            tools.Echo({text: 'x' + i, delay_ms: 150})));
         return {joined: parts.join(','), n: parts.length};")
    .await;
    assert!(!r.is_error, "{}", text(&r));
    assert_eq!(
        r.structured_content,
        Some(json!({"joined": "x1,x2,x3,x4", "n": 4}))
    );
    assert!(
        text(&r).contains("(4 tool calls, 0 failed)"),
        "{}",
        text(&r)
    );
    assert_eq!(
        fake.peak.load(Ordering::SeqCst),
        4,
        "the four calls overlapped"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_that_are_not_concurrency_safe_run_one_at_a_time() {
    let (r, fake) = run(
        "await Promise.all([1,2,3].map(i => tools.Write({text: 'w', delay_ms: 50})));
         return 'ok';",
    )
    .await;
    assert!(!r.is_error, "{}", text(&r));
    assert_eq!(fake.write_peak.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_content_reaches_the_program_as_an_object() {
    let (r, _) = run("const d = await tools.Data({}); return d.n * 2;").await;
    assert_eq!(r.structured_content, Some(json!(14)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_or_unknown_tool_rejects_and_the_program_can_handle_it() {
    let (r, _) = run("const out = [];
         try { await tools.Fail({}); } catch (e) { out.push(e.tool + ': ' + e.message); }
         try { await tools.Nope({}); } catch (e) { out.push(e.message); }
         return out;")
    .await;
    let v = r.structured_content.clone().unwrap();
    assert_eq!(v[0], "Fail: it broke");
    assert!(
        v[1].as_str()
            .unwrap()
            .contains("cannot be called from code"),
        "{v}"
    );
    assert!(text(&r).contains("2 failed"), "{}", text(&r));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncaught_error_is_reported_with_the_calls_that_failed() {
    let (r, _) = run("await tools.Echo({text: 'a'}); await tools.Fail({}); return 1;").await;
    assert!(r.is_error);
    let t = text(&r);
    assert!(t.contains("The program threw"), "{t}");
    assert!(t.contains("it broke"), "{t}");
    assert!(t.contains("Failed calls"), "{t}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reaching_for_node_or_modules_says_what_to_do_instead() {
    let (r, _) = run("const fs = require('fs'); return 1;").await;
    assert!(r.is_error);
    assert!(
        text(&r).contains("require is not available in RunCode"),
        "{}",
        text(&r)
    );

    let (r, _) = run("import fs from 'fs';\nreturn 1;").await;
    assert!(r.is_error);
    assert!(text(&r).contains("no modules"), "{}", text(&r));
}

#[tokio::test(flavor = "multi_thread")]
async fn args_and_the_standard_library_are_there() {
    let (r, _, _) = run_with(
        "const m = new Map(args.items.map((x, i) => [x, i]));
         return {size: m.size, max: Math.max(...args.items), json: JSON.stringify([...m.keys()])};",
        json!({"items": [3, 9, 4]}),
        limits(),
    )
    .await;
    assert_eq!(
        r.structured_content,
        Some(json!({"size": 3, "max": 9, "json": "[3,9,4]"}))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_becomes_an_agent_call_with_its_schema() {
    let (r, fake) = run(
        "const v = await agent('review a.rs', {type: 'explore', label: 'a.rs',
            schema: {type: 'object'}});
         return v.input;",
    )
    .await;
    assert!(!r.is_error, "{}", text(&r));
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls[0].0, "Agent");
    assert_eq!(
        calls[0].1,
        json!({"prompt": "review a.rs", "description": "a.rs", "subagent_type": "explore",
               "output_schema": {"type": "object"}})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_loop_is_cut_off_at_the_time_limit() {
    let (r, _, _) = run_with(
        "while (true) {}",
        json!({}),
        Limits {
            time: Duration::from_millis(300),
            ..Limits::default()
        },
    )
    .await;
    assert!(r.is_error);
    assert!(text(&r).contains("ran out of time"), "{}", text(&r));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_call_is_cut_off_at_the_time_limit_too() {
    let (r, _, _) = run_with(
        "await tools.Echo({text: 'x', delay_ms: 60000}); return 1;",
        json!({}),
        Limits {
            time: Duration::from_millis(300),
            ..Limits::default()
        },
    )
    .await;
    assert!(r.is_error);
    assert!(text(&r).contains("ran out of time"), "{}", text(&r));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_call_limit_ends_the_run() {
    let (r, _, _) = run_with(
        "for (let i = 0; i < 10; i++) { await tools.Echo({text: 'x'}); } return 1;",
        json!({}),
        Limits {
            calls: 3,
            ..limits()
        },
    )
    .await;
    assert!(r.is_error);
    assert!(text(&r).contains("more than 3 tool calls"), "{}", text(&r));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_request_lets_the_program_wrap_up() {
    let fake = Arc::new(Fake::default());
    let mut ctx = ToolContext::for_test("/tmp".into());
    ctx.tool_use_id = "toolu_stop".into();
    ctx.dispatch = Some(fake.clone());
    let session = ctx.session.clone();
    let tool = RunCodeTool::new(&[], limits());
    let running = tokio::spawn(async move {
        tool.call(
            json!({"code": "let n = 0;
                while (!stopRequested()) { await tools.Echo({text: 'x', delay_ms: 20}); n++; }
                return {stopped: true, done: n > 0};"}),
            ctx,
            ProgressSender::noop("t"),
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(session.cancel_running_task("toolu_stop"));
    let r = running.await.unwrap();
    assert_eq!(
        r.structured_content,
        Some(json!({"stopped": true, "done": true}))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_sent_to_the_task_reach_the_program() {
    let fake = Arc::new(Fake::default());
    let mut ctx = ToolContext::for_test("/tmp".into());
    ctx.tool_use_id = "toolu_msg".into();
    ctx.dispatch = Some(fake.clone());
    let session = ctx.session.clone();
    let tool = RunCodeTool::new(&[], limits());
    let running = tokio::spawn(async move {
        tool.call(
            json!({"code": "for (let i = 0; i < 100; i++) {
                    const m = messages(); if (m.length) return m;
                    await tools.Echo({text: 'x', delay_ms: 20}); }
                return [];"}),
            ctx,
            ProgressSender::noop("t"),
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let task = session.find_running_task("toolu_msg").unwrap();
    task.inbox
        .get()
        .unwrap()
        .deliver("skip the rest".into())
        .unwrap();
    let r = running.await.unwrap();
    assert_eq!(r.structured_content, Some(json!(["skip the rest"])));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_is_a_code_task_with_a_call_log() {
    let (_, _, ctx) = run_with(
        "log('starting'); await tools.Echo({text: 'hi'}); return 1;",
        json!({}),
        limits(),
    )
    .await;
    let task = ctx.session.find_running_task("toolu_code").unwrap();
    assert_eq!(task.task_type, TaskType::Code);
    let output = task.output.lock().unwrap().clone();
    assert!(output.contains("starting"), "{output}");
    assert!(output.contains("→ Echo(text: hi)"), "{output}");
    assert!(output.contains("← Echo(text: hi) ok"), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_log_comes_back_with_the_result() {
    let (r, _) = run("console.log('saw', {n: 2}); log('second'); return 1;").await;
    let t = text(&r);
    assert!(t.contains("Log:\nsaw {\"n\":2}\nsecond"), "{t}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_run_ends_when_the_session_does() {
    let fake = Arc::new(Fake::default());
    let mut ctx = ToolContext::for_test("/tmp".into());
    ctx.tool_use_id = "toolu_bg".into();
    ctx.dispatch = Some(fake.clone());
    let (session, session_cancel) = (ctx.session.clone(), ctx.cancel.clone());
    let tool = RunCodeTool::new(
        &[],
        Limits {
            stop_grace: Duration::from_secs(60),
            ..limits()
        },
    );
    let started = tool
        .call(
            json!({"code": "while (true) { await tools.Echo({text: 'x', delay_ms: 20}); }",
                   "background": true}),
            ctx,
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
    let task_id = text(&started)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    let task = session.find_running_task(&task_id).expect("registered");
    tokio::time::sleep(Duration::from_millis(150)).await;
    session_cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task.wait_ended())
        .await
        .expect("the run is cut off well before the stop grace");
    let calls = fake.calls.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        fake.calls.lock().unwrap().len(),
        calls,
        "no calls after it ended"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_stop_on_a_background_run_still_gets_the_grace_period() {
    let fake = Arc::new(Fake::default());
    let mut ctx = ToolContext::for_test("/tmp".into());
    ctx.dispatch = Some(fake.clone());
    let session = ctx.session.clone();
    let tool = RunCodeTool::new(&[], limits());
    let started = tool
        .call(
            json!({"code": "let n = 0;
                while (!stopRequested()) { await tools.Echo({text: 'x', delay_ms: 20}); n++; }
                return {wrapped_up: n > 0};",
                   "background": true}),
            ctx,
            ProgressSender::noop("t"),
        )
        .await
        .unwrap();
    let task_id = text(&started)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(session.cancel_running_task(&task_id));
    let task = session.find_running_task(&task_id).unwrap();
    let wrapped_up = tokio::time::timeout(Duration::from_secs(5), async {
        while !task
            .output
            .lock()
            .unwrap()
            .contains("{\"wrapped_up\":true}")
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(wrapped_up.is_ok(), "{}", task.output.lock().unwrap());
}

#[test]
fn the_log_keeps_only_its_end_while_the_program_runs() {
    let mut log = ProgramLog::new(100);
    for i in 0..10_000 {
        log.push(&format!("第{i}行 of the log"));
    }
    assert!(log.text.len() <= 800, "{} bytes kept", log.text.len());
    let tail = log.tail();
    assert!(tail.contains("(last 100 characters)"), "{tail}");
    assert!(tail.ends_with("第9999行 of the log"), "{tail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn import_and_export_statements_get_the_no_modules_message() {
    let (r, _) = run("const a = 1;\nexport const b = a;\nreturn b;").await;
    assert!(r.is_error);
    assert!(
        text(&r).contains("line 2 uses import/export"),
        "{}",
        text(&r)
    );

    let (r, _) = run("const m = await import('fs'); return 1;").await;
    assert!(
        text(&r).contains("there are no modules here"),
        "{}",
        text(&r)
    );

    let (r, _) = run("const s = `first\nimport this line is text\n`; return s.length;").await;
    assert!(!r.is_error, "{}", text(&r));
    assert_eq!(r.structured_content, Some(json!(31)));
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_thrown_messages_and_oversized_results_are_cut() {
    let small = Limits {
        call_result_chars: 100,
        result_chars: 120,
        ..limits()
    };
    let (r, _, _) = run_with(
        "const out = {};
         try { await tools.Fail({text: 'e'.repeat(5000)}); } catch (e) { out.error = e.message.length; }
         out.data = typeof (await tools.Data({pad: 'p'.repeat(5000)}));
         return out;",
        json!({}),
        small.clone(),
    )
    .await;
    let v = r.structured_content.clone().expect("an object");
    assert!(v["error"].as_u64().unwrap() < 150, "{v}");
    assert_eq!(v["data"], "string", "too big to hand over as an object");

    let (r, _, _) = run_with("throw new Error('t'.repeat(5000));", json!({}), small).await;
    assert!(r.is_error);
    assert!(!text(&r).contains(&"t".repeat(121)), "{}", text(&r));
    assert!(text(&r).contains("cut at 120 characters"), "{}", text(&r));
}

#[tokio::test(flavor = "multi_thread")]
async fn serial_calls_keep_their_order_and_do_not_hold_slots_while_they_wait() {
    let (r, fake, _) = run_with(
        "await Promise.all([
            tools.Write({text: 'w1', delay_ms: 300}),
            tools.Write({text: 'w2', delay_ms: 10}),
            tools.Echo({text: 'r1', delay_ms: 50}),
            tools.Write({text: 'w3', delay_ms: 10}),
         ]);
         return 'ok';",
        json!({}),
        Limits {
            parallel: 2,
            ..limits()
        },
    )
    .await;
    assert!(!r.is_error, "{}", text(&r));
    let calls = fake.calls.lock().unwrap();
    let order: Vec<&str> = calls
        .iter()
        .map(|(_, input)| input["text"].as_str().unwrap())
        .collect();
    let at = |t: &str| order.iter().position(|o| *o == t).unwrap();
    assert!(at("w1") < at("w2") && at("w2") < at("w3"), "{order:?}");
    assert!(
        at("r1") < at("w2"),
        "the read ran while the writes queued: {order:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn notify_in_the_foreground_goes_to_the_log() {
    let (r, _, ctx) = run_with("notify('halfway there'); return 1;", json!({}), limits()).await;
    assert!(text(&r).contains("Log:\nhalfway there"), "{}", text(&r));
    assert!(ctx.session.take_notices().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_is_a_stub_that_says_what_to_use() {
    let (r, _) = run("return fs.readFileSync('a.txt');").await;
    assert!(r.is_error);
    assert!(
        text(&r).contains("fs is not available in RunCode"),
        "{}",
        text(&r)
    );
}

#[tokio::test]
async fn the_description_lists_call_shapes_and_the_guide_says_the_rest() {
    let tool = RunCodeTool::new(
        &[
            CallableTool {
                name: "Read".into(),
                description: "Read a file (text/image/PDF) with optional line range. More.".into(),
                input_schema: json!({"properties": {"file_path": {}, "limit": {}},
                                     "required": ["file_path"]}),
                result: Some("text".into()),
            },
            CallableTool {
                name: "mcp__x__do-it".into(),
                description: "Does it".into(),
                input_schema: json!({"properties": {"a": {}}}),
                result: None,
            },
        ],
        Limits::default(),
    );
    let d = tool.description();
    assert!(
        d.contains("tools.Read({file_path, limit?}) → text\n"),
        "{d}"
    );
    assert!(d.contains("callTool(\"mcp__x__do-it\", {a?})"), "{d}");
    assert!(d.contains("workflows or multi-agent orchestration"));
    assert!(
        !d.contains("stopRequested"),
        "the details belong to the guide"
    );

    let ctx = base::tool::PromptContext {
        cwd: "/tmp".into(),
        model: String::new(),
        session_id: String::new(),
        is_interactive: false,
        all_tool_names: vec![],
        allowed_agent_types: vec![],
        path_style: Default::default(),
    };
    let guide = tool.detailed_prompt(&ctx).await.expect("a guide");
    assert!(
        guide.contains("tools.Read({file_path, limit?}) → text — Read a file (text/image/PDF) with optional line range."),
        "{guide}"
    );
    assert!(guide.contains("callTool(\"mcp__x__do-it\", {a?}) — Does it"));
    assert!(guide.contains("stopRequested()"));
}
