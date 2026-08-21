# Release process

How to cut a release of `llm-quota-tray`. Follow this verbatim —
the CHANGELOG discipline and the `Cargo.toml` `version` field have
to stay in lockstep, or downstream users get a stale lockfile.

## TL;DR

```sh
# 0. Pre-flight
git checkout main && git pull --rebase
cargo test                       # all green?
cargo build --release            # builds clean?

# 1. Bump version + write the release entry
$EDITOR Cargo.toml               # bump [package].version
$EDITOR CHANGELOG.md             # rename [Unreleased] → [X.Y.Z] - DATE

# 2. Commit + tag
git add Cargo.toml CHANGELOG.md
git commit -m "release: vX.Y.Z"
git tag -s vX.Y.Z -m "vX.Y.Z"    # signed tag, see "Signing" below
git push origin main --follow-tags

# 3. Wait for CI to pass on the tag push, then
gh release create vX.Y.Z \
  --title "vX.Y.Z" \
  --notes-file <(sed -n '/^## \[X.Y.Z\]/,/^## \[/p' CHANGELOG.md | sed '$d')
```

That's it. The full process below explains what each step is doing
and the gotchas.

## Versioning rules

This project follows [Semantic Versioning 2.0](https://semver.org/spec/v2.0.0.html).
The `0.x` line is the pre-1.0 development track, where minor versions
may contain breaking changes (per semver §4). Until 1.0:

* **Patch bump (0.3.0 → 0.3.1)** — bug fixes only. No user-visible
  schema change, no example-template change, no provider-template
  shape change. Examples that qualify: a keyring fix, a stale-on-
  error regression, an RSS-guard trigger.
* **Minor bump (0.3.0 → 0.4.0)** — anything user-visible. New config
  fields with defaults are usually OK here (additive change); a new
  `AuthConfig` variant counts too. Any new provider template in
  `examples/providers/` is minor-bump territory.
* **Major bump (0.x → 1.0)** — explicitly promised stable config
  schema and stable CLI flags. Not there yet.

When you do introduce a breaking config-file change, **don't** bump
the major version in the pre-1.0 line. Use a minor bump and call it
out under `### Changed` with a `⚠️ breaking` prefix, like the v0.3.0
CHANGELOG entry does for the `minimax-quota-tray → llm-quota-tray`
rename.

## CHANGELOG discipline

The CHANGELOG follows [Keep a Changelog 1.1](https://keepachangelog.com/en/1.1.0/).
The structure under each version heading is fixed:

* `### Added` — new user-visible features
* `### Changed` — changes to existing behavior
* `### Deprecated` — soon-to-be-removed features
* `### Removed` — features removed this release
* `### Fixed` — bug fixes
* `### Security` — security fixes (cross-reference SECURITY.md if
  there's a CVE)

Each bullet should be **one line + sub-bullets with the commit
SHAs** that introduced the change. The v0.3.0 entry is the template:
it groups related commits (`4e93fad`, `13dc789`) on a single
one-line item with a sub-bullet for the migration note.

To find the commits for the next release:

```sh
# All commits since the last tag, oneline, with SHAs:
git log v0.3.0..HEAD --oneline

# Group by area:
git log v0.3.0..HEAD --oneline -- src/keyring.rs
git log v0.3.0..HEAD --oneline -- examples/providers/
```

## Tag signing

The repo uses **signed tags** so users can verify the release came
from you. If you don't already have a signing key set up:

```sh
# One-time setup
gpg --full-generate-key               # RSA, 4096-bit, your @dtg01100.dev email
git config --global user.signingkey <KEY-ID>
git config --global tag.gpgSign true

# Per-tag
git tag -s vX.Y.Z -m "vX.Y.Z"
```

The `git push --follow-tags` above will refuse to push unsigned tags
by default once `tag.gpgSign true` is set. If you haven't configured
signing and you don't want to, just push the tag explicitly without
`-s`:

```sh
git tag vX.Y.Z -m "vX.Y.Z"
git push origin vX.Y.Z
```

…but add "configure GPG signing" to your TODO. Unsigned tags are a
real friction point for downstream packagers.

## CI on the tag push

`.github/workflows/ci.yml` runs on every push to `main` and every
PR. The tag push itself does **not** trigger a separate workflow —
it re-runs the `main` workflow against the tagged commit. Before
you push the tag:

```sh
# Make sure the release commit passes locally
cargo test
cargo build --release
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

If the CI run on the tag push fails (e.g. someone merged something
broken between your local test and the push), **delete the tag
locally and remotely, fix the breakage, re-tag**:

```sh
git tag -d vX.Y.Z
git push origin :refs/tags/vX.Y.Z
# fix the breakage, recommit
git tag -s vX.Y.Z -m "vX.Y.Z"
git push origin main --follow-tags
```

## GitHub release notes

The `gh release create` line in the TL;DR pulls the section out of
the CHANGELOG automatically. The `sed` pipeline extracts from
`## [X.Y.Z]` to the next `## [` heading and strips the trailing one.
The output lands in the release body — that's what users see in the
GitHub UI and what `cargo update` notification emails link to.

After the release goes up, double-check:

* The release is marked "Latest" on the repo's Releases tab.
* The `target` binary in the release notes points at the right
  architecture. This project doesn't ship prebuilt binaries — the
  install flow is `git clone && ./install.sh` — so there's nothing
  to attach unless you want to start.

## Cargo.lock policy

`Cargo.lock` **is** committed (it's a binary crate with a CLI
contract, not a library). When you bump a dep version in the lock
file, the version bump goes in `### Changed` if it's user-visible
(a fix in a transitive dep that surfaces in the tray's behavior) or
stays out of the CHANGELOG entirely if it's invisible (an indirect
dep with no behavior change).

## Post-release sanity check

After tagging, run the v0.3.0 verification drill — same one the
CHANGELOG has been audited against:

```sh
# Fresh checkout, from scratch
git clone https://github.com/dtg01100/llm-quota-tray.git /tmp/release-check
cd /tmp/release-check
./install.sh

# Should compile, install, and bring up a tray icon.
# Click chip → "Set API Key…" should accept a key and round-trip
# it via secret-tool.

# Verify release binary size is still in the documented ~5.5 MB band
ls -lh ~/.local/bin/llm-quota-tray
```

If anything regresses, fix it and cut a patch release (0.X.1)
rather than trying to amend the tag.

## When to skip the release entirely

Some weeks don't warrant a release. If the only `main` commits
since the last tag are doc fixes, CI workflow tweaks, or
changelog-formatting cleanups, do **not** cut a release just to
have a tag — bump nothing, merge to `main`, and wait for the next
real change. The `[Unreleased]` section in CHANGELOG can sit empty
for as long as it needs to.
