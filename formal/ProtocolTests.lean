import IshSemantics.Protocol
import IshSemantics.Policy

open Lean IshSemantics IshSemantics.Protocol IshSemantics.Policy

def require (condition : Bool) (message : String) : IO Unit :=
  unless condition do throw (IO.userError message)

def isError : Except α β → Bool
  | .error _ => true
  | .ok _ => false

def main : IO Unit := do
  let golden ← IO.FS.readFile "fixtures/protocol-v1.jsonl"
  let mut session : Session := {}
  for line in golden.splitOn "\n" do
    unless line.isEmpty do
      match acceptLine session line with
      | .error error => throw (IO.userError s!"golden rejected: {repr error}")
      | .ok (next, request) =>
        require (request.source == [0, 128, 255]) "raw source bytes changed"
        require (request.state.inherited == [([255], [0, 254])]) "raw environment bytes changed"
        require (isError (acceptLine next line)) "duplicate case id accepted"
        let roundTrip ← match decodeRequest (encodeRequest request) with
          | .error error => throw (IO.userError s!"round trip rejected: {repr error}")
          | .ok value => pure value
        require (roundTrip.state == request.state && roundTrip.source == request.source &&
          roundTrip.family == request.family && roundTrip.fuel == request.fuel &&
          roundTrip.id == request.id && roundTrip.input == request.input) "request round trip changed"
        session := next
  require (session.ids.length == 6) "missing family goldens"
  let invalid ← IO.FS.readFile "fixtures/protocol-invalid.jsonl"
  let mut rejected := 0
  for line in invalid.splitOn "\n" do
    unless line.isEmpty do
      require (isError (acceptLine {} line)) s!"malformed input accepted: {line}"
      rejected := rejected + 1
  require (rejected == 12) "missing rejection goldens"
  let placementRequest : Protocol.Request := {
    id := "placement-pipeline"
    family := .policy
    source := "printf input | command command \"$TARGET\" source destination > placement-redirection".toUTF8.toList
    state := { variables := [("TARGET", "cp".toUTF8.toList)] }
    fuel := 128
    input := .mkObj [
      ("mode", toJson "placement"),
      ("target", encodeBytes "cp".toUTF8.toList),
      ("context", toJson "pipeline")]
  }
  match Protocol.decodeRequest (Protocol.encodeRequest placementRequest) with
  | .error error => throw (IO.userError s!"placement request rejected after round trip: {repr error}")
  | .ok request =>
    require (request.input == placementRequest.input) "placement request input changed"
    require (Policy.run request == .success (.mkObj [
      ("allowed", toJson false),
      ("status", toJson (2 : Nat)),
      ("class", toJson "foreground")])) "pipeline placement was not refused"
  for byte in List.range 256 do
    match decodeBytes (encodeBytes [byte.toUInt8]) with
    | .ok bytes => require (bytes == [byte.toUInt8]) "byte round trip changed"
    | .error reason => throw (IO.userError reason)
  let outcomes : List (Outcome Json) := [
    .success (.mkObj [("status", toJson (0 : Nat))]),
    .semanticError (.arithmetic "division by zero")
      { variables := [("rhs", [49])], lastStatus := 2 }
      [.write 1 [114, 104, 115]],
    .unsupportedDomain "control flow", .fuelExhausted]
  let expected ← IO.FS.readFile "fixtures/protocol-outcomes.jsonl"
  let actual := String.intercalate "\n" (outcomes.map fun result =>
    (encodeResponse "outcomes" result).compress) ++ "\n"
  require (actual == expected) "outcome golden changed"
  IO.println "protocol: 6 family goldens, 12 rejected inputs, placement policy request, 256 byte round trips, 4 outcomes"
