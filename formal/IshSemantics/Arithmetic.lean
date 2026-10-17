import Lean.Elab.Tactic.Omega
import IshSemantics.Protocol

namespace IshSemantics.Arithmetic

abbrev Value := BitVec 64
abbrev Store := Bindings
def minimum : Int := -9223372036854775808
def maximum : Int := 9223372036854775807
def wrap (n : Int) : Value := BitVec.ofInt 64 n

theorem signed_range (v : Value) : minimum ≤ v.toInt ∧ v.toInt ≤ maximum :=
  ⟨BitVec.le_toInt v, BitVec.toInt_le⟩
theorem signed_roundtrip (n : Int) (lo : minimum ≤ n) (hi : n ≤ maximum) :
    (wrap n).toInt = n := by
  apply BitVec.toInt_ofInt_eq_self (by decide)
  · exact lo
  · change n < 9223372036854775808
    simp [maximum] at hi
    omega
theorem representation_roundtrip (v : Value) : wrap v.toInt = v := BitVec.ofInt_toInt

theorem wrap_periodic (n : Int) : wrap (n + 18446744073709551616) = wrap n := by
  apply BitVec.eq_of_toNat_eq
  simp [wrap]

inductive Error where
  | literalRange | divisionByZero | divisionOverflow | invalidShift
  deriving Repr, BEq, DecidableEq
inductive Unary where
  | plus | neg | logicalNot | bitNot
  deriving Repr, BEq, DecidableEq
inductive Binary where
  | add | sub | mul | div | rem | shl | shr | bitAnd | bitOr | bitXor
  | eq | ne | lt | le | gt | ge
  deriving Repr, BEq, DecidableEq

inductive AssignmentOp where
  | add | sub | mul | div | rem | shl | shr | bitAnd | bitOr | bitXor
  deriving Repr, BEq, DecidableEq

def AssignmentOp.binary : AssignmentOp → Binary
  | .add => .add | .sub => .sub | .mul => .mul | .div => .div | .rem => .rem
  | .shl => .shl | .shr => .shr | .bitAnd => .bitAnd | .bitOr => .bitOr | .bitXor => .bitXor

def precedence : Binary → Nat
  | .bitOr => 1 | .bitXor => 2 | .bitAnd => 3
  | .eq | .ne => 4 | .lt | .le | .gt | .ge => 5
  | .shl | .shr => 6 | .add | .sub => 7 | .mul | .div | .rem => 8

def truth (p : Bool) : Value := if p then 1 else 0

def unary : Unary → Value → Value
  | .plus, v => v
  | .neg, v => wrap (-v.toInt)
  | .logicalNot, v => truth (v == 0)
  | .bitNot, v => ~~~v

def binary (op : Binary) (a b : Value) : Except Error Value :=
  match op with
  | .add => .ok (wrap (a.toInt + b.toInt))
  | .sub => .ok (wrap (a.toInt - b.toInt))
  | .mul => .ok (wrap (a.toInt * b.toInt))
  | .div | .rem =>
    if b == 0 then .error .divisionByZero
    else if a.toInt == minimum && b.toInt == -1 then .error .divisionOverflow
    else .ok (wrap (if op == .div then a.toInt.tdiv b.toInt else a.toInt.tmod b.toInt))
  | .shl | .shr =>
    if b.toInt < 0 || b.toInt ≥ 64 then .error .invalidShift
    else .ok (if op == .shl then a <<< b.toNat else a.sshiftRight b.toNat)
  | .bitAnd => .ok (a &&& b)
  | .bitOr => .ok (a ||| b)
  | .bitXor => .ok (a ^^^ b)
  | .eq => .ok (truth (a == b))
  | .ne => .ok (truth (a != b))
  | .lt => .ok (truth (a.toInt < b.toInt))
  | .le => .ok (truth (a.toInt ≤ b.toInt))
  | .gt => .ok (truth (a.toInt > b.toInt))
  | .ge => .ok (truth (a.toInt ≥ b.toInt))

theorem addition_low_bits (a b : Value) : binary .add a b = .ok (a + b) := by
  simp [binary, wrap, BitVec.ofInt_add]

theorem division_by_zero (a : Value) : binary .div a 0 = .error .divisionByZero := by
  simp [binary]

