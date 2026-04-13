pub fn run() -> anyhow::Result<()> {
    eprintln!("\x1b[36;1mCommands:\x1b[0m");
    eprintln!("  /help, /h, /?     Show this help");
    eprintln!("  Trailing \\        Continue onto the next line (\\\\ does not)");
    eprintln!("  /clear            Clear conversation history");
    eprintln!("  /compact          Manually compact context");
    eprintln!("  /cost             Show token usage and cost");
    eprintln!("  /commit           Generate a git commit with AI message");
    eprintln!("  /review           AI code review of current changes");
    eprintln!("  /memory, /mem     Show memory status");
    eprintln!("  /model [name]     List Anthropic/OpenAI/Google/xAI models or switch immediately");
    eprintln!("  /config [key val] Show or set config");
    eprintln!("  /diff             Show git diff");
    eprintln!("  /resume [id]      Resume a previous session");
    eprintln!("  /save <name>      Save current session under a named file");
    eprintln!("  /delete, /del     Delete a session by id");
    eprintln!("  /exit, /quit, /q  Exit");
    Ok(())
}
