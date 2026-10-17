import IshSemantics.Protocol

namespace IshSemantics.Effects
open Lean

abbrev Environment := List (Bytes × Bytes)

def lookup (entries : Environment) (name : Bytes) : Option Bytes :=
  (entries.find? (fun entry => entry.1 == name)).map Prod.snd

def namedBytes (entries : Bindings) : Environment :=
  entries.map fun (name, value) => (name.toUTF8.data.toList, value)

def childValue (inherited exported assignments : Environment) (name : Bytes) : Option Bytes :=
  (lookup assignments name).orElse fun _ =>
    (lookup exported name).orElse fun _ => lookup inherited name

def childEnvironment (inherited exported assignments : Environment) : Environment :=
  let names := ((inherited ++ exported ++ assignments).map Prod.fst).eraseDups
  let entries := names.filterMap fun name => (childValue inherited exported assignments name).map (name, ·)
  entries.mergeSort fun a b => compare a.1 b.1 != .gt

theorem child_environment_entry (inherited exported assignments : Environment) (name value : Bytes) :
    (name, value) ∈ childEnvironment inherited exported assignments ↔
    name ∈ (inherited ++ exported ++ assignments).map Prod.fst ∧
      childValue inherited exported assignments name = some value := by
  simp only [childEnvironment, List.mem_mergeSort, List.mem_filterMap]
  constructor
  · rintro ⟨key, present, result⟩
    cases found : childValue inherited exported assignments key with
    | none => simp [found] at result
    | some bytes =>
      simp [found] at result
      rcases result with ⟨rfl, rfl⟩
      exact ⟨List.mem_eraseDups.mp present, found⟩
  · rintro ⟨present, found⟩
    exact ⟨name, List.mem_eraseDups.mpr present, by simp [found]⟩

theorem prefix_shadows (inherited exported assignments : Environment) (name value : Bytes)
    (assigned : lookup assignments name = some value) :
    childValue inherited exported assignments name = some value := by
  simp [childValue, assigned]

theorem export_shadows (inherited exported assignments : Environment) (name value : Bytes)
    (unassigned : lookup assignments name = none) (exportedValue : lookup exported name = some value) :
    childValue inherited exported assignments name = some value := by
  simp [childValue, unassigned, exportedValue]

theorem inherited_preserved (inherited exported assignments : Environment) (name : Bytes)
    (unassigned : lookup assignments name = none) (unexported : lookup exported name = none) :
    childValue inherited exported assignments name = lookup inherited name := by
  simp [childValue, unassigned, unexported]

