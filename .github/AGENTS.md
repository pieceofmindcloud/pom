# AGENTS.md - .github

The workflows in `workflows/` publish the Linux, macOS and Windows packages.
Linux (`ksia`) uses a persistent self-hosted runner. macOS has both the original
GitHub-hosted workflow and a self-hosted copy for `macbook`; Windows remains
GitHub-hosted. Keep both macOS workflows' build and package steps aligned; each
DMG includes the plain-text `packaging/macos/README.txt` guide. The Linux and
both macOS source builds go through the composite action `actions/llama-cache`,
which resolves the current SHA of the
`pom-tensor-parallel` branch, restores the
checkout by SHA and validates the clone before Cargo uses it. The full strategy,
including `sccache`, lives in `docs/subsystems/build-and-run.md`, section "Cache in GitHub Actions".

When changing the cache strategy, keep invalidation separated by system,
architecture and fork SHA. Changes to the lockfile must keep composing the
Cargo cache key in the Windows workflow. Do not trigger a release to test
convenience changes without the operator's explicit authorization.

## Maintaining this file

Keep this file as a short map of the CI automation. The YAML and the build
documentation are the source of truth; do not replicate here commands or details
that already live in those files.