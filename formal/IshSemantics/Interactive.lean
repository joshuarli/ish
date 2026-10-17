import IshSemantics.Protocol

namespace IshSemantics.Interactive
open IshSemantics Lean

structure InteractiveState where
  text : List Char
  cursor : Nat
  killRing : List Char
  deriving Repr, BEq, Inhabited

inductive InteractiveOp where
  | insertChar (value : Char)
  | insertString (value : List Char)
  | deleteBack
  | deleteForward
  | moveLeft
  | moveRight
  | moveHome
  | moveEnd
  | moveWordLeft
  | moveWordRight
  | killToStart
  | killToEnd
  | killWordBack
  | killWordForward
  | yank
  deriving Repr, BEq, Inhabited

def validCursor (state : InteractiveState) : Prop :=
  state.cursor ≤ state.text.length

def stateWith (text : List Char) (cursor : Nat) (killRing : List Char) : InteractiveState :=
  { text, cursor := min cursor text.length, killRing }

@[simp]
theorem stateWith_validCursor (text : List Char) (cursor : Nat) (killRing : List Char) :
    validCursor (stateWith text cursor killRing) := by
  exact Nat.min_le_right _ _

def codepointInRange (value lower upper : Nat) : Bool :=
  decide (lower ≤ value ∧ value ≤ upper)

def rustWhitespace (value : Char) : Bool :=
  let codepoint := value.toNat
  codepointInRange codepoint 0x0009 0x000D ||
  codepoint == 0x0020 ||
  codepoint == 0x0085 ||
  codepoint == 0x00A0 ||
  codepoint == 0x1680 ||
  codepointInRange codepoint 0x2000 0x200A ||
  codepoint == 0x2028 ||
  codepoint == 0x2029 ||
  codepoint == 0x202F ||
  codepoint == 0x205F ||
  codepoint == 0x3000

def notRustWhitespace (value : Char) : Bool :=
  !rustWhitespace value

def wordBackStart (text : List Char) (cursor : Nat) : Nat :=
  let reversedPrefix := (text.take cursor).reverse
  let whitespace := reversedPrefix.takeWhile rustWhitespace
  let word := (reversedPrefix.drop whitespace.length).takeWhile notRustWhitespace
  cursor - whitespace.length - word.length

def wordForwardEnd (text : List Char) (cursor : Nat) : Nat :=
  let suffix := text.drop cursor
  let whitespace := suffix.takeWhile rustWhitespace
  let word := (suffix.drop whitespace.length).takeWhile notRustWhitespace
  cursor + whitespace.length + word.length

def wordMoveForwardEnd (text : List Char) (cursor : Nat) : Nat :=
  let suffix := text.drop cursor
  let word := suffix.takeWhile notRustWhitespace
  let whitespace := (suffix.drop word.length).takeWhile rustWhitespace
  cursor + word.length + whitespace.length

def cursorAtOrBeforeByte : List Char → Nat → Nat
  | [], _ => 0
  | value :: rest, byteCursor =>
      if byteCursor < value.utf8Size then
        0
      else
        1 + cursorAtOrBeforeByte rest (byteCursor - value.utf8Size)

def fromString (source : String) (byteCursor : Nat) : InteractiveState :=
  stateWith source.toList (cursorAtOrBeforeByte source.toList byteCursor) []

def textString (state : InteractiveState) : String :=
  String.ofList state.text

def textBytes (state : InteractiveState) : List UInt8 :=
  (textString state).toUTF8.data.toList

def byteCursor (state : InteractiveState) : Nat :=
  (String.ofList (state.text.take state.cursor)).utf8ByteSize

def byteLength (state : InteractiveState) : Nat :=
  (String.ofList state.text).utf8ByteSize

