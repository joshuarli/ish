import IshSemantics.Protocol
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
  | tilde (user : Bytes)
  | substitution (id : String) (quoted : Bool)
  | arithmetic (id : String) (quoted : Bool)
  deriving Repr
structure Glob where
  key : Bytes
  quoted : List Bool
  results : List Bytes
  deriving Repr
/-- Words describe the supported domain independently of the production parser:
ordinary parameters with normal or literal default expansion, ASCII IFS, quoting,
quoted positional expansion, tilde, and successful read-only effect observations.
Substitution observations are raw captured stdout before trailing newline removal;
arithmetic observations are the resulting decimal bytes. Pathname observations
supply authoritative ordered matches for a byte pattern and its quote mask.
The production bridge checks source parsing and the observations separately. -/
structure Input where
  words : List (List Part)
  observations : Bindings
  globs : List Glob
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
def whitespace (byte : UInt8) : Bool :=
  byte == 32 || byte == 9 || byte == 10 || byte == 11 || byte == 12 || byte == 13
def ifsWhitespace (ifs : Bytes) (byte : UInt8) : Bool := ifs.contains byte && whitespace byte
def ifsOther (ifs : Bytes) (byte : UInt8) : Bool := ifs.contains byte && !whitespace byte

theorem dropWhile_length (p : UInt8 → Bool) (bytes : Bytes) :
    (bytes.dropWhile p).length ≤ bytes.length := by
  induction bytes with
  | nil => simp
  | cons byte rest ih =>
    simp only [List.dropWhile_cons]
    split <;> simp_all <;> omega

def afterWhitespace (ifs rest : Bytes) : Bytes :=
  match rest.dropWhile (ifsWhitespace ifs) with
  | [] => []
  | next :: more => if ifsOther ifs next then more.dropWhile (ifsWhitespace ifs)
      else next :: more

theorem afterWhitespace_length (ifs rest : Bytes) :
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

def splitTokens (ifs : Bytes) : Bytes → List (Option UInt8 × Bool)
  | [] => []
  | byte :: rest =>
    if ifsWhitespace ifs byte then
      (none, false) :: splitTokens ifs (afterWhitespace ifs rest)
    else if ifsOther ifs byte then
      (none, true) :: splitTokens ifs (rest.dropWhile (ifsWhitespace ifs))
    else (some byte, false) :: splitTokens ifs rest
termination_by bytes => bytes.length
decreasing_by
  · have h := afterWhitespace_length ifs rest
    simp only [List.length_cons]
    omega
  · have h := dropWhile_length (ifsWhitespace ifs) rest
    simp only [List.length_cons]
    omega
  · simp

