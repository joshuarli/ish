import IshSemantics.Expansion

namespace IshSemantics.ExpansionTests
open Lean IshSemantics IshSemantics.Expansion

private def check [BEq α] [Repr α] (name : String) (actual expected : α) :
    Elab.Command.CommandElabM Unit := do
  unless actual == expected do
    throwError "{name}: expected {repr expected}, received {repr actual}"

private def checkExcept [BEq ε] [Repr ε] [BEq α] [Repr α] (name : String)
    (actual expected : Except ε α) : Elab.Command.CommandElabM Unit := do
  let pair : Except ε α → Option ε × Option α
    | .error error => (some error, none)
    | .ok value => (none, some value)
  check name (pair actual) (pair expected)

private def emptyInput : Input := ⟨[], [], [], []⟩
private def splitFields (ifs bytes : Bytes) : List Bytes := (finish (split ifs bytes {})).map erase

run_cmd do
  check "UTF8 scalar units" (units [97, 240, 159, 152, 128, 255]) [[97], [240, 159, 152, 128], [255]]
  check "overlong UTF8 bytes stay distinct" (units [192, 160]) [[192], [160]]
  check "surrogate UTF8 bytes stay distinct" (units [237, 160, 128]) [[237], [160], [128]]
  check "largest scalar unit" (units [244, 143, 191, 191]) [[244, 143, 191, 191]]
  check "out of range scalar stays bytes" (units [244, 144, 128, 128]) [[244], [144], [128], [128]]
  check "UTF8 whitespace is one delimiter" (splitFields [194, 160] [194, 160, 97, 194, 160, 98]) [[97], [98]]
  check "UTF8 shared prefix is not delimiter" (splitFields [194, 160] [194, 161]) [[194, 161]]
  check "invalid whitespace-looking byte is nonwhite" (splitFields [160] [160]) [[]]
  check "valid whitespace alone has no field" (splitFields [194, 160] [194, 160]) []
  check "invalid and scalar units differ" (splitFields [160] [97, 194, 160, 98, 160, 99]) [[97, 194, 160, 98], [99]]
  check "private use scalar differs from invalid byte" (splitFields [128] [238, 130, 128]) [[238, 130, 128]]
  check "astral nonwhite delimiter creates empty field" (splitFields [240, 159, 152, 128] [240, 159, 152, 128]) [[]]
  let context : ShellState := { positional := [[97, 32, 98], []], lastStatus := 7 }
  let primitive : Input := { emptyInput with
    observations := [
    ("parameter:$", [52, 50]), ("parameter:0", [105, 115, 104]), ("parameter:-", [])] }
  checkExcept "status parameter" (parameterValue primitive context "?") (.ok (some [55]))
  checkExcept "count parameter" (parameterValue primitive context "#") (.ok (some [50]))
  checkExcept "pid oracle" (parameterValue primitive context "$") (.ok (some [52, 50]))
  checkExcept "arg0 oracle" (parameterValue primitive context "0") (.ok (some [105, 115, 104]))
  checkExcept "flags oracle" (parameterValue primitive context "-") (.ok (some []))
  checkExcept "absent background pid" (parameterValue primitive context "!") (.ok none)
  checkExcept "first positional" (parameterValue primitive context "1") (.ok (some [97, 32, 98]))
  checkExcept "unset positional" (parameterValue primitive context "9") (.ok none)
  checkExcept "multidigit follows production ordinary lookup" (parameterValue primitive context "10") (.ok none)
  check "unquoted at preserves positional fields" (expandWord emptyInput context [.parameter "@" false none])
    (.success ⟨[[97, 32, 98], []], context, []⟩)
  check "unquoted star splits positional fields" (expandWord emptyInput context [.parameter "*" false none])
    (.success ⟨[[97], [98]], context, []⟩)
  check "quoted star joins and preserves empties" (expandWord emptyInput context [.parameter "*" true none])
    (.success ⟨[[97, 32, 98, 32]], context, []⟩)
  check "quoted at zero fields" (expandWord emptyInput {} [.parameter "@" true none]) (.success ⟨[], {}, []⟩)
  check "quoted star one empty field" (expandWord emptyInput {} [.parameter "*" true none]) (.success ⟨[[]], {}, []⟩)
  check "missing IFS uses space join" (joinPositionals [32, 9, 10] [[97], [98]]) [97, 32, 98]
  check "empty IFS concatenates" (joinPositionals [] [[97], [98]]) [97, 98]
  check "star joins with entire first UTF8 unit" (joinPositionals [240, 159, 152, 128, 58] [[97], [98]])
    [97, 240, 159, 152, 128, 98]
  let statusInput : Input := { emptyInput with
    observations := [("sub0", [])]
    effects := [("sub0", { status := some 1 })] }
  check "substitution status is visible later in word"
    (expandWord statusInput {} [.substitution "sub0" true, .parameter "?" true none])
    (.success ⟨[[49]], { lastStatus := 1 }, []⟩)
  check "substitution status survives word boundary"
    (expand statusInput {} [[.substitution "sub0" false], [.parameter "?" true none]])
    (.success ⟨[[49]], { lastStatus := 1 }, []⟩)
  let variableInput : Input := { emptyInput with
    observations := [("arith0", [51])]
    effects := [("arith0", { bindings := [("X", [57])] })] }
  check "arithmetic write is visible to later parameter"
    (expandWord variableInput {} [.arithmetic "arith0" true, .parameter "X" true none])
    (.success ⟨[[51, 57]], { variables := [("X", [57])] }, []⟩)
  let failInput : Input := { variableInput with
    observations := [("arith0", [51]), ("arith1", [])]
    effects := [("arith0", { bindings := [("X", [57])] }),
      ("arith1", { bindings := [("Y", [55])], error := some (.arithmetic "division by zero") })] }
  check "arithmetic failure retains prior and failing writes"
    (expandWord failInput { lastStatus := 11 } [.arithmetic "arith0" true, .arithmetic "arith1" true])
    (.semanticError (.arithmetic "division by zero")
      { variables := [("Y", [55]), ("X", [57])], lastStatus := 11 } [])
  let failedSub : Input := { emptyInput with
    observations := [("sub0", [120])]
    effects := [("sub0", { status := some 1, error := some (.expansion "read failure") })] }
  check "production swallows substitution error but retains status"
    (expandWord failedSub {} [.substitution "sub0" true])
    (.success ⟨[[]], { lastStatus := 1 }, []⟩)
  let ifsInput : Input := { emptyInput with
    observations := [("arith0", [49])]
    effects := [("arith0", { bindings := [("IFS", [49])] })] }
  let before : ShellState := { variables := [("IFS", [58]), ("V", [97, 49, 98])] }
  check "final IFS splits earlier expansion fragments"
    (expandWord ifsInput before [.parameter "V" false none, .arithmetic "arith0" false])
    (.success ⟨[[97], [98]], { variables := [("IFS", [49]), ("V", [97, 49, 98])] }, []⟩)
  check "bad substitution preserves incoming state"
    (expandWord emptyInput context [.badSubstitution "bad"])
    (.semanticError (.expansion "bad: bad substitution") context [])
  let first : Effect := { status := some 2, bindings := [("X", [49])] }
  let second : Effect := { status := some 3, bindings := [("X", [50])] }
  check "effect order cannot commute"
    (applyEffect (applyEffect {} first) second == applyEffect (applyEffect {} second) first) false

