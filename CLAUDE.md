# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Releasing to crates.io — use the pipeline, not local commands

Releases are done **entirely by the CI pipeline**, never by hand. Do **not** run
`cargo publish`, and do **not** create or push the `vX.Y.Z` tag manually — the
pipeline does both, and a pre-existing tag makes it fail.

**Tests run on the pipeline.** Don't re-run the full test suite locally before a
release just to gate it; CI (`ci.yml`) already covers that.

### Release steps

1. Bump `version` in `Cargo.toml`, update `CHANGELOG.md`, commit, and push to `main`.
2. Dispatch the publish workflow:
   ```
   gh workflow run publish-crates-io.yml --ref main
   ```
   (Add `-f dry_run=true` to validate without tagging/releasing/publishing.)
3. The workflow (`.github/workflows/publish-crates-io.yml`, `workflow_dispatch`
   only) then, on its own:
   - derives the tag `v<version>` from `Cargo.toml`,
   - **fails preflight if that tag already exists on origin** — so never pre-create it,
   - creates the git tag + GitHub Release (auto-generated notes),
   - runs `cargo publish`.

To cut a new release, bump the version — the pipeline owns the tag.
