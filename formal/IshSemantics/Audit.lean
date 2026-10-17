import IshSemantics.Arithmetic
import IshSemantics.Effects
import IshSemantics.Effects.Tests
import IshSemantics.Evaluation
import IshSemantics.Evaluation.Tests
import IshSemantics.Expansion
import IshSemantics.ExpansionTests
import IshSemantics.Interactive
import IshSemantics.Policy
import IshSemantics.Protocol

open Lean

namespace IshSemantics.Audit

def families : List Name := [
  `IshSemantics.Protocol,
  `IshSemantics.Policy,
  `IshSemantics.Expansion,
  `IshSemantics.Arithmetic,
  `IshSemantics.Evaluation,
  `IshSemantics.Effects,
  `IshSemantics.Interactive]

def permittedAxioms : List Name := [`propext, `Quot.sound, `Classical.choice]

def belongsToFamily (family declaration : Name) : Bool :=
  family.isPrefixOf (privateToUserName declaration)

def theoremNames (env : Environment) (family : Name) : List Name :=
  env.constants.toList.filterMap fun (name, info) =>
    if belongsToFamily family name then
      match info with
      | .thmInfo _ => some name
      | _ => none
    else none

def axiomNames (env : Environment) (family : Name) : List Name :=
  env.constants.toList.filterMap fun (name, info) =>
    if belongsToFamily family name then
      match info with
      | .axiomInfo _ => some name
      | _ => none
    else none

def coverageReferences (document : String) : List String :=
  (document.splitOn "`").filter fun piece => piece.startsWith "IshSemantics."

run_cmd do
  let env ← getEnv
  let mut audited := []
  for family in families do
    let declarations := theoremNames env family
    if declarations.isEmpty then
      throwError m!"no theorem declarations found in {family}"
    let customAxioms := axiomNames env family
    if !customAxioms.isEmpty then
      throwError m!"axiom declarations in {family}: {customAxioms}"
    for declaration in declarations do
      let axioms ← Lean.collectAxioms declaration
      let unexpected := axioms.filter fun dependency => !permittedAxioms.contains dependency
      if !unexpected.isEmpty then
        throwError m!"{declaration} depends on unpermitted axioms: {unexpected}"
    audited := audited ++ declarations.map Name.toString
    logInfo m!"{family}: axiom audit passed"
  let coverage ← IO.FS.readFile "../docs/verification/coverage.md"
  let references := coverageReferences coverage
  if references.isEmpty then
    throwError "coverage document has no qualified theorem links"
  for reference in references do
    if !audited.contains reference then
      throwError m!"coverage links to a theorem outside the audit: {reference}"
  for family in families do
    let familyPrefix := family.toString ++ "."
    if !(references.any fun reference => reference.startsWith familyPrefix) then
      throwError m!"coverage document has no theorem link for {family}"
  logInfo m!"coverage document: validated {references.length} theorem links"

end IshSemantics.Audit
