# Agent Hooks

Python scripts invoked by Claude Code and Codex hooks. Claude Code loads
`.claude/settings.json`; Codex loads `.codex/hooks.json`.

All hooks are executed via `uv run` to ensure consistent Python environment.
