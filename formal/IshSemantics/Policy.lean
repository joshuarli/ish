import Lean.Elab.Tactic.Omega
import IshSemantics.Syntax
import IshSemantics.Protocol
namespace IshSemantics.Policy
open Lean
inductive Rejection where
  | background | compound | functionDefinition | source | unsupportedBuiltin | arguments
  | parameterOperator | shellBuiltinOutsideLine
  deriving Repr, BEq, DecidableEq
inductive Exec where
  | inShell | hybrid | foreground | native
  deriving Repr, BEq, DecidableEq

def classify (name : Bytes) : Option Exec :=
  if ["?", "alias", "copy-scrollback", "denv", "exit", "fg", "ish-dump", "t", "trash", "type", "w", "which", "z"].any
      (fun text => text.toUTF8.toList == name) then some .inShell
  else if ["cd", "history"].any (fun text => text.toUTF8.toList == name) then some .hybrid
  else if ["cp", "mv"].any (fun text => text.toUTF8.toList == name) then some .foreground
  else if ["c", "l"].any (fun text => text.toUTF8.toList == name) then some .native
  else none

def forbiddenTarget (name : Bytes) : Bool :=
  ["eval", "source", "."].any (fun text => text.toUTF8.toList == name)

def unavailableEpshBuiltin (name : Bytes) : Bool :=
  ["return", "break", "continue", "readonly", "shift", "eval", ".", "source",
    "read", "local", "exec", "wait", "trap", "umask", "getopts"].any
      (fun text => text.toUTF8.toList == name)

def shellNameBytes (name : Bytes) : Bool := Id.run do
  let initial b := b == 95 || (65 ≤ b.toNat && b.toNat ≤ 90) || (97 ≤ b.toNat && b.toNat ≤ 122)
  match name with
  | [] => return false
  | head :: tail => return initial head && tail.all (fun b => initial b || (48 ≤ b.toNat && b.toNat ≤ 57))

def dynamicAllowed (name : Bytes) : Bool :=
  !forbiddenTarget name && !unavailableEpshBuiltin name && classify name != some .inShell
mutual
  def staticParts : List WordPart → Option Bytes
    | [] => some []
    | part :: rest => do return (← staticPart part) ++ (← staticParts rest)
  def staticPart : WordPart → Option Bytes
    | .literal bytes | .singleQuoted bytes => some bytes
    | .doubleQuoted parts => staticParts parts
    | _ => none
end

def staticText : Word → Option Bytes
  | .mk parts => staticParts parts

def targetRejection (args : List Word) : Option Rejection := do
  let first ← args.head?
  let name ← staticText first
  if forbiddenTarget name then some .source
  else if classify name == some .inShell then some .shellBuiltinOutsideLine
  else if unavailableEpshBuiltin name then some .unsupportedBuiltin
  else if name == "set".toUTF8.toList && (match args[1]?.bind staticText with
      | some argument => !shellNameBytes argument
      | none => false) then some .arguments
  else none

inductive Tree where
  | node (reason : Option Rejection) (children : List Tree)
  deriving Repr
