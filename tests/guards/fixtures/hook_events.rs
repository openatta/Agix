fn fire(runner: &Runner) {
    runner.run(HookEvent::StopFailure, ());
    if runner.has_hooks_for(HookEvent::Stop) {}
    tokio::select! {
        _ = runner.run(HookEvent::PostToolUseFailure, ()) => {}
    }
}