def step (state : InteractiveState) (operation : InteractiveOp) : InteractiveState :=
  let cursor := min state.cursor state.text.length
  match operation with
  | .insertChar value =>
      stateWith (state.text.take cursor ++ [value] ++ state.text.drop cursor)
        (cursor + 1) state.killRing
  | .insertString value =>
      stateWith (state.text.take cursor ++ value ++ state.text.drop cursor)
        (cursor + value.length) state.killRing
  | .deleteBack =>
      if cursor = 0 then
        stateWith state.text cursor state.killRing
      else
        stateWith (state.text.take (cursor - 1) ++ state.text.drop cursor)
          (cursor - 1) state.killRing
  | .deleteForward =>
      if cursor = state.text.length then
        stateWith state.text cursor state.killRing
      else
        stateWith (state.text.take cursor ++ state.text.drop (cursor + 1))
          cursor state.killRing
  | .moveLeft =>
      stateWith state.text (if cursor = 0 then cursor else cursor - 1) state.killRing
  | .moveRight =>
      stateWith state.text
        (if cursor = state.text.length then cursor else cursor + 1) state.killRing
  | .moveHome => stateWith state.text 0 state.killRing
  | .moveEnd => stateWith state.text state.text.length state.killRing
  | .moveWordLeft => stateWith state.text (wordBackStart state.text cursor) state.killRing
  | .moveWordRight => stateWith state.text (wordMoveForwardEnd state.text cursor) state.killRing
  | .killToStart =>
      if cursor = 0 then
        stateWith state.text cursor state.killRing
      else
        stateWith (state.text.drop cursor) 0 (state.text.take cursor)
  | .killToEnd =>
      if cursor = state.text.length then
        stateWith state.text cursor state.killRing
      else
        stateWith (state.text.take cursor) cursor (state.text.drop cursor)
  | .killWordBack =>
      let start := wordBackStart state.text cursor
      if start = cursor then
        stateWith state.text cursor state.killRing
      else
        stateWith (state.text.take start ++ state.text.drop cursor)
          start ((state.text.take cursor).drop start)
  | .killWordForward =>
      let finish := wordForwardEnd state.text cursor
      if cursor = finish then
        stateWith state.text cursor state.killRing
      else
        stateWith (state.text.take cursor ++ state.text.drop finish)
          cursor ((state.text.drop cursor).take (finish - cursor))
  | .yank =>
      if state.killRing.isEmpty then
        stateWith state.text cursor state.killRing
      else
        stateWith (state.text.take cursor ++ state.killRing ++ state.text.drop cursor)
          (cursor + state.killRing.length) state.killRing

def runOperations (state : InteractiveState) (operations : List InteractiveOp) : InteractiveState :=
  operations.foldl step state

def decodeUtf8 (value : Json) : Except String String := do
  let bytes ← Protocol.decodeBytes value
  match String.fromUTF8? (ByteArray.mk bytes.toArray) with
  | some text => pure text
  | none => throw "text is not valid UTF-8"

def decodeOperation (value : Json) : Except String InteractiveOp := do
  let tag ← (← value.getObjVal? "op").getStr?
  match tag with
  | "insert_char" => do
    Protocol.exactObject ["op", "text"] value
    let text ← decodeUtf8 (← value.getObjVal? "text")
    match text.toList with
    | [character] => pure (.insertChar character)
    | _ => throw "insert_char requires exactly one Unicode scalar"
  | "insert_str" => do
    Protocol.exactObject ["op", "text"] value
    let text ← decodeUtf8 (← value.getObjVal? "text")
    return .insertString text.toList
  | "delete_back" => do
    Protocol.exactObject ["op"] value
    pure .deleteBack
  | "delete_forward" => do
    Protocol.exactObject ["op"] value
    pure .deleteForward
  | "move_left" => do
    Protocol.exactObject ["op"] value
    pure .moveLeft
  | "move_right" => do
    Protocol.exactObject ["op"] value
    pure .moveRight
  | "move_home" => do
    Protocol.exactObject ["op"] value
    pure .moveHome
  | "move_end" => do
    Protocol.exactObject ["op"] value
    pure .moveEnd
  | "move_word_left" => do
    Protocol.exactObject ["op"] value
    pure .moveWordLeft
  | "move_word_right" => do
    Protocol.exactObject ["op"] value
    pure .moveWordRight
  | "kill_to_start" => do
    Protocol.exactObject ["op"] value
    pure .killToStart
  | "kill_to_end" => do
    Protocol.exactObject ["op"] value
    pure .killToEnd
  | "kill_word_back" => do
    Protocol.exactObject ["op"] value
    pure .killWordBack
  | "kill_word_forward" => do
    Protocol.exactObject ["op"] value
    pure .killWordForward
  | "yank" => do
    Protocol.exactObject ["op"] value
    pure .yank
  | _ => throw s!"unknown interactive operation: {tag}"

def decodeInput (value : Json) : Except String (Nat × List InteractiveOp) := do
  Protocol.exactObject ["cursor", "operations"] value
  let cursor ← (← value.getObjVal? "cursor").getNat?
  let operations ← (← (← value.getObjVal? "operations").getArr?).toList.mapM decodeOperation
  return (cursor, operations)

