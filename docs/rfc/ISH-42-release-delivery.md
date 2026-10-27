# ISH-42: Release delivery and diagnostics

Status: accepted; Linux implementation verified (2026-10-22); native macOS panic probe passed (2026-10-23)

## Decision

Publish only from the repository's default branch, publish SHA-256 checksums for
the final release archives, verify downloaded executable tools before use, and
retain panic diagnostics in release builds. The small size benefit of silent
panics does not justify losing the evidence needed to fix a broken shell.

The following requirements apply to changes in
[`release.yml`](../../.github/workflows/release.yml),
[`check-refinement.py`](../../scripts/check-refinement.py),
[`Makefile`](../../Makefile), and [`Dockerfile`](../../Dockerfile). This decision
retains the implementation and acceptance requirements below.

## Ref and publication policy

- Add a preflight job that **fails with an explicit diagnostic** unless the
  event is `workflow_dispatch` and `github.ref` is
  `refs/heads/${{ github.event.repository.default_branch }}`. The current default
  branch is `master`; derive it from repository metadata rather than hardcoding
  it. Reject tag and feature-branch dispatches before builds or write-permission
  jobs run. A skipped job that leaves a green run is not a sufficient guard.
- Build and test the dispatch's immutable `github.sha`, and publish
  `release-<full-sha>` with that exact target. Advancement of the default branch
  during a build does not change the release target. Preserve the existing
  target matrix and all its build/test/link verification gates.
- Keep top-level `permissions: {}`. Preflight/build need at most
  `contents: read`; remove build's unused `actions: write`. Only the publishing job gets
  `contents: write`, plus the `actions: read` needed for artifact retrieval.
- Check the guard again before publication, require every build to succeed,
  and validate the exact expected artifact set. Missing, duplicate, or extra
  binaries must fail before any release is created. Download only artifacts
  from this run; do not accept an arbitrary run ID or artifact URL.
- An existing release/tag is an explicit failure requiring inspection. Never
  force-move SHA-named tags or silently overwrite existing assets. The guard
  prevents accidental dispatches; it does not defend against a maintainer who
  can edit the workflow itself.

[GitHub's manual-workflow documentation](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow)
explicitly permits selecting other branches, so the dispatch trigger alone
does not enforce this policy.

## Artifact and tool integrity

After packaging the five expected ZIPs (one macOS archive and dynamic/static
archives for each Linux architecture), generate a deterministically sorted
`SHA256SUMS` containing their exact basenames and hashes. Verify it locally
before publishing, then attach it alongside those ZIPs. Hash the final bytes
after all signing, copying, and packaging. Document both `sha256sum -c` and the
macOS `shasum -a 256 -c` verification commands in the release/development guide.
Checksums detect incorrect bytes relative to that manifest; they do not add a
separate signing authority or prove the build is reproducible.

Pin the Aeneas release and rustup installer version, with separate reviewed
SHA-256 values for each supported architecture and artifact. Change the
refinement script's rustup URL from the moving `dist` URL to a versioned
`archive/<version>/.../rustup-init` URL. Its GNU-host installers need their own
digests: do not reuse the Dockerfile's musl-installer values.

Verify downloads **before extraction, chmod, or execution**. Unsupported
architectures, missing digest entries, or mismatches fail closed with the
artifact name and expected/actual hash. Do not fetch the trusted digest from
the same unpinned URL at runtime. Use the same reviewed version/digest data for
overlapping Docker/script artifacts while keeping different target triples
distinct. Treat existing unverified tool caches as untrusted: discard and
repopulate them from verified archives, and publish cache completion atomically
only after verification and extraction succeed.

## Panic behavior

Keep Cargo's `panic = "abort"`. Remove `-Cpanic=immediate-abort` from every
release path, including build-std, and remove `-Zlocation-detail=none` so a
panic retains its message and source location. Keep the existing optimization,
LTO, and stripping settings. No unwinding or custom panic framework is needed.

Rust documents that
[`immediate-abort` skips panic hooks](https://doc.rust-lang.org/rustc/codegen-options/index.html#panic),
whereas ordinary
[`panic = "abort"` invokes the hook](https://doc.rust-lang.org/std/process/fn.abort.html).
This change restores diagnostics, not destructor execution, terminal cleanup,
or recovery from a panic. Expected operational failures still use errors.

## Acceptance and rollback

- Exercise the guard with default-branch, other-branch, and tag event fixtures;
  rejected events must fail before publication. Confirm all five expected
  archives pass verification and a missing archive or modified byte fails.
- With a local download fixture, prove corrupted archives/installers cannot
  reach extraction/execution, and legacy unverified caches are not reused.
- Build a small panic probe using the actual release flags for each supported
  build path. It must terminate abnormally and emit a recognizable message and
  source location on stderr. Do not add a user-triggered panic command to ish.
  Record the binary-size difference as information, not a reason to restore
  silent panics.
- Document rollback as installing a previous release after verifying its
  checksum. If a published prerelease is defective, withdraw that release and
  its tag deliberately after inspection; do not reassign its name to different
  bytes. Fixes get a new commit and therefore a new release tag.

The release workflow, downloader, build flags, and documentation changes are
implemented and locally validated on Linux. The native macOS release build and
panic probe passed on 2026-10-23; the diagnostic and size observations are in
[filesystem testing](../filesystem-testing.md#native-macos-arm64-acceptance-2026-10-23).