theorem remainder_by_zero (a : Value) : binary .rem a 0 = .error .divisionByZero := by
  simp [binary]

inductive Expr where
  | literal (n : Int)
  | variable (name : String)
  | unary (op : Unary) (arg : Expr)
  | binary (op : Binary) (left right : Expr)
  | assign (name : String) (rhs : Expr)
  | compound (op : AssignmentOp) (name : String) (rhs : Expr)
  | and (left right : Expr)
  | or (left right : Expr)
  | conditional (condition yes no : Expr)
  deriving Repr

def decimalDigits : Bytes → Nat → Option Nat
  | [], accumulator => some accumulator
  | byte :: rest, accumulator =>
    if 48 ≤ byte.toNat && byte.toNat ≤ 57 then
      decimalDigits rest (accumulator * 10 + (byte.toNat - 48))
    else none

def unsignedDecimal : Bytes → Option Nat
  | [] => none
  | bytes => decimalDigits bytes 0

def decimal : Bytes → Option Int
  | 43 :: rest => (unsignedDecimal rest).map Int.ofNat
  | 45 :: rest => (unsignedDecimal rest).map Int.negOfNat
  | bytes => (unsignedDecimal bytes).map Int.ofNat

inductive BinaryToken where
  | literal (n : Int)
  | variable (name : String)
  | operator (op : Binary)
  | prefix (op : Unary)
  | leftParen | rightParen
  deriving Repr, BEq

mutual
  def parseBinaryFragment : Nat → Nat → List BinaryToken → Except String (Expr × List BinaryToken)
    | 0, _, _ => .error "fuel exhausted"
    | fuel + 1, minimumPrecedence, tokens => do
      let (left, rest) ← parseBinaryAtom fuel tokens
      parseBinaryRest fuel minimumPrecedence left rest
  def parseBinaryAtom : Nat → List BinaryToken → Except String (Expr × List BinaryToken)
    | 0, _ => .error "fuel exhausted"
    | fuel + 1, tokens =>
      match tokens with
      | .literal n :: rest => .ok (.literal n, rest)
      | .variable name :: rest => .ok (.variable name, rest)
      | .prefix op :: rest => do
        let (arg, rest) ← parseBinaryAtom fuel rest
        return (.unary op arg, rest)
      | .leftParen :: rest => do
        let (expr, rest) ← parseBinaryFragment fuel 1 rest
        match rest with
        | .rightParen :: rest => return (expr, rest)
        | _ => throw "missing right parenthesis"
      | _ => .error "expected arithmetic operand"
  def parseBinaryRest : Nat → Nat → Expr → List BinaryToken → Except String (Expr × List BinaryToken)
    | 0, _, _, _ => .error "fuel exhausted"
    | fuel + 1, minimumPrecedence, left, tokens =>
      match tokens with
      | .operator op :: rest =>
        if precedence op < minimumPrecedence then .ok (left, tokens)
        else do
          let (right, rest) ← parseBinaryFragment fuel (precedence op + 1) rest
          parseBinaryRest fuel minimumPrecedence (.binary op left right) rest
      | _ => .ok (left, tokens)
end

theorem multiplication_precedes_addition :
    parseBinaryFragment 20 1 [.literal 2, .operator .add, .literal 3, .operator .mul, .literal 4] =
    .ok (.binary .add (.literal 2) (.binary .mul (.literal 3) (.literal 4)), []) := rfl

theorem subtraction_associates_left :
    parseBinaryFragment 20 1 [.literal 9, .operator .sub, .literal 3, .operator .sub, .literal 2] =
    .ok (.binary .sub (.binary .sub (.literal 9) (.literal 3)) (.literal 2), []) := rfl

theorem parentheses_override_precedence :
    parseBinaryFragment 30 1 [.leftParen, .literal 2, .operator .add, .literal 3, .rightParen,
      .operator .mul, .literal 4] =
    .ok (.binary .mul (.binary .add (.literal 2) (.literal 3)) (.literal 4), []) := rfl

def lookup (name : String) (store : Store) : Value :=
  match store.find? (fun entry => entry.1 == name) with
  | some (_, bytes) =>
    let parsed := decimal bytes
    match parsed with
    | some n => if minimum ≤ n && n ≤ maximum then wrap n else 0
    | none => 0
  | none => 0

