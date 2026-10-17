# Expansion generation coverage

The semantic bridge keeps its first 53 expansion request IDs as replay fixtures.
Requests from index 53 onward use a seeded combinatorial schedule.

## Generated dimensions

The schedule covers 315 combinations in each period. A seed rotates the order of
each dimension and selects the literal affixes for each request.

| Dimension | Values |
| --- | --- |
| `IFS` bytes | space, tab, and newline, colon, raw `0xff`, Unicode `λ`, empty, whitespace plus colon, whitespace plus raw `0xff` |
| `VALUE` shape | leading delimiter, adjacent delimiters, trailing delimiter, all three boundaries, empty |
| Source context | unquoted and quoted `${VALUE}`, unquoted and quoted affixes, quoted affixes around an unquoted expansion, quoted and unquoted `$@` with affixes, quoted and unquoted `$*` |

Colon cases include `:a`, `a::b`, `a::b:`, and `:a::b:`. The `a::b:` case
tests adjacent and trailing delimiters. The `:a::b:` case also tests a leading
delimiter. Empty `IFS` cases keep colon bytes in `VALUE` to check that they do
not split. Mixed IFS cases put spaces and tabs around adjacent colon or raw-byte
delimiters. Positional cases include an empty argument and delimiter-bearing
values.

The Rust generator test checks every IFS byte class, delimiter boundary, source
context, raw byte value, Unicode value, empty value, and deterministic seed
behavior. It also validates every generated request against the bridge domain.

## Run comparisons

Run 368 expansion cases to compare the original 53 requests and one full generated
period with the Lean model:

```sh
cargo run --example semantic_bridge -p ish -- --family expansion --cases 368 --seed 20261017
```

The bridge validates each request before sending it to Lean. A mismatch fails the
run and saves a minimized request under `formal/fixtures/bridge-regressions/`.

The schedule uses seeded dimensions and a bounded request grammar. It does not
use compiler instrumentation or feedback from previous outcomes, so it is not
coverage-guided fuzzing.