mutual
  def commandTree : Command → Tree
    | .simple assigns args redirs => .node (targetRejection args)
        (assigns.map assignmentTree ++ args.map wordTree ++ redirs.map redirTree)
    | .pipeline commands _ => .node none (commands.map commandTree)
    | .and left right | .or left right | .sequence left right =>
        .node none [commandTree left, commandTree right]
    | .not body => .node none [commandTree body]
    | .background body redirs => .node (some .background) (commandTree body :: redirs.map redirTree)
    | .funcDef _ body => .node (some .functionDefinition) [commandTree body]
    | .subshell body redirs | .braceGroup body redirs =>
        .node (some .compound) (commandTree body :: redirs.map redirTree)
    | .ifThen condition thenPart elsePart => .node (some .compound)
        ([commandTree condition, commandTree thenPart] ++ (match elsePart with | none => [] | some body => [commandTree body]))
    | .whileLoop condition body | .untilLoop condition body =>
        .node (some .compound) [commandTree condition, commandTree body]
    | .forLoop _ words body => .node (some .compound)
        ((match words with | none => [] | some items => items.map wordTree) ++ [commandTree body])
    | .caseOf word arms => .node (some .compound) (wordTree word :: arms.map armTree)
  termination_by x => sizeOf x
  decreasing_by all_goals simp_wf; all_goals first | (have _h := List.sizeOf_lt_of_mem (by assumption); omega) | omega
  def wordTree : Word → Tree
    | .mk parts => .node none (parts.map partTree)
  termination_by x => sizeOf x
  decreasing_by all_goals simp_wf; all_goals first | (have _h := List.sizeOf_lt_of_mem (by assumption); omega) | omega
  def partTree : WordPart → Tree
    | .literal _ | .singleQuoted _ | .tilde _ => .node none []
    | .doubleQuoted parts | .arith parts => .node none (parts.map partTree)
    | .param _ op => opTree op
    | .cmdSubst body | .backtick body => commandTree body
  termination_by x => sizeOf x
  decreasing_by all_goals simp_wf; all_goals first | (have _h := List.sizeOf_lt_of_mem (by assumption); omega) | omega
  def opTree : ParamOp → Tree
    | .normal | .badSubst => .node none []
    | .length => .node (some .parameterOperator) []
    | .default _ parts | .assign _ parts | .error _ parts | .alternative _ parts
    | .trimSuffixSmall parts | .trimSuffixLarge parts | .trimPrefixSmall parts | .trimPrefixLarge parts =>
        .node (some .parameterOperator) (parts.map partTree)
  termination_by x => sizeOf x
  decreasing_by all_goals simp_wf; all_goals first | (have _h := List.sizeOf_lt_of_mem (by assumption); omega) | omega
  def assignmentTree : Assignment → Tree
    | .mk _ value => wordTree value
  termination_by x => sizeOf x
  decreasing_by all_goals simp_wf; all_goals first | (have _h := List.sizeOf_lt_of_mem (by assumption); omega) | omega
  def redirTree : Redir → Tree
    | .mk _ kind => redirKindTree kind
  termination_by x => sizeOf x
  decreasing_by all_goals simp_wf; all_goals first | (have _h := List.sizeOf_lt_of_mem (by assumption); omega) | omega
  def redirKindTree : RedirKind → Tree
    | .input word | .output word | .clobber word | .append word | .readWrite word
    | .dupInput word | .dupOutput word => wordTree word
    | .hereDoc body | .hereDocStrip body => hereDocTree body
  termination_by x => sizeOf x
  decreasing_by all_goals simp_wf; all_goals first | (have _h := List.sizeOf_lt_of_mem (by assumption); omega) | omega
  def hereDocTree : HereDocBody → Tree
    | .literal _ => .node none []
    | .parsed parts => .node none (parts.map partTree)
  termination_by x => sizeOf x
  decreasing_by all_goals simp_wf; all_goals first | (have _h := List.sizeOf_lt_of_mem (by assumption); omega) | omega
  def armTree : CaseArm → Tree
    | .mk patterns body => .node none (patterns.map wordTree ++ (match body with | none => [] | some command => [commandTree command]))
  termination_by x => sizeOf x
  decreasing_by all_goals simp_wf; all_goals first | (have _h := List.sizeOf_lt_of_mem (by assumption); omega) | omega
end
mutual
  def checkTree : Tree → Option Rejection
    | .node reason children => reason.or (checkForest children)
  def checkForest : List Tree → Option Rejection
    | [] => none
    | child :: rest => (checkTree child).or (checkForest rest)
end
inductive Contains : Tree → Rejection → Prop where
  | root : Contains (.node (some rejection) children) rejection
  | child : child ∈ children → Contains child rejection → Contains (.node reason children) rejection
