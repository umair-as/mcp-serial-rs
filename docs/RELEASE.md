# Release Process

This project releases via a single manually-dispatched GitHub Actions
workflow (`.github/workflows/release.yml`) that checks, builds, tags, and
publishes in one run — no separate release branch or tag-push trigger.

## 1. Version bump

Bump `version` in `Cargo.toml` to the target `X.Y.Z` (see "Choosing the
version bump" below). No dedicated process here — bundle it into the last
feature PR going into the release if one's still open, or push it as its own
small commit/PR if not.

## 2. Refresh CHANGELOG.md

`CHANGELOG.md` is generated from git history via
[git-cliff](https://github.com/orhun/git-cliff) (`cliff.toml`) and committed
alongside the version bump — regenerate it before merging, passing the
version being released so the section is labeled `[X.Y.Z]` rather than
`[Unreleased]` (the tag itself doesn't need to exist yet for this):

```sh
GITHUB_REPO=umair-as/mcp-serial-rs git-cliff --config cliff.toml --tag vX.Y.Z -o CHANGELOG.md
```

The release workflow uses the same `cliff.toml` to generate the GitHub
Release body, so keeping `CHANGELOG.md` in sync is just re-running this
locally before the version-bump PR. To preview only the section for the
release you're about to cut, without touching the file:

```sh
GITHUB_REPO=umair-as/mcp-serial-rs git-cliff --config cliff.toml \
  --unreleased --tag vX.Y.Z --strip header
```

Commit subjects drive classification (features/fixes/security/docs/etc — see
`cliff.toml`'s `commit_parsers`); keep using Conventional Commit prefixes
(`feat:`, `fix:`, `docs:`, `test:`, `ci:`/`build:`, `chore:`, `style:`) as
this project already does. A `feat!:`/`fix!:` subject (or any `type!:`)
surfaces under a dedicated "Breaking Changes" section.

## 3. Pre-release checks (see SECURITY.md)

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `cargo build --release`
- `cargo audit`
- `cargo deny check bans licenses sources`

CI (`ci.yml`, `dependency-security.yml`) runs all of these on every PR; the
release workflow re-runs fmt/clippy/test/build as a final gate before
tagging, so a green PR merge doesn't skip verification at release time.

## 4. Run the release workflow

Merge the version-bump PR to `main`, then dispatch `release.yml` (Actions →
release → Run workflow) with:

- `version`: the same `X.Y.Z` as `Cargo.toml` — the workflow refuses to run
  if they don't match, and refuses if the `vX.Y.Z` tag already exists.
- `prerelease`: `true` for a release candidate, `false` otherwise.

The workflow validates the version, re-runs the full check suite, builds the
release binary, packages
`mcp-serial-rs-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` + its `.sha256`,
generates the GitHub Release notes from `cliff.toml` via git-cliff, and
creates the `vX.Y.Z` tag plus the GitHub Release with the binary attached.

## Choosing the version bump

This project is pre-1.0 (`0.y.z`); treat the minor component as the
meaningful boundary:

- **Patch** (e.g. `0.2.0` → `0.2.1`): bug fixes only — no new tool
  params/fields, no behavior change for an existing caller.
- **Minor** (e.g. `0.2.0` → `0.3.0`): new backward-compatible
  functionality — a new optional field, a new tool, a new opt-in behavior.
  This has been every release to date.
- **Major**: reserved for `1.0.0`, or a genuine breaking change to an
  existing tool's params/output shape or default behavior. Hasn't happened
  yet.
