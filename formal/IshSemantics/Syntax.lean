import IshSemantics.Types

namespace IshSemantics

mutual
inductive Command where
  | simple (assigns : List Assignment) (args : List Word) (redirs : List Redir)
  | pipeline (commands : List Command) (bang : Bool)
  | and (left right : Command)
  | or (left right : Command)
  | sequence (left right : Command)
  | subshell (body : Command) (redirs : List Redir)
  | braceGroup (body : Command) (redirs : List Redir)
  | ifThen (condition thenPart : Command) (elsePart : Option Command)
  | whileLoop (condition body : Command)
  | untilLoop (condition body : Command)
  | forLoop (name : String) (words : Option (List Word)) (body : Command)
  | caseOf (word : Word) (arms : List CaseArm)
  | funcDef (name : String) (body : Command)
  | not (body : Command)
  | background (body : Command) (redirs : List Redir)
  deriving Repr
inductive Word where
  | mk (parts : List WordPart)
  deriving Repr
inductive WordPart where
  | literal (bytes : Bytes)
  | singleQuoted (bytes : Bytes)
  | doubleQuoted (parts : List WordPart)
  | param (name : String) (op : ParamOp)
  | cmdSubst (command : Command)
  | backtick (command : Command)
  | arith (parts : List WordPart)
  | tilde (user : Bytes)
  deriving Repr
inductive ParamOp where
  | normal | length | badSubst
  | default (colon : Bool) (parts : List WordPart)
  | assign (colon : Bool) (parts : List WordPart)
  | error (colon : Bool) (parts : List WordPart)
  | alternative (colon : Bool) (parts : List WordPart)
  | trimSuffixSmall (parts : List WordPart)
  | trimSuffixLarge (parts : List WordPart)
  | trimPrefixSmall (parts : List WordPart)
  | trimPrefixLarge (parts : List WordPart)
  deriving Repr
inductive Assignment where
  | mk (name : String) (value : Word)
  deriving Repr
inductive Redir where
  | mk (fd : Int) (kind : RedirKind)
  deriving Repr
inductive RedirKind where
  | input (word : Word)
  | output (word : Word)
  | clobber (word : Word)
  | append (word : Word)
  | readWrite (word : Word)
  | dupInput (word : Word)
  | dupOutput (word : Word)
  | hereDoc (body : HereDocBody)
  | hereDocStrip (body : HereDocBody)
  deriving Repr
inductive HereDocBody where
  | literal (bytes : Bytes)
  | parsed (parts : List WordPart)
  deriving Repr
inductive CaseArm where
  | mk (patterns : List Word) (body : Option Command)
  deriving Repr
end

structure Program where
  commands : List Command
  deriving Repr

def Word.parts : Word → List WordPart
  | .mk parts => parts

def Assignment.name : Assignment → String
  | .mk name _ => name

def Assignment.value : Assignment → Word
  | .mk _ value => value

def Redir.fd : Redir → Int
  | .mk fd _ => fd

def Redir.kind : Redir → RedirKind
  | .mk _ kind => kind

end IshSemantics