private theorem or_none {a b : Option Rejection} (h : a.or b = none) : a = none ∧ b = none := by
  cases a <;> simp_all [Option.or]
private theorem forest_member (trees : List Tree) (tree : Tree)
    (h : checkForest trees = none) (mem : tree ∈ trees) : checkTree tree = none := by
  induction trees with
  | nil => simp at mem
  | cons first rest ih =>
    obtain ⟨a, b⟩ := or_none h
    simp only [List.mem_cons] at mem
    rcases mem with rfl | mem
    · exact a
    · exact ih b mem

theorem accepted_excludes_reachable (tree : Tree) (rejection : Rejection)
    (accepted : checkTree tree = none) : ¬ Contains tree rejection := by
  intro violation
  induction violation with
  | root => simp [checkTree, Option.or] at accepted
  | child mem violation ih =>
    obtain ⟨_, childrenSafe⟩ := or_none accepted
    exact ih (forest_member _ _ childrenSafe mem)
inductive Route where
  | epsh | builtin (name : Bytes) | reject (reason : Rejection)
  deriving Repr, BEq, DecidableEq

def checkedRoute (trees : List Tree) (route : Route) : Route :=
  match checkForest trees with
  | none => route
  | some reason => .reject reason

def plan (program : Program) : Route :=
  match program.commands with
  | [.simple [] (first :: rest) []] =>
    match staticText first with
    | some name =>
      match classify name with
      | some .inShell | some .hybrid => checkedRoute (rest.map wordTree) (.builtin name)
      | _ => checkedRoute (program.commands.map commandTree) .epsh
    | none => checkedRoute (program.commands.map commandTree) .epsh
  | _ => checkedRoute (program.commands.map commandTree) .epsh

theorem plan_deterministic (program : Program) (left right : Route)
    (hleft : plan program = left) (hright : plan program = right) : left = right :=
  hleft.symm.trans hright

def planWithState (program : Program) (state : ShellState) : Route × ShellState × Trace :=
  (plan program, state, [])
theorem planner_pure (program : Program) (state : ShellState) :
    (planWithState program state).2 = (state, []) := rfl

def enforceTarget (target : Bytes) (state : ShellState) (prior : Trace) : Outcome Execution :=
  if dynamicAllowed target then .success { state, status := 0, trace := prior }
  else .semanticError (.policy target) { state with lastStatus := 2 } prior

theorem dynamic_refusal_retains_prior_effects (target : Bytes) (state : ShellState) (prior : Trace)
    (refused : dynamicAllowed target = false) :
    enforceTarget target state prior =
      .semanticError (.policy target) { state with lastStatus := 2 } prior := by
  simp [enforceTarget, refused]

def literalWord (text : String) : Word := .mk [.literal text.toUTF8.toList]
example : plan ⟨[.simple [] [literalWord "cd"] []]⟩ = .builtin "cd".toUTF8.toList := by decide +kernel
example : plan ⟨[.simple [] [literalWord "l"] []]⟩ = .epsh := by decide +kernel
example : plan ⟨[.pipeline [.simple [] [literalWord "z"] []] false]⟩ =
    .reject .shellBuiltinOutsideLine := by decide +kernel
example : plan ⟨[.simple [] [.mk [.arith [.doubleQuoted [.cmdSubst (.background (.simple [] [] []) [])]]]] []]⟩ =
    .reject .background := by decide +kernel
example : plan ⟨[.simple [] [] [.mk 0 (.hereDoc (.parsed [.backtick (.funcDef "f" (.simple [] [] []))]))]]⟩ =
    .reject .functionDefinition := by decide +kernel
example : dynamicAllowed "eval".toUTF8.toList = false := by decide +kernel
example : dynamicAllowed "cd".toUTF8.toList = true := by decide +kernel