def write (name : String) (value : Value) (store : Store) : Store :=
  (name, value.toInt.repr.toUTF8.data.toList) :: store.filter (fun entry => entry.1 != name)

inductive Result where
  | value (value : Value)
  | error (error : Error)
  | exhausted
  deriving Repr, BEq, DecidableEq
structure Run where
  result : Result
  store : Store
  deriving Repr, BEq, DecidableEq

def withValue (result : Run) (next : Value → Store → Run) : Run :=
  match result.result with
  | .value value => next value result.store
  | _ => result

def finishAssignment (name : String) (result : Except Error Value) (store : Store) : Run :=
  match result with
  | .ok value => ⟨.value value, write name value store⟩
  | .error error => ⟨.error error, store⟩

def run : Nat → Expr → Store → Run
  | 0, _, store => ⟨.exhausted, store⟩
  | fuel + 1, expr, store =>
    match expr with
    | .literal n =>
      if 0 ≤ n && n ≤ maximum then ⟨.value (wrap n), store⟩
      else ⟨.error .literalRange, store⟩
    | .variable name => ⟨.value (lookup name store), store⟩
    | .unary op arg => withValue (run fuel arg store) fun value store =>
      ⟨.value (unary op value), store⟩
    | .binary op left right => withValue (run fuel left store) fun a store =>
      withValue (run fuel right store) fun b store =>
        match binary op a b with
        | .ok value => ⟨.value value, store⟩
        | .error error => ⟨.error error, store⟩
    | .assign name rhs => withValue (run fuel rhs store) fun value store =>
      finishAssignment name (.ok value) store
    | .compound op name rhs => withValue (run fuel rhs store) fun value store =>
      finishAssignment name (binary op.binary (lookup name store) value) store
    | .and left right => withValue (run fuel left store) fun value store =>
      if value == 0 then ⟨.value 0, store⟩
      else withValue (run fuel right store) fun value store => ⟨.value (truth (value != 0)), store⟩
    | .or left right => withValue (run fuel left store) fun value store =>
      if value != 0 then ⟨.value 1, store⟩
      else withValue (run fuel right store) fun value store => ⟨.value (truth (value != 0)), store⟩
    | .conditional condition yes no => withValue (run fuel condition store) fun value store =>
      run fuel (if value != 0 then yes else no) store

theorem failed_assignment_preserves_destination (name : String) (error : Error) (store : Store) :
    (finishAssignment name (.error error) store).store = store := rfl

theorem compound_failure_preserves_rhs_effects (op : Binary) (name : String)
    (value : Value) (store : Store) (error : Error)
    (fails : binary op (lookup name store) value = .error error) :
    finishAssignment name (binary op (lookup name store) value) store = ⟨.error error, store⟩ := by
  rw [fails]
  rfl

theorem compound_rhs_failure (fuel : Nat) (op : AssignmentOp) (name : String)
    (rhs : Expr) (before after : Store) (error : Error)
    (h : run fuel rhs before = ⟨.error error, after⟩) :
    run (fuel + 1) (.compound op name rhs) before = ⟨.error error, after⟩ := by
  simp [run, h, withValue]

theorem and_lazy (fuel : Nat) (left right : Expr) (before after : Store)
    (h : run fuel left before = ⟨.value 0, after⟩) :
    run (fuel + 1) (.and left right) before = ⟨.value 0, after⟩ := by
  simp [run, h, withValue]

theorem or_lazy (fuel : Nat) (left right : Expr) (before after : Store) (value : Value)
    (nonzero : value != 0) (h : run fuel left before = ⟨.value value, after⟩) :
    run (fuel + 1) (.or left right) before = ⟨.value 1, after⟩ := by
  simp only [run, h, withValue, nonzero, ite_true]

theorem conditional_lazy (fuel : Nat) (condition yes no : Expr) (before after : Store)
    (value : Value) (h : run fuel condition before = ⟨.value value, after⟩) :
    run (fuel + 1) (.conditional condition yes no) before =
      run fuel (if value != 0 then yes else no) after := by
  simp [run, h, withValue]

theorem deterministic (fuel : Nat) (expr : Expr) (store : Store) (a b : Run)
    (ha : run fuel expr store = a) (hb : run fuel expr store = b) : a = b := ha.symm.trans hb