private def checkWire (name : String) (actual expected : Outcome Json) :
    Elab.Command.CommandElabM Unit :=
  check name (Protocol.encodeOutcome id actual).compress (Protocol.encodeOutcome id expected).compress

private def schemaFailure (entry : Json) : Option String :=
  match decodeInput (.mkObj [("words", .arr #[]), ("globs", .arr #[]), ("observations", .arr #[entry])]) with
  | .error message => some message
  | .ok _ => none

run_cmd do
  check "oracle status range is strict"
    (schemaFailure (.mkObj [("id", .str "x"), ("bytes", .arr #[]), ("status", toJson (256 : Nat))]))
    (some "oracle status outside 0..255")
  check "oracle binding names are ordinary variables"
    (schemaFailure (.mkObj [("id", .str "x"), ("bytes", .arr #[]),
      ("bindings", .arr #[.arr #[.str "@", .arr #[]]])]))
    (some "invalid oracle binding name")
  check "oracle error schema rejects unknown kind"
    (schemaFailure (.mkObj [("id", .str "x"), ("bytes", .arr #[]),
      ("error", .mkObj [("kind", .str "syntax"), ("message", .str "x")])]))
    (some "invalid oracle error kind")
  check "oracle schema rejects unknown field"
    (schemaFailure (.mkObj [("id", .str "x"), ("bytes", .arr #[]), ("extra", .null)]))
    (some "unknown field: extra")
  check "legacy observation remains accepted"
    (schemaFailure (.mkObj [("id", .str "x"), ("bytes", .arr #[])])) none
  let state : ShellState := { positional := [[97], [98]], variables := [("IFS", [])] }
  let input : Json := .mkObj [
    ("words", .arr #[.arr #[.mkObj [("tag", .str "parameter"), ("name", .str "*"),
      ("quoted", .bool true), ("default", .null)]]]),
    ("observations", .arr #[]), ("globs", .arr #[])]
  checkWire "wire success exposes state and trace"
    (evalRequest { id := "wire", family := .expansion, source := [], state, fuel := 10, input })
    (.success (.mkObj [("fields", .arr #[Protocol.encodeBytes [97, 98]]),
      ("state", Protocol.encodeState state), ("trace", .arr #[])]))
  let bad : Json := .mkObj [("words", .arr #[.arr #[.mkObj [
    ("tag", .str "bad_substitution"), ("name", .str "bad")]]]),
    ("observations", .arr #[]), ("globs", .arr #[])]
  checkWire "wire bad substitution is semantic error with retained state"
    (evalRequest { id := "bad", family := .expansion, source := [], state, fuel := 10, input := bad })
    (.semanticError (.expansion "bad: bad substitution") state [])

end IshSemantics.ExpansionTests