def split (ifs bytes : Bytes) (acc : Accumulator) : Accumulator :=
  (splitTokens ifs bytes).foldl (fun acc (byte, force) => match byte with
    | some byte => append acc [⟨byte, false⟩]
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

def expandPart (input : Input) (state : ShellState) (acc : Accumulator)
    (part : Part) : Except String Accumulator := do
  let ifs := (resolve state "IFS").getD [32, 9, 10]
  match part with
  | .literal bytes => return append acc (protect false bytes)
  | .quoted bytes => return append acc (protect true bytes)
  | .parameter name quoted fallback =>
    return insertValue quoted ifs (parameter (resolve state name) fallback) acc
  | .at => return positional state.positional acc
  | .tilde user =>
    let bytes := if user.isEmpty then (resolve state "HOME").getD [126] else 126 :: user
    return append acc (protect true bytes)
  | .substitution id quoted =>
    let some bytes := lookup input.observations id | throw s!"missing substitution oracle: {id}"
    return insertValue quoted ifs (stripNewlines bytes) acc
  | .arithmetic id quoted =>
    let some bytes := lookup input.observations id | throw s!"missing arithmetic oracle: {id}"
    return insertValue quoted ifs bytes acc

def activeGlob (field : Field) : Bool := field.any fun atom =>
  !atom.quotedFlag && (atom.byte == 42 || atom.byte == 63 || atom.byte == 91)
def pathname (globs : List Glob) (field : Field) : Except String (List Bytes) := do
  if !activeGlob field then return [erase field]
  let some observation := globs.find? (fun entry => entry.key == erase field && entry.quoted == field.map Atom.quotedFlag)
    | throw "missing pathname oracle"
  return if observation.results.isEmpty then [erase field] else observation.results

def expandWord (input : Input) (state : ShellState) (word : List Part) : Except String (List Bytes) := do
  let acc ← word.foldlM (expandPart input state) {}
  return (← (finish acc).mapM (pathname input.globs)).flatten
def expand (input : Input) (state : ShellState) : Except String (List Bytes) := do
  return (← input.words.mapM (expandWord input state)).flatten

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
    unless name.toList.head?.any (fun ch => ch.isAlpha && ch.toNat < 128 || ch == '_') &&
        name.toList.all (fun ch => ch.isAlphanum && ch.toNat < 128 || ch == '_') do
      throw "unsupported parameter name"
    let quoted ← (← value.getObjVal? "quoted").getBool?
    let fallback ← value.getObjVal? "default"
    let fallback ← if fallback == .null then pure none else do
      Protocol.exactObject ["colon", "bytes"] fallback
      pure (some (← (← fallback.getObjVal? "colon").getBool?,
        ← Protocol.decodeBytes (← fallback.getObjVal? "bytes")))
    return .parameter name quoted fallback
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
  let observations ← (← (← value.getObjVal? "observations").getArr?).toList.mapM fun observation => do
    Protocol.exactObject ["id", "bytes"] observation
    return (← (← observation.getObjVal? "id").getStr?,
      ← Protocol.decodeBytes (← observation.getObjVal? "bytes"))
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
  return { words, observations, globs }
def evalRequest (request : Protocol.Request) : Outcome Json :=
  if request.family != .expansion then .unsupportedDomain "expected expansion family"
  else match decodeInput request.input with
  | .error reason => .unsupportedDomain reason
  | .ok input =>
    if ((resolve request.state "IFS").getD []).any (fun byte => byte.toNat ≥ 128) then
      .unsupportedDomain "non-ASCII IFS"
    else if request.fuel < input.words.foldl (fun count word => count + word.length + 1) 1 then .fuelExhausted
    else match expand input request.state with
    | .error reason => .unsupportedDomain reason
    | .ok fields => .success (.mkObj [("fields", .arr (fields.map Protocol.encodeBytes).toArray)])

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
example : (finish (split [32] [32, 97, 32, 32, 98, 32] {})).map erase = [[97], [98]] := by simp [split, splitTokens, afterWhitespace, ifsWhitespace, ifsOther, whitespace, append, boundary, finish, erase]
example : (finish (split [58] [58, 97, 58, 58, 98, 58] {})).map erase = [[], [97], [], [98]] := by simp [split, splitTokens, ifsWhitespace, ifsOther, whitespace, append, boundary, finish, erase]
example : (finish (split [32, 58] [97, 32, 58, 32, 98] {})).map erase = [[97], [98]] := by simp [split, splitTokens, afterWhitespace, ifsWhitespace, ifsOther, whitespace, append, boundary, finish, erase]
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
example : expandPart ⟨[], [], []⟩ {} {} (.substitution "missing" true) = .error "missing substitution oracle: missing" := by rfl
example : (match decodePart (.mkObj [("tag", .str "at"), ("extra", .null)]) with
    | .error message => message == "unknown field: extra"
    | .ok _ => false) = true := by decide

end IshSemantics.Expansion

namespace IshSemantics.Expansion

theorem split_whitespace_only (ifs : Bytes) (byte : UInt8)
    (isWhitespace : ifsWhitespace ifs byte = true) :
    finish (split ifs [byte] {}) = [] := by
  simp [split, splitTokens, isWhitespace, afterWhitespace, boundary, finish]

theorem split_nonwhite_only (ifs : Bytes) (byte : UInt8)
    (notWhitespace : ifsWhitespace ifs byte = false)
    (isOther : ifsOther ifs byte = true) :
    finish (split ifs [byte] {}) = [[]] := by
  simp [split, splitTokens, notWhitespace, isOther, boundary, finish]

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
    expandWord input state [.quoted bytes] = .ok [bytes] := by
  simp [expandWord, expandPart, append, finish, quoted_pathname_preservation]

theorem tilde_user_preserved (input : Input) (state : ShellState) (user : Bytes)
    (nonempty : user.isEmpty = false) :
    expandWord input state [.tilde user] = .ok [126 :: user] := by
  simp [expandWord, expandPart, nonempty, append, finish, quoted_pathname_preservation]

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