theorem fuel_zero (expr : Expr) (store : Store) :
    run 0 expr store = ⟨.exhausted, store⟩ := rfl

theorem addition_wraps : (wrap (maximum + 1)).toInt = minimum := by rfl
theorem subtraction_wraps : (wrap (minimum - 1)).toInt = maximum := by rfl
theorem negation_wraps : (unary .neg (wrap minimum)).toInt = minimum := by rfl
theorem division_overflow : binary .div (wrap minimum) (wrap (-1)) = .error .divisionOverflow := by rfl
theorem remainder_overflow : binary .rem (wrap minimum) (wrap (-1)) = .error .divisionOverflow := by rfl
theorem negative_shift : binary .shl 1 (wrap (-1)) = .error .invalidShift := by rfl
theorem signed_right_shift : binary .shr (wrap (-3)) 1 = .ok (wrap (-2)) := by rfl
theorem left_shift_63 : binary .shl 1 63 = .ok (wrap minimum) := by rfl
theorem shift_64 : binary .shl 1 64 = .error .invalidShift := by rfl
theorem precedence_evaluation : run 8 (.binary .add (.literal 2) (.binary .mul (.literal 3) (.literal 4))) [] =
    ⟨.value 14, []⟩ := by rfl
theorem lazy_assignment_error : run 8 (.and (.literal 0) (.assign "x" (.binary .div (.literal 1) (.literal 0)))) [] =
    ⟨.value 0, []⟩ := by rfl
theorem rhs_effect_survives_error : run 8 (.compound .div "x" (.assign "y" (.literal 0))) [("x", [55])] =
    ⟨.error .divisionByZero, [("y", [48]), ("x", [55])]⟩ := by rfl


open Lean

def unaryName : String → Except String Unary
  | "+" => .ok .plus | "-" => .ok .neg | "!" => .ok .logicalNot | "~" => .ok .bitNot
  | _ => .error "unknown unary operator"

def binaryName : String → Except String Binary
  | "+" => .ok .add | "-" => .ok .sub | "*" => .ok .mul
  | "/" => .ok .div | "%" => .ok .rem | "<<" => .ok .shl | ">>" => .ok .shr
  | "&" => .ok .bitAnd | "|" => .ok .bitOr | "^" => .ok .bitXor
  | "==" => .ok .eq | "!=" => .ok .ne | "<" => .ok .lt
  | "<=" => .ok .le | ">" => .ok .gt | ">=" => .ok .ge
  | _ => .error "unknown binary operator"

def assignmentName : String → Except String AssignmentOp
  | "+" => .ok .add | "-" => .ok .sub | "*" => .ok .mul
  | "/" => .ok .div | "%" => .ok .rem | "<<" => .ok .shl | ">>" => .ok .shr
  | "&" => .ok .bitAnd | "|" => .ok .bitOr | "^" => .ok .bitXor
  | _ => .error "operator has no compound assignment"

def validName (name : String) : Bool :=
  let initial := fun c : Char => (c.toNat < 128) && (c.isAlpha || c == '_')
  match name.toList with
  | [] => false
  | head :: tail => initial head && tail.all (fun c => initial c || c.isDigit)

def decodeName (json : Json) : Except String String := do
  let name ← json.getStr?
  unless validName name do throw "arithmetic variable outside ordinary ASCII names"
  return name

def decodeTree : Nat → Json → Except String Expr
  | 0, _ => .error "arithmetic tree fuel exhausted"
  | fuel + 1, json => do
    let args ← json.getArr?
    match args.toList with
    | [Json.str "lit", n] => return .literal (← n.getInt?)
    | [Json.str "var", name] => return .variable (← decodeName name)
    | [Json.str "unary", op, arg] =>
      return .unary (← unaryName (← op.getStr?)) (← decodeTree fuel arg)
    | [Json.str "binary", op, left, right] =>
      return .binary (← binaryName (← op.getStr?)) (← decodeTree fuel left) (← decodeTree fuel right)
    | [Json.str "assign", name, rhs] => return .assign (← decodeName name) (← decodeTree fuel rhs)
    | [Json.str "compound", op, name, rhs] =>
      let operator ← assignmentName (← op.getStr?)
      return .compound operator (← decodeName name) (← decodeTree fuel rhs)
    | [Json.str "and", left, right] => return .and (← decodeTree fuel left) (← decodeTree fuel right)
    | [Json.str "or", left, right] => return .or (← decodeTree fuel left) (← decodeTree fuel right)
    | [Json.str "if", condition, yes, no] =>
      return .conditional (← decodeTree fuel condition) (← decodeTree fuel yes) (← decodeTree fuel no)
    | _ => throw "invalid arithmetic tree"

