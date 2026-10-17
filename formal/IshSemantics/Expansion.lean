import IshSemantics.Protocol
import IshSemantics.Evaluation
namespace IshSemantics.Expansion
open Lean
structure Atom where
  byte : UInt8
  quotedFlag : Bool
  deriving Repr, BEq
abbrev Field := List Atom
def protect (quoted : Bool) (bytes : Bytes) : Field := bytes.map fun byte => ⟨byte, quoted⟩
def erase (field : Field) : Bytes := field.map Atom.byte
def lookup (bindings : Bindings) (name : String) : Option Bytes :=
  (bindings.find? fun entry => entry.1 == name).map Prod.snd
def resolve (state : ShellState) (name : String) : Option Bytes :=
  (lookup state.variables name).orElse fun _ =>
    (lookup state.exported name).orElse fun _ =>
      (state.inherited.find? fun entry => entry.1 == name.toUTF8.toList).map Prod.snd

def parameter (value : Option Bytes) (fallback : Option (Bool × Bytes)) : Bytes :=
  match fallback with
  | none => value.getD []
  | some (colon, bytes) => match value with
    | none => bytes
    | some present => if colon && present.isEmpty then bytes else present
inductive Part where
  | literal (bytes : Bytes)
  | quoted (bytes : Bytes)
  | parameter (name : String) (quoted : Bool) (fallback : Option (Bool × Bytes))
  | at
  | badSubstitution (name : String)
  | tilde (user : Bytes)
  | substitution (id : String) (quoted : Bool)
  | arithmetic (id : String) (quoted : Bool)
  deriving Repr
structure Glob where
  key : Bytes
  quoted : List Bool
  results : List Bytes
  deriving Repr
structure Effect where
  status : Option UInt8 := none
  bindings : Bindings := []
  error : Option SemanticError := none
  deriving Repr, BEq

/-- Words describe parameters, quoting, byte-preserving UTF-8 or invalid-byte IFS,
positionals, tilde, and effect observations independently of production parsing.
Substitution bytes are captured stdout before newline removal; arithmetic bytes
are the decimal result. Substitution effects update status but not parent bindings;
arithmetic effects may update bindings, including on failure. Pathname observations
give ordered matches for bytes and their quote mask. The trace projection omits
child I/O; it makes no claim about external effect order. -/
structure Input where
  words : List (List Part)
  observations : Bindings
  globs : List Glob
  effects : List (String × Effect) := []
  deriving Repr
structure Accumulator where
  done : List Field := []
  current : Option Field := none
  deriving Repr, BEq
def append (acc : Accumulator) (field : Field) : Accumulator :=
  { acc with current := some (acc.current.getD [] ++ field) }
def boundary (force : Bool) (acc : Accumulator) : Accumulator :=
  match acc.current with
  | some field => ⟨acc.done ++ [field], none⟩
  | none => if force then ⟨acc.done ++ [[]], none⟩ else acc
def finish (acc : Accumulator) : List Field := acc.done ++ acc.current.toList
def continuation (byte : UInt8) : Bool := 128 ≤ byte.toNat && byte.toNat ≤ 191

def validTwo (a b : UInt8) : Bool :=
  194 ≤ a.toNat && a.toNat ≤ 223 && continuation b

def validThree (a b c : UInt8) : Bool :=
  224 ≤ a.toNat && a.toNat ≤ 239 && continuation b && continuation c &&
    (a != 224 || 160 ≤ b.toNat) && (a != 237 || b.toNat ≤ 159)

def validFour (a b c d : UInt8) : Bool :=
  240 ≤ a.toNat && a.toNat ≤ 244 && continuation b && continuation c && continuation d &&
    (a != 240 || 144 ≤ b.toNat) && (a != 244 || b.toNat ≤ 143)

def units : Bytes → List Bytes
  | [] => []
  | [a] => [[a]]
  | [a, b] => if validTwo a b then [[a, b]] else [[a], [b]]
  | [a, b, c] =>
    if validThree a b c then [[a, b, c]]
    else if validTwo a b then [[a, b], [c]]
    else [a] :: units [b, c]
  | a :: b :: c :: d :: rest =>
    if validFour a b c d then [a, b, c, d] :: units rest
    else if validThree a b c then [a, b, c] :: units (d :: rest)
    else if validTwo a b then [a, b] :: units (c :: d :: rest)
    else [a] :: units (b :: c :: d :: rest)
termination_by bytes => bytes.length

def whitespace (unit : Bytes) : Bool :=
  match unit with
  | [byte] => byte == 32 || (9 ≤ byte.toNat && byte.toNat ≤ 13)
  | [194, 133] | [194, 160] | [225, 154, 128] | [226, 128, 168]
  | [226, 128, 169] | [226, 128, 175] | [226, 129, 159] | [227, 128, 128] => true
  | [226, 128, byte] => 128 ≤ byte.toNat && byte.toNat ≤ 138
  | _ => false

def ifsWhitespace (ifs : List Bytes) (unit : Bytes) : Bool := ifs.contains unit && whitespace unit

def ifsOther (ifs : List Bytes) (unit : Bytes) : Bool := ifs.contains unit && !whitespace unit

