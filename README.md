# Cersei

This is a fork of Cersei featuring additional tools. The new tools are 
- web development tools that launch the browser (essentially on a local host) and communicate with it, obtaining DOM, CSS, console, network, and other browser-related information.
- database tools (MySQL and PostgreSQL) that can be configure for safe read-only operation (default), but otherwise provide access to database.
- safe read only git tool for read only git operations. 

The number of supported agents are limited (use /model without parameters to see what is available and /model <name> to switch). From the other side, support for available models is currently maintained. You need to define your API keys as system properties.

While still can be a library, the crate is now extended to provide the own main.rs. The tools are configured via tools.yaml that must be present in the folder where the application starts.

It is your responsibility to watch what the agents are doing and approve what is expected only (read the license for details).

## Project Instructions

You can brief the AI on startup using instruction files placed in the working directory:

- **`AGENTS.md`** — A project-level briefing read from the working directory on startup. Use this to describe the project, its conventions, and any guidance relevant to AI agents (compatible with the OpenAI Codex `AGENTS.md` convention).
- **`.abstract/instructions.md`** — Project-specific instructions for this tool. Run `cersei init` to create a template. Suitable for tool-specific preferences and constraints.

Both files are injected into the system prompt as cached sections, so they are available throughout the session without consuming repeated token budget.