def decodeSource (source : Bytes) : Except String String :=
  match String.fromUTF8? (ByteArray.mk source.toArray) with
  | some text => .ok text
  | none => .error "source is not valid UTF-8"

def encodeResult (state : InteractiveState) : Json := .mkObj [
  ("text", Protocol.encodeBytes (textBytes state)),
  ("cursor", toJson (byteCursor state))]

def evalInput (source : Bytes) (input : Json) (fuel : Nat) : Outcome Json :=
  match decodeSource source, decodeInput input with
  | .error reason, _ => .unsupportedDomain reason
  | _, .error reason => .unsupportedDomain reason
  | .ok text, .ok (cursor, operations) =>
      if operations.length > fuel then .fuelExhausted
      else
        let initial := fromString text cursor
        .success (encodeResult (runOperations initial operations))

def evalRequest (request : Protocol.Request) : Outcome Json :=
  if request.family == .interactive then
    evalInput request.source request.input request.fuel
  else
    .unsupportedDomain "request family is not interactive"

def unicodeInsertionRequest : Protocol.Request := {
  id := "unicode-insertion"
  family := .interactive
  source := [97, 98, 195, 169, 120]
  state := {}
  fuel := 1
  input := .mkObj [
    ("cursor", toJson (3 : Nat)),
    ("operations", .arr #[.mkObj [
      ("op", toJson "insert_char"),
      ("text", Protocol.encodeBytes [226, 152, 131])]])]
}

example : (evalRequest unicodeInsertionRequest == .success (.mkObj [
    ("text", Protocol.encodeBytes [97, 98, 226, 152, 131, 195, 169, 120]),
    ("cursor", toJson (5 : Nat))])) = true := by native_decide