theorem dropWhile_length {α : Type} (p : α → Bool) (values : List α) :
    (values.dropWhile p).length ≤ values.length := by
  induction values with
  | nil => simp
  | cons value rest ih =>
    simp only [List.dropWhile_cons]
    split <;> simp_all <;> omega

def afterWhitespace (ifs rest : List Bytes) : List Bytes :=
  match rest.dropWhile (ifsWhitespace ifs) with
  | [] => []
  | next :: more => if ifsOther ifs next then more.dropWhile (ifsWhitespace ifs)
      else next :: more

theorem afterWhitespace_length (ifs rest : List Bytes) :
    (afterWhitespace ifs rest).length ≤ rest.length := by
  have h := dropWhile_length (ifsWhitespace ifs) rest
  unfold afterWhitespace
  split
  · simp
  · rename_i next more eq
    rw [eq] at h
    split
    · have hm := dropWhile_length (ifsWhitespace ifs) more
      simp only [List.length_cons] at h
      omega
    · exact h

def splitTokens (ifs : List Bytes) : List Bytes → List (Option Bytes × Bool)
  | [] => []
  | unit :: rest =>
    if ifsWhitespace ifs unit then
      (none, false) :: splitTokens ifs (afterWhitespace ifs rest)
    else if ifsOther ifs unit then
      (none, true) :: splitTokens ifs (rest.dropWhile (ifsWhitespace ifs))
    else (some unit, false) :: splitTokens ifs rest
termination_by values => values.length
decreasing_by
  · have h := afterWhitespace_length ifs rest
    simp only [List.length_cons]
    omega
  · have h := dropWhile_length (ifsWhitespace ifs) rest
    simp only [List.length_cons]
    omega
  · simp

def split (ifs bytes : Bytes) (acc : Accumulator) : Accumulator :=
  (splitTokens (units ifs) (units bytes)).foldl (fun acc (unit, force) => match unit with
    | some unit => append acc (protect false unit)
    | none => boundary force acc) acc
def insertValue (quoted : Bool) (ifs bytes : Bytes) (acc : Accumulator) : Accumulator :=
  if quoted || ifs.isEmpty then append acc (protect quoted bytes) else split ifs bytes acc
def positional (args : List Bytes) (acc : Accumulator) : Accumulator :=
  match args with
  | [] => acc
  | first :: rest => rest.foldl
      (fun acc bytes => append (boundary false acc) (protect true bytes))
      (append acc (protect true first))
def stripNewlines (bytes : Bytes) : Bytes := (bytes.reverse.dropWhile (· == 10)).reverse

inductive ExpansionFailure where
  | missingOracle (reason : String)
  | semantic (error : SemanticError)
  deriving Repr, BEq

def decimal (value : Nat) : Bytes := (toString value).toUTF8.toList

def positionalIndex (name : String) : Option Nat :=
  if name == "1" then some 0 else if name == "2" then some 1
  else if name == "3" then some 2 else if name == "4" then some 3
  else if name == "5" then some 4 else if name == "6" then some 5
  else if name == "7" then some 6 else if name == "8" then some 7
  else if name == "9" then some 8 else none

def parameterValue (input : Input) (state : ShellState) (name : String) :
    Except ExpansionFailure (Option Bytes) := do
  if name == "?" then return some (decimal state.lastStatus.toNat)
  if name == "#" then return some (decimal state.positional.length)
  if name == "!" then return lookup input.observations "parameter:!"
  if name == "$" || name == "0" || name == "-" then
    let some value := lookup input.observations ("parameter:" ++ name)
      | throw (.missingOracle ("missing special parameter oracle: " ++ name))
    return some value
  match positionalIndex name with
  | some index => return state.positional[index]?
  | none => return resolve state name

def joinPositionals (ifs : Bytes) (args : List Bytes) : Bytes :=
  List.intercalate ((units ifs).headD []) args

def effectFor (input : Input) (id : String) : Effect :=
  ((input.effects.find? (fun entry => entry.1 == id)).map Prod.snd).getD {}

def applyEffect (state : ShellState) (effect : Effect) : ShellState :=
  let state := Evaluation.assign state effect.bindings
  { state with lastStatus := effect.status.getD state.lastStatus }

inductive Action where
  | append (bytes : Bytes) (quoted : Bool)
  | split (bytes : Bytes)
  | boundary
  deriving Repr, BEq

def valueActions (quoted : Bool) (bytes : Bytes) : List Action :=
  [if quoted then .append bytes true else .split bytes]

def positionalActions (quoted : Bool) : List Bytes → List Action
  | [] => []
  | first :: rest => .append first quoted :: rest.flatMap (fun bytes => [.boundary, .append bytes quoted])

