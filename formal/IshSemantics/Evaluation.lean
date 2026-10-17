import IshSemantics.Protocol

namespace IshSemantics.Evaluation
open Lean

inductive Expr where
  | simple (assignments : Bindings) (argv : List Bytes) (observation : Nat)
      (assignmentStatus : Option UInt8)
  | sequence (left right : Expr)
  | and (left right : Expr)
  | or (left right : Expr)
  | not (body : Expr)
  | pipeline (commands : List Expr) (bang : Bool)
  | subshell (body : Expr)
  deriving Repr

structure Options where
  errexit : Bool
  pipefail : Bool
  deriving Repr

structure LeafObservation where
  status : UInt8
  stdout : Bytes
  stderr : Bytes
  deriving Repr

structure Result where
  execution : Execution
  halted : Bool := false
  deriving Repr

def setBinding (bindings : Bindings) (name : String) (value : Bytes) : Bindings :=
  (name, value) :: bindings.filter (fun pair => pair.1 != name)

def assign (state : ShellState) (bindings : Bindings) : ShellState :=
  bindings.foldl (fun state (name, value) =>
    { state with variables := setBinding state.variables name value
                 exported := if state.exported.any (fun pair => pair.1 == name)
                   then setBinding state.exported name value else state.exported }) state

def setEnvironment (environment : List (Bytes × Bytes)) (name value : Bytes) :=
  (name, value) :: environment.filter (fun pair => pair.1 != name)

def environment (state : ShellState) (bindings : Bindings) : List (Bytes × Bytes) :=
  (state.exported ++ bindings).foldl (fun env (name, value) =>
    setEnvironment env name.toUTF8.toList value) state.inherited

def invert (status : UInt8) : UInt8 := if status == 0 then 1 else 0

def pipelineStatus (pipefail : Bool) (statuses : List UInt8) : UInt8 :=
  statuses.foldl (fun previous status =>
    if pipefail && status == 0 then previous else status) 0

def finish (opts : Options) (tested leaf : Bool) (execution : Execution) : Result :=
  { execution, halted := opts.errexit && leaf && !tested && execution.status != 0 }

def appendExecution (first second : Execution) : Execution :=
  { second with stdout := first.stdout ++ second.stdout
                stderr := first.stderr ++ second.stderr
                trace := first.trace ++ second.trace }

def prepend (first : Execution) : Outcome Result → Outcome Result
  | .success second => .success { second with execution := appendExecution first second.execution }
  | .semanticError error state trace => .semanticError error state (first.trace ++ trace)
  | .unsupportedDomain reason => .unsupportedDomain reason
  | .fuelExhausted => .fuelExhausted

