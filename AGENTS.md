# Agent Guidelines: Living Documentation Maintenance

## Continuous Design Alignment

When implementing code changes, refactoring, or making commits, you are required to keep `DESIGN.md` in sync with the codebase.

### Requirements:

1. **Sync Decisions:** Immediately update `DESIGN.md` whenever architectural, design, or implementation decisions are made, altered, or clarified.
2. **Correct Outdated Specs:** If existing text in `DESIGN.md` is found to be incorrect, outdated, or implemented differently than originally specified, edit it to accurately reflect the new reality.
3. **Commit Together:** Include updates to `DESIGN.md` in the same commit (or pull request) as the code changes that prompted them. Never leave design documentation updates for a separate follow-up task.
