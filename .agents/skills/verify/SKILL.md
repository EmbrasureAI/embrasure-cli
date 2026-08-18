---
name: verify
description: Run Embrasure's local verification loop for dbt changes. Use when implementing, debugging, or reviewing dbt models, tests, macros, seeds, snapshots, or SQL. Check against the PR base, fix every finding or gap, and rerun until clean.
---

# Verify

Run from the dbt project root after making changes.

## Loop

1. Use the PR target as the base. Default to `origin/main` when no other target is known.
2. Run:

   ```sh
   embrasure check --base origin/main --json
   ```

3. Handle the exit code:
   - `0`: stop; the check passed.
   - `1`: fix every finding and rerun.
   - `2`: resolve or explain every coverage gap. Do not call this a pass.
   - `3`: fix the setup, execution, or cleanup error. Run `embrasure doctor` for configuration or access problems.
4. Repeat until the full check exits `0`.

Use `--select <model> --downstream none` or `--mode quick` during iteration. Finish without those flags so Embrasure validates the complete downstream path. A dry run is only a preview.

## Guardrails

- Fix the root cause. Do not weaken tests, thresholds, coverage, or critical-model settings just to pass.
- Confirm that changed business behavior matches the request.
- Do not run ad hoc writes against production.
- If credentials, permissions, or business intent block a safe fix, stop and report the blocker.

## Report

Report the base, fixes, models validated, downstream impact, and remaining gaps. Use the final Embrasure result as evidence.