def partActions (input : Input) (state : ShellState) (part : Part) :
    Except ExpansionFailure (List Action) := do
  let ifs := (resolve state "IFS").getD [32, 9, 10]
  match part with
  | .literal bytes => return [.append bytes false]
  | .quoted bytes => return [.append bytes true]
  | .parameter name quoted fallback =>
    if fallback.isNone && name == "@" then return positionalActions quoted state.positional
    if fallback.isNone && name == "*" then
      return if quoted then valueActions true (joinPositionals ifs state.positional)
        else state.positional.flatMap (fun bytes => [.boundary, .split bytes])
    let value ← parameterValue input state name
    return valueActions quoted (parameter value fallback)
  | .badSubstitution name => throw (.semantic (.expansion (name ++ ": bad substitution")))
  | .at => return positionalActions true state.positional
  | .tilde user =>
    let bytes := if user.isEmpty then (resolve state "HOME").getD [126] else 126 :: user
    return [.append bytes true]
  | .substitution id quoted =>
    let some bytes := lookup input.observations id
      | throw (.missingOracle s!"missing substitution oracle: {id}")
    if let some error := (effectFor input id).error then throw (.semantic error)
    return valueActions quoted (stripNewlines bytes)
  | .arithmetic id quoted =>
    let some bytes := lookup input.observations id
      | throw (.missingOracle s!"missing arithmetic oracle: {id}")
    if let some error := (effectFor input id).error then throw (.semantic error)
    return valueActions quoted bytes

def perform (ifs : Bytes) (acc : Accumulator) : Action → Accumulator
  | .append bytes quoted => append acc (protect quoted bytes)
  | .split bytes => insertValue false ifs bytes acc
  | .boundary => boundary false acc

def performAll (ifs : Bytes) (actions : List Action) (acc : Accumulator) : Accumulator :=
  actions.foldl (perform ifs) acc

def activeGlob (field : Field) : Bool := field.any fun atom =>
  !atom.quotedFlag && (atom.byte == 42 || atom.byte == 63 || atom.byte == 91)
def pathname (globs : List Glob) (field : Field) : Except ExpansionFailure (List Bytes) := do
  if !activeGlob field then return [erase field]
  let some observation := globs.find? (fun entry => entry.key == erase field && entry.quoted == field.map Atom.quotedFlag)
    | throw (.missingOracle "missing pathname oracle")
  return if observation.results.isEmpty then [erase field] else observation.results

structure Staged where
  state : ShellState
  actions : List Action := []
  deriving Repr, BEq

structure Result where
  fields : List Bytes
  state : ShellState
  trace : Trace := []
  deriving Repr, BEq

def partEffect (input : Input) : Part → Effect
  | .substitution id _ => { effectFor input id with bindings := [] }
  | .arithmetic id _ => effectFor input id
  | _ => {}

def stagePart (input : Input) (staged : Staged) (part : Part) : Outcome Staged :=
  let state := applyEffect staged.state (partEffect input part)
  match partActions input staged.state part with
  | .ok actions => .success ⟨state, staged.actions ++ actions⟩
  | .error (.missingOracle reason) => .unsupportedDomain reason
  | .error (.semantic error) => .semanticError error state []

def stageWord (input : Input) : Staged → List Part → Outcome Staged
  | staged, [] => .success staged
  | staged, part :: rest =>
    match stagePart input staged part with
    | .success next => stageWord input next rest
    | .semanticError error state trace => .semanticError error state trace
    | .unsupportedDomain reason => .unsupportedDomain reason
    | .fuelExhausted => .fuelExhausted

def expandWord (input : Input) (state : ShellState) (word : List Part) : Outcome Result :=
  match stageWord input ⟨state, []⟩ word with
  | .success staged =>
    let acc := performAll ((resolve staged.state "IFS").getD [32, 9, 10]) staged.actions {}
    match (finish acc).mapM (pathname input.globs) with
    | .ok fields => .success ⟨fields.flatten, staged.state, []⟩
    | .error (.missingOracle reason) => .unsupportedDomain reason
    | .error (.semantic error) => .semanticError error staged.state []
  | .semanticError error state trace => .semanticError error state trace
  | .unsupportedDomain reason => .unsupportedDomain reason
  | .fuelExhausted => .fuelExhausted

def expand (input : Input) : ShellState → List (List Part) → Outcome Result
  | state, [] => .success ⟨[], state, []⟩
  | state, word :: rest =>
    match expandWord input state word with
    | .success first =>
      match expand input first.state rest with
      | .success later => .success { later with fields := first.fields ++ later.fields }
      | .semanticError error state trace => .semanticError error state trace
      | .unsupportedDomain reason => .unsupportedDomain reason
      | .fuelExhausted => .fuelExhausted
    | .semanticError error state trace => .semanticError error state trace
    | .unsupportedDomain reason => .unsupportedDomain reason
    | .fuelExhausted => .fuelExhausted

def ordinaryName (name : String) : Bool :=
  name.toList.head?.any (fun ch => ch.isAlpha && ch.toNat < 128 || ch == '_') &&
    name.toList.all (fun ch => ch.isAlphanum && ch.toNat < 128 || ch == '_')

