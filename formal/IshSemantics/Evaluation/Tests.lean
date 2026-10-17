import IshSemantics.Evaluation

namespace IshSemantics.Evaluation.Tests

def bytes (value : String) : Bytes := value.toUTF8.toList
def leaf (index : Nat) : Expr := .simple [] [bytes "oracle", bytes (toString index)] index none
def observations : List LeafObservation := [
  { status := 7, stdout := bytes "failed", stderr := [] },
  { status := 0, stdout := bytes "ok", stderr := [] },
  { status := 3, stdout := [], stderr := bytes "error" }]
def opts : Options := { errexit := false, pipefail := false }
def strict : Options := { errexit := true, pipefail := false }
def status (result : Outcome Result) : Option UInt8 :=
  match result with
  | .success value => some value.execution.status
  | _ => none
def events (result : Outcome Result) : Trace :=
  match result with
  | .success value => value.execution.trace
  | .semanticError _ _ trace => trace
  | _ => []
def variables (result : Outcome Result) : Bindings :=
  match result with
  | .success value => value.execution.state.variables
  | _ => []

example : status (eval observations opts 5 false (.sequence (leaf 0) (leaf 1)) {}) = some 0 := by rfl
example : status (eval observations strict 5 false (.sequence (leaf 0) (leaf 1)) {}) = some 7 := by rfl
example : events (eval observations strict 5 false (.sequence (leaf 0) (leaf 1)) {}) =
    events (eval observations strict 5 false (leaf 0) {}) := by rfl
example : status (eval observations strict 5 false (.or (leaf 0) (leaf 1)) {}) = some 0 := by rfl
example : events (eval observations strict 5 false (.and (leaf 0) (leaf 2)) {}) =
    events (eval observations opts 5 false (leaf 0) {}) := by rfl
example : status (eval observations strict 8 false
    (.sequence (.and (leaf 0) (leaf 2)) (leaf 1)) {}) = some 0 := by rfl
example : status (eval observations strict 8 false
    (.sequence (.not (leaf 1)) (leaf 2)) {}) = some 3 := by rfl
example : status (eval observations strict 8 false
    (.or (.sequence (leaf 0) (leaf 1)) (leaf 2)) {}) = some 0 := by rfl
example : status (eval observations opts 8 false
    (.pipeline [leaf 0, leaf 1] false) {}) = some 0 := by rfl
example : status (eval observations { opts with pipefail := true } 8 false
    (.pipeline [leaf 0, leaf 2, leaf 1] false) {}) = some 3 := by rfl
example : status (eval observations strict 8 false
    (.sequence (.pipeline [leaf 1, leaf 1] true) (leaf 2)) {}) = some 3 := by rfl
example : status (eval observations opts 8 false
    (.simple [("x", bytes "value")] [] 999 none) { lastStatus := 9 }) = some 0 := by rfl
example : status (eval observations opts 8 false
    (.simple [("x", bytes "value")] [] 999 (some 1)) { lastStatus := 9 }) = some 1 := by rfl
example : status (eval observations opts 8 false
    (.simple [("x", bytes "value")] [] 999 (some 0)) { lastStatus := 9 }) = some 0 := by rfl
example : variables (eval observations opts 8 false
    (.simple [("x", bytes "value")] [] 999 none) {}) = [("x", bytes "value")] := by rfl
example : variables (eval observations opts 8 false
    (.simple [("x", bytes "temporary")] [bytes "oracle"] 1 none)
    { variables := [("x", bytes "original")] }) = [("x", bytes "original")] := by rfl
example : variables (eval observations opts 8 false
    (.subshell (.simple [("x", bytes "child")] [] 0 none))
    { variables := [("x", bytes "parent")] }) = [("x", bytes "parent")] := by rfl
example : variables (eval observations opts 8 false
    (.pipeline [.simple [("x", bytes "child")] [] 0 none, leaf 1] false)
    { variables := [("x", bytes "parent")] }) = [("x", bytes "parent")] := by rfl
example : environment
    { inherited := [(bytes "x", bytes "inherited")], exported := [("x", bytes "exported")] }
    [("x", bytes "prefix")] = [(bytes "x", bytes "prefix")] := by
  simp [environment, setEnvironment, bytes]
example : assign { exported := [("x", bytes "old")] } [("x", bytes "new")] =
    { variables := [("x", bytes "new")], exported := [("x", bytes "new")] } := by rfl
example : eval observations opts 0 false (leaf 0) {} = .fuelExhausted := by rfl
example : eval observations opts 1 false (.sequence (leaf 0) (leaf 1)) {} = .fuelExhausted := by rfl
example : eval [] opts 1 false (leaf 0) {} = .unsupportedDomain "missing leaf observation" := by rfl
example : match decodeObservation (.mkObj [
    ("status", Lean.toJson (256 : Nat)), ("stdout", .arr #[]), ("stderr", .arr #[])]) with
    | .error _ => true | .ok _ => false = true := by rfl
example : match decodeObservation (.mkObj [
    ("status", Lean.toJson (0 : Nat)), ("stdout", .arr #[]), ("stderr", .arr #[]),
    ("unknown", .null)]) with
    | .error _ => true | .ok _ => false = true := by rfl
example : match decodeExpr 2 (.mkObj [
    ("tag", Lean.toJson "not"), ("body", .null), ("unknown", .null)]) with
    | .error _ => true | .ok _ => false = true := by rfl
example : match decodeExpr 2 (.mkObj [
    ("tag", Lean.toJson "simple"), ("assignments", .arr #[]), ("argv", .arr #[]),
    ("observation", Lean.toJson (0 : Nat)), ("assignment_status", Lean.toJson (256 : Nat))]) with
    | .error _ => true | .ok _ => false = true := by rfl

end IshSemantics.Evaluation.Tests
