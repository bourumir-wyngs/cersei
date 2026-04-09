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

## Network Sandboxing

Shell tools (Bash, Npm, Npx, Cargo, Process) run with network access **disabled by default**. The AI must explicitly request network access by setting `network: "full"` in the tool input, which triggers a user approval prompt.

Sandboxing is implemented via [firejail](https://firejail.wordpress.com/). Install it with:

```bash
sudo apt install firejail
```

When firejail is available, commands that don't request network run under `firejail --net=none`, blocking all outbound connections. When firejail is not installed, commands run unsandboxed (a warning is shown once).

### User approval prompt

When the AI requests `network: "full"`, you will see:

```
  Network access: Npm
  npm install react
  [Y]es  [N]o  [S]ession  [A]lways
```

- **Y** / Enter — allow network for this invocation
- **N** — block network (run sandboxed)
- **S** — allow for the rest of the session
- **A** — always allow for this tool

Pass `--no-permissions` to skip all prompts and allow everything (CI/headless mode).

# Bridge "sandbox" (local network access only)

## Create
This tool supports running applications withing the sandbox (multiple processes like backend server, database can run within the sandbox and see each other, but they otherwise do not see or are accessible even for/from local host. 

sudo ip link add sandbox type bridge
sudo ip addr add 10.200.1.1/24 dev sandbox
sudo ip link set sandbox up

# server will start at
http://10.200.1.2:3000

## Check
ip a show sandbox

## Run app on sandbox
firejail --net=sandbox node app.js

IMPORTANT: It is your responsibility to create the "sandbox" bridge before running cersei, and ensure it is 
configured correctly.