def decodePart (value : Json) : Except String Part := do
  let tag ← (← value.getObjVal? "tag").getStr?
  match tag with
  | "literal" | "quoted" =>
    Protocol.exactObject ["tag", "bytes"] value
    let bytes ← Protocol.decodeBytes (← value.getObjVal? "bytes")
    return if tag == "literal" then .literal bytes else .quoted bytes
  | "parameter" =>
    Protocol.exactObject ["tag", "name", "quoted", "default"] value
    let name ← (← value.getObjVal? "name").getStr?
    unless (["?", "#", "$", "-", "!", "@", "*"].contains name) ||
        (!name.isEmpty && name.toList.all Char.isDigit) ||
        ordinaryName name do
      throw "invalid parameter name"
    let quoted ← (← value.getObjVal? "quoted").getBool?
    let fallback ← value.getObjVal? "default"
    let fallback ← if fallback == .null then pure none else do
      Protocol.exactObject ["colon", "bytes"] fallback
      pure (some (← (← fallback.getObjVal? "colon").getBool?,
        ← Protocol.decodeBytes (← fallback.getObjVal? "bytes")))
    return .parameter name quoted fallback
  | "bad_substitution" =>
    Protocol.exactObject ["tag", "name"] value
    return .badSubstitution (← (← value.getObjVal? "name").getStr?)
  | "at" =>
    Protocol.exactObject ["tag"] value
    return .at
  | "tilde" =>
    Protocol.exactObject ["tag", "user"] value
    return .tilde (← Protocol.decodeBytes (← value.getObjVal? "user"))
  | "substitution" | "arithmetic" =>
    Protocol.exactObject ["tag", "id", "quoted"] value
    let id ← (← value.getObjVal? "id").getStr?
    let quoted ← (← value.getObjVal? "quoted").getBool?
    return if tag == "substitution" then .substitution id quoted else .arithmetic id quoted
  | _ => throw "unsupported expansion part"
def decodeInput (value : Json) : Except String Input := do
  Protocol.exactObject ["words", "observations", "globs"] value
  let words ← (← (← value.getObjVal? "words").getArr?).toList.mapM fun word => do
    (← word.getArr?).toList.mapM decodePart
  let entries ← (← (← value.getObjVal? "observations").getArr?).toList.mapM fun observation => do
    let object ← observation.getObj?
    for (key, _) in object.toList do
      unless ["id", "bytes", "status", "bindings", "error"].contains key do
        throw s!"unknown field: {key}"
    let id ← (← observation.getObjVal? "id").getStr?
    let bytes ← Protocol.decodeBytes (← observation.getObjVal? "bytes")
    let status ← match observation.getObjVal? "status" with
      | .error _ => pure none
      | .ok status => do
        let value ← status.getNat?
        unless value < 256 do throw "oracle status outside 0..255"
        pure (some value.toUInt8)
    let bindings ← match observation.getObjVal? "bindings" with
      | .error _ => pure []
      | .ok bindings => do
        let pairs ← (← bindings.getArr?).toList.mapM (Protocol.decodePair Json.getStr? Protocol.decodeBytes)
        unless pairs.all (fun pair => ordinaryName pair.1) do throw "invalid oracle binding name"
        pure pairs
    let error ← match observation.getObjVal? "error" with
      | .error _ => pure none
      | .ok error => do
        Protocol.exactObject ["kind", "message"] error
        let kind ← (← error.getObjVal? "kind").getStr?
        let message ← (← error.getObjVal? "message").getStr?
        match kind with
        | "arithmetic" => pure (some (SemanticError.arithmetic message))
        | "expansion" => pure (some (SemanticError.expansion message))
        | _ => throw "invalid oracle error kind"
    return (id, bytes, { status, bindings, error : Effect })
  let observations := entries.map (fun (id, bytes, _) => (id, bytes))
  let effects := entries.map (fun (id, _, effect) => (id, effect))
  for (id, _, effect) in entries do
    if id.startsWith "sub" && !effect.bindings.isEmpty then
      throw "command substitution cannot change parent bindings"
  unless (observations.map Prod.fst).eraseDups.length == observations.length do
    throw "duplicate observation id"
  let globs ← (← (← value.getObjVal? "globs").getArr?).toList.mapM fun glob => do
    Protocol.exactObject ["pattern", "quoted", "matches"] glob
    let key ← Protocol.decodeBytes (← glob.getObjVal? "pattern")
    let quoted ← (← (← glob.getObjVal? "quoted").getArr?).toList.mapM Json.getBool?
    unless key.length == quoted.length do throw "pathname quote mask length"
    let results ← (← (← glob.getObjVal? "matches").getArr?).toList.mapM Protocol.decodeBytes
    return (⟨key, quoted, results⟩ : Glob)
  unless (globs.map (fun glob => (glob.key, glob.quoted))).eraseDups.length == globs.length do throw "duplicate glob pattern"
  return { words, observations, globs, effects }
def evalRequest (request : Protocol.Request) : Outcome Json :=
  if request.family != .expansion then .unsupportedDomain "expected expansion family"
  else match decodeInput request.input with
  | .error reason => .unsupportedDomain reason
  | .ok input =>
    if request.fuel < input.words.foldl (fun count word => count + word.length + 1) 1 then .fuelExhausted
    else match expand input request.state input.words with
    | .success result => .success (.mkObj [
        ("fields", .arr (result.fields.map Protocol.encodeBytes).toArray),
        ("state", Protocol.encodeState result.state),
        ("trace", .arr (result.trace.map Protocol.encodeTraceEvent).toArray)])
    | .semanticError error state trace => .semanticError error state trace
    | .unsupportedDomain reason => .unsupportedDomain reason
    | .fuelExhausted => .fuelExhausted