def eval (observations : List LeafObservation) (opts : Options) :
    Nat → Bool → Expr → ShellState → Outcome Result
  | 0, _, _, _ => .fuelExhausted
  | fuel + 1, tested, expr, state =>
    match expr with
    | .simple bindings argv index assignmentStatus =>
      if argv.isEmpty then
        let status := assignmentStatus.getD 0
        .success (finish opts tested true
          { state := { assign state bindings with lastStatus := status }, status })
      else match observations[index]? with
        | none => .unsupportedDomain "missing leaf observation"
        | some observation =>
          let trace := [.invoke argv (environment state bindings) state.cwd] ++
            (if observation.stdout.isEmpty then [] else [.write 1 observation.stdout]) ++
            (if observation.stderr.isEmpty then [] else [.write 2 observation.stderr])
          .success (finish opts tested true
            { state := { state with lastStatus := observation.status },
              status := observation.status, stdout := observation.stdout,
              stderr := observation.stderr, trace })
    | .sequence left right =>
      match eval observations opts fuel tested left state with
      | .success first => if first.halted then .success first
          else prepend first.execution (eval observations opts fuel tested right first.execution.state)
      | .semanticError error state trace => .semanticError error state trace
      | .unsupportedDomain reason => .unsupportedDomain reason
      | .fuelExhausted => .fuelExhausted
    | .and left right =>
      match eval observations opts fuel true left state with
      | .success first => if first.execution.status != 0 then .success first
          else prepend first.execution (eval observations opts fuel tested right first.execution.state)
      | .semanticError error state trace => .semanticError error state trace
      | .unsupportedDomain reason => .unsupportedDomain reason
      | .fuelExhausted => .fuelExhausted
    | .or left right =>
      match eval observations opts fuel true left state with
      | .success first => if first.execution.status == 0 then .success first
          else prepend first.execution (eval observations opts fuel tested right first.execution.state)
      | .semanticError error state trace => .semanticError error state trace
      | .unsupportedDomain reason => .unsupportedDomain reason
      | .fuelExhausted => .fuelExhausted
    | .not body =>
      match eval observations opts fuel true body state with
      | .success result =>
        let status := invert result.execution.status
        .success { execution := { result.execution with
          status := status
          state := { result.execution.state with lastStatus := status } } }
      | .semanticError error state trace => .semanticError error state trace
      | .unsupportedDomain reason => .unsupportedDomain reason
      | .fuelExhausted => .fuelExhausted
    | .subshell body =>
      match eval observations opts fuel tested body state with
      | .success result => .success (finish opts tested true
          { result.execution with state := { state with lastStatus := result.execution.status } })
      | .semanticError error _ trace => .semanticError error state trace
      | .unsupportedDomain reason => .unsupportedDomain reason
      | .fuelExhausted => .fuelExhausted
    | .pipeline commands bang =>
      match commands with
      | [] => .unsupportedDomain "empty pipeline"
      | [command] =>
        match eval observations opts fuel (tested || bang) command state with
        | .success result =>
          let status := if bang then invert result.execution.status else result.execution.status
          .success (finish opts (tested || bang) true
            { result.execution with status, state := { result.execution.state with lastStatus := status } })
        | .semanticError error state trace => .semanticError error state trace
        | .unsupportedDomain reason => .unsupportedDomain reason
        | .fuelExhausted => .fuelExhausted
      | _ =>
        let stages := commands.map (fun command => eval observations opts fuel (tested || bang) command state)
        let collected := stages.foldl (fun acc stage =>
          match acc, stage with
          | .success (statuses, trace, stderr, _), .success result =>
            .success (statuses ++ [result.execution.status], trace ++ result.execution.trace,
              stderr ++ result.execution.stderr, result.execution.stdout)
          | .success _, .semanticError error state trace => .semanticError error state trace
          | .success _, .unsupportedDomain reason => .unsupportedDomain reason
          | .success _, .fuelExhausted => .fuelExhausted
          | .semanticError error state trace, _ => .semanticError error state trace
          | .unsupportedDomain reason, _ => .unsupportedDomain reason
          | .fuelExhausted, _ => .fuelExhausted)
          (.success ([], [], [], []) : Outcome (List UInt8 × Trace × Bytes × Bytes))
        match collected with
        | .success (statuses, trace, stderr, stdout) =>
          let status := pipelineStatus opts.pipefail statuses
          let status := if bang then invert status else status
          .success (finish opts (tested || bang) true
            { state := { state with lastStatus := status }, status, stdout, stderr, trace })
        | .semanticError error state trace => .semanticError error state trace
        | .unsupportedDomain reason => .unsupportedDomain reason
        | .fuelExhausted => .fuelExhausted

def decodeExpr : Nat → Json → Except String Expr
  | 0, _ => .error "expression exceeds decoding depth"
  | fuel + 1, value => do
    let tag ← (← value.getObjVal? "tag").getStr?
    match tag with
    | "simple" =>
      Protocol.exactObject ["tag", "assignments", "argv", "observation", "assignment_status"] value
      let assignments ← Protocol.decodeBindings (← value.getObjVal? "assignments")
      let argv ← (← (← value.getObjVal? "argv").getArr?).toList.mapM Protocol.decodeBytes
      let observation ← (← value.getObjVal? "observation").getNat?
      let assignmentStatus ← do
        match (← value.getObjVal? "assignment_status") with
        | .null => pure none
        | status => do
          let status ← status.getNat?
          unless status < 256 do throw "assignment status outside 0..255"
          pure (some status.toUInt8)
      return .simple assignments argv observation assignmentStatus
    | "sequence" | "and" | "or" =>
      Protocol.exactObject ["tag", "left", "right"] value
      let left ← decodeExpr fuel (← value.getObjVal? "left")
      let right ← decodeExpr fuel (← value.getObjVal? "right")
      return if tag == "sequence" then .sequence left right
        else if tag == "and" then .and left right else .or left right
    | "not" | "subshell" =>
      Protocol.exactObject ["tag", "body"] value
      let body ← decodeExpr fuel (← value.getObjVal? "body")
      return if tag == "not" then .not body else .subshell body
    | "pipeline" =>
      Protocol.exactObject ["tag", "commands", "bang"] value
      return .pipeline
        (← (← (← value.getObjVal? "commands").getArr?).toList.mapM (decodeExpr fuel))
        (← (← value.getObjVal? "bang").getBool?)
    | _ => throw "unknown evaluation expression"

