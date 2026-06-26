# Workflow Rules

## Plan Mode
- Enter plan mode for non-trivial tasks (3+ steps or architectural decisions)
- If something goes sideways, STOP and re-plan immediately
- Write detailed specs upfront to reduce ambiguity

## Subagent Strategy
- Use subagents liberally to keep main context window clean
- Offload research, exploration, and parallel analysis to subagents
- One task per subagent for focused execution

## Self-Improvement
- After ANY correction from the user: update `tasks/lessons.md` with the pattern
- Write rules for yourself that prevent the same mistake
- Review lessons at session start

## Verification
- Speed above all. Ship fast, don't block on perfect correctness.
- Capture failures in unit tests and fix them as they arise.
- Diff behavior between main and your changes when relevant.

## Autonomous Bug Fixing
- When given a bug report: just fix it. Don't ask for hand-holding.
- Point at logs, errors, failing tests — then resolve them.
- Zero context switching required from the user.

## Task Management
1. Plan first: write plan to `tasks/todo.md` with checkable items
2. Track progress: mark items complete as you go
3. Explain changes: high-level summary at each step
4. Document results: add review section to `tasks/todo.md`
5. Capture lessons: update `tasks/lessons.md` after corrections
