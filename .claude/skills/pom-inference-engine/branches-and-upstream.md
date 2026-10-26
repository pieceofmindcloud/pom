# Branches in the engine fork, and proposals upstream

## Remotes and branches

| Name | What it is |
|---|---|
| `upstream/<default>` | The official engine. Read only. |
| `origin` | The fork. Everything is pushed here. |
| `feature/<capability>` | One engine capability, branched from `upstream/<default>`, never from `pom`. |
| `fix/<defect>` | One defect, same rules. |
| `pom-glue/<topic>` | A change that cannot be made generic: merged into `pom`, never proposed upstream. Prefer the plugin's bridge. |
| `pom` | Integration: `upstream/<default>` plus every branch above, merged with `--no-ff`. `engine.lock` pins a `pom` commit. |

Follow the official naming convention when there is one (`CONTRIBUTING`,
recent merged PR branches).

## Rules for a feature or fix branch

- One capability per branch, with the engine's own tests and docs, in the
  engine's style (commit prefixes, formatting, test layout).
- Neutral: commits and diff name the capability, never the POM, its plugins,
  people, machines or local paths. A POM-specific value becomes a generic
  option (a metadata key prefix, a port, a path) the plugin sets at run time.
- `bash scripts/check-neutral.sh <branch>` passes before the merge into `pom`
  (`NEUTRAL_EXTRA_TERMS` adds machine and people names).
- No merge commits and no temporary diagnostics on the branch.

## Integrating

```bash
git switch pom
git merge --no-ff feature/<capability>
git push origin feature/<capability> pom
tag="pom-$(date +%Y%m%d)-$(git rev-parse --short HEAD)"
git tag "$tag" && git push origin "$tag"
```

Then move `ENGINE_COMMIT` in the plugin's `engine.lock`, in its own commit.
The tag keeps every pinned commit reachable. When the official branch moves,
merge `upstream/<default>` into `pom` (never rewrite `pom`) and rebase each
open feature branch on it (force-push only those).

## Proposing upstream (only when the operator asks)

For each branch the operator names, confirming before each outward step:

1. Rebase on the latest `upstream/<default>`, run the engine's tests, rerun
   `check-neutral.sh`.
2. Follow the official `CONTRIBUTING`, PR template, CLA and AI-disclosure rules.
3. Describe it in the engine's terms: problem, design, tests, numbers for
   speed. No POM context.
4. `gh pr create --repo <upstream owner>/<repo> --head <fork owner>:<branch>
   --base <default> --draft` unless a ready PR is asked for.
5. Report the URLs; answer review on the same branch; once merged, merge
   `upstream/<default>` into `pom` and drop the branch.