theorem checked_route_excludes_reachable (trees : List Tree) (route : Route)
    (accepted : checkedRoute trees route = route)
    (ordinary : ∀ reason, route ≠ .reject reason)
    (tree : Tree) (member : tree ∈ trees) (reason : Rejection) : ¬ Contains tree reason := by
  have safe : checkForest trees = none := by
    cases h : checkForest trees with
    | none => rfl
    | some rejected =>
      simp only [checkedRoute, h] at accepted
      exact False.elim (ordinary rejected accepted.symm)
  exact accepted_excludes_reachable tree reason (forest_member trees tree safe member)

theorem whole_line_shell_routing (first : Word) (rest : List Word) (name : Bytes) (exec : Exec)
    (text : staticText first = some name) (kind : classify name = some exec)
    (shell : exec = .inShell ∨ exec = .hybrid)
    (safe : checkForest (rest.map wordTree) = none) :
    plan ⟨[.simple [] (first :: rest) []]⟩ = .builtin name := by
  rcases shell with rfl | rfl <;> simp [plan, text, kind, checkedRoute, safe]

theorem whole_line_native_routing (first : Word) (rest : List Word) (name : Bytes)
    (text : staticText first = some name) (kind : classify name = some .native)
    (safe : checkForest [commandTree (.simple [] (first :: rest) [])] = none) :
    plan ⟨[.simple [] (first :: rest) []]⟩ = .epsh := by
  simp [plan, text, kind, checkedRoute, safe]

inductive TargetPolicy where
  | unrestricted | ish
  deriving Repr, BEq

inductive PlacementContext where
  | parent | negatedParent | pipeline | negatedPipeline | substitution | negatedSubstitution
  deriving Repr, BEq, DecidableEq

def decodePlacementContext (name : String) : Except String PlacementContext :=
  match name with
  | "parent" => .ok .parent
  | "negated-parent" => .ok .negatedParent
  | "pipeline" => .ok .pipeline
  | "negated-pipeline" => .ok .negatedPipeline
  | "substitution" => .ok .substitution
  | "negated-substitution" => .ok .negatedSubstitution
  | _ => .error "unknown placement context"

def execName : Exec → String
  | .inShell => "in_shell"
  | .hybrid => "hybrid"
  | .foreground => "foreground"
  | .native => "native"

def encodeExec : Option Exec → Json
  | none => .null
  | some exec => toJson (execName exec)

def placementAllowed (context : PlacementContext) : Bool :=
  context == .parent || context == .negatedParent

def negatedPlacement (context : PlacementContext) : Bool :=
  context == .negatedParent || context == .negatedPipeline || context == .negatedSubstitution

def enclosingPlacementStatus (context : PlacementContext) (targetStatus : Nat) : Nat :=
  if negatedPlacement context && targetStatus != 130 then
    if targetStatus == 0 then 1 else 0
  else targetStatus

def targetAllowedAt (target : Bytes) (context : PlacementContext) : Bool :=
  dynamicAllowed target && placementAllowed context

def enforcePlacementTarget (target : Bytes) (context : PlacementContext)
    (state : ShellState) (prior : Trace) : Outcome Execution :=
  if targetAllowedAt target context then .success { state, status := 0, trace := prior }
  else .semanticError (.policy target) { state with lastStatus := 2 } prior

theorem placement_refusal_retains_prior_effects (target : Bytes) (context : PlacementContext)
    (state : ShellState) (prior : Trace) (refused : targetAllowedAt target context = false) :
    enforcePlacementTarget target context state prior =
      .semanticError (.policy target) { state with lastStatus := 2 } prior := by
  simp [enforcePlacementTarget, refused]

def allowsTarget : TargetPolicy → Bytes → Bool
  | .unrestricted, _ => true
  | .ish, target => dynamicAllowed target

theorem unrestricted_preserves_epsh (target : Bytes) :
    allowsTarget .unrestricted target = true := rfl

