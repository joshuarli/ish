# Bridge protocol v1

This reference describes the development JSON-lines interface in `IshSemantics.Protocol`.
The interface has no production runtime dependency.

Each line contains one JSON object. Each request has these required fields:

| Field | Value |
| --- | --- |
| `version` | Integer `1` |
| `id` | Nonempty string, unique within the driver session |
| `family` | `policy`, `expansion`, `arithmetic`, `evaluation`, `effects`, or `interactive` |
| `source` | Byte array |
| `state` | Shell state object |
| `fuel` | Nonnegative integer |
| `input` | Family input object |

A byte array contains integers from 0 through 255. JSON strings never carry shell bytes.
NUL and invalid UTF-8 remain representable. Each family checks any OS preconditions before execution.
The session records an ID after structural validation. A failed request leaves the session unchanged.

The shell state has exactly these required fields:

| Field | Value |
| --- | --- |
| `variables` | Array of `[name, bytes]` pairs, with string names |
| `exported` | Array of `[name, bytes]` pairs, with string names |
| `inherited` | Array of `[nameBytes, valueBytes]` pairs |
| `cwd` | Byte array |
| `positional` | Array of byte arrays |
| `last_status` | Integer from 0 through 255 |

Each binding array rejects duplicate names. Arrays retain their order.
The request and state reject unknown or missing fields. Family drivers validate their own `input` keys.
A family driver must reject unknown input fields and unsupported selected-family cases.
`exactObject` provides the shared field check.

Responses have exactly `version`, `id`, and `outcome`.
The outcome is one of these objects:

| Tag | Other required fields |
| --- | --- |
| `success` | `value`: family result |
| `semantic_error` | `error`: semantic error object |
| `unsupported_domain` | `reason`: string |
| `fuel_exhausted` | None |

Semantic error objects use `kind` as the discriminator.
The `syntax`, `arithmetic`, `expansion`, and `environment` kinds contain a `message` string.
The `policy` kind contains a byte-array `target`.
The `redirection` and `restoration` kinds contain a nonnegative `fd` and a `message` string.
Error text is diagnostic. Differential comparisons use the kind and any structured payload required by the family.
Fuel exhaustion never denotes success or a termination proof.

`encodeExecution` produces `state`, `status`, `stdout`, `stderr`, and `trace`.
Trace events retain execution order. Their `tag` values and payloads are:

| Tag | Payload |
| --- | --- |
| `invoke` | `argv`: byte arrays, `environment`: byte-pair array, `cwd`: bytes |
| `read`, `write` | `fd`: natural number, `bytes`: bytes |
| `open` | `fd`: natural number, `path`: bytes, `mode`: open mode |
| `duplicate` | `source`, `destination`: natural numbers |
| `close` | `fd`: natural number |
| `change_directory` | `path`: bytes |

Open modes are `input`, `output`, `append`, `read_write`, and `clobber`.
These traces describe abstract observations. They do not establish kernel descriptor safety.

`fixtures/protocol-v1.jsonl` contains one accepted request per family.
`fixtures/protocol-invalid.jsonl` contains rejected boundary inputs.
`fixtures/protocol-outcomes.jsonl` fixes the four response tags.
`lake exe protocolTests` checks these fixtures and every single-byte round trip from the `formal` directory.
The fixture inputs are empty objects. They check the shared boundary before family validation.