theorem byte_preservation (quoted : Bool) (bytes : Bytes) : erase (protect quoted bytes) = bytes := by
  simp [erase, protect, List.map_map, Function.comp_def]
theorem quotedFlag_no_glob (bytes : Bytes) : activeGlob (protect true bytes) = false := by
  simp [activeGlob, protect, List.any_map, Function.comp_def]
theorem quoted_pathname_preservation (bytes : Bytes) (globs : List Glob) :
    pathname globs (protect true bytes) = .ok [bytes] := by
  simp [pathname, quotedFlag_no_glob, byte_preservation]
  rfl
theorem unset_default (colon : Bool) (fallback : Bytes) :
    parameter none (some (colon, fallback)) = fallback := by rfl
theorem empty_without_colon (fallback : Bytes) : parameter (some []) (some (false, fallback)) = [] := by rfl
theorem empty_with_colon (fallback : Bytes) : parameter (some []) (some (true, fallback)) = fallback := by rfl
theorem append_associative (acc : Accumulator) (left right : Field) :
    append (append acc left) right = append acc (left ++ right) := by
  simp [append, List.append_assoc]
theorem positional_zero (acc : Accumulator) : positional [] acc = acc := by rfl
theorem positional_one (bytes : Bytes) : finish (positional [bytes] {}) = [protect true bytes] := by rfl
theorem positional_two (first second : Bytes) :
    finish (positional [first, second] {}) = [protect true first, protect true second] := by
  simp [positional, append, boundary, finish]
theorem boundary_keeps_order (done : List Field) (field : Field) :
    finish (boundary false ⟨done, some field⟩) = done ++ [field] := by simp [finish, boundary]
theorem trailing_newline_removed (bytes : Bytes) : stripNewlines (bytes ++ [10]) = stripNewlines bytes := by
  simp [stripNewlines, List.reverse_append]
example : stripNewlines [255, 10, 0, 10, 10] = [255, 10, 0] := by decide
example : (finish (split [32] [32, 97, 32, 32, 98, 32] {})).map erase = [[97], [98]] := by simp [split, units, validTwo, validThree, validFour, continuation, protect, splitTokens, afterWhitespace, ifsWhitespace, ifsOther, whitespace, append, boundary, finish, erase]
example : (finish (split [58] [58, 97, 58, 58, 98, 58] {})).map erase = [[], [97], [], [98]] := by simp [split, units, validTwo, validThree, validFour, continuation, protect, splitTokens, ifsWhitespace, ifsOther, whitespace, append, boundary, finish, erase]
example : (finish (split [32, 58] [97, 32, 58, 32, 98] {})).map erase = [[97], [98]] := by simp [split, units, validTwo, validThree, validFour, continuation, protect, splitTokens, afterWhitespace, ifsWhitespace, ifsOther, whitespace, append, boundary, finish, erase]
example : (finish (positional [[], [255], []] {})).map erase = [[], [255], []] := by decide
example : pathname [⟨[42], [false], [[97]]⟩] (protect true [42]) = .ok [[42]] := by rfl
example : pathname [⟨[42], [false], []⟩] (protect false [42]) = .ok [[42]] := by rfl
end IshSemantics.Expansion

namespace IshSemantics.Expansion

theorem quoted_ignores_ifs (ifs bytes : Bytes) (acc : Accumulator) :
    insertValue true ifs bytes acc = append acc (protect true bytes) := by
  simp [insertValue]

theorem empty_ifs_preserves_value (quoted : Bool) (bytes : Bytes) (acc : Accumulator) :
    insertValue quoted [] bytes acc = append acc (protect quoted bytes) := by
  simp [insertValue]

theorem nonwhite_delimiter_keeps_empty (done : List Field) :
    finish (boundary true ⟨done, none⟩) = done ++ [[]] := by
  simp [boundary, finish]

theorem whitespace_delimiter_discards_absence (done : List Field) :
    boundary false ⟨done, none⟩ = ⟨done, none⟩ := by rfl

theorem positional_tail_order (args : List Bytes) (done : List Field) (field : Field) :
    finish (args.foldl (fun acc bytes => append (boundary false acc) (protect true bytes))
      ⟨done, some field⟩) = done ++ field :: args.map (protect true) := by
  induction args generalizing done field with
  | nil => simp [finish]
  | cons head tail ih =>
    simp only [List.foldl_cons]
    change finish (tail.foldl (fun acc bytes => append (boundary false acc) (protect true bytes))
      ⟨done ++ [field], some (protect true head)⟩) = _
    rw [ih]
    simp [List.append_assoc]

theorem positional_order (args : List Bytes) :
    finish (positional args {}) = args.map (protect true) := by
  cases args with
  | nil => rfl
  | cons head tail =>
    change finish (tail.foldl (fun acc bytes => append (boundary false acc) (protect true bytes))
      ⟨[], some (protect true head)⟩) = _
    rw [positional_tail_order]
    rfl

