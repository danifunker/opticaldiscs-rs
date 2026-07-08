# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Pipelines — two of them

- **`ci.yml` (CI)** runs on **every branch push and pull request**: build + test
  (`--all-features` and no-features) across Linux/macOS/Windows, plus clippy, fmt,
  and docs. It is also **reusable** (`workflow_call`).
- **`publish-crates-io.yml` (Release)** is **manual** (`workflow_dispatch`) and
  gates on the CI pipeline before doing anything.

**Tests run on the pipeline.** Don't re-run the full test suite locally before a
release just to gate it; CI already covers that.

## Releasing to crates.io — use the pipeline, not local commands

Releases are done **entirely by the Release pipeline**, never by hand. Do **not**
run `cargo publish`, and do **not** create or push the `vX.Y.Z` tag manually — the
pipeline does both, and a pre-existing tag makes it fail.

### Release steps

1. Bump `version` in `Cargo.toml`, update `CHANGELOG.md`, commit, and push.
2. Dispatch the Release workflow:
   ```
   gh workflow run publish-crates-io.yml --ref main
   ```
3. The workflow reads the tag `v<version>` from `Cargo.toml`, **fails if that tag
   already exists on origin** (so never pre-create it), creates the git tag +
   GitHub Release (auto-generated notes), and runs `cargo publish`.

### Release modes (dispatch inputs)

The Release workflow has three modes, all of which first run the full CI build on
all OSes:

| Input | Effect |
|---|---|
| *(none)* | Full release: build gate → tag from `Cargo.toml` → publish to crates.io |
| `-f build_only=true` | Only verify the build on all OSes; **no** tag, **no** publish |
| `-f dry_run=true` | Build gate + `cargo publish --dry-run`; **no** tag, **no** publish |

To cut a new release, bump the version — the pipeline owns the tag.
