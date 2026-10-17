import IshSemantics.Types

namespace IshSemantics.Protocol
open Lean

inductive Family where
  | policy | expansion | arithmetic | evaluation | effects | interactive
  deriving Repr, BEq, Inhabited

def Family.name : Family → String
  | .policy => "policy"
  | .expansion => "expansion"
  | .arithmetic => "arithmetic"
  | .evaluation => "evaluation"
  | .effects => "effects"
  | .interactive => "interactive"

def parseFamily (name : String) : Except String Family :=
  match name with
  | "policy" => .ok .policy
  | "expansion" => .ok .expansion
  | "arithmetic" => .ok .arithmetic
  | "evaluation" => .ok .evaluation
  | "effects" => .ok .effects
  | "interactive" => .ok .interactive
  | _ => .error "unknown family"

structure Request where
  id : String
  family : Family
  source : Bytes
  state : ShellState
  fuel : Nat
  input : Json

inductive BoundaryError where
  | malformed (reason : String)
  | unsupportedVersion (version : Nat)
  | duplicateId (id : String)
  deriving Repr, BEq

structure Session where
  ids : List String := []
  deriving Repr, BEq, Inhabited

def exactObject (fields : List String) (value : Json) : Except String Unit := do
  let object ← value.getObj?
  for (key, _) in object.toList do
    unless fields.contains key do throw s!"unknown field: {key}"
  for key in fields do
    let _ ← value.getObjVal? key
  pure ()

def decodeBytes (value : Json) : Except String Bytes := do
  let array ← value.getArr?
  array.toList.mapM fun item => do
    let byte ← item.getNat?
    if byte < 256 then pure byte.toUInt8 else throw "byte outside 0..255"

def encodeBytes (bytes : Bytes) : Json :=
  .arr (bytes.map (fun byte => toJson byte.toNat)).toArray

def decodePair (left : Json → Except String α) (right : Json → Except String β)
    (value : Json) : Except String (α × β) := do
  let array ← value.getArr?
  match array.toList with
  | [a, b] => return (← left a, ← right b)
  | _ => throw "expected pair"

def decodeBindings (value : Json) : Except String Bindings := do
  let pairs ← (← value.getArr?).toList.mapM (decodePair Json.getStr? decodeBytes)
  let names := pairs.map Prod.fst
  unless names.eraseDups.length == names.length do throw "duplicate binding"
  return pairs

def decodeEnvironment (value : Json) : Except String (List (Bytes × Bytes)) := do
  let pairs ← (← value.getArr?).toList.mapM (decodePair decodeBytes decodeBytes)
  let names := pairs.map Prod.fst
  unless names.eraseDups.length == names.length do throw "duplicate environment name"
  return pairs

