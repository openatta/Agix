//! `agix-plugin-compile` — command-line front end for [`plugin_compiler`].

use anyhow::Result;

fn main() {
    if let Err(e) = run(std::env::args().skip(1).collect()) {
        eprintln!("agix-plugin-compile: {e:#}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<()> {
    let Some(dir) = args.first() else {
        anyhow::bail!("usage: agix-plugin-compile <plugin-directory>");
    };
    let dir = std::path::PathBuf::from(dir);
    let compiled = plugin_compiler::compile_plugin(&dir)?;

    // Nothing to compile is a fine outcome — a plugin may be MCP-only — but
    // it should be visible, because "compiled 0 components" and "compiled the
    // one you expected" look identical in a log that only reports success.
    println!(
        "compiled {} component(s) in {}",
        compiled.len(),
        dir.display()
    );
    for path in compiled {
        println!("  {}", path.display());
    }
    Ok(())
}
