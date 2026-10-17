use crate::error::Span;
use crate::shell_bytes::ShellBytes;

/// A complete shell program: a list of complete commands separated by
/// newlines or semicolons.
#[derive(Debug, Clone)]
pub struct Program {
    pub commands: Vec<Command>,
}

/// A shell command (any node in the command grammar).
#[derive(Debug, Clone)]
pub enum Command {
    /// Simple command: optional assignments, command words, and redirections.
    /// The first command word is the command name when present.
    /// `FOO=bar cmd arg1 arg2 >file`
    Simple {
        assigns: Vec<Assignment>,
        args: Vec<Word>,
        redirs: Vec<Redir>,
        span: Span,
    },
    /// Pipeline: `cmd1 | cmd2 | cmd3`, optionally negated with `!`
    Pipeline {
        commands: Vec<Command>,
        bang: bool,
        span: Span,
    },
    /// AND list: `left && right`
    And(Box<Command>, Box<Command>),
    /// OR list: `left || right`
    Or(Box<Command>, Box<Command>),
    /// Sequential: `left ; right` or `left <newline> right`
    Sequence(Box<Command>, Box<Command>),
    /// Subshell: `( list )`
    Subshell {
        body: Box<Command>,
        redirs: Vec<Redir>,
        span: Span,
    },
    /// Brace group: `{ list; }`
    BraceGroup {
        body: Box<Command>,
        redirs: Vec<Redir>,
        span: Span,
    },
    /// If statement: `if cond; then body; [elif ...;] [else ...;] fi`
    If {
        cond: Box<Command>,
        then_part: Box<Command>,
        else_part: Option<Box<Command>>,
        span: Span,
    },
    /// While loop: `while cond; do body; done`
    While {
        cond: Box<Command>,
        body: Box<Command>,
        span: Span,
    },
    /// Until loop: `until cond; do body; done`
    Until {
        cond: Box<Command>,
        body: Box<Command>,
        span: Span,
    },
    /// For loop: `for var [in words...]; do body; done`
    /// `words` is None for `for var; do ...` (iterates over "$@")
    For {
        var: String,
        words: Option<Vec<Word>>,
        body: Box<Command>,
        span: Span,
    },
    /// Case statement: `case word in (pattern) body ;; ... esac`
    Case {
        word: Word,
        arms: Vec<CaseArm>,
        span: Span,
    },
    /// Function definition: `name() body`
    FuncDef {
        name: String,
        body: Box<Command>,
        span: Span,
    },
    /// Negation: `! command`
    Not(Box<Command>),
    /// Background: `command &`
    Background {
        cmd: Box<Command>,
        redirs: Vec<Redir>,
    },
}

/// A case arm: `pattern1 | pattern2) body ;;`
#[derive(Debug, Clone)]
pub struct CaseArm {
    pub patterns: Vec<Word>,
    pub body: Option<Command>,
    pub span: Span,
}

/// Variable assignment: `name=word`
#[derive(Debug, Clone)]
pub struct Assignment {
    pub name: String,
    pub value: Word,
    pub span: Span,
}

/// A shell word is one lexical word token made from concatenated parts.
/// Expansion may turn it into zero or more fields. For example,
/// `hello"world"${x}` is three parts: Literal, DoubleQuoted, Param.
#[derive(Debug, Clone)]
pub struct Word {
    pub parts: Vec<WordPart>,
    pub span: Span,
}

/// A component of a shell word.
#[derive(Debug, Clone)]
pub enum WordPart {
    /// Unquoted literal text
    Literal(ShellBytes),
    /// Single-quoted string (no expansion)
    SingleQuoted(ShellBytes),
    /// Double-quoted region (expansions apply, but no field splitting or globbing)
    DoubleQuoted(Vec<WordPart>),
    /// Parameter/variable expansion: `$var`, `${var}`, `${var:-default}`, `${#var}`, etc.
    Param(ParamExpr),
    /// Command substitution: `$(command)`
    CmdSubst(Box<Command>),
    /// Backtick command substitution: `` `command` ``
    Backtick(Box<Command>),
    /// Arithmetic expansion: `$((expression))`
    Arith(Vec<WordPart>),
    /// Tilde prefix: `~` or `~user`
    Tilde(ShellBytes),
}

/// Parameter expansion expression.
#[derive(Debug, Clone)]
pub struct ParamExpr {
    /// Variable name, or special char (`@`, `*`, `#`, `?`, `-`, `$`, `!`, `0`-`9`)
    pub name: String,
    /// Operation to perform
    pub op: ParamOp,
    pub span: Span,
}

/// Parameter expansion operations (POSIX 2.6.2).
#[derive(Debug, Clone)]
pub enum ParamOp {
    /// `$var` or `${var}` — simple expansion
    Normal,
    /// `${#var}` — string length
    Length,
    /// `${var-word}` or `${var:-word}` — use default
    Default { colon: bool, word: Vec<WordPart> },
    /// `${var=word}` or `${var:=word}` — assign default
    Assign { colon: bool, word: Vec<WordPart> },
    /// `${var?word}` or `${var:?word}` — error if unset
    Error { colon: bool, word: Vec<WordPart> },
    /// `${var+word}` or `${var:+word}` — use alternative
    Alternative { colon: bool, word: Vec<WordPart> },
    /// `${var%pattern}` — remove smallest suffix
    TrimSuffixSmall(Vec<WordPart>),
    /// `${var%%pattern}` — remove largest suffix
    TrimSuffixLarge(Vec<WordPart>),
    /// `${var#pattern}` — remove smallest prefix
    TrimPrefixSmall(Vec<WordPart>),
    /// `${var##pattern}` — remove largest prefix
    TrimPrefixLarge(Vec<WordPart>),
    /// Invalid substitution syntax — produces error at expansion time
    BadSubst,
}

/// I/O redirection.
#[derive(Debug, Clone)]
pub struct Redir {
    /// File descriptor number (default: 0 for input, 1 for output)
    pub fd: i32,
    pub kind: RedirKind,
    pub span: Span,
}

/// Pre-parsed heredoc body.
#[derive(Debug, Clone)]
pub enum HereDocBody {
    /// Quoted heredoc (<<'EOF') — no expansion, literal text
    Literal(ShellBytes),
    /// Unquoted heredoc (<<EOF) — contains expandable word parts
    Parsed(Vec<WordPart>),
}

/// Redirection type.
#[derive(Debug, Clone)]
pub enum RedirKind {
    /// `< file`
    Input(Word),
    /// `> file`
    Output(Word),
    /// `>| file` (clobber)
    Clobber(Word),
    /// `>> file`
    Append(Word),
    /// `<> file` (read-write)
    ReadWrite(Word),
    /// `<& fd` or `<& -`
    DupInput(Word),
    /// `>& fd` or `>& -`
    DupOutput(Word),
    /// `<< delimiter` (here-document)
    HereDoc(HereDocBody),
    /// `<<- delimiter` (here-document, strip tabs)
    HereDocStrip(HereDocBody),
}