theorem positional_count (args : List Bytes) :
    (finish (positional args {})).length = args.length := by
  rw [positional_order, List.length_map]

theorem preserve_nonnewline_tail (leading : Bytes) (byte : UInt8) (h : byte ≠ 10) :
    stripNewlines (leading ++ [byte]) = leading ++ [byte] := by
  simp [stripNewlines, List.reverse_append, h]

theorem unmatched_pathname (field : Field) (globs : List Glob) (observation : Glob)
    (active : activeGlob field = true)
    (found : globs.find? (fun entry => entry.key == erase field && entry.quoted == field.map Atom.quotedFlag)
      = some observation)
    (empty : observation.results = []) : pathname globs field = .ok [erase field] := by
  simp [pathname, active, found, empty]
  rfl

example : (finish (append (positional [] (append {} (protect true [112]))) (protect true [115]))).map erase
    = [[112, 115]] := by decide
example : partActions ⟨[], [], [], []⟩ {} (.substitution "missing" true) = .error (.missingOracle "missing substitution oracle: missing") := by rfl
example : (match decodePart (.mkObj [("tag", .str "at"), ("extra", .null)]) with
    | .error message => message == "unknown field: extra"
    | .ok _ => false) = true := by decide

end IshSemantics.Expansion

namespace IshSemantics.Expansion

theorem split_whitespace_only (ifs unit : Bytes)
    (single : units unit = [unit]) (isWhitespace : ifsWhitespace (units ifs) unit = true) :
    finish (split ifs unit {}) = [] := by
  simp [split, single, splitTokens, isWhitespace, afterWhitespace, boundary, finish]

theorem split_nonwhite_only (ifs unit : Bytes)
    (single : units unit = [unit])
    (notWhitespace : ifsWhitespace (units ifs) unit = false)
    (isOther : ifsOther (units ifs) unit = true) :
    finish (split ifs unit {}) = [[]] := by
  simp [split, single, splitTokens, notWhitespace, isOther, boundary, finish]

end IshSemantics.Expansion

namespace IshSemantics.Expansion

theorem trailing_newlines_removed (bytes : Bytes) (count : Nat) :
    stripNewlines (bytes ++ List.replicate count 10) = stripNewlines bytes := by
  induction count with
  | zero => simp
  | succ count ih =>
    rw [← List.replicate_append_replicate (n := count) (m := 1), List.replicate_one, ← List.append_assoc,
      trailing_newline_removed, ih]

theorem quoted_word_preservation (input : Input) (state : ShellState) (bytes : Bytes) :
    expandWord input state [.quoted bytes] = .success ⟨[bytes], state, []⟩ := by
  have staged : stageWord input ⟨state, []⟩ [.quoted bytes] =
      .success ⟨state, [.append bytes true]⟩ := by rfl
  have paths : [protect true bytes].mapM (pathname input.globs) = .ok [[bytes]] := by
    rw [List.mapM_cons, quoted_pathname_preservation]
    rfl
  unfold expandWord
  rw [staged]
  change (match [protect true bytes].mapM (pathname input.globs) with
    | .ok fields => Outcome.success (⟨fields.flatten, state, []⟩ : Result)
    | .error (.missingOracle reason) => .unsupportedDomain reason
    | .error (.semantic error) => .semanticError error state []) = _
  rw [paths]
  rfl

theorem tilde_user_preserved (input : Input) (state : ShellState) (user : Bytes)
    (nonempty : user.isEmpty = false) :
    expandWord input state [.tilde user] = .success ⟨[126 :: user], state, []⟩ := by
  have actions : partActions input state (.tilde user) = .ok [.append (126 :: user) true] := by
    simp [partActions, nonempty]
    rfl
  have same : stageWord input ⟨state, []⟩ [.tilde user] =
      stageWord input ⟨state, []⟩ [.quoted (126 :: user)] := by
    simp only [stageWord, stagePart]
    rw [actions]
    rfl
  unfold expandWord
  rw [same]
  exact quoted_word_preservation input state (126 :: user)

end IshSemantics.Expansion

