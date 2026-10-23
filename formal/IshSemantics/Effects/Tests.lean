import IshSemantics.Effects

namespace IshSemantics.Effects.Tests
open Lean

def request (input : Json) (fuel : Nat := 8) : Protocol.Request :=
  { id := "effects-test", family := .effects, source := [], state := {}, fuel, input }

def bytesInput (bytes : Bytes) : Json := .mkObj [
  ("mode", toJson "bytes"), ("bytes", Protocol.encodeBytes bytes)]

def isSuccess (outcome : Outcome Json) (expected : Json) : Bool :=
  match outcome with
  | .success actual => actual == expected
  | _ => false

def checkBytes : IO Unit := do
  unless isSuccess (evalRequest (request (bytesInput [0, 128, 194, 169, 238, 130, 128, 255])))
    (.mkObj [("bytes", Protocol.encodeBytes [0, 128, 194, 169, 238, 130, 128, 255]),
      ("c_string", toJson false)]) do
    throw (IO.userError "raw byte boundary changed")

  unless isSuccess (evalRequest (request (bytesInput [128, 255])))
    (.mkObj [("bytes", Protocol.encodeBytes [128, 255]), ("c_string", toJson true)]) do
    throw (IO.userError "raw byte boundary changed")

example : (match evalRequest (request (.mkObj [
    ("mode", toJson "bytes"), ("bytes", .arr #[toJson (256 : Nat)])])) with
    | .unsupportedDomain _ => true | _ => false) = true := by decide

example : (match evalRequest (request (.mkObj [
    ("mode", toJson "bytes"), ("bytes", .arr #[]), ("extra", toJson true)])) with
    | .unsupportedDomain _ => true | _ => false) = true := by decide

example : (match evalRequest (request (bytesInput []) 0) with
    | .fuelExhausted => true | _ => false) = true := by decide

def checkEnvironment : IO Unit := do
  unless isSuccess (evalRequest { (request (.mkObj [
    ("mode", toJson "environment"),
    ("assignments", Protocol.encodeBindings [("A", [51])])])) with
    state := {
      inherited := [([65], [49]), ([255], [128])]
      exported := [("A", [50])]
      variables := [("PRIVATE", [52])] } })
    (.mkObj [("environment", Protocol.encodeEnvironment [([65], [51]), ([255], [128])])]) do
    throw (IO.userError "environment precedence changed")

example : (match evalRequest (request (.mkObj [
    ("mode", toJson "redirection"),
    ("descriptors", .arr #[.arr #[toJson (1 : Nat), toJson (7 : Nat)]]),
    ("operations", .arr #[.mkObj [
      ("target", toJson (1 : Nat)), ("copy", toJson (10 : Nat)),
      ("kind", toJson "open"), ("resource", toJson (8 : Nat)), ("restore", toJson false)]]),
    ("permanent", toJson false)])) with
    | .semanticError (.restoration 1 _) _ _ => true | _ => false) = true := by decide

end IshSemantics.Effects.Tests

def main : IO Unit := do
  IshSemantics.Effects.Tests.checkBytes
  IshSemantics.Effects.Tests.checkEnvironment
  IO.println "effects: byte boundaries, environment precedence, strict inputs, restoration errors"