theorem ish_dynamic_source_refusal (target : Bytes) (forbidden : forbiddenTarget target = true) :
    allowsTarget .ish target = false := by
  simp [allowsTarget, dynamicAllowed, forbidden]

theorem ish_dynamic_shell_state_refusal (target : Bytes) (shell : classify target = some .inShell) :
    allowsTarget .ish target = false := by
  cases forbidden : forbiddenTarget target <;>
    cases unavailable : unavailableEpshBuiltin target <;>
    simp [allowsTarget, dynamicAllowed, shell, forbidden, unavailable] <;> decide

example : dynamicAllowed "cp".toUTF8.toList = true := by decide +kernel
example : dynamicAllowed "mv".toUTF8.toList = true := by decide +kernel
example : decodePlacementContext "pipeline" = .ok .pipeline := by rfl
example : decodePlacementContext "negated-parent" = .ok .negatedParent := by rfl
example : decodePlacementContext "parallel" = .error "unknown placement context" := by rfl

theorem cp_foreground_class : classify "cp".toUTF8.toList = some .foreground := by decide +kernel
theorem mv_foreground_class : classify "mv".toUTF8.toList = some .foreground := by decide +kernel
theorem t_in_shell_class : classify "t".toUTF8.toList = some .inShell := by decide +kernel
theorem trash_in_shell_class : classify "trash".toUTF8.toList = some .inShell := by decide +kernel
theorem foreground_parent_allowed : targetAllowedAt "cp".toUTF8.toList .parent = true := by decide +kernel
theorem foreground_pipeline_refused : targetAllowedAt "cp".toUTF8.toList .pipeline = false := by decide +kernel
theorem foreground_substitution_refused : targetAllowedAt "mv".toUTF8.toList .substitution = false := by decide +kernel
theorem trash_dynamic_refused : dynamicAllowed "trash".toUTF8.toList = false := by decide +kernel
theorem negated_foreground_parent_allowed : targetAllowedAt "mv".toUTF8.toList .negatedParent = true := by decide +kernel
theorem negated_foreground_pipeline_refused : targetAllowedAt "mv".toUTF8.toList .negatedPipeline = false := by decide +kernel
theorem negated_foreground_substitution_refused : targetAllowedAt "mv".toUTF8.toList .negatedSubstitution = false := by decide +kernel
theorem negated_parent_success_inverts : enclosingPlacementStatus .negatedParent 0 = 1 := by decide +kernel
theorem negated_pipeline_refusal_inverts : enclosingPlacementStatus .negatedPipeline 2 = 0 := by decide +kernel
theorem negated_status_preserves_cancellation : enclosingPlacementStatus .negatedPipeline 130 = 130 := by decide +kernel

theorem static_foreground_plan_remains_epsh :
    plan ⟨[.simple [] [literalWord "cp", literalWord "source", literalWord "destination"] []]⟩ = .epsh ∧
    plan ⟨[.pipeline [.simple [] [literalWord "cp", literalWord "source", literalWord "destination"] []] true]⟩ = .epsh ∧
    plan ⟨[.pipeline [.simple [] [literalWord "cp", literalWord "source", literalWord "destination"] [],
      .simple [] [literalWord "cat"] []] false]⟩ = .epsh ∧
    plan ⟨[.simple [] [literalWord "echo",
      .mk [.cmdSubst (.simple [] [literalWord "mv", literalWord "source", literalWord "destination"] [])]] []]⟩ = .epsh := by
  decide +kernel


def arrayItems (json : Json) : Except String (List Json) :=
  return (← json.getArr?).toList

def decodeOptional (decode : Json → Except String α) (json : Json) : Except String (Option α) :=
  match json with
  | .null => pure none
  | _ => return some (← decode json)

