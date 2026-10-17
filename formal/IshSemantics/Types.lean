import Lean

namespace IshSemantics

abbrev Bytes := List UInt8
abbrev Bindings := List (String × Bytes)

structure ShellState where
  variables : Bindings := []
  exported : Bindings := []
  inherited : List (Bytes × Bytes) := []
  cwd : Bytes := []
  positional : List Bytes := []
  lastStatus : UInt8 := 0
  deriving Repr, BEq, Inhabited

inductive SemanticError where
  | syntax (message : String)
  | policy (target : Bytes)
  | arithmetic (message : String)
  | expansion (message : String)
  | redirection (fd : Nat) (message : String)
  | restoration (fd : Nat) (message : String)
  | environment (message : String)
  deriving Repr, BEq, Inhabited

inductive OpenMode where
  | input | output | append | readWrite | clobber
  deriving Repr, BEq, Inhabited

inductive TraceEvent where
  | invoke (argv : List Bytes) (environment : List (Bytes × Bytes)) (cwd : Bytes)
  | read (fd : Nat) (bytes : Bytes)
  | write (fd : Nat) (bytes : Bytes)
  | open (fd : Nat) (path : Bytes) (mode : OpenMode)
  | duplicate (source destination : Nat)
  | close (fd : Nat)
  | changeDirectory (path : Bytes)
  deriving Repr, BEq, Inhabited

abbrev Trace := List TraceEvent

inductive Outcome (α : Type) where
  | success (value : α)
  | semanticError (error : SemanticError) (state : ShellState) (trace : Trace)
  | unsupportedDomain (reason : String)
  | fuelExhausted
  deriving Repr, BEq, Inhabited

structure Execution where
  state : ShellState
  status : UInt8
  stdout : Bytes := []
  stderr : Bytes := []
  trace : Trace := []
  deriving Repr, BEq, Inhabited

structure Observation where
  argv : List Bytes
  result : Outcome Execution
  deriving Repr, BEq, Inhabited

end IshSemantics
