pub fn run() -> anyhow::Result<()> {
    eprintln!("\x1b[36;1mCommands:\x1b[0m");
    eprintln!("  /help, /h, /?     Show this help");
    eprintln!("  Trailing \\        Continue onto the next line (\\\\ does not)");
    eprintln!("  Ctrl+F            Add an instruction while the agent is running");
    eprintln!("  /clear            Clear conversation history");
    eprintln!("  /checkpoint       Save a file-history checkpoint");
    eprintln!("  /changes          Show tracked changes since the latest checkpoint");
    eprintln!("  /rollback         Roll tracked files back to the latest checkpoint");
    eprintln!("  /compact          Manually compact context");
    eprintln!("  /cost             Show token usage and cost");
    eprintln!("  /commit           Generate a git commit with AI message");
    eprintln!("  /review           AI code review of current changes");
    eprintln!("  /reviewer [name]  List reviewer models or switch the reviewer model");
    eprintln!("  /tools            List model-visible tools for coding and reviewer agents");
    eprintln!("  /effort [value]   Show or set effort: tokens or low, medium, high, max");
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