mutual
  def decodeCommand : Nat → Json → Except String Command
    | 0, _ => .error "fuel exhausted"
    | fuel + 1, json => do
      match ← arrayItems json with
      | [.str "simple", assigns, args, redirs] =>
        return .simple (← (← arrayItems assigns).mapM (decodeAssignment fuel))
          (← (← arrayItems args).mapM (decodeWord fuel))
          (← (← arrayItems redirs).mapM (decodeRedir fuel))
      | [.str "pipeline", commands, bang] =>
        return .pipeline (← (← arrayItems commands).mapM (decodeCommand fuel)) (← bang.getBool?)
      | [.str "and", left, right] => return .and (← decodeCommand fuel left) (← decodeCommand fuel right)
      | [.str "or", left, right] => return .or (← decodeCommand fuel left) (← decodeCommand fuel right)
      | [.str "sequence", left, right] => return .sequence (← decodeCommand fuel left) (← decodeCommand fuel right)
      | [.str "not", body] => return .not (← decodeCommand fuel body)
      | [.str "background", body, redirs] =>
        return .background (← decodeCommand fuel body) (← (← arrayItems redirs).mapM (decodeRedir fuel))
      | [.str "subshell", body, redirs] =>
        return .subshell (← decodeCommand fuel body) (← (← arrayItems redirs).mapM (decodeRedir fuel))
      | [.str "brace_group", body, redirs] =>
        return .braceGroup (← decodeCommand fuel body) (← (← arrayItems redirs).mapM (decodeRedir fuel))
      | [.str "func_def", name, body] => return .funcDef (← name.getStr?) (← decodeCommand fuel body)
      | [.str "if", condition, thenPart, elsePart] =>
        return .ifThen (← decodeCommand fuel condition) (← decodeCommand fuel thenPart)
          (← decodeOptional (decodeCommand fuel) elsePart)
      | [.str "while", condition, body] => return .whileLoop (← decodeCommand fuel condition) (← decodeCommand fuel body)
      | [.str "until", condition, body] => return .untilLoop (← decodeCommand fuel condition) (← decodeCommand fuel body)
      | [.str "for", name, words, body] =>
        return .forLoop (← name.getStr?)
          (← decodeOptional (fun value => do (← arrayItems value).mapM (decodeWord fuel)) words)
          (← decodeCommand fuel body)
      | [.str "case", word, arms] =>
        return .caseOf (← decodeWord fuel word) (← (← arrayItems arms).mapM (decodeArm fuel))
      | _ => throw "invalid command tag or arity"
  def decodeWord : Nat → Json → Except String Word
    | 0, _ => .error "fuel exhausted"
    | fuel + 1, json => return .mk (← (← arrayItems json).mapM (decodePart fuel))
  def decodePart : Nat → Json → Except String WordPart
    | 0, _ => .error "fuel exhausted"
    | fuel + 1, json => do
      match ← arrayItems json with
      | [.str "literal", bytes] => return .literal (← Protocol.decodeBytes bytes)
      | [.str "single_quoted", bytes] => return .singleQuoted (← Protocol.decodeBytes bytes)
      | [.str "tilde", bytes] => return .tilde (← Protocol.decodeBytes bytes)
      | [.str "double_quoted", parts] => return .doubleQuoted (← (← arrayItems parts).mapM (decodePart fuel))
      | [.str "arith", parts] => return .arith (← (← arrayItems parts).mapM (decodePart fuel))
      | [.str "param", name, op] => return .param (← name.getStr?) (← decodeOp fuel op)
      | [.str "cmd_subst", body] => return .cmdSubst (← decodeCommand fuel body)
      | [.str "backtick", body] => return .backtick (← decodeCommand fuel body)
      | _ => throw "invalid word-part tag or arity"
  def decodeOp : Nat → Json → Except String ParamOp
    | 0, _ => .error "fuel exhausted"
    | fuel + 1, json => do
      match ← arrayItems json with
      | [.str "normal"] => return .normal
      | [.str "length"] => return .length
      | [.str "bad_subst"] => return .badSubst
      | [.str "default", colon, parts] => return .default (← colon.getBool?) (← (← arrayItems parts).mapM (decodePart fuel))
      | [.str "assign", colon, parts] => return .assign (← colon.getBool?) (← (← arrayItems parts).mapM (decodePart fuel))
      | [.str "error", colon, parts] => return .error (← colon.getBool?) (← (← arrayItems parts).mapM (decodePart fuel))
      | [.str "alternative", colon, parts] => return .alternative (← colon.getBool?) (← (← arrayItems parts).mapM (decodePart fuel))
      | [.str "trim_suffix_small", parts] => return .trimSuffixSmall (← (← arrayItems parts).mapM (decodePart fuel))
      | [.str "trim_suffix_large", parts] => return .trimSuffixLarge (← (← arrayItems parts).mapM (decodePart fuel))
      | [.str "trim_prefix_small", parts] => return .trimPrefixSmall (← (← arrayItems parts).mapM (decodePart fuel))
      | [.str "trim_prefix_large", parts] => return .trimPrefixLarge (← (← arrayItems parts).mapM (decodePart fuel))
      | _ => throw "invalid parameter-operation tag or arity"
  def decodeAssignment : Nat → Json → Except String Assignment
    | 0, _ => .error "fuel exhausted"
    | fuel + 1, json => do
      match ← arrayItems json with
      | [name, word] => return .mk (← name.getStr?) (← decodeWord fuel word)
      | _ => throw "invalid assignment arity"
  def decodeRedir : Nat → Json → Except String Redir
    | 0, _ => .error "fuel exhausted"
    | fuel + 1, json => do
      match ← arrayItems json with
      | [fd, kind] => return .mk (← fd.getInt?) (← decodeRedirKind fuel kind)
      | _ => throw "invalid redirection arity"
  def decodeRedirKind : Nat → Json → Except String RedirKind
    | 0, _ => .error "fuel exhausted"
    | fuel + 1, json => do
      match ← arrayItems json with
      | [.str "input", word] => return .input (← decodeWord fuel word)
      | [.str "output", word] => return .output (← decodeWord fuel word)
      | [.str "clobber", word] => return .clobber (← decodeWord fuel word)
      | [.str "append", word] => return .append (← decodeWord fuel word)
      | [.str "read_write", word] => return .readWrite (← decodeWord fuel word)
      | [.str "dup_input", word] => return .dupInput (← decodeWord fuel word)
      | [.str "dup_output", word] => return .dupOutput (← decodeWord fuel word)
      | [.str "here_doc", body] => return .hereDoc (← decodeHereDoc fuel body)
      | [.str "here_doc_strip", body] => return .hereDocStrip (← decodeHereDoc fuel body)
      | _ => throw "invalid redirection-kind tag or arity"
  def decodeHereDoc : Nat → Json → Except String HereDocBody
    | 0, _ => .error "fuel exhausted"
    | fuel + 1, json => do
      match ← arrayItems json with
      | [.str "literal", bytes] => return .literal (← Protocol.decodeBytes bytes)
      | [.str "parsed", parts] => return .parsed (← (← arrayItems parts).mapM (decodePart fuel))
      | _ => throw "invalid here-document tag or arity"
  def decodeArm : Nat → Json → Except String CaseArm
    | 0, _ => .error "fuel exhausted"
    | fuel + 1, json => do
      match ← arrayItems json with
      | [patterns, body] =>
        return .mk (← (← arrayItems patterns).mapM (decodeWord fuel))
          (← decodeOptional (decodeCommand fuel) body)
      | _ => throw "invalid case-arm arity"