def literalsValid : Expr → Bool
  | .literal n => 0 ≤ n && n ≤ maximum
  | .variable _ => true
  | .unary _ arg | .assign _ arg | .compound _ _ arg => literalsValid arg
  | .binary _ left right | .and left right | .or left right => literalsValid left && literalsValid right
  | .conditional condition yes no => literalsValid condition && literalsValid yes && literalsValid no

def Error.code : Error → String
  | .literalRange => "literal_range"
  | .divisionByZero => "division_by_zero"
  | .divisionOverflow => "division_overflow"
  | .invalidShift => "invalid_shift"

def canonical (store : Store) : Store := store.mergeSort (fun a b => a.1 ≤ b.1)

def updateState (state : ShellState) (store : Store) : ShellState :=
  { state with
    variables := canonical store
    exported := canonical (state.exported.map fun (name, bytes) =>
      (name, (store.find? (fun entry => entry.1 == name)).map Prod.snd |>.getD bytes)) }

def eval (input : Json) (state : ShellState) (fuel : Nat) : Outcome Json :=
  match Protocol.exactObject ["tree"] input with
  | .error message => .unsupportedDomain message
  | .ok () =>
    match input.getObjVal? "tree" >>= decodeTree fuel with
    | .error "arithmetic tree fuel exhausted" => .fuelExhausted
    | .error message => .unsupportedDomain message
    | .ok expr =>
      if !literalsValid expr then .semanticError (.arithmetic Error.literalRange.code) state []
      else
        let result := run fuel expr state.variables
        let variables := canonical result.store
        match result.result with
        | .value value => .success (.mkObj [
          ("value", toJson value.toInt), ("variables", Protocol.encodeBindings variables)])
        | .error error => .semanticError (.arithmetic error.code) (updateState state result.store) []
        | .exhausted => .fuelExhausted

theorem literal_range_checked_before_lazy_evaluation :
    eval (.mkObj [("tree", .arr #[toJson "and", .arr #[toJson "lit", toJson (0 : Nat)],
      .arr #[toJson "lit", toJson (9223372036854775808 : Nat)]])]) {} 8 =
    .semanticError (.arithmetic "literal_range") {} [] := rfl

theorem decimal_plus : lookup "x" [("x", [43, 52, 50])] = 42 := rfl

theorem invalid_utf8_variable : lookup "x" [("x", [255])] = 0 := rfl

theorem variable_range : lookup "x" [("x", [57, 50, 50, 51, 51, 55, 50, 48, 51, 54,
    56, 53, 52, 55, 55, 53, 56, 48, 56])] = 0 := rfl

theorem nonnumeric_variable : lookup "x" [("x", [97])] = 0 := rfl

theorem remainder_truncates_toward_zero : binary .rem (wrap (-5)) 2 = .ok (wrap (-1)) := rfl

theorem multiplication_wraps : binary .mul (wrap maximum) 2 = .ok (wrap (-2)) := rfl

theorem compound_reads_destination_after_rhs :
    run 8 (.compound .add "x" (.assign "x" (.literal 2))) [("x", [55])] =
    ⟨.value 4, [("x", [52])]⟩ := rfl

theorem failed_compound_does_not_rollback_rhs :
    run 8 (.compound .div "x" (.assign "x" (.literal 0))) [("x", [55])] =
    ⟨.error .divisionByZero, [("x", [48])]⟩ := rfl

theorem comparison_has_no_compound_assignment :
    decodeTree 4 (.arr #[toJson "compound", toJson "==", toJson "x",
      .arr #[toJson "lit", toJson (1 : Nat)]]) = .error "operator has no compound assignment" := rfl

def evalRequest (request : Protocol.Request) : Outcome Json :=
  eval request.input request.state request.fuel

end IshSemantics.Arithmetic
