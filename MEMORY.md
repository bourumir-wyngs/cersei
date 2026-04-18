# Cersei Memory System & Graph Memory

This document explains the Cersei memory architecture, specifically the **Graph Memory** system, and the tools available to agents for interacting with it.

## The 3-Tier Memory Architecture

Cersei provides context to agents using a multi-layered approach:
1. **Tier 1: Graph Memory (Grafeo)** - An active, embedded graph database (typically `~/.abstract/{project}_graph.db`, for example `~/.abstract/cersei_graph.db`). It uses the same project naming as persisted permissions; `<project>` defaults to the folder name where `cersei` started, and `--project NAME` overrides it. This stores structured, relationship-aware facts that can be queried dynamically.
2. **Tier 2: Flat-file Memory (`memdir`)** - Markdown files representing specific topics, loaded into the prompt or used as a fallback.
3. **Tier 3: Static Prompt Context** - Static files like this `MEMORY.md` and `CLAUDE.md` hierarchy, which are injected directly into the agent's system prompt via `MemoryManager::build_context()`.

## What is Graph Memory and How Does it Work?

Graph Memory allows the agent to retain and retrieve precise knowledge across different sessions dynamically. Instead of dumping all historical context into the system prompt (which wastes tokens and dilutes attention), facts are stored as a graph:

- **Nodes**: 
  - `Memory`: The actual stored fact or observation.
  - `Topic`: Categories or tags applied to memories.
  - `Session`: The execution session in which the memory was created.
- **Edges**: Relationships (e.g., a memory is tagged with a topic, or memory A is linked to memory B).

When `MemoryManager::recall(query)` is invoked, the system queries the Grafeo backend. It matches the query against stored nodes, traversing edges to find related context. If the graph backend is disabled (via config or compile flags) or yields no matches, the manager gracefully falls back to text-scanning the Tier 2 flat-file directory.

## Agent Memory Tools

Agents can now actively interact with the Graph Memory using two primary tools:

### 1. `MemoryStore`
Used to durably record new, important information that should persist across sessions.
- **Parameters**:
  - `content` (string): The fact, rule, or observation to store (e.g., "The integration tests require Redis to be running on port 6379").
  - `memory_type` (string): The category of the memory (`project`, `user`, `reference`, `feedback`). Defaults to `project`.
  - `confidence` (number): Float between 0.0 and 1.0 indicating certainty.
- **When to use**: 
- **Permissions**: Always available; does not require permission approval.
  - Upon discovering a tricky workaround or undocumented project quirk.
  - When the user states a formatting preference or operational constraint.
  - After identifying the root cause of a recurring bug.
- **When NOT to use**: Do not store ephemeral session state (e.g., "I am currently editing file X" or "I just ran cargo check").

### 2. `MemoryRecall`
Used to query the graph for past context related to the current task.
- **Parameters**:
  - `query` (string): The search string or keywords (e.g., "redis integration test").
  - `limit` (integer): Maximum number of results to return (default 5).
- **When to use**: 
- **Permissions**: Always available; does not require permission approval.
  - When starting a new, complex task to check for established conventions.
  - When encountering an obscure error message.
  - When looking up a user's known preferences before formatting output.

## Best Practices for Agents

- **Query before you guess**: If a test fails with a strange environment error or a command isn't working as expected, use `MemoryRecall` to see if a previous session already documented the solution.
- **Store high-signal facts**: Keep Graph Memory clean by using `MemoryStore` only for "gotchas", persistent architectural rules, and explicit user instructions.
- **Self-Correction**: If you make a mistake and the user corrects you, use `MemoryStore` to remember the correction so future agents do not repeat the same mistake.
