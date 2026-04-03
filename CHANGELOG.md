# Changelog

## [0.1.1] - 2026-04-03

### Added

- **Schema versioning and migration engine** (`graph_migrate.rs`). Graph databases now store a `(:SchemaVersion)` node. On open, the code checks the version and runs sequential migrations automatically. Each migration is idempotent.
- **Confidence decay**. Memory nodes track `last_validated_at` and `decay_rate`. `effective_confidence()` computes time-decayed confidence at read time — old memories lose weight without manual cleanup.
- **Embedding readiness**. Memory nodes include `embedding_model_version` (empty by default) preparing for future vector-based semantic recall.
- **`revalidate_memory(id)`** resets the decay clock on a memory node.
- **`schema_version()`** on `GraphMemory` for inspecting the current graph version.
- **Centralized GQL queries**. All 15+ scattered query strings in `graph.rs` extracted into a `mod gql` block.

### Changed

- `GraphMemory::open()` and `open_in_memory()` now auto-detect schema version and migrate on startup. No API change — MemoryManager callers are unaffected.
- `store_memory()` writes v2 fields (`last_validated_at`, `decay_rate`, `embedding_model_version`) on new nodes. Public signature unchanged.
- Default provider in Abstract CLI changed from `anthropic` to `auto` (detects from environment variables).
- `README.md` updated with Abstract CLI section, three-way benchmark table, and docs link.

### Fixed

- Empty `ANTHROPIC_API_KEY` environment variable no longer treated as valid auth.
- `run_tool_bench_claude.sh` (renamed from `run_tool_bench.sh`) — `grep` recall measurement no longer fails silently on no match.

## [0.1.0] - 2026-04-02

Initial release.

### Core SDK

- **cersei-types**: Provider-agnostic types — `Message`, `ContentBlock`, `Usage`, `StopReason`, `StreamEvent`, `CerseiError`.
- **cersei-provider**: `Provider` trait with Anthropic and OpenAI implementations. SSE streaming, token counting, prompt caching, extended thinking, OAuth support.
- **cersei-tools**: `Tool` trait, 34 built-in tools across 7 categories (filesystem, shell, web, planning, scheduling, orchestration, other). `#[derive(Tool)]` proc macro. Permission system with 6 levels. Bash command safety classifier. Skill discovery (Claude Code + OpenCode formats). Shell state persistence across invocations.
- **cersei-agent**: `Agent` builder with 20+ configuration options. Agentic loop with tool dispatch and multi-turn conversations. 26-variant event system (`AgentEvent`). `AgentStream` with bidirectional control. Auto-compact at configurable context threshold. Effort levels (Low/Medium/High/Max). Sub-agent orchestration. Coordinator mode. Auto-dream background consolidation. Session memory extraction.
- **cersei-memory**: `Memory` trait with `JsonlMemory` and `InMemory` backends. `MemoryManager` composing 3 tiers: Grafeo graph DB, flat files (memdir), CLAUDE.md hierarchy. Session storage via append-only JSONL with tombstone soft-delete. Auto-dream 3-gate consolidation system.
- **cersei-hooks**: `Hook` trait for lifecycle middleware. Pre/post tool use, model turn events. `ShellHook` for external command integration.
- **cersei-mcp**: MCP client over JSON-RPC 2.0 stdio transport. Tool discovery, resource enumeration, environment variable expansion.
- **cersei**: Facade crate re-exporting all sub-crates via `prelude::*`.

### Graph Memory

- Grafeo embedded graph database with 3 node types (`Memory`, `Session`, `Topic`) and 2 edge types (`RELATES_TO`, `TAGGED`).
- Graph recall in 98 microseconds (indexed lookup vs file-by-file text scan).
- Graph ON adds zero overhead to scan and context building, 92.5% faster recall.

### Benchmarks

- Tool dispatch: Edit 0.02ms, Read 0.09ms, Grep 6ms, Bash 17ms.
- Memory scan: 1.2ms for 100 files.
- Session I/O: 27us write, 268us load (100 entries).
- Context build: 45us (CLAUDE.md + MEMORY.md).

### Examples

- `simple_agent`, `custom_tools`, `streaming_events`, `multi_listener`, `resumable_session`, `custom_provider`, `hooks_middleware`, `benchmark_io`, `usage_report`, `coding_agent`, `oauth_login`.
- 5 stress test suites: core infrastructure, tools, orchestration, skills, memory. 160 unit tests, 262 stress checks.

### Documentation

- 10 markdown guides covering getting started, providers, tools, agent lifecycle, events, memory, hooks, permissions, architecture, benchmarks.