def encodeBindings (bindings : Bindings) : Json :=
  .arr (bindings.map fun (name, value) => .arr #[toJson name, encodeBytes value]).toArray

def encodeEnvironment (bindings : List (Bytes × Bytes)) : Json :=
  .arr (bindings.map fun (name, value) => .arr #[encodeBytes name, encodeBytes value]).toArray

def decodeState (value : Json) : Except String ShellState := do
  exactObject ["variables", "exported", "inherited", "cwd", "positional", "last_status"] value
  let variables ← decodeBindings (← value.getObjVal? "variables")
  let exported ← decodeBindings (← value.getObjVal? "exported")
  let inherited ← decodeEnvironment (← value.getObjVal? "inherited")
  let cwd ← decodeBytes (← value.getObjVal? "cwd")
  let positional ← (← (← value.getObjVal? "positional").getArr?).toList.mapM decodeBytes
  let status ← (← value.getObjVal? "last_status").getNat?
  unless status < 256 do throw "status outside 0..255"
  return { variables, exported, inherited, cwd, positional, lastStatus := status.toUInt8 }

def encodeState (state : ShellState) : Json := .mkObj [
  ("variables", encodeBindings state.variables),
  ("exported", encodeBindings state.exported),
  ("inherited", encodeEnvironment state.inherited),
  ("cwd", encodeBytes state.cwd),
  ("positional", .arr (state.positional.map encodeBytes).toArray),
  ("last_status", toJson state.lastStatus.toNat)]

def decodeRequest (value : Json) : Except BoundaryError Request := do
  let parsed : Except String (Nat × Request) := do
    exactObject ["version", "id", "family", "source", "state", "fuel", "input"] value
    let version ← (← value.getObjVal? "version").getNat?
    let id ← (← value.getObjVal? "id").getStr?
    unless !id.isEmpty do throw "empty case id"
    let family ← parseFamily (← (← value.getObjVal? "family").getStr?)
    let source ← decodeBytes (← value.getObjVal? "source")
    let state ← decodeState (← value.getObjVal? "state")
    let fuel ← (← value.getObjVal? "fuel").getNat?
    let input ← value.getObjVal? "input"
    let _ ← input.getObj?
    return (version, { id, family, source, state, fuel, input })
  match parsed with
  | .error reason => throw (.malformed reason)
  | .ok (version, request) =>
    if version == 1 then return request else throw (.unsupportedVersion version)

def encodeRequest (request : Request) : Json := .mkObj [
  ("version", toJson (1 : Nat)), ("id", toJson request.id),
  ("family", toJson request.family.name), ("source", encodeBytes request.source),
  ("state", encodeState request.state), ("fuel", toJson request.fuel),
  ("input", request.input)]

def accept (session : Session) (request : Request) : Except BoundaryError Session :=
  if session.ids.contains request.id then .error (.duplicateId request.id)
  else .ok { ids := request.id :: session.ids }

def acceptLine (session : Session) (line : String) :
    Except BoundaryError (Session × Request) := do
  let json ← (Json.parse line).mapError BoundaryError.malformed
  let request ← decodeRequest json
  let next ← accept session request
  return (next, request)

def encodeTraceEvent : TraceEvent → Json
  | .invoke argv environment cwd => .mkObj [
      ("tag", toJson "invoke"), ("argv", .arr (argv.map encodeBytes).toArray),
      ("environment", encodeEnvironment environment), ("cwd", encodeBytes cwd)]
  | .read fd bytes => .mkObj [("tag", toJson "read"), ("fd", toJson fd), ("bytes", encodeBytes bytes)]
  | .write fd bytes => .mkObj [("tag", toJson "write"), ("fd", toJson fd), ("bytes", encodeBytes bytes)]
  | .open fd path mode =>
      let name := match mode with
        | .input => "input"
        | .output => "output"
        | .append => "append"
        | .readWrite => "read_write"
        | .clobber => "clobber"
      .mkObj [("tag", toJson "open"), ("fd", toJson fd), ("path", encodeBytes path), ("mode", toJson name)]
  | .duplicate source destination => .mkObj [
      ("tag", toJson "duplicate"), ("source", toJson source), ("destination", toJson destination)]
  | .close fd => .mkObj [("tag", toJson "close"), ("fd", toJson fd)]
  | .changeDirectory path => .mkObj [("tag", toJson "change_directory"), ("path", encodeBytes path)]

def encodeExecution (execution : Execution) : Json := .mkObj [
  ("state", encodeState execution.state), ("status", toJson execution.status.toNat),
  ("stdout", encodeBytes execution.stdout), ("stderr", encodeBytes execution.stderr),
  ("trace", .arr (execution.trace.map encodeTraceEvent).toArray)]

def encodeError : SemanticError → Json
  | .syntax message => .mkObj [("kind", toJson "syntax"), ("message", toJson message)]
  | .policy target => .mkObj [("kind", toJson "policy"), ("target", encodeBytes target)]
  | .arithmetic message => .mkObj [("kind", toJson "arithmetic"), ("message", toJson message)]
  | .expansion message => .mkObj [("kind", toJson "expansion"), ("message", toJson message)]
  | .redirection fd message => .mkObj [("kind", toJson "redirection"), ("fd", toJson fd), ("message", toJson message)]
  | .restoration fd message => .mkObj [("kind", toJson "restoration"), ("fd", toJson fd), ("message", toJson message)]
  | .environment message => .mkObj [("kind", toJson "environment"), ("message", toJson message)]

def encodeOutcome (encode : α → Json) : Outcome α → Json
  | .success value => .mkObj [("tag", toJson "success"), ("value", encode value)]
  | .semanticError error => .mkObj [("tag", toJson "semantic_error"), ("error", encodeError error)]
  | .unsupportedDomain reason => .mkObj [("tag", toJson "unsupported_domain"), ("reason", toJson reason)]
  | .fuelExhausted => .mkObj [("tag", toJson "fuel_exhausted")]

def encodeResponse (id : String) (outcome : Outcome Json) : Json := .mkObj [
  ("version", toJson (1 : Nat)), ("id", toJson id),
  ("outcome", encodeOutcome (fun value => value) outcome)]

theorem duplicate_rejected (session : Session) (request : Request)
    (present : session.ids.contains request.id = true) :
    accept session request = .error (.duplicateId request.id) := by
  unfold accept
  rw [present]
  rfl

theorem accepted_id_recorded (session : Session) (request : Request)
    (next : Session) (accepted : accept session request = .ok next) :
    request.id ∈ next.ids := by
  simp only [accept] at accepted
  split at accepted
  · contradiction
  · cases accepted
    simp

end IshSemantics.Protocol
