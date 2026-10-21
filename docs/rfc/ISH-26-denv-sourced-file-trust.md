# ISH-26: Trust for sourced denv files

Status: draft

## Summary

Decide whether files that a trusted `.envrc` sources must pass the same trust
check as discovered files, and enforce the answer.

## Motivation

denv trusts a discovered `.envrc` or `.env` by hashing its contents and checking
the allow store. The stdlib prepended to every `.envrc` defines `source_env`,
`source_env_if_exists`, `source_up`, `dotenv` and `dotenv_if_exists`, which run
`. "$file"` directly. The sourced file is never hashed. A parent `.envrc` that
was edited after `denv allow`, or revoked with `denv deny`, runs when a trusted
child calls `source_up`. `dotenv` executes `.env` as shell code, while the README
says `.env` files are read as variable assignments.

## Design

Enforce the trust model at the point of sourcing. Option A removes the sourcing
functions from the stdlib. Option B provides the same function names through an
ish-owned check that hashes the target, consults the allow store and refuses an
untrusted file with a message naming it. `dotenv` parses assignments in either
option and does not execute the file.

## Alternatives considered

- Keep the functions and document that sourcing is transitively trusted. This
  matches the trust the user already gave the child, but contradicts the README
  statement that editing a file revokes trust and lets `git pull` change what
  runs.
- Hash all sourced files into the child's trust record at `denv allow` time.
  This needs a dependency walk that shell evaluation cannot do statically.

## Open questions

1. Is direnv-style `source_up` worth keeping? Recommend option B for
   `source_up` and `source_env`, and removing `dotenv` sourcing in favor of
   assignment parsing.
2. Should a refusal stop the whole `.envrc` evaluation or only that call?
   Recommend stopping, so a partial environment is never applied.

## Decision

Pending. Implementation must cover an untrusted parent reached through
`source_up`, a modified sourced file, and a `.env` containing command
substitution. Update the README denv section and delete this RFC.