end

def rejectionName : Rejection → String
  | .background => "background"
  | .compound => "compound"
  | .functionDefinition => "function_definition"
  | .source => "source"
  | .unsupportedBuiltin => "unsupported_builtin"
  | .arguments => "arguments"
  | .parameterOperator => "parameter_operator"
  | .shellBuiltinOutsideLine => "shell_builtin_outside_line"

def encodeRoute : Route → Json
  | .epsh => .mkObj [("route", toJson "epsh"), ("builtin", .null), ("rejection", .null)]
  | .builtin name => .mkObj [("route", toJson "builtin"),
      ("builtin", toJson (String.fromUTF8! ⟨name.toArray⟩)), ("rejection", .null)]
  | .reject reason => .mkObj [("route", toJson "reject"), ("builtin", .null),
      ("rejection", toJson (rejectionName reason))]

def run (request : Protocol.Request) : Outcome Json :=
  let decoded : Except String Json := do
    let mode ← (← request.input.getObjVal? "mode").getStr?
    match mode with
    | "static" =>
      Protocol.exactObject ["mode", "ast"] request.input
      let ast ← request.input.getObjVal? "ast"
      let commands ← (← arrayItems ast).mapM (decodeCommand request.fuel)
      pure (encodeRoute (plan ⟨commands⟩))
    | "dynamic" =>
      Protocol.exactObject ["mode", "target"] request.input
      let target ← Protocol.decodeBytes (← request.input.getObjVal? "target")
      let allowed := dynamicAllowed target
      pure (.mkObj [("allowed", toJson allowed), ("status", toJson (if allowed then (0 : Nat) else 2))])
    | "placement" =>
      Protocol.exactObject ["mode", "target", "context"] request.input
      let target ← Protocol.decodeBytes (← request.input.getObjVal? "target")
      let contextName ← (← request.input.getObjVal? "context").getStr?
      let context ← decodePlacementContext contextName
      let allowed := targetAllowedAt target context
      let targetStatus := if allowed then (0 : Nat) else 2
      pure (.mkObj [
        ("allowed", toJson allowed),
        ("status", toJson (enclosingPlacementStatus context targetStatus)),
        ("class", encodeExec (classify target))])
    | _ => throw "unknown policy mode"
  match decoded with
  | .ok value => .success value
  | .error "fuel exhausted" => .fuelExhausted
  | .error reason => .unsupportedDomain reason

