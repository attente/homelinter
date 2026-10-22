# Agent Guidelines: Living Documentation Maintenance

## Continuous Design Alignment

When implementing code changes, refactoring, or making commits, you are required to keep `DESIGN.md` in sync with the codebase.

### Requirements:

1. **Sync Decisions:** Immediately update `DESIGN.md` whenever architectural, design, or implementation decisions are made, altered, or clarified.
2. **Correct Outdated Specs:** If existing text in `DESIGN.md` is found to be incorrect, outdated, or implemented differently than originally specified, edit it to accurately reflect the new reality.
3. **Commit Together:** Include updates to `DESIGN.md` in the same commit (or pull request) as the code changes that prompted them. Never leave design documentation updates for a separate follow-up task.

## Cache Clearing And Invalidation

The persistent metadata cache is user data: every entry costs provider requests to rebuild, so losing it wastes time, money, and rate limit and changes results the user already reviewed. Treat clearing it and invalidating it wholesale as destructive operations.

### Requirements:

1. **Explicit confirmation first:** Before clearing the cache (the service `DELETE /cache` path, the TUI `C` key, a CLI cache-clear option) or before any change that invalidates existing entries, stop and ask the user for explicit confirmation, stating what will be lost and what rebuilding costs. Do not assume approval from the surrounding task.
2. **Invalidation means the cache key:** A change invalidates the cache when it alters the cache key algorithm or its inputs: the `(kind, provider_id, provider_settings_hash, input_hash)` tuple or its serialization, the fields that feed `provider_settings_hash` (for example `description.prompt` or `geocoder.neighbourhood_cities`, and any execution-only setting newly moved into the key), the content-identity or coordinate inputs, or a schema/version bump that drops entries. Never make such a change as a side effect of an unrelated task.
3. **No implicit clearing:** Do not clear or invalidate the cache from ordinary save, scan, configuration, startup, or test paths that touch a real user cache; development and tests must use an isolated cache location.
4. **Document it:** Record the confirmed behavior in `DESIGN.md` ("Cache Implementation", "Cache Key Construction") in the same commit as the code change that clears or invalidates entries.
