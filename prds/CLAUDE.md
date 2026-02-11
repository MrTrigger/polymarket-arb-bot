# Ralph Agent Instructions

You are an autonomous coding agent running in a loop.

## Your Task

1. Read `prd.json` in this directory
2. Read `progress.txt` - check Codebase Patterns section FIRST
3. Check you're on correct branch from `branchName`. Create from main if needed.
4. Pick highest priority story where `passes: false`
5. Implement that ONE story
6. Run quality checks (typecheck, lint, test)
7. If checks pass, commit: `feat: [Story ID] - [Story Title]`
8. Update prd.json: set `passes: true` for completed story
9. Append progress to `progress.txt`
10. Update CLAUDE.md files if you discover reusable patterns

## Progress Report Format

APPEND to progress.txt:

```
## [Date/Time] - [Story ID]
- What was implemented
- Files changed
- **Learnings for future iterations:**
  - Patterns discovered
  - Gotchas encountered
  - Useful context
---
```

## Consolidate Patterns

Add reusable patterns to `## Codebase Patterns` at TOP of progress.txt:

```
## Codebase Patterns
- Use X pattern for Y
- Always do Z when changing W
```

## Update CLAUDE.md Files

When you discover reusable knowledge about a directory:
1. Check for existing CLAUDE.md in that directory
2. Add patterns, gotchas, conventions
3. Do NOT add story-specific details

## Stop Condition

After completing a story, check if ALL stories have `passes: true`.

If ALL complete: output `<promise>COMPLETE</promise>`
If not: end normally (next iteration continues)