def decodeObservation (value : Json) : Except String LeafObservation := do
  Protocol.exactObject ["status", "stdout", "stderr"] value
  let status ← (← value.getObjVal? "status").getNat?
  unless status < 256 do throw "status outside 0..255"
  return {
    status := status.toUInt8
    stdout := ← Protocol.decodeBytes (← value.getObjVal? "stdout")
    stderr := ← Protocol.decodeBytes (← value.getObjVal? "stderr") }

def evalRequest (request : Protocol.Request) : Outcome Json :=
  let parsed : Except String (Expr × List LeafObservation × Options) := do
    Protocol.exactObject ["program", "observations", "options"] request.input
    let expr ← decodeExpr 1024 (← request.input.getObjVal? "program")
    let observations ← (← (← request.input.getObjVal? "observations").getArr?).toList.mapM decodeObservation
    let options ← request.input.getObjVal? "options"
    Protocol.exactObject ["errexit", "pipefail"] options
    return (expr, observations, {
      errexit := ← (← options.getObjVal? "errexit").getBool?,
      pipefail := ← (← options.getObjVal? "pipefail").getBool? })
  match parsed with
  | .error reason => .unsupportedDomain reason
  | .ok (expr, observations, opts) =>
    match eval observations opts request.fuel false expr request.state with
    | .success result => .success (Protocol.encodeExecution result.execution)
    | .semanticError error state trace => .semanticError error state trace
    | .unsupportedDomain reason => .unsupportedDomain reason
    | .fuelExhausted => .fuelExhausted

theorem fuel_zero (observations opts tested expr state) :
    eval observations opts 0 tested expr state = .fuelExhausted := rfl

theorem tested_suppresses_errexit (opts execution) :
    (finish opts true true execution).halted = false := by
  simp [finish]

theorem compound_does_not_trigger_errexit (opts tested execution) :
    (finish opts tested false execution).halted = false := by
  simp [finish]

theorem untested_failure_halts (opts : Options) (execution : Execution)
    (enabled : opts.errexit = true) (failed : (execution.status != 0) = true) :
    (finish opts false true execution).halted = true := by
  simp [finish, enabled, failed]

theorem and_failure_skips (observations opts fuel tested left right state first)
    (evaluated : eval observations opts fuel true left state = .success first)
    (failed : (first.execution.status != 0) = true) :
    eval observations opts (fuel + 1) tested (.and left right) state = .success first := by
  simp [eval, evaluated, failed]

theorem or_success_skips (observations opts fuel tested left right state first)
    (evaluated : eval observations opts fuel true left state = .success first)
    (succeeded : (first.execution.status == 0) = true) :
    eval observations opts (fuel + 1) tested (.or left right) state = .success first := by
  simp [eval, evaluated, succeeded]

theorem halted_sequence_skips (observations opts fuel tested left right state first)
    (evaluated : eval observations opts fuel tested left state = .success first)
    (halted : first.halted = true) :
    eval observations opts (fuel + 1) tested (.sequence left right) state = .success first := by
  simp [eval, evaluated, halted]

theorem sequence_composes (observations opts fuel tested left right state first second)
    (leftEvaluated : eval observations opts fuel tested left state = .success first)
    (continuing : first.halted = false)
    (rightEvaluated : eval observations opts fuel tested right first.execution.state = .success second) :
    eval observations opts (fuel + 1) tested (.sequence left right) state =
      .success { second with execution := appendExecution first.execution second.execution } := by
  simp [eval, leftEvaluated, continuing, rightEvaluated, prepend]

theorem composition_trace (first second : Execution) :
    (appendExecution first second).trace = first.trace ++ second.trace := rfl

theorem composition_status (first second : Execution) :
    (appendExecution first second).status = second.status := rfl

theorem composition_state (first second : Execution) :
    (appendExecution first second).state = second.state := rfl

theorem composition_associative (first second third : Execution) :
    appendExecution (appendExecution first second) third =
      appendExecution first (appendExecution second third) := by
  simp [appendExecution, List.append_assoc]