theorem environment_name_noninterference (i e a i' e' a' : Environment) (name : Bytes)
    (hi : lookup i name = lookup i' name) (he : lookup e name = lookup e' name)
    (ha : lookup a name = lookup a' name) : childValue i e a name = childValue i' e' a' name := by
  simp [childValue, hi, he, ha]

structure RawValue where
  bytes : Bytes
  deriving Repr, BEq

def fromBytes (bytes : Bytes) : RawValue := ⟨bytes⟩
def toBytes (value : RawValue) : Bytes := value.bytes

def toCString (value : RawValue) : Except Unit Bytes :=
  if value.bytes.contains 0 then .error () else .ok (value.bytes ++ [0])

theorem raw_byte_roundtrip (bytes : Bytes) : toBytes (fromBytes bytes) = bytes := rfl

theorem cstring_rejects_nul (bytes : Bytes) (nul : bytes.contains 0 = true) :
    toCString (fromBytes bytes) = .error () := by
  unfold toCString fromBytes
  rw [nul]
  rfl

theorem cstring_preserves_bytes (bytes : Bytes) (nulFree : bytes.contains 0 = false) :
    toCString (fromBytes bytes) = .ok (bytes ++ [0]) := by
  unfold toCString fromBytes
  rw [nulFree]
  rfl

def encodeOctet (byte : UInt8) : Nat :=
  if byte.toNat < 128 then byte.toNat else 57344 + byte.toNat

def decodeOctet (codepoint : Nat) : UInt8 :=
  (if codepoint < 128 then codepoint else codepoint - 57344).toUInt8

theorem octet_roundtrip (byte : UInt8) : decodeOctet (encodeOctet byte) = byte := by
  unfold encodeOctet decodeOctet
  split
  · simp
  · have high : ¬57344 + byte.toNat < 128 := by omega
    simp [high, Nat.add_sub_cancel_left]

theorem escaped_octets_roundtrip (bytes : Bytes) :
    (bytes.map encodeOctet).map decodeOctet = bytes := by
  simp [List.map_map, Function.comp_def, octet_roundtrip]

structure Directories where
  shells : Nat → Bytes
  process : Bytes

def changeDirectory (directories : Directories) (shell : Nat) (path : Bytes)
    (succeeded : Bool) : Directories :=
  if succeeded then { directories with shells := fun other => if other = shell then path else directories.shells other }
  else directories

theorem cwd_selected (d : Directories) (shell : Nat) (path : Bytes) :
    (changeDirectory d shell path true).shells shell = path := by simp [changeDirectory]

theorem cwd_other_shell (d : Directories) (shell other : Nat) (path : Bytes) (success : Bool)
    (different : other ≠ shell) :
    (changeDirectory d shell path success).shells other = d.shells other := by
  cases success <;> simp [changeDirectory, different]

theorem cwd_process_unchanged (d : Directories) (shell : Nat) (path : Bytes) (success : Bool) :
    (changeDirectory d shell path success).process = d.process := by
  cases success <;> rfl

theorem cwd_failure_unchanged (d : Directories) (shell : Nat) (path : Bytes) :
    changeDirectory d shell path false = d := rfl

abbrev DescriptorMap := Nat → Option Nat

def setDescriptor (descriptors : DescriptorMap) (fd : Nat) (value : Option Nat) : DescriptorMap :=
  fun probe => if probe = fd then value else descriptors probe

structure SavedDescriptor where
  target : Nat
  copy : Nat
  originallyClosed : Bool

inductive RedirectKind where
  | open | duplicate | close | fail
  deriving Repr, BEq, DecidableEq

structure Redirection where
  target : Nat
  copy : Nat
  kind : RedirectKind
  resource : Nat
  restore : Bool
  deriving Repr

def freshCopy (map : DescriptorMap) (op : Redirection) : Prop :=
  op.copy ≠ op.target ∧ map op.copy = none
instance (map : DescriptorMap) (op : Redirection) : Decidable (freshCopy map op) :=
  inferInstanceAs (Decidable (_ ∧ _))

def saveDescriptor (map : DescriptorMap) (op : Redirection) : DescriptorMap × SavedDescriptor :=
  (setDescriptor map op.copy (map op.target), ⟨op.target, op.copy, (map op.target).isNone⟩)

def applyRedirection (map : DescriptorMap) (op : Redirection) : Option DescriptorMap :=
  match op.kind with
  | .open => some (setDescriptor map op.target (some op.resource))
  | .close => some (setDescriptor map op.target none)
  | .duplicate => (map op.resource).map fun resource => setDescriptor map op.target (some resource)
  | .fail => none

def restoreDescriptor (map : DescriptorMap) (saved : SavedDescriptor) (succeeded : Bool) :
    Except Nat DescriptorMap :=
  if succeeded then
    if saved.originallyClosed then .ok (setDescriptor (setDescriptor map saved.target none) saved.copy none)
    else match map saved.copy with
      | none => .error saved.target
      | some resource => .ok (setDescriptor (setDescriptor map saved.target (some resource)) saved.copy none)
  else .error saved.target

theorem owned_copy_restores (map : DescriptorMap) (op : Redirection) (value : Option Nat)
    (fresh : freshCopy map op) :
    restoreDescriptor (setDescriptor (saveDescriptor map op).1 op.target value)
      (saveDescriptor map op).2 true = .ok map := by
  rcases fresh with ⟨different, absent⟩
  unfold restoreDescriptor saveDescriptor
  simp only
  cases original : map op.target with
  | none =>
    simp only [Option.isNone_none, ↓reduceIte]
    congr 1
    funext fd
    by_cases hc : fd = op.copy
    · subst fd; simp [setDescriptor, absent]
    · by_cases ht : fd = op.target
      · subst fd; simp [setDescriptor, Ne.symm different, original]
      · simp [setDescriptor, hc, ht]
  | some resource =>
    simp only [Option.isNone_some, Bool.false_eq_true, ↓reduceIte]
    simp only [setDescriptor, different, ↓reduceIte]
    congr 1
    funext fd
    by_cases hc : fd = op.copy
    · subst fd; simp [setDescriptor, absent]
    · by_cases ht : fd = op.target
      · subst fd; simp [setDescriptor, Ne.symm different, original]
      · simp [setDescriptor, hc, ht]

theorem saved_unchanged_restores (map : DescriptorMap) (op : Redirection)
    (fresh : freshCopy map op) :
    restoreDescriptor (saveDescriptor map op).1 (saveDescriptor map op).2 true = .ok map := by
  have h := owned_copy_restores map op (map op.target) fresh
  have same : setDescriptor (saveDescriptor map op).1 op.target (map op.target) = (saveDescriptor map op).1 := by
    funext fd
    by_cases ht : fd = op.target
    · subst fd; simp [setDescriptor, saveDescriptor, Ne.symm fresh.1]
    · simp [setDescriptor, ht]
  rw [same] at h
  exact h

theorem restoration_failure_explicit (map : DescriptorMap) (saved : SavedDescriptor) :
    restoreDescriptor map saved false = .error saved.target := rfl

structure RedirectResult where
  descriptors : DescriptorMap
  setupError : Bool

inductive RedirectError where
  | invalidCopy | restoration (fd : Nat)
  deriving Repr, BEq

def temporaryRedirections (map : DescriptorMap) : List Redirection → Except RedirectError RedirectResult
  | [] => .ok ⟨map, false⟩
  | op :: rest => do
    if freshCopy map op then
      let (savedMap, saved) := saveDescriptor map op
      let (after, failed) ← match applyRedirection savedMap op with
        | none => pure (savedMap, true)
        | some changed => do
          let result ← temporaryRedirections changed rest
          pure (result.descriptors, result.setupError)
      let restored ← (restoreDescriptor after saved op.restore).mapError RedirectError.restoration
      pure ⟨restored, failed⟩
    else .error .invalidCopy

def Restorable (map : DescriptorMap) : List Redirection → Prop
  | [] => True
  | op :: rest => freshCopy map op ∧ op.restore = true ∧
    ∀ changed, applyRedirection (saveDescriptor map op).1 op = some changed → Restorable changed rest

theorem apply_changes_only_target (map : DescriptorMap) (op : Redirection) (changed : DescriptorMap)
    (applied : applyRedirection map op = some changed) :
    ∃ value, changed = setDescriptor map op.target value := by
  cases kind : op.kind <;> simp [applyRedirection, kind] at applied
  · exact ⟨some op.resource, applied.symm⟩
  · cases resource : map op.resource <;> simp [resource] at applied
    exact ⟨_, applied.symm⟩
  · exact ⟨none, applied.symm⟩

theorem temporary_restores_original (map : DescriptorMap) (ops : List Redirection)
    (valid : Restorable map ops) :
    ∃ failed, temporaryRedirections map ops = .ok ⟨map, failed⟩ := by
  induction ops generalizing map with
  | nil => exact ⟨false, rfl⟩
  | cons op rest ih =>
    rcases valid with ⟨fresh, restores, remaining⟩
    unfold temporaryRedirections
    simp only [fresh]
    cases applied : applyRedirection (saveDescriptor map op).1 op with
    | none =>
      refine ⟨true, ?_⟩
      simp [restores, saved_unchanged_restores map op fresh, Except.mapError]
      rfl
    | some changed =>
      rcases ih changed (remaining changed applied) with ⟨failed, recursive⟩
      rcases apply_changes_only_target _ _ _ applied with ⟨value, rfl⟩
      refine ⟨failed, ?_⟩
      simp only [recursive, restores]
      change Except.map (fun restored => RedirectResult.mk restored failed)
        ((restoreDescriptor (setDescriptor (saveDescriptor map op).1 op.target value)
          (saveDescriptor map op).2 true).mapError RedirectError.restoration) = _
      rw [owned_copy_restores map op value fresh]
      rfl

def permanentRedirections (map : DescriptorMap) : List Redirection → RedirectResult
  | [] => ⟨map, false⟩
  | op :: rest => match applyRedirection map op with
    | none => ⟨map, true⟩
    | some changed => permanentRedirections changed rest

def decodeDescriptors (value : Json) : Except String (List (Nat × Nat)) := do
  let entries ← (← value.getArr?).toList.mapM (Protocol.decodePair Json.getNat? Json.getNat?)
  let names := entries.map Prod.fst
  unless names.eraseDups.length == names.length do throw "duplicate descriptor"
  return entries

def decodeRedirection (value : Json) : Except String Redirection := do
  Protocol.exactObject ["target", "copy", "kind", "resource", "restore"] value
  let target ← (← value.getObjVal? "target").getNat?
  let copy ← (← value.getObjVal? "copy").getNat?
  let kind ← match ← (← value.getObjVal? "kind").getStr? with
    | "open" => pure RedirectKind.open
    | "duplicate" => pure .duplicate
    | "close" => pure .close
    | "fail" => pure .fail
    | _ => throw "unknown redirection kind"
  let resource ← (← value.getObjVal? "resource").getNat?
  let restore ← (← value.getObjVal? "restore").getBool?
  return ⟨target, copy, kind, resource, restore⟩

def validName (name : Bytes) : Bool :=
  !name.isEmpty && !name.contains 0 && !name.contains 61

def validEnvironment (entries : Environment) : Bool :=
  entries.all fun (name, value) => validName name && !value.contains 0

def shellName (name : String) : Bool := Id.run do
  let bytes := name.toUTF8.data.toList
  let first b := b == 95 || (65 ≤ b.toNat && b.toNat ≤ 90) || (97 ≤ b.toNat && b.toNat ≤ 122)
  match bytes with
  | [] => return false
  | head :: tail => return first head && tail.all (fun b => first b || (48 ≤ b.toNat && b.toNat ≤ 57))

def descriptorTable (entries : List (Nat × Nat)) : DescriptorMap :=
  fun fd => (entries.find? fun entry => entry.1 == fd).map Prod.snd

def encodeDescriptors (map : DescriptorMap) (probes : List Nat) : Json :=
  .arr ((probes.eraseDups.mergeSort (· ≤ ·)).filterMap fun fd =>
    (map fd).map fun resource => Json.arr #[toJson fd, toJson resource]).toArray

def evalInput (request : Protocol.Request) : Except String (Outcome Json) := do
  let input := request.input
  let mode ← (← input.getObjVal? "mode").getStr?
  match mode with
  | "environment" =>
    Protocol.exactObject ["mode", "assignments"] input
    let assignments ← Protocol.decodeBindings (← input.getObjVal? "assignments")
    unless (request.state.exported ++ assignments).all (fun entry => shellName entry.1) do
      throw "exported and assigned names must be shell identifiers"
    let inherited := request.state.inherited
    let exported := namedBytes request.state.exported
    let assignments := namedBytes assignments
    unless validEnvironment (inherited ++ exported ++ assignments) do
      throw "environment names or values cannot cross the OS boundary"
    if request.fuel = 0 then return .fuelExhausted
    return .success (.mkObj [("environment", Protocol.encodeEnvironment (childEnvironment inherited exported assignments))])
  | "bytes" =>
    Protocol.exactObject ["mode", "bytes"] input
    let bytes ← Protocol.decodeBytes (← input.getObjVal? "bytes")
    if request.fuel = 0 then return .fuelExhausted
    return .success (.mkObj [("bytes", Protocol.encodeBytes (toBytes (fromBytes bytes))),
      ("c_string", toJson (!(toBytes (fromBytes bytes)).contains 0))])
  | "cwd" =>
    Protocol.exactObject ["mode", "shell", "path", "success", "other_cwds"] input
    let shell ← (← input.getObjVal? "shell").getNat?
    let path ← Protocol.decodeBytes (← input.getObjVal? "path")
    let success ← (← input.getObjVal? "success").getBool?
    let others ← (← (← input.getObjVal? "other_cwds").getArr?).toList.mapM Protocol.decodeBytes
    unless !path.contains 0 && !request.state.cwd.contains 0 && others.all (fun p => !p.contains 0) do
      throw "cwd paths cannot contain NUL"
    if request.fuel = 0 then return .fuelExhausted
    let directories : Directories := ⟨fun _ => request.state.cwd, []⟩
    let next := changeDirectory directories shell path success
    return .success (.mkObj [("cwd", Protocol.encodeBytes (next.shells shell)),
      ("other_cwds", .arr (others.map Protocol.encodeBytes).toArray)])
  | "redirection" =>
    Protocol.exactObject ["mode", "descriptors", "operations", "permanent"] input
    let entries ← decodeDescriptors (← input.getObjVal? "descriptors")
    let operations ← (← (← input.getObjVal? "operations").getArr?).toList.mapM decodeRedirection
    let permanent ← (← input.getObjVal? "permanent").getBool?
    if request.fuel ≤ operations.length then return .fuelExhausted
    let initial := descriptorTable entries
    let probes := entries.map Prod.fst ++ operations.flatMap (fun op => [op.target, op.copy])
    let result := if permanent then .ok (permanentRedirections initial operations)
      else temporaryRedirections initial operations
    match result with
    | .error .invalidCopy => throw "saved descriptor copy is not fresh"
    | .error (.restoration fd) =>
      return .semanticError (.restoration fd "descriptor restoration failed") request.state []
    | .ok result => return .success (.mkObj [
        ("descriptors", encodeDescriptors result.descriptors probes),
        ("setup_error", toJson result.setupError)])
  | _ => throw "unknown effects mode"

def evalRequest (request : Protocol.Request) : Outcome Json :=
  match evalInput request with
  | .ok outcome => outcome
  | .error reason => .unsupportedDomain reason

theorem unexported_variables_do_not_escape (state : ShellState) (variables : Bindings)
    (assignments : Environment) :
    childEnvironment state.inherited (namedBytes state.exported) assignments =
    childEnvironment ({ state with variables := variables }).inherited
      (namedBytes ({ state with variables := variables }).exported) assignments := rfl

def emptyDescriptors : DescriptorMap := fun _ => none

def exampleInitial : DescriptorMap := setDescriptor emptyDescriptors 1 (some 7)
def exampleOpen : Redirection := ⟨1, 10, .open, 8, true⟩
def exampleFailure : Redirection := ⟨2, 11, .fail, 0, true⟩

theorem example_partial_setup_rolls_back :
    (temporaryRedirections exampleInitial [exampleOpen, exampleFailure]).map
      (fun result => (result.descriptors 1, result.descriptors 2, result.descriptors 10,
        result.descriptors 11, result.setupError)) = .ok (some 7, none, none, none, true) := rfl

theorem example_ordered_duplication :
    (permanentRedirections exampleInitial [exampleOpen, ⟨2, 11, .duplicate, 1, true⟩]).descriptors 2 =
      some 8 := rfl

theorem example_permanent_is_not_restored :
    (permanentRedirections exampleInitial [exampleOpen]).descriptors 1 ≠ exampleInitial 1 := by decide

theorem example_nonfresh_copy_loses_original :
    restoreDescriptor
      (setDescriptor (saveDescriptor exampleInitial ⟨1, 1, .open, 8, true⟩).1 1 (some 8))
      (saveDescriptor exampleInitial ⟨1, 1, .open, 8, true⟩).2 true ≠ .ok exampleInitial := by
  intro equality
  have observed := congrArg (fun result => result.map (fun map => map 1)) equality
  have bad : (Except.ok (none : Option Nat) : Except Nat (Option Nat)) = .ok (some 7) := observed
  cases bad

theorem example_shadowing_needs_precedence :
    childValue [([65], [49])] [([65], [50])] [([65], [51])] [65] = some [51] := rfl

theorem example_invalid_utf8_roundtrip :
    toBytes (fromBytes [97, 128, 255]) = [97, 128, 255] := rfl

theorem example_nul_fails_cstring : toCString (fromBytes [97, 0, 255]) = .error () := rfl

end IshSemantics.Effects
