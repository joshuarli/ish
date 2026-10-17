import EpshShift
import IshSemantics.Arithmetic

namespace IshSemantics.Refinement

open Aeneas Aeneas.Std

def projectCount (result : Result (Option U32)) : Option (Option Nat) :=
  match result.match with
  | .ok count => some (count.map UScalar.val)
  | .vis _ _ => none
  | .div => none

theorem shift_count_refines (value : I64) :
    projectCount (epsh_shift.shift.count value) =
      some (if 0 ≤ value.val ∧ value.val < 64 then some value.val.toNat else none) := by
  unfold epsh_shift.shift.count
  simp only [IScalar.le_equiv]
  change projectCount (if 0 ≤ value.val then
      if value.val ≤ 63 then _ else _ else _) = _
  by_cases lo : 0 ≤ value.val
  · by_cases hi : value.val ≤ 63
    · simp [lo, hi, projectCount, lift, IScalar.hcast_val_eq,
        show value.val % 4294967296 = value.val by omega,
        show value.val < 64 by omega]
    · simp [lo, hi, projectCount, show ¬ value.val < 64 by omega]
  · simp [lo, projectCount]

theorem shift_count_matches_model (value : I64) :
    ∃ count, projectCount (epsh_shift.shift.count value) = some count ∧
      match count with
      | none => ∀ a : Arithmetic.Value,
          Arithmetic.binary .shl a value.bv = .error .invalidShift ∧
          Arithmetic.binary .shr a value.bv = .error .invalidShift
      | some n => ∀ a : Arithmetic.Value,
          Arithmetic.binary .shl a value.bv = .ok (a <<< n) ∧
          Arithmetic.binary .shr a value.bv = .ok (a.sshiftRight n) := by
  refine ⟨_, shift_count_refines value, ?_⟩
  by_cases valid : 0 ≤ value.val ∧ value.val < 64
  · simp only [valid]
    have natural : value.val.toNat = value.bv.toNat := by
      apply BitVec.toNat_toInt_of_msb
      cases sign : value.bv.msb
      · rfl
      · have negative := BitVec.toInt_neg_of_msb_true sign
        change 0 ≤ value.bv.toInt ∧ _ at valid
        omega
    have guard : (value.bv.toInt < 0 || value.bv.toInt ≥ 64) = false := by
      simp only [Bool.or_eq_false_iff, decide_eq_false_iff_not]
      change ¬ value.val < 0 ∧ ¬ value.val ≥ 64
      omega
    intro a
    simp only [Arithmetic.binary, guard, Bool.false_eq_true, ↓reduceIte, natural]
    constructor <;> rfl
  · simp only [valid, ↓reduceIte]
    have guard : (value.bv.toInt < 0 || value.bv.toInt ≥ 64) = true := by
      simp only [Bool.or_eq_true, decide_eq_true_eq]
      change value.val < 0 ∨ value.val ≥ 64
      omega
    intro a
    simp only [Arithmetic.binary, guard, ↓reduceIte, and_self]

run_cmd do
  let permitted : List Lean.Name := [`propext, `Quot.sound, `Classical.choice]
  for declaration in [`IshSemantics.Refinement.shift_count_refines,
      `IshSemantics.Refinement.shift_count_matches_model] do
    let axioms ← Lean.collectAxioms declaration
    let unexpected := axioms.filter fun dependency =>
      !permitted.contains dependency
    unless unexpected.isEmpty do
      throwError m!"{declaration} depends on unpermitted axioms: {unexpected}"
    Lean.logInfo m!"{declaration}: axiom audit passed"

end IshSemantics.Refinement