theorem decoder_zero_fuel (json : Json) : decodeCommand 0 json = .error "fuel exhausted" := rfl

def mutationCommand : Command := .background (.simple [] [] []) []
def mutationWord : Word := .mk [.cmdSubst mutationCommand]

example : checkTree (commandTree (.simple [.mk "x" mutationWord] [] [])) = some .background := by decide +kernel
example : checkTree (commandTree (.simple [] [] [.mk 1 (.output mutationWord)])) = some .background := by decide +kernel
example : checkTree (commandTree (.simple [] [] [.mk 0 (.hereDocStrip (.parsed [.cmdSubst mutationCommand]))])) = some .background := by decide +kernel
example : checkTree (commandTree (.simple [] [.mk [.param "x" (.default false [])]] [])) =
    some .parameterOperator := by decide +kernel
example : checkTree (commandTree (.simple [] [.mk [.param "x" .badSubst]] [])) = none := by decide +kernel
example : plan ⟨[.simple [] [.mk [.param "target" .normal]] []]⟩ = .epsh := by decide +kernel
example : plan ⟨[.simple [] [literalWord "eval"] []]⟩ = .reject .source := by decide +kernel
example : plan ⟨[.simple [] [literalWord "exec"] []]⟩ = .reject .unsupportedBuiltin := by decide +kernel
example : plan ⟨[.simple [] [literalWord "set", literalWord "-e"] []]⟩ = .reject .arguments := by decide +kernel
example : plan ⟨[.pipeline [.simple [] [literalWord "cd"] []] false]⟩ = .epsh := by decide +kernel
example : decodeCommand 20 (.arr #[toJson "simple", .arr #[], .arr #[], .arr #[], .null]) =
    .error "invalid command tag or arity" := rfl

end IshSemantics.Policy