theorem error_retains_prefix (first : Execution) (error state trace) :
    prepend first (.semanticError error state trace) =
      .semanticError error state (first.trace ++ trace) := rfl

theorem environment_prefix_overrides (state bindings name value) :
    environment state (bindings ++ [(name, value)]) =
      setEnvironment (environment state bindings) name.toUTF8.toList value := by
  simp [environment, List.foldl_append]

theorem environment_last_assignment_wins (environment name first second) :
    setEnvironment (setEnvironment environment name first) name second =
      setEnvironment environment name second := by
  simp [setEnvironment, List.filter_filter]

theorem bare_assignment_selects_final_substitution_status
    (observations opts fuel tested bindings index assignmentStatus state) :
    eval observations opts (fuel + 1) tested (.simple bindings [] index assignmentStatus) state =
      .success (finish opts tested true {
        state := { assign state bindings with lastStatus := assignmentStatus.getD 0 }
        status := assignmentStatus.getD 0 }) := by
  simp [eval]

theorem pipeline_snoc (pipefail statuses status) :
    pipelineStatus pipefail (statuses ++ [status]) =
      if pipefail && status == 0 then pipelineStatus pipefail statuses else status := by
  simp [pipelineStatus, List.foldl_append]

theorem pipeline_last_without_pipefail (statuses status) :
    pipelineStatus false (statuses ++ [status]) = status := by
  simp [pipeline_snoc]

theorem pipeline_rightmost_failure (statuses status : List UInt8) :
    ∀ failure : UInt8, (failure != 0) = true →
      pipelineStatus true (statuses ++ [failure] ++ status.filter (· == 0)) = failure := by
  intro failure failed
  have nonzero : failure ≠ 0 := by simpa using failed
  have zeros : ∀ tail : List UInt8,
      (tail.filter (· == 0)).foldl (fun previous status =>
        if status = 0 then previous else status) failure = failure := by
    intro tail
    induction tail with
    | nil => rfl
    | cons item tail ih =>
      by_cases zero : (item == 0) = true
      · have equal : item = 0 := by simpa using zero
        simp [equal, ih]
      · simp [zero, ih]
  simp [pipelineStatus, List.foldl_append, nonzero, zeros]

theorem negation_tests_body (observations opts fuel tested body state result)
    (evaluated : eval observations opts fuel true body state = .success result) :
    eval observations opts (fuel + 1) tested (.not body) state =
      .success { execution := { result.execution with
        status := invert result.execution.status
        state := { result.execution.state with lastStatus := invert result.execution.status } } } := by
  simp [eval, evaluated]

theorem subshell_restores_state (observations opts fuel tested body state result)
    (evaluated : eval observations opts fuel tested body state = .success result) :
    eval observations opts (fuel + 1) tested (.subshell body) state =
      .success (finish opts tested true { result.execution with
        state := { state with lastStatus := result.execution.status } }) := by
  simp [eval, evaluated]

theorem invocation_preserves_scope
    (observations opts fuel tested bindings argv index assignmentStatus state observation)
    (nonempty : argv.isEmpty = false)
    (observed : observations[index]? = some observation) :
    eval observations opts (fuel + 1) tested (.simple bindings argv index assignmentStatus) state =
      .success (finish opts tested true {
        state := { state with lastStatus := observation.status }
        status := observation.status
        stdout := observation.stdout
        stderr := observation.stderr
        trace := [.invoke argv (environment state bindings) state.cwd] ++
          (if observation.stdout.isEmpty then [] else [.write 1 observation.stdout]) ++
          (if observation.stderr.isEmpty then [] else [.write 2 observation.stderr]) }) := by
  simp [eval, nonempty, observed]

theorem pipeline_two_isolates (observations opts fuel tested left right state first second bang)
    (leftEvaluated : eval observations opts fuel (tested || bang) left state = .success first)
    (rightEvaluated : eval observations opts fuel (tested || bang) right state = .success second) :
    eval observations opts (fuel + 1) tested (.pipeline [left, right] bang) state =
      let selected := pipelineStatus opts.pipefail [first.execution.status, second.execution.status]
      let status := if bang then invert selected else selected
      .success (finish opts (tested || bang) true {
        state := { state with lastStatus := status }
        status := status
        stdout := second.execution.stdout
        stderr := first.execution.stderr ++ second.execution.stderr
        trace := first.execution.trace ++ second.execution.trace }) := by
  simp [eval, leftEvaluated, rightEvaluated]

end IshSemantics.Evaluation
