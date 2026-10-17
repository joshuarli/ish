import IshSemantics.Arithmetic
import IshSemantics.Effects
import IshSemantics.Evaluation
import IshSemantics.Interactive
import IshSemantics.Policy
import IshSemantics.Protocol

open Lean IshSemantics IshSemantics.Protocol

def dispatch (request : Request) : Outcome Json :=
  match request.family with
  | .policy => IshSemantics.Policy.run request
  | .expansion => .unsupportedDomain "expansion adapter is not integrated"
  | .arithmetic => IshSemantics.Arithmetic.evalRequest request
  | .interactive => IshSemantics.Interactive.evalRequest request
  | .evaluation => IshSemantics.Evaluation.evalRequest request
  | .effects => IshSemantics.Effects.evalRequest request

def encodeBoundaryError : BoundaryError → Json
  | .malformed reason => .mkObj [
      ("error", .mkObj [("tag", toJson "malformed"), ("message", toJson reason)]),
      ("version", toJson (1 : Nat))]
  | .unsupportedVersion version => .mkObj [
      ("error", .mkObj [("tag", toJson "unsupported_version"), ("version", toJson version)]),
      ("version", toJson (1 : Nat))]
  | .duplicateId id => .mkObj [
      ("error", .mkObj [("tag", toJson "duplicate_id"), ("id", toJson id)]),
      ("version", toJson (1 : Nat))]

def processLine (session : Session) (line : String) : Session × Json :=
  match acceptLine session line with
  | .error error => (session, encodeBoundaryError error)
  | .ok (next, request) =>
    (next, encodeResponse request.id (dispatch request))

partial def serve (stdin stdout : IO.FS.Stream) (session : Session) : IO Unit := do
  let line ← stdin.getLine
  if line.isEmpty then
    pure ()
  else
    let (next, response) := processLine session line
    stdout.putStrLn response.compress
    serve stdin stdout next

def main : IO Unit := do
  let stdin ← IO.getStdin
  let stdout ← IO.getStdout
  serve stdin stdout {}