namespace IshSemantics.Expansion
open Lean
example : (match evalRequest {
    id := "fuel", family := .expansion, source := [], state := {}, fuel := 0,
    input := .mkObj [("words", .arr #[]), ("observations", .arr #[]), ("globs", .arr #[])] } with
  | .fuelExhausted => true
  | _ => false) = true := by decide

example : (match decodeInput (.mkObj [
    ("words", .arr #[]), ("observations", .arr #[]),
    ("globs", .arr #[.mkObj [("pattern", .arr #[toJson (42 : Nat)]),
      ("quoted", .arr #[]), ("matches", .arr #[])]])]) with
  | .error message => message == "pathname quote mask length"
  | .ok _ => false) = true := by decide

example : resolve { variables := [("X", [1])], exported := [("X", [2])], inherited := [([88], [3])] } "X" = some [1] := by decide
end IshSemantics.Expansion

namespace IshSemantics.Expansion

theorem units_preserve_bytes (bytes : Bytes) : (units bytes).flatten = bytes := by
  induction bytes using units.induct <;> simp_all [units]

theorem status_parameter (input : Input) (state : ShellState) :
    parameterValue input state "?" = .ok (some (decimal state.lastStatus.toNat)) := by
  simp [parameterValue]
  rfl

theorem count_parameter (input : Input) (state : ShellState) :
    parameterValue input state "#" = .ok (some (decimal state.positional.length)) := by
  simp [parameterValue]
  rfl

theorem background_parameter (input : Input) (state : ShellState) :
    parameterValue input state "!" = .ok (lookup input.observations "parameter:!") := by
  simp [parameterValue]
  rfl

theorem first_positional_parameter (input : Input) (state : ShellState) :
    parameterValue input state "1" = .ok state.positional[0]? := by
  simp [parameterValue, positionalIndex]
  rfl

theorem bad_substitution_retains_state (input : Input) (staged : Staged) (name : String) :
    stagePart input staged (.badSubstitution name) =
      .semanticError (.expansion (name ++ ": bad substitution")) staged.state [] := by
  rfl

theorem apply_effect_status (state : ShellState) (effect : Effect) :
    (applyEffect state effect).lastStatus = effect.status.getD state.lastStatus := by
  simp only [applyEffect]
  have preserved : (Evaluation.assign state effect.bindings).lastStatus = state.lastStatus := by
    induction effect.bindings generalizing state with
    | nil => rfl
    | cons head rest ih =>
      simp only [Evaluation.assign, List.foldl_cons]
      exact ih _
  rw [preserved]

theorem readonly_effect_identity (state : ShellState) : applyEffect state {} = state := by
  rfl

theorem quoted_star_always_one_field (ifs : Bytes) (args : List Bytes) :
    finish (performAll ifs (valueActions true (joinPositionals ifs args)) {}) =
      [protect true (joinPositionals ifs args)] := by
  rfl

theorem join_no_positionals (ifs : Bytes) : joinPositionals ifs [] = [] := by rfl

theorem join_one_positional (ifs bytes : Bytes) : joinPositionals ifs [bytes] = bytes := by
  simp [joinPositionals]

theorem join_empty_ifs (args : List Bytes) : joinPositionals [] args = args.flatten := by
  induction args with
  | nil => rfl
  | cons head tail ih =>
    cases tail <;> simp_all [joinPositionals, units, List.intercalate, List.intersperse]

end IshSemantics.Expansion

namespace IshSemantics.Expansion

def continueStage (next : Staged → Outcome Staged) : Outcome Staged → Outcome Staged
  | .success staged => next staged
  | .semanticError error state trace => .semanticError error state trace
  | .unsupportedDomain reason => .unsupportedDomain reason
  | .fuelExhausted => .fuelExhausted

theorem stage_word_composition (input : Input) (first second : List Part) (staged : Staged) :
    stageWord input staged (first ++ second) =
      continueStage (fun next => stageWord input next second) (stageWord input staged first) := by
  induction first generalizing staged with
  | nil => rfl
  | cons part rest ih =>
    cases h : stagePart input staged part <;> simp [stageWord, h, continueStage, ih]

theorem stage_error_retains_prior_state (input : Input) (first rest : List Part)
    (staged : Staged) (error : SemanticError) (state : ShellState) (trace : Trace)
    (failed : stageWord input staged first = .semanticError error state trace) :
    stageWord input staged (first ++ rest) = .semanticError error state trace := by
  rw [stage_word_composition, failed]
  rfl

theorem assign_status_independent (state : ShellState) (status : UInt8) (bindings : Bindings) :
    Evaluation.assign { state with lastStatus := status } bindings =
      { Evaluation.assign state bindings with lastStatus := status } := by
  induction bindings generalizing state with
  | nil => rfl
  | cons head rest ih =>
    simpa only [Evaluation.assign, List.foldl_cons, List.foldl_nil] using
      ih (Evaluation.assign state [head])

theorem assign_append (state : ShellState) (first second : Bindings) :
    Evaluation.assign state (first ++ second) =
      Evaluation.assign (Evaluation.assign state first) second := by
  simp only [Evaluation.assign, List.foldl_append]

theorem assign_retains_status (state : ShellState) (bindings : Bindings) :
    (Evaluation.assign state bindings).lastStatus = state.lastStatus := by
  induction bindings generalizing state with
  | nil => rfl
  | cons head rest ih =>
    simpa only [Evaluation.assign, List.foldl_cons, List.foldl_nil] using
      ih (Evaluation.assign state [head])

theorem effect_composition (state : ShellState) (first second : Effect) :
    applyEffect (applyEffect state first) second = applyEffect state {
      status := second.status.orElse (fun _ => first.status),
      bindings := first.bindings ++ second.bindings } := by
  simp only [applyEffect, assign_status_independent, assign_append, assign_retains_status]
  cases first.status <;> cases second.status <;> rfl

theorem positional_actions (ifs : Bytes) (args : List Bytes) (acc : Accumulator) :
    performAll ifs (positionalActions true args) acc = positional args acc := by
  cases args with
  | nil => rfl
  | cons first rest =>
    have tail : ∀ (acc : Accumulator),
        performAll ifs (rest.flatMap (fun bytes => [.boundary, .append bytes true])) acc =
          rest.foldl (fun acc bytes => append (boundary false acc) (protect true bytes)) acc := by
      induction rest with
      | nil => intro acc; rfl
      | cons head rest ih =>
        intro acc
        simpa only [List.flatMap_cons, performAll, List.foldl_append, List.foldl_cons,
          List.foldl_nil, perform] using ih (append (boundary false acc) (protect true head))
    simpa only [positionalActions, performAll, List.foldl_cons, perform, positional] using
      tail (append acc (protect true first))

theorem quoted_positional_action_count (ifs : Bytes) (args : List Bytes) :
    (finish (performAll ifs (positionalActions true args) {})).length = args.length := by
  rw [positional_actions, positional_count]

end IshSemantics.Expansion

namespace IshSemantics.Expansion

theorem positional_parameter (input : Input) (state : ShellState) (name : String) (index : Nat)
    (found : positionalIndex name = some index) :
    parameterValue input state name = .ok state.positional[index]? := by
  unfold positionalIndex at found
  repeat' split at found
  all_goals simp_all [parameterValue, positionalIndex]
  all_goals rfl

theorem non_delimiter_unit_preserved (ifs unit : Bytes)
    (single : units unit = [unit]) (absent : (units ifs).contains unit = false) :
    finish (split ifs unit {}) = [protect false unit] := by
  have absent' : unit ∉ units ifs := by simpa using absent
  simp [split, single, splitTokens, ifsWhitespace, ifsOther, absent', append, finish]

theorem arithmetic_error_retains_updates (input : Input) (staged : Staged)
    (id : String) (quoted : Bool) (bytes : Bytes) (effect : Effect) (error : SemanticError)
    (observed : lookup input.observations id = some bytes)
    (delta : effectFor input id = effect) (failed : effect.error = some error) :
    stagePart input staged (.arithmetic id quoted) =
      .semanticError error (applyEffect staged.state effect) [] := by
  simp only [stagePart, partEffect, partActions, observed, delta, failed]
  rfl

theorem substitution_error_propagates (input : Input) (staged : Staged)
    (id : String) (quoted : Bool) (bytes : Bytes) (effect : Effect) (error : SemanticError)
    (observed : lookup input.observations id = some bytes)
    (delta : effectFor input id = effect) (failed : effect.error = some error) :
    stagePart input staged (.substitution id quoted) =
      .semanticError error (applyEffect staged.state { effect with bindings := [] }) [] := by
  simp only [stagePart, partEffect, partActions, observed, delta, failed]
  rfl

theorem substitution_isolates_bindings (input : Input) (state : ShellState)
    (id : String) (quoted : Bool) :
    (applyEffect state (partEffect input (.substitution id quoted))).variables = state.variables := by
  simp [partEffect, applyEffect, Evaluation.assign]

theorem binding_delta_last_write_wins (state : ShellState) (name : String) (first last : Bytes) :
    resolve (applyEffect state { bindings := [(name, first), (name, last)] }) name = some last := by
  simp [applyEffect, Evaluation.assign, Evaluation.setBinding, resolve, lookup]

end IshSemantics.Expansion

namespace IshSemantics.Expansion

theorem units_are_nonempty (bytes : Bytes) : [] ∉ units bytes := by
  induction bytes using units.induct <;> simp_all [units]

theorem quoted_star_first_unit (ifs left right : Bytes) :
    joinPositionals ifs [left, right] = left ++ (units ifs).headD [] ++ right := by
  simp [joinPositionals, List.intercalate]

theorem positional_action_tail (ifs : Bytes) (quoted : Bool) (args : List Bytes)
    (done : List Field) (field : Field) :
    finish (performAll ifs (args.flatMap (fun bytes => [.boundary, .append bytes quoted]))
      ⟨done, some field⟩) = done ++ field :: args.map (protect quoted) := by
  induction args generalizing done field with
  | nil => simp [performAll, finish]
  | cons head rest ih =>
    change finish (performAll ifs (rest.flatMap (fun bytes => [.boundary, .append bytes quoted]))
      ⟨done ++ [field], some (protect quoted head)⟩) = _
    rw [ih]
    simp [List.append_assoc]

theorem positional_action_order (ifs : Bytes) (quoted : Bool) (args : List Bytes) :
    finish (performAll ifs (positionalActions quoted args) {}) = args.map (protect quoted) := by
  cases args with
  | nil => rfl
  | cons head rest =>
    change finish (performAll ifs (rest.flatMap (fun bytes => [.boundary, .append bytes quoted]))
      ⟨[], some (protect quoted head)⟩) = _
    rw [positional_action_tail]
    rfl

theorem at_parameter_actions (input : Input) (state : ShellState) (quoted : Bool) :
    partActions input state (.parameter "@" quoted none) = .ok (positionalActions quoted state.positional) := by
  simp [partActions]
  rfl

end IshSemantics.Expansion
