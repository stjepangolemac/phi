---
name: planning
description: Create, maintain, resume, and finish a lightweight local plan for nontrivial multi-step work.
---

# Planning

Use `.phi/PLAN.md` as persistent execution state for nontrivial, multi-step work. Do not create a plan for a simple request that can be completed directly.

## Start or resume

Before planning new work, check for `.phi/PLAN.md`. If it exists, read it and resume from its stage, current task, blockers, and notes instead of reconstructing progress from conversation history. This is also how work resumes after a new session or compaction.

When starting a plan in a Git workspace, add the plan's repository-relative path to the repository-local exclude file reported by `git rev-parse --git-path info/exclude`. Do not modify `.gitignore` or another shared ignore file. Verify the plan is ignored before continuing.

Create a human-readable Markdown file with this shape:

```markdown
# Goal

What we are trying to accomplish.

**Stage:** planning

# Tasks

- [ ] **Current:** Inspect the relevant code and constraints.
- [ ] Implement the smallest coherent change.
- [ ] Validate and review the result.

# Blockers

- None.

# Notes

Important decisions and context needed to resume work.
```

When `context_mark` is available, mark the start of planning and the switch to execution with concise descriptive labels. The plan file remains the durable execution state; context markers only create optional compaction boundaries.

## Maintain

- Keep one stage for the plan as a whole: `planning`, `execution`, or `done`.
- Use `planning` while defining or materially revising the approach, `execution` while carrying it out, and `done` only when the whole plan is complete.
- Keep one flat task list. Do not add milestones, subtasks, dependency graphs, or user-visible stable task IDs.
- During `planning` or `execution`, mark exactly one unchecked task with `**Current:**`. Switch the marker when focus changes; tasks do not have their own plan stages.
- Check off completed tasks and update blockers or notes at meaningful checkpoints, not after every small action.
- Keep enough decisions, validation results, and resume context in the plan to continue without relying on the full conversation.
- When `context_mark` is available, use the same generic marker when the current task materially changes. Do not mark trivial steps or assume markers grant permissions.

## Finish

When all work and validation are complete, check off the remaining tasks, set the plan stage to `done`, record any final context needed for handoff, and then delete `.phi/PLAN.md`. Do not commit the plan.

When `context_mark` is available, a final marker may close the completed planning/execution span before handoff. Planning is only one producer of these generic boundaries; no active plan is required to use them.

This workflow is intentionally skill-first and uses normal file-reading and editing tools rather than a slash-command suite or machine-heavy schema. Its interaction model takes design inspiration from the planning workflows in OpenAI Codex and Claude Code.
