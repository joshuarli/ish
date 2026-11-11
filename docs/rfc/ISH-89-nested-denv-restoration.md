# ISH-89: Nested denv restoration

Status: draft

## Summary

A nested ish can inherit an active project environment without inheriting the
information needed to remove it. Choose whether that environment remains a
removable project layer or becomes the child's permanent baseline. The
recommendation is an independently owned, removable layer.

## Motivation

[`init`](../../src/denv.rs) replaces the inherited record ID, while inherited
`__DENV_STATE` can skip refresh. The [ticket](../../TODO.md#ish-89--p2--nested-denv-sessions-lose-project-restoration-provenance)
reproduces a child retaining project variables after clearing project markers.

The [environment ownership decision](ISH-15-environment-and-shell-ownership.md)
requires exact restoration and independent sessions. It does not define the
nested-shell baseline. This RFC proposes that missing contract.

## Design

Capture inherited project identity before assigning a fresh session ID. Validate
the inherited reference/layer and copy its restoration data into a child-owned
record. Never share record ownership, delete the parent's record, or recover by PID alone.

Resolve the record at its original storage location, even if the project changed
HOME or DENV_DATA_DIR. Preserve raw bytes, absent and declared-but-unset bindings,
and exported/local state. Publish through the existing record/store validation
boundary. Do not rerun project code to reconstruct a lost baseline.

Acceptance covers entering and leaving nested shells, revocation, parent exit,
independent cleanup, changed storage variables, missing or malformed inherited
records, and caller overrides of inherited variables. A failed adoption must
leave the inherited environment intact and report that restoration is unavailable.

## Alternatives considered

- Treat inherited values as the child's baseline. Leaving/revoking then cannot
  remove them; inherited metadata and documentation must state that contract.
- Reuse the parent's active record. Reject this because either session's
  cleanup could destroy the other's restoration state.

## Open questions

1. Should nested shells inherit a removable layer or a permanent baseline?
   Recommend the removable layer with independent ownership.
2. When inherited provenance is unavailable or caller overrides conflict, should
   startup reject adoption or continue with a marked inherited baseline?
   Recommend continuing with an explicit diagnostic and dirty state, without
   claiming that project removal restored the pre-project environment.

## Decision

Pending owner choice.