example : (evalInput [195] (.mkObj [
    ("cursor", toJson (0 : Nat)), ("operations", .arr #[])]) 0 ==
    .unsupportedDomain "source is not valid UTF-8") = true := by decide

example : (evalInput [97] (.mkObj [
    ("cursor", toJson (0 : Nat)),
    ("operations", .arr #[.mkObj [("op", toJson "insert_char"),
      ("text", Protocol.encodeBytes [97, 98])]])]) 1 ==
    .unsupportedDomain "insert_char requires exactly one Unicode scalar") = true := by decide

example : (evalInput [97] (.mkObj [
    ("cursor", toJson (0 : Nat)),
      ("operations", .arr #[.mkObj [("op", toJson "move_right")]])]) 0 ==
    .fuelExhausted) = true := by decide

theorem cursorAtOrBeforeByte_le_length (text : List Char) (byteCursor : Nat) :
    cursorAtOrBeforeByte text byteCursor ≤ text.length := by
  induction text generalizing byteCursor with
  | nil => simp [cursorAtOrBeforeByte]
  | cons value rest inductionHypothesis =>
      by_cases beforeCharacter : byteCursor < value.utf8Size
      · simp [cursorAtOrBeforeByte, beforeCharacter]
      · have restBound := inductionHypothesis (byteCursor - value.utf8Size)
        simp [cursorAtOrBeforeByte, beforeCharacter]
        omega

theorem fromString_validCursor (source : String) (byteCursor : Nat) :
    validCursor (fromString source byteCursor) := by
  exact stateWith_validCursor _ _ _

theorem byteCursor_is_utf8Boundary (state : InteractiveState) :
    String.Pos.Raw.IsValid (String.ofList state.text) (String.Pos.Raw.mk (byteCursor state)) := by
  rw [String.Pos.Raw.isValid_ofList]
  exact ⟨state.cursor, rfl⟩

theorem byteCursor_le_byteLength (state : InteractiveState) :
    byteCursor state ≤ byteLength state := by
  let textPrefix := String.ofList (state.text.take state.cursor)
  let suffix := String.ofList (state.text.drop state.cursor)
  have charsSplit := List.take_append_drop state.cursor state.text
  have split : String.ofList state.text = textPrefix ++ suffix := by
    simpa only [String.ofList_append] using (congrArg String.ofList charsSplit).symm
  rw [byteCursor, byteLength, split, String.utf8ByteSize_append]
  exact Nat.le_add_right _ _

theorem step_preserves_cursor_bound (state : InteractiveState) (operation : InteractiveOp) :
    validCursor (step state operation) := by
  cases operation <;> simp only [step] <;>
    first
    | exact stateWith_validCursor _ _ _
    | split <;> exact stateWith_validCursor _ _ _

theorem run_preserves_cursor_bound (state : InteractiveState)
    (operations : List InteractiveOp) (valid : validCursor state) :
    validCursor (runOperations state operations) := by
  induction operations generalizing state with
  | nil => simpa [runOperations] using valid
  | cons operation rest inductionHypothesis =>
      simp only [runOperations, List.foldl_cons]
      exact inductionHypothesis (step state operation)
        (step_preserves_cursor_bound state operation)

theorem step_preserves_utf8_cursor_invariant (state : InteractiveState)
    (operation : InteractiveOp) :
    validCursor (step state operation) ∧
      byteCursor (step state operation) ≤ byteLength (step state operation) ∧
      String.Pos.Raw.IsValid (String.ofList (step state operation).text)
        (String.Pos.Raw.mk (byteCursor (step state operation))) := by
  exact ⟨step_preserves_cursor_bound state operation,
    byteCursor_le_byteLength (step state operation),
    byteCursor_is_utf8Boundary (step state operation)⟩

theorem run_preserves_utf8_cursor_invariant (state : InteractiveState)
    (operations : List InteractiveOp) (valid : validCursor state) :
    validCursor (runOperations state operations) ∧
      byteCursor (runOperations state operations) ≤ byteLength (runOperations state operations) ∧
      String.Pos.Raw.IsValid (String.ofList (runOperations state operations).text)
        (String.Pos.Raw.mk (byteCursor (runOperations state operations))) := by
  exact ⟨run_preserves_cursor_bound state operations valid,
    byteCursor_le_byteLength (runOperations state operations),
    byteCursor_is_utf8Boundary (runOperations state operations)⟩

theorem deleteBack_at_start (state : InteractiveState) (atStart : state.cursor = 0) :
    step state .deleteBack = state := by
  cases state with
  | mk text cursor killRing => simp_all [step, stateWith]

theorem deleteForward_at_end (state : InteractiveState) (atEnd : state.cursor = state.text.length) :
    step state .deleteForward = state := by
  cases state with
  | mk text cursor killRing => simp_all [step, stateWith]

theorem killToStart_at_start (state : InteractiveState) (atStart : state.cursor = 0) :
    step state .killToStart = state := by
  cases state with
  | mk text cursor killRing => simp_all [step, stateWith]

theorem killToEnd_at_end (state : InteractiveState) (atEnd : state.cursor = state.text.length) :
    step state .killToEnd = state := by
  cases state with
  | mk text cursor killRing => simp_all [step, stateWith]

theorem moveLeft_at_start (state : InteractiveState) (atStart : state.cursor = 0) :
    step state .moveLeft = state := by
  cases state with
  | mk text cursor killRing => simp_all [step, stateWith]

theorem moveRight_at_end (state : InteractiveState) (atEnd : state.cursor = state.text.length) :
    step state .moveRight = state := by
  cases state with
  | mk text cursor killRing => simp_all [step, stateWith]

theorem killWordBack_with_empty_range (state : InteractiveState) (valid : validCursor state)
    (emptyRange : wordBackStart state.text state.cursor = state.cursor) :
    step state .killWordBack = state := by
  have cursorBound : min state.cursor state.text.length = state.cursor := Nat.min_eq_left valid
  simp [step, stateWith, cursorBound, emptyRange]

theorem killWordForward_with_empty_range (state : InteractiveState) (valid : validCursor state)
    (emptyRange : wordForwardEnd state.text state.cursor = state.cursor) :
    step state .killWordForward = state := by
  have cursorBound : min state.cursor state.text.length = state.cursor := Nat.min_eq_left valid
  simp [step, stateWith, cursorBound, emptyRange]

theorem yank_with_empty_ring (state : InteractiveState) (valid : validCursor state)
    (empty : state.killRing = []) :
    step state .yank = state := by
  have cursorBound : min state.cursor state.text.length = state.cursor := Nat.min_eq_left valid
  cases state with
  | mk text cursor killRing => simp_all [step, stateWith, validCursor]

theorem killToStart_then_yank_restores (state : InteractiveState)
    (valid : validCursor state) (afterStart : 0 < state.cursor) :
    let restored := step (step state .killToStart) .yank
    restored.text = state.text ∧ restored.cursor = state.cursor ∧
      restored.killRing = state.text.take state.cursor := by
  have cursorBound : min state.cursor state.text.length = state.cursor := Nat.min_eq_left valid
  have killed : step state .killToStart =
      { text := state.text.drop state.cursor, cursor := 0,
        killRing := state.text.take state.cursor } := by
    change (if min state.cursor state.text.length = 0 then
      stateWith state.text (min state.cursor state.text.length) state.killRing
    else
      stateWith (state.text.drop (min state.cursor state.text.length)) 0
        (state.text.take (min state.cursor state.text.length))) = _
    rw [ite_eq_right (by rw [cursorBound]; omega)]
    simp [stateWith, cursorBound]
  have killedRingLength : 0 < (state.text.take state.cursor).length := by
    rw [List.length_take]
    omega
  have killedRingNonempty : (state.text.take state.cursor).isEmpty = false := by
    cases taken : state.text.take state.cursor with
    | nil =>
        simp [taken] at killedRingLength
    | cons value rest => simp
  rw [killed]
  simp [step, stateWith, killedRingNonempty, List.take_append_drop,
    List.length_take, List.length_drop, cursorBound] <;> omega

theorem killToEnd_then_yank_restores (state : InteractiveState)
    (valid : validCursor state) (beforeEnd : state.cursor < state.text.length) :
    let restored := step (step state .killToEnd) .yank
    restored.text = state.text ∧ restored.cursor = state.text.length ∧
      restored.killRing = state.text.drop state.cursor := by
  have cursorBound : min state.cursor state.text.length = state.cursor := Nat.min_eq_left valid
  have killed : step state .killToEnd =
      { text := state.text.take state.cursor, cursor := state.cursor,
        killRing := state.text.drop state.cursor } := by
    change (if min state.cursor state.text.length = state.text.length then
      stateWith state.text (min state.cursor state.text.length) state.killRing
    else
      stateWith (state.text.take (min state.cursor state.text.length))
        (min state.cursor state.text.length) (state.text.drop (min state.cursor state.text.length))) = _
    rw [ite_eq_right (by rw [cursorBound]; omega)]
    simp [stateWith, cursorBound]
  have killedRingLength : 0 < (state.text.drop state.cursor).length := by
    rw [List.length_drop]
    omega
  have killedRingNonempty : (state.text.drop state.cursor).isEmpty = false := by
    cases suffix : state.text.drop state.cursor with
    | nil =>
        simp [suffix] at killedRingLength
    | cons value rest => simp
  rw [killed]
  simp [step, stateWith, killedRingNonempty, List.take_append_drop,
    List.take_take, List.length_take, List.length_drop, cursorBound] <;> omega

example : byteCursor (fromString "éx" 1) = 0 := by decide

example : byteCursor (fromString "éx" 2) = 2 := by decide

example : byteCursor (fromString "éx" 99) = 3 := by decide

example : textBytes (fromString "é" 2) = [195, 169] := by decide

example : textString (runOperations (fromString "éx" 2) [.insertChar '雪']) = "é雪x" := by decide

example : byteCursor (runOperations (fromString "éx" 2) [.insertString "雪🙂".toList]) = 9 := by decide

example : textString (runOperations (fromString "aé" 3) [.deleteBack]) = "a" := by decide

example : textString (runOperations (fromString "aé" 1) [.deleteForward]) = "a" := by decide

example :
    let moved := step (fromString "café λ" 4) .moveWordRight
    textBytes moved = [99, 97, 102, 195, 169, 32, 206, 187] ∧ byteCursor moved = 6 := by decide

example :
    let killed := step (fromString "café λ" 4) .killWordForward
    textString killed = "caf λ" ∧ killed.killRing = "é".toList ∧ byteCursor killed = 3 := by decide

example : rustWhitespace ' ' = true := by decide

example :
    let killed := step (fromString "ab cd" 6) .killWordBack
    textString killed = "ab " ∧ killed.killRing = "cd".toList ∧ byteCursor killed = 4 := by decide

example :
    let restored := runOperations (fromString " abc def" 2) [.killWordForward, .yank]
    textString restored = " abc def" ∧ restored.killRing = "abc".toList := by decide

example :
    let restored := runOperations (fromString "abc def" 4) [.killWordBack, .yank]
    textString restored = "abc def" ∧ byteCursor restored = 4 ∧
      restored.killRing = "abc ".toList := by decide

end Interactive
