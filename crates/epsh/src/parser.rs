use crate::ast::*;
use crate::error::{ShellError, Span};
use crate::lexer::{Lexer, PendingHereDoc, Token, parts_have_quoting, parts_to_text};

/// Recursive-descent parser for POSIX shell grammar.
///
/// Grammar (simplified):
/// ```text
/// program      = linebreak complete_commands linebreak
/// complete_cmds = complete_cmd { newline_list complete_cmd }
/// complete_cmd  = and_or { (';' | '&') and_or } [';' | '&']
/// and_or        = pipeline { ('&&' | '||') linebreak pipeline }
/// pipeline      = ['!'] command { '|' linebreak command }
/// command       = simple_cmd | compound_cmd [redirects] | func_def
/// compound_cmd  = brace_group | subshell | if_clause | for_clause
///               | while_clause | until_clause | case_clause
/// simple_cmd    = { assignment | word | redirect }  (at least one)
/// ```
pub struct Parser {
    lexer: Lexer,
    /// Heredoc bodies read at newlines, waiting to be patched into the AST.
    heredoc_bodies: Vec<HereDocBody>,
}

impl Parser {
    pub fn new(source: &str) -> Self {
        Parser {
            lexer: Lexer::new(source),
            heredoc_bodies: Vec::new(),
        }
    }

    pub fn new_bytes(source: &[u8]) -> Self {
        Parser {
            lexer: Lexer::new_bytes(source),
            heredoc_bodies: Vec::new(),
        }
    }

    /// Parse the entire input as a shell program.
    pub fn parse(&mut self) -> Result<Program, ShellError> {
        let commands = self.parse_program()?;
        Ok(Program { commands })
    }

    /// Peek at the next token without consuming it.
    fn peek(&mut self) -> Result<(Token, Span), ShellError> {
        let (tok, span) = self.lexer.next_token()?;
        self.lexer.push_back(tok.clone(), span);
        Ok((tok, span))
    }

    /// Consume the next token.
    fn next(&mut self) -> Result<(Token, Span), ShellError> {
        self.lexer.next_token()
    }

    /// Consume the next token, expecting a specific token type.
    fn expect(&mut self, expected: &Token) -> Result<Span, ShellError> {
        let (tok, span) = self.next()?;
        if &tok == expected {
            Ok(span)
        } else {
            Err(ShellError::Syntax {
                msg: format!("expected {expected:?}, got {tok:?}"),
                span,
            })
        }
    }

    /// Consume any newlines (linebreak in the grammar).
    fn skip_newlines(&mut self) -> Result<(), ShellError> {
        loop {
            let (tok, span) = self.next()?;
            if tok == Token::Newline {
                // Dash reads heredoc bodies at newlines (parseheredoc in readtoken).
                // We store a dummy command for patching if there are pending heredocs.
                if !self.lexer.pending_heredocs.is_empty() {
                    let pending: Vec<PendingHereDoc> =
                        std::mem::take(&mut self.lexer.pending_heredocs);
                    for heredoc in &pending {
                        let raw = self.lexer.read_heredoc_body(heredoc)?;
                        self.heredoc_bodies
                            .push(make_heredoc_body(raw, heredoc.quoted));
                    }
                }
            } else {
                self.lexer.push_back(tok, span);
                return Ok(());
            }
        }
    }

    /// Patch empty heredoc bodies in a command tree with the actual content.
    fn patch_heredocs(cmd: &mut Command, bodies: &[HereDocBody]) {
        let mut idx = 0;
        Self::patch_heredocs_inner(cmd, bodies, &mut idx);
    }

    fn patch_heredoc_redirs(redirs: &mut [Redir], bodies: &[HereDocBody], idx: &mut usize) {
        for redir in redirs.iter_mut() {
            if *idx < bodies.len() {
                match &redir.kind {
                    RedirKind::HereDoc(HereDocBody::Literal(s)) if s.is_empty() => {
                        redir.kind = RedirKind::HereDoc(bodies[*idx].clone());
                        *idx += 1;
                    }
                    RedirKind::HereDocStrip(HereDocBody::Literal(s)) if s.is_empty() => {
                        redir.kind = RedirKind::HereDocStrip(bodies[*idx].clone());
                        *idx += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    fn patch_heredocs_inner(cmd: &mut Command, bodies: &[HereDocBody], idx: &mut usize) {
        match cmd {
            Command::Simple { redirs, .. } => {
                Self::patch_heredoc_redirs(redirs, bodies, idx);
            }
            Command::Pipeline { commands, .. } => {
                for c in commands.iter_mut() {
                    Self::patch_heredocs_inner(c, bodies, idx);
                }
            }
            Command::And(l, r) | Command::Or(l, r) | Command::Sequence(l, r) => {
                Self::patch_heredocs_inner(l, bodies, idx);
                Self::patch_heredocs_inner(r, bodies, idx);
            }
            Command::Subshell { body, redirs, .. } | Command::BraceGroup { body, redirs, .. } => {
                Self::patch_heredocs_inner(body, bodies, idx);
                Self::patch_heredoc_redirs(redirs, bodies, idx);
            }
            Command::FuncDef { body, .. } => {
                Self::patch_heredocs_inner(body, bodies, idx);
            }
            Command::If {
                cond,
                then_part,
                else_part,
                ..
            } => {
                Self::patch_heredocs_inner(cond, bodies, idx);
                Self::patch_heredocs_inner(then_part, bodies, idx);
                if let Some(e) = else_part {
                    Self::patch_heredocs_inner(e, bodies, idx);
                }
            }
            Command::While { cond, body, .. } | Command::Until { cond, body, .. } => {
                Self::patch_heredocs_inner(cond, bodies, idx);
                Self::patch_heredocs_inner(body, bodies, idx);
            }
            Command::For { body, .. } => {
                Self::patch_heredocs_inner(body, bodies, idx);
            }
            Command::Case { arms, .. } => {
                for arm in arms.iter_mut() {
                    if let Some(b) = &mut arm.body {
                        Self::patch_heredocs_inner(b, bodies, idx);
                    }
                }
            }
            Command::Not(inner) => {
                Self::patch_heredocs_inner(inner, bodies, idx);
            }
            Command::Background { cmd, redirs } => {
                Self::patch_heredocs_inner(cmd, bodies, idx);
                Self::patch_heredoc_redirs(redirs, bodies, idx);
            }
        }
    }

    /// program = linebreak complete_commands linebreak EOF
    fn parse_program(&mut self) -> Result<Vec<Command>, ShellError> {
        self.skip_newlines()?;
        let mut commands = Vec::new();

        loop {
            let (tok, _) = self.peek()?;
            if tok == Token::Eof {
                break;
            }
            let mut cmd = self.parse_complete_command()?;
            self.read_pending_heredocs(&mut cmd)?;
            commands.push(cmd);
            self.skip_newlines()?;
        }

        Ok(commands)
    }

    /// complete_command = and_or { (';' | '&') and_or } [';' | '&']
    /// Used at the top level where newlines terminate a command.
    fn parse_complete_command(&mut self) -> Result<Command, ShellError> {
        self.parse_command_list(false)
    }

    /// compound_list = and_or { (';' | '&' | newline) and_or } [';' | '&']
    /// Used inside compound commands where newlines act as separators.
    fn parse_compound_list(&mut self) -> Result<Command, ShellError> {
        self.parse_command_list(true)
    }

    fn read_pending_heredocs(&mut self, cmd: &mut Command) -> Result<(), ShellError> {
        // First try: read from lexer directly (bodies haven't been consumed by newline yet)
        if !self.lexer.pending_heredocs.is_empty() {
            let pending: Vec<PendingHereDoc> = std::mem::take(&mut self.lexer.pending_heredocs);
            let mut bodies = Vec::new();
            for heredoc in &pending {
                let raw = self.lexer.read_heredoc_body(heredoc)?;
                bodies.push(make_heredoc_body(raw, heredoc.quoted));
            }
            Self::patch_heredocs(cmd, &bodies);
        }
        // Second: apply any bodies that were read at newlines (via skip_newlines)
        if !self.heredoc_bodies.is_empty() {
            let bodies: Vec<HereDocBody> = std::mem::take(&mut self.heredoc_bodies);
            Self::patch_heredocs(cmd, &bodies);
        }
        Ok(())
    }

    fn parse_command_list(&mut self, allow_newline_sep: bool) -> Result<Command, ShellError> {
        let mut cmd = self.parse_and_or()?;

        loop {
            let (tok, _span) = self.peek()?;
            match tok {
                Token::Semi => {
                    self.next()?;
                    self.skip_newlines()?;
                    let (next_tok, _) = self.peek()?;
                    if is_command_start(&next_tok) {
                        let right = self.parse_and_or()?;
                        cmd = Command::Sequence(Box::new(cmd), Box::new(right));
                    }
                }
                Token::Newline if allow_newline_sep => {
                    self.next()?;
                    // Read heredoc bodies that follow this newline
                    if !self.lexer.pending_heredocs.is_empty() {
                        let pending: Vec<PendingHereDoc> =
                            std::mem::take(&mut self.lexer.pending_heredocs);
                        for heredoc in &pending {
                            let raw = self.lexer.read_heredoc_body(heredoc)?;
                            self.heredoc_bodies
                                .push(make_heredoc_body(raw, heredoc.quoted));
                        }
                    }
                    self.skip_newlines()?;
                    let (next_tok, _) = self.peek()?;
                    if is_command_start(&next_tok) {
                        let right = self.parse_and_or()?;
                        cmd = Command::Sequence(Box::new(cmd), Box::new(right));
                    }
                }
                Token::Amp => {
                    self.next()?;
                    cmd = Command::Background {
                        cmd: Box::new(cmd),
                        redirs: Vec::new(),
                    };
                    if allow_newline_sep {
                        self.skip_newlines()?;
                    }
                    let (next_tok, _) = self.peek()?;
                    if is_command_start(&next_tok) {
                        let right = self.parse_and_or()?;
                        cmd = Command::Sequence(Box::new(cmd), Box::new(right));
                    }
                }
                _ => break,
            }
        }

        Ok(cmd)
    }

    /// and_or = pipeline { ('&&' | '||') linebreak pipeline }
    fn parse_and_or(&mut self) -> Result<Command, ShellError> {
        let mut left = self.parse_pipeline()?;

        loop {
            let (tok, _) = self.peek()?;
            match tok {
                Token::And => {
                    self.next()?;
                    self.skip_newlines()?;
                    let right = self.parse_pipeline()?;
                    left = Command::And(Box::new(left), Box::new(right));
                }
                Token::Or => {
                    self.next()?;
                    self.skip_newlines()?;
                    let right = self.parse_pipeline()?;
                    left = Command::Or(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }

        Ok(left)
    }

    /// pipeline = ['!'] command { '|' linebreak command }
    fn parse_pipeline(&mut self) -> Result<Command, ShellError> {
        let (tok, span) = self.peek()?;
        let bang = tok == Token::Bang;
        if bang {
            self.next()?;
        }

        let first = self.parse_command()?;
        let mut commands = vec![first];

        loop {
            let (tok, _) = self.peek()?;
            if tok == Token::Pipe {
                self.next()?;
                self.skip_newlines()?;
                commands.push(self.parse_command()?);
            } else {
                break;
            }
        }

        if commands.len() == 1 && !bang {
            Ok(commands.into_iter().next().unwrap())
        } else {
            Ok(Command::Pipeline {
                commands,
                bang,
                span,
            })
        }
    }

    /// command = compound_command [redirects]
    ///         | func_def
    ///         | simple_command
    fn parse_command(&mut self) -> Result<Command, ShellError> {
        let (tok, _span) = self.peek()?;

        match tok {
            Token::LParen => self.parse_subshell(),
            Token::Lbrace => self.parse_brace_group(),
            Token::If => self.parse_if(),
            Token::While => self.parse_while(),
            Token::Until => self.parse_until(),
            Token::For => self.parse_for(),
            Token::Case => self.parse_case(),
            _ => {
                // Could be a function definition: name() { ... }
                // or a simple command. We need to check for name followed by (
                self.parse_simple_command_or_func()
            }
        }
    }

    /// simple_command = { assignment } { word | redirect } (at least one element)
    /// Also handles function definitions: name() compound_command
    fn parse_simple_command_or_func(&mut self) -> Result<Command, ShellError> {
        let span = self.lexer.span();
        let mut assigns = Vec::new();
        let mut args = Vec::new();
        let mut redirs = Vec::new();

        // First, collect leading assignments
        loop {
            let (tok, tok_span) = self.peek()?;
            match tok {
                Token::Assignment { name, value } => {
                    self.next()?;
                    assigns.push(Assignment {
                        name: name.clone(),
                        value: Word {
                            parts: value.clone(),
                            span: tok_span,
                        },
                        span: tok_span,
                    });
                }
                _ => break,
            }
        }

        // Collect arguments and redirections
        loop {
            let (tok, tok_span) = self.peek()?;
            match &tok {
                Token::Word(parts, had_q) => {
                    let text = parts_to_text(parts);

                    // IO_NUMBER: digit-only unquoted word before redirect operator
                    if !had_q
                        && !text.is_empty()
                        && text.len() <= 9
                        && text.chars().all(|c| c.is_ascii_digit())
                    {
                        // Peek at what follows — if it's a redirect, use as fd number
                        self.next()?; // consume the digit word
                        let (next_tok, next_span) = self.peek()?;
                        if next_tok.is_redir() {
                            let fd: i32 = text.parse().unwrap_or(-1);
                            self.next()?; // consume redirect op
                            let mut redir = self.parse_redir_after_op(&next_tok, next_span)?;
                            redir.fd = fd;
                            redirs.push(redir);
                            continue;
                        }
                        // Not a redirect — treat as normal word
                        // (already consumed, push as argument)
                        // Check for function definition: name() { ... }
                        if args.is_empty() && assigns.is_empty() {
                            let (next, _next_span) = self.peek()?;
                            if next == Token::LParen {
                                self.next()?;
                                self.expect(&Token::RParen)?;
                                self.skip_newlines()?;
                                let body = self.parse_command()?;
                                return Ok(Command::FuncDef {
                                    name: text,
                                    body: Box::new(body),
                                    span: tok_span,
                                });
                            }
                        }
                        args.push(Word {
                            parts: parts.clone(),
                            span: tok_span,
                        });
                        continue;
                    }

                    self.next()?;

                    // Check for function definition: name() { ... }
                    if args.is_empty() && assigns.is_empty() {
                        let (next, _next_span) = self.peek()?;
                        if next == Token::LParen {
                            self.next()?;
                            self.expect(&Token::RParen)?;
                            self.skip_newlines()?;
                            let body = self.parse_command()?;
                            return Ok(Command::FuncDef {
                                name: text,
                                body: Box::new(body),
                                span: tok_span,
                            });
                        }
                    }

                    args.push(Word {
                        parts: parts.clone(),
                        span: tok_span,
                    });
                }
                // After command name, assignment tokens are treated as regular args
                // (e.g., `local X=value`, `export FOO=bar`)
                Token::Assignment { name, value } if !args.is_empty() => {
                    self.next()?;
                    let mut parts = vec![WordPart::Literal(format!("{}=", name).into())];
                    parts.extend(value.clone());
                    args.push(Word {
                        parts,
                        span: tok_span,
                    });
                }
                // Reserved words used as arguments (after command name)
                tok if args.is_empty() && !tok.is_redir() => break,
                // After we have a command name, reserved words become regular words
                Token::If
                | Token::Then
                | Token::Else
                | Token::Elif
                | Token::Fi
                | Token::Do
                | Token::Done
                | Token::Case
                | Token::Esac
                | Token::While
                | Token::Until
                | Token::For
                | Token::In
                | Token::Lbrace
                | Token::Rbrace
                | Token::Bang
                    if !args.is_empty() =>
                {
                    self.next()?;
                    let text = reserved_word_text(&tok);
                    args.push(Word {
                        parts: vec![WordPart::Literal(text.to_string().into())],
                        span: tok_span,
                    });
                }
                _ if tok.is_redir() => {
                    self.next()?;
                    let redir = self.parse_redir_after_op(&tok, tok_span)?;
                    redirs.push(redir);
                }
                _ => break,
            }
        }

        if assigns.is_empty() && args.is_empty() && redirs.is_empty() {
            let (tok, span) = self.next()?;
            return Err(ShellError::Syntax {
                msg: format!("unexpected token: {tok:?}"),
                span,
            });
        }

        Ok(Command::Simple {
            assigns,
            args,
            redirs,
            span,
        })
    }

    /// Parse a redirection after the operator token has been consumed.
    fn parse_redir_after_op(&mut self, op: &Token, span: Span) -> Result<Redir, ShellError> {
        // Default fd: 0 for input, 1 for output
        let fd = match op {
            Token::Less | Token::DLess | Token::DLessDash | Token::LessAnd | Token::LessGreat => 0,
            _ => 1,
        };

        match op {
            Token::DLess | Token::DLessDash => {
                // Here-document: read delimiter word
                let strip_tabs = *op == Token::DLessDash;
                self.lexer.recognize_reserved = false;
                let (delim_tok, delim_span) = self.next()?;
                self.lexer.recognize_reserved = true;
                let (delim_parts, had_quoting) = match delim_tok {
                    Token::Word(parts, q) => (parts, q),
                    other => {
                        return Err(ShellError::Syntax {
                            msg: format!("expected here-doc delimiter, got {other:?}"),
                            span: delim_span,
                        });
                    }
                };

                // Delimiter is quoted if any quoting was present in source
                let quoted = had_quoting || parts_have_quoting(&delim_parts);
                let delimiter = parts_to_text(&delim_parts);

                self.lexer.pending_heredocs.push(PendingHereDoc {
                    delimiter: delimiter.into(),
                    strip_tabs,
                    quoted,
                });

                // Return a placeholder — the body will be filled in later
                // when read_heredocs is called after the full command line.
                Ok(Redir {
                    fd,
                    kind: if strip_tabs {
                        RedirKind::HereDocStrip(HereDocBody::Literal(String::new().into()))
                    } else {
                        RedirKind::HereDoc(HereDocBody::Literal(String::new().into()))
                    },
                    span,
                })
            }
            _ => {
                // All other redirections take a word argument
                self.lexer.recognize_reserved = false;
                let (word_tok, word_span) = self.next()?;
                self.lexer.recognize_reserved = true;
                let word = match word_tok {
                    Token::Word(parts, _) => Word {
                        parts,
                        span: word_span,
                    },
                    // Accept reserved words as filenames
                    ref tok => {
                        let text = reserved_word_text(tok);
                        if text.is_empty() {
                            return Err(ShellError::Syntax {
                                msg: format!("expected filename, got {word_tok:?}"),
                                span: word_span,
                            });
                        }
                        Word {
                            parts: vec![WordPart::Literal(text.to_string().into())],
                            span: word_span,
                        }
                    }
                };

                let kind = match op {
                    Token::Less => RedirKind::Input(word),
                    Token::Great => RedirKind::Output(word),
                    Token::DGreat => RedirKind::Append(word),
                    Token::Clobber => RedirKind::Clobber(word),
                    Token::LessGreat => RedirKind::ReadWrite(word),
                    Token::LessAnd => RedirKind::DupInput(word),
                    Token::GreatAnd => RedirKind::DupOutput(word),
                    _ => unreachable!(),
                };

                Ok(Redir { fd, kind, span })
            }
        }
    }

    /// subshell = '(' compound_list ')'
    fn parse_subshell(&mut self) -> Result<Command, ShellError> {
        let span = self.expect(&Token::LParen)?;
        self.skip_newlines()?;
        let body = self.parse_compound_list()?;
        self.skip_newlines()?;
        self.expect(&Token::RParen)?;
        let redirs = self.parse_redirections()?;
        Ok(Command::Subshell {
            body: Box::new(body),
            redirs,
            span,
        })
    }

    /// brace_group = '{' compound_list '}'
    fn parse_brace_group(&mut self) -> Result<Command, ShellError> {
        let span = self.expect(&Token::Lbrace)?;
        self.skip_newlines()?;
        let body = self.parse_compound_list()?;
        self.skip_newlines()?;
        self.expect(&Token::Rbrace)?;
        let redirs = self.parse_redirections()?;
        Ok(Command::BraceGroup {
            body: Box::new(body),
            redirs,
            span,
        })
    }

    /// if_clause = 'if' compound_list 'then' compound_list
    ///             { 'elif' compound_list 'then' compound_list }
    ///             [ 'else' compound_list ] 'fi'
    fn parse_if(&mut self) -> Result<Command, ShellError> {
        let span = self.expect(&Token::If)?;
        self.skip_newlines()?;
        let cond = self.parse_compound_list()?;
        self.skip_newlines()?;
        self.expect(&Token::Then)?;
        self.skip_newlines()?;
        let then_part = self.parse_compound_list()?;
        self.skip_newlines()?;

        let (tok, _) = self.peek()?;
        let else_part = match tok {
            Token::Elif => Some(Box::new(self.parse_elif()?)),
            Token::Else => {
                self.next()?;
                self.skip_newlines()?;
                let body = self.parse_compound_list()?;
                self.skip_newlines()?;
                self.expect(&Token::Fi)?;
                Some(Box::new(body))
            }
            Token::Fi => {
                self.next()?;
                None
            }
            _ => {
                let (tok, span) = self.next()?;
                return Err(ShellError::Syntax {
                    msg: format!("expected 'elif', 'else', or 'fi', got {tok:?}"),
                    span,
                });
            }
        };

        let redirs = self.parse_redirections()?;
        let mut cmd = Command::If {
            cond: Box::new(cond),
            then_part: Box::new(then_part),
            else_part,
            span,
        };
        if !redirs.is_empty() {
            cmd = Command::BraceGroup {
                body: Box::new(cmd),
                redirs,
                span,
            };
        }
        Ok(cmd)
    }

    fn parse_elif(&mut self) -> Result<Command, ShellError> {
        let span = self.expect(&Token::Elif)?;
        self.skip_newlines()?;
        let cond = self.parse_compound_list()?;
        self.skip_newlines()?;
        self.expect(&Token::Then)?;
        self.skip_newlines()?;
        let then_part = self.parse_compound_list()?;
        self.skip_newlines()?;

        let (tok, _) = self.peek()?;
        let else_part = match tok {
            Token::Elif => Some(Box::new(self.parse_elif()?)),
            Token::Else => {
                self.next()?;
                self.skip_newlines()?;
                let body = self.parse_compound_list()?;
                self.skip_newlines()?;
                self.expect(&Token::Fi)?;
                Some(Box::new(body))
            }
            Token::Fi => {
                self.next()?;
                None
            }
            _ => {
                let (tok, span) = self.next()?;
                return Err(ShellError::Syntax {
                    msg: format!("expected 'elif', 'else', or 'fi', got {tok:?}"),
                    span,
                });
            }
        };

        Ok(Command::If {
            cond: Box::new(cond),
            then_part: Box::new(then_part),
            else_part,
            span,
        })
    }

    /// while_clause = 'while' compound_list 'do' compound_list 'done'
    fn parse_while(&mut self) -> Result<Command, ShellError> {
        let span = self.expect(&Token::While)?;
        self.skip_newlines()?;
        let cond = self.parse_compound_list()?;
        self.skip_newlines()?;
        self.expect(&Token::Do)?;
        self.skip_newlines()?;
        let body = self.parse_compound_list()?;
        self.skip_newlines()?;
        self.expect(&Token::Done)?;
        let redirs = self.parse_redirections()?;
        let mut cmd = Command::While {
            cond: Box::new(cond),
            body: Box::new(body),
            span,
        };
        if !redirs.is_empty() {
            cmd = Command::BraceGroup {
                body: Box::new(cmd),
                redirs,
                span,
            };
        }
        Ok(cmd)
    }

    /// until_clause = 'until' compound_list 'do' compound_list 'done'
    fn parse_until(&mut self) -> Result<Command, ShellError> {
        let span = self.expect(&Token::Until)?;
        self.skip_newlines()?;
        let cond = self.parse_compound_list()?;
        self.skip_newlines()?;
        self.expect(&Token::Do)?;
        self.skip_newlines()?;
        let body = self.parse_compound_list()?;
        self.skip_newlines()?;
        self.expect(&Token::Done)?;
        let redirs = self.parse_redirections()?;
        let mut cmd = Command::Until {
            cond: Box::new(cond),
            body: Box::new(body),
            span,
        };
        if !redirs.is_empty() {
            cmd = Command::BraceGroup {
                body: Box::new(cmd),
                redirs,
                span,
            };
        }
        Ok(cmd)
    }

    /// for_clause = 'for' name [linebreak 'in' wordlist ';'] linebreak
    ///              'do' compound_list 'done'
    fn parse_for(&mut self) -> Result<Command, ShellError> {
        let span = self.expect(&Token::For)?;

        // Variable name
        self.lexer.recognize_reserved = false;
        let (name_tok, name_span) = self.next()?;
        self.lexer.recognize_reserved = true;
        let var = match name_tok {
            Token::Word(parts, _) => parts_to_text(&parts),
            other => {
                return Err(ShellError::Syntax {
                    msg: format!("expected variable name after 'for', got {other:?}"),
                    span: name_span,
                });
            }
        };

        self.skip_newlines()?;

        // Optional 'in' word-list ';'
        let (tok, _) = self.peek()?;
        let words = if tok == Token::In {
            self.next()?;
            let mut words = Vec::new();
            loop {
                let (tok, tok_span) = self.peek()?;
                match tok {
                    Token::Word(parts, _) => {
                        self.next()?;
                        words.push(Word {
                            parts: parts.clone(),
                            span: tok_span,
                        });
                    }
                    Token::Semi | Token::Newline => {
                        self.next()?;
                        break;
                    }
                    _ => break,
                }
            }
            Some(words)
        } else {
            // No 'in' clause — iterate over "$@"
            // Consume optional ; or newline
            let (tok, _) = self.peek()?;
            if tok == Token::Semi {
                self.next()?;
            }
            None
        };

        self.skip_newlines()?;
        self.expect(&Token::Do)?;
        self.skip_newlines()?;
        let body = self.parse_compound_list()?;
        self.skip_newlines()?;
        self.expect(&Token::Done)?;
        let redirs = self.parse_redirections()?;
        let mut cmd = Command::For {
            var,
            words,
            body: Box::new(body),
            span,
        };
        if !redirs.is_empty() {
            cmd = Command::BraceGroup {
                body: Box::new(cmd),
                redirs,
                span,
            };
        }
        Ok(cmd)
    }

    /// case_clause = 'case' word linebreak 'in' linebreak
    ///               { case_item } 'esac'
    fn parse_case(&mut self) -> Result<Command, ShellError> {
        let span = self.expect(&Token::Case)?;

        // Case word
        self.lexer.recognize_reserved = false;
        let (word_tok, word_span) = self.next()?;
        self.lexer.recognize_reserved = true;
        let word = match word_tok {
            Token::Word(parts, _) => Word {
                parts,
                span: word_span,
            },
            other => {
                return Err(ShellError::Syntax {
                    msg: format!("expected word after 'case', got {other:?}"),
                    span: word_span,
                });
            }
        };

        self.skip_newlines()?;
        self.expect(&Token::In)?;
        self.skip_newlines()?;

        let mut arms = Vec::new();
        loop {
            let (tok, _) = self.peek()?;
            if tok == Token::Esac {
                self.next()?;
                break;
            }
            arms.push(self.parse_case_arm()?);
            self.skip_newlines()?;
        }

        let redirs = self.parse_redirections()?;
        let mut cmd = Command::Case { word, arms, span };
        if !redirs.is_empty() {
            cmd = Command::BraceGroup {
                body: Box::new(cmd),
                redirs,
                span,
            };
        }
        Ok(cmd)
    }

    /// case_item = ['('] pattern { '|' pattern } ')' [compound_list] ';;'
    fn parse_case_arm(&mut self) -> Result<CaseArm, ShellError> {
        let span = self.lexer.span();

        // Optional leading (
        let (tok, _) = self.peek()?;
        if tok == Token::LParen {
            self.next()?;
        }

        // Patterns separated by |
        let mut patterns = Vec::new();
        loop {
            self.lexer.recognize_reserved = false;
            let (tok, tok_span) = self.next()?;
            self.lexer.recognize_reserved = true;
            let pat = match tok {
                Token::Word(parts, _) => Word {
                    parts,
                    span: tok_span,
                },
                ref other => {
                    let text = reserved_word_text(other);
                    if text.is_empty() {
                        return Err(ShellError::Syntax {
                            msg: format!("expected pattern in case arm, got {tok:?}"),
                            span: tok_span,
                        });
                    }
                    Word {
                        parts: vec![WordPart::Literal(text.to_string().into())],
                        span: tok_span,
                    }
                }
            };
            patterns.push(pat);

            let (tok, _) = self.peek()?;
            if tok == Token::Pipe {
                self.next()?;
            } else {
                break;
            }
        }

        self.expect(&Token::RParen)?;
        self.skip_newlines()?;

        // Body (optional — could be empty before ;;)
        let (tok, _) = self.peek()?;
        let body = if tok == Token::SemiSemi || tok == Token::Esac {
            None
        } else {
            Some(self.parse_compound_list()?)
        };

        // ;; or esac
        let (tok, _) = self.peek()?;
        if tok == Token::SemiSemi {
            self.next()?;
            self.skip_newlines()?;
        }
        // If no ;;, the next token should be esac (handled by caller)

        Ok(CaseArm {
            patterns,
            body,
            span,
        })
    }

    /// Parse optional trailing redirections after a compound command.
    fn parse_redirections(&mut self) -> Result<Vec<Redir>, ShellError> {
        let mut redirs = Vec::new();
        loop {
            let (tok, span) = self.peek()?;
            if tok.is_redir() {
                self.next()?;
                redirs.push(self.parse_redir_after_op(&tok, span)?);
            } else if let Token::Word(parts, false) = &tok {
                // IO_NUMBER: digit word before redirect
                let text = parts_to_text(parts);
                if !text.is_empty() && text.len() <= 9 && text.chars().all(|c| c.is_ascii_digit()) {
                    // Check if next-next token is a redirect
                    self.next()?; // consume digit
                    let (next, next_span) = self.peek()?;
                    if next.is_redir() {
                        let fd: i32 = text.parse().unwrap_or(-1);
                        self.next()?;
                        let mut redir = self.parse_redir_after_op(&next, next_span)?;
                        redir.fd = fd;
                        redirs.push(redir);
                    } else {
                        // Not a redirect — push back the digit word
                        self.lexer
                            .push_back(Token::Word(parts.clone(), false), span);
                        break;
                    }
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(redirs)
    }
}

/// Parse a raw word string into WordPart nodes.
/// Kept for heredoc body expansion (called from redirect.rs).
pub fn parse_word_parts(raw: &str) -> Vec<WordPart> {
    parse_word_parts_impl(raw, false)
}

/// Parse a raw byte string into WordPart nodes.
pub fn parse_word_parts_bytes(raw: &[u8]) -> Vec<WordPart> {
    parse_word_parts(&crate::encoding::bytes_to_str(raw))
}

/// Build a HereDocBody from a raw heredoc body string.
fn make_heredoc_body(raw: String, quoted: bool) -> HereDocBody {
    if quoted {
        HereDocBody::Literal(raw.into())
    } else {
        HereDocBody::Parsed(parse_word_parts_heredoc(&raw))
    }
}

/// Parse word parts in heredoc context: single quotes and double quotes are
/// literal, and only \$, \`, \\, \<newline> are special backslash escapes
/// (NOT \" — unlike regular double quotes).
fn parse_word_parts_heredoc(raw: &str) -> Vec<WordPart> {
    let mut parts = Vec::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '$' => {
                i += 1;
                if let Some(part) = parse_dollar(&chars, &mut i, true) {
                    parts.push(part);
                } else {
                    parts.push(WordPart::Literal("$".into()));
                }
            }
            '`' => {
                i += 1; // skip opening `
                let start = i;
                while i < chars.len() && chars[i] != '`' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                let content: String = chars[start..i].iter().collect();
                if i < chars.len() {
                    i += 1; // skip closing `
                }
                let cmd = parse_cmdsubst_content(&content);
                parts.push(WordPart::Backtick(Box::new(cmd)));
            }
            '\\' => {
                i += 1;
                if i < chars.len() {
                    if chars[i] == '\n' {
                        // \<newline> = line continuation
                        i += 1;
                    } else if matches!(chars[i], '$' | '`' | '\\') {
                        // Heredoc: only \$, \`, \\ are special (NOT \")
                        parts.push(WordPart::Literal(chars[i].to_string().into()));
                        i += 1;
                    } else {
                        // Other \X — preserve the backslash
                        parts.push(WordPart::Literal(format!("\\{}", chars[i]).into()));
                        i += 1;
                    }
                }
            }
            _ => {
                // Accumulate literal text (quotes are literal in heredoc)
                let start = i;
                while i < chars.len() && !matches!(chars[i], '$' | '`' | '\\') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                if !text.is_empty() {
                    parts.push(WordPart::Literal(text.into()));
                }
            }
        }
    }

    coalesce_literals(parts)
}

fn parse_word_parts_impl(raw: &str, in_dquote: bool) -> Vec<WordPart> {
    let mut parts = Vec::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '\'' if !in_dquote => {
                // Single-quoted string (only in unquoted context)
                i += 1; // skip opening '
                let start = i;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                let content: String = chars[start..i].iter().collect();
                parts.push(WordPart::SingleQuoted(content.into()));
                if i < chars.len() {
                    i += 1; // skip closing '
                }
            }
            '"' => {
                // Double-quoted string — parse inner parts
                i += 1; // skip opening "
                let inner = parse_double_quoted_parts(&chars, &mut i);
                parts.push(WordPart::DoubleQuoted(inner));
                if i < chars.len() && chars[i] == '"' {
                    i += 1; // skip closing "
                }
            }
            '$' => {
                i += 1;
                if let Some(part) = parse_dollar(&chars, &mut i, in_dquote) {
                    parts.push(part);
                } else {
                    // Bare $
                    parts.push(WordPart::Literal("$".into()));
                }
            }
            '`' => {
                i += 1; // skip opening `
                let start = i;
                while i < chars.len() && chars[i] != '`' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                let content: String = chars[start..i].iter().collect();
                if i < chars.len() {
                    i += 1; // skip closing `
                }
                let cmd = parse_cmdsubst_content(&content);
                parts.push(WordPart::Backtick(Box::new(cmd)));
            }
            '\\' => {
                i += 1;
                if i < chars.len() {
                    if chars[i] == '\n' {
                        // \<newline> = line continuation
                        i += 1;
                    } else if matches!(chars[i], '$' | '`' | '"' | '\\') {
                        // \$, \`, \", \\ are special
                        parts.push(WordPart::Literal(chars[i].to_string().into()));
                        i += 1;
                    } else {
                        // Other \X — preserve the backslash
                        parts.push(WordPart::Literal(format!("\\{}", chars[i]).into()));
                        i += 1;
                    }
                }
            }
            '~' if parts.is_empty() => {
                // Tilde expansion (only at start of word or after : in assignments)
                i += 1;
                let start = i;
                while i < chars.len()
                    && chars[i] != '/'
                    && chars[i] != ':'
                    && !chars[i].is_whitespace()
                {
                    i += 1;
                }
                let user: String = chars[start..i].iter().collect();
                parts.push(WordPart::Tilde(user.into()));
            }
            _ => {
                // Accumulate literal text
                let start = i;
                while i < chars.len()
                    && !matches!(chars[i], '$' | '`' | '\\')
                    && chars[i] != '"'
                    && (in_dquote || chars[i] != '\'')
                    && !(chars[i] == '~' && i == 0)
                {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                if !text.is_empty() {
                    parts.push(WordPart::Literal(text.into()));
                }
            }
        }
    }

    coalesce_literals(parts)
}

fn parse_double_quoted_parts(chars: &[char], i: &mut usize) -> Vec<WordPart> {
    let mut parts = Vec::new();
    let mut literal = String::new();

    while *i < chars.len() && chars[*i] != '"' {
        match chars[*i] {
            '$' => {
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(std::mem::take(&mut literal).into()));
                }
                *i += 1;
                if let Some(part) = parse_dollar(chars, i, true) {
                    parts.push(part);
                } else {
                    literal.push('$');
                }
            }
            '`' => {
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(std::mem::take(&mut literal).into()));
                }
                *i += 1;
                let start = *i;
                while *i < chars.len() && chars[*i] != '`' {
                    if chars[*i] == '\\' {
                        *i += 1;
                    }
                    *i += 1;
                }
                let content: String = chars[start..*i].iter().collect();
                if *i < chars.len() {
                    *i += 1;
                }
                let cmd = parse_cmdsubst_content(&content);
                parts.push(WordPart::Backtick(Box::new(cmd)));
            }
            '\\' => {
                *i += 1;
                if *i < chars.len() {
                    // In double quotes, backslash only escapes $, `, ", \, and newline
                    let c = chars[*i];
                    if matches!(c, '$' | '`' | '"' | '\\' | '\n') {
                        if c != '\n' {
                            literal.push(c);
                        }
                    } else {
                        literal.push('\\');
                        literal.push(c);
                    }
                    *i += 1;
                }
            }
            c => {
                literal.push(c);
                *i += 1;
            }
        }
    }

    if !literal.is_empty() {
        parts.push(WordPart::Literal(literal.into()));
    }

    parts
}

fn parse_dollar(chars: &[char], i: &mut usize, in_dquote: bool) -> Option<WordPart> {
    if *i >= chars.len() {
        return None;
    }

    match chars[*i] {
        '{' => {
            *i += 1;
            Some(parse_brace_param(chars, i, in_dquote))
        }
        '(' => {
            *i += 1;
            if *i < chars.len() && chars[*i] == '(' {
                // $(( — arithmetic
                *i += 1;
                let mut content = Vec::new();
                let mut depth = 1u32;
                while *i < chars.len() {
                    if chars[*i] == ')'
                        && *i + 1 < chars.len()
                        && chars[*i + 1] == ')'
                        && depth == 1
                    {
                        *i += 2;
                        break;
                    } else if chars[*i] == '(' {
                        depth += 1;
                        content.push(chars[*i]);
                        *i += 1;
                    } else if chars[*i] == ')' {
                        depth -= 1;
                        content.push(chars[*i]);
                        *i += 1;
                    } else {
                        content.push(chars[*i]);
                        *i += 1;
                    }
                }
                let text: String = content.into_iter().collect();
                let inner_parts = parse_word_parts(&text);
                Some(WordPart::Arith(inner_parts))
            } else {
                // $( — command substitution
                let mut content = String::new();
                let mut depth = 1u32;
                while *i < chars.len() {
                    match chars[*i] {
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                *i += 1;
                                break;
                            }
                            content.push(')');
                            *i += 1;
                        }
                        '(' => {
                            depth += 1;
                            content.push('(');
                            *i += 1;
                        }
                        '\'' => {
                            content.push('\'');
                            *i += 1;
                            while *i < chars.len() && chars[*i] != '\'' {
                                content.push(chars[*i]);
                                *i += 1;
                            }
                            if *i < chars.len() {
                                content.push('\'');
                                *i += 1;
                            }
                        }
                        c => {
                            content.push(c);
                            *i += 1;
                        }
                    }
                }
                let cmd = parse_cmdsubst_content(&content);
                Some(WordPart::CmdSubst(Box::new(cmd)))
            }
        }
        // Special parameters
        c @ ('@' | '*' | '#' | '?' | '-' | '$' | '!') => {
            *i += 1;
            Some(WordPart::Param(ParamExpr {
                name: c.to_string(),
                op: ParamOp::Normal,
                span: Span::default(),
            }))
        }
        // Positional parameters $0-$9
        c @ '0'..='9' => {
            *i += 1;
            Some(WordPart::Param(ParamExpr {
                name: c.to_string(),
                op: ParamOp::Normal,
                span: Span::default(),
            }))
        }
        // Variable name
        c if c == '_' || c.is_ascii_alphabetic() => {
            let start = *i;
            while *i < chars.len() && (chars[*i] == '_' || chars[*i].is_ascii_alphanumeric()) {
                *i += 1;
            }
            let name: String = chars[start..*i].iter().collect();
            Some(WordPart::Param(ParamExpr {
                name,
                op: ParamOp::Normal,
                span: Span::default(),
            }))
        }
        _ => None,
    }
}

fn parse_brace_param(chars: &[char], i: &mut usize, in_dquote: bool) -> WordPart {
    if *i >= chars.len() {
        return WordPart::Literal("${".into());
    }

    // ${#var} — length
    let length =
        *i < chars.len() && chars[*i] == '#' && *i + 1 < chars.len() && chars[*i + 1] != '}';
    if length {
        *i += 1;
    }

    // Read variable name
    let start = *i;
    // Special params: single char like @, *, #, ?, -, $, !
    if *i < chars.len() && matches!(chars[*i], '@' | '*' | '#' | '?' | '-' | '$' | '!') {
        *i += 1;
    } else if *i < chars.len() && chars[*i].is_ascii_digit() {
        // Positional: ${10} etc.
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            *i += 1;
        }
    } else {
        while *i < chars.len() && (chars[*i] == '_' || chars[*i].is_ascii_alphanumeric()) {
            *i += 1;
        }
    }
    let name: String = chars[start..*i].iter().collect();

    if length {
        // Skip to }
        while *i < chars.len() && chars[*i] != '}' {
            *i += 1;
        }
        if *i < chars.len() {
            *i += 1;
        }
        return WordPart::Param(ParamExpr {
            name,
            op: ParamOp::Length,
            span: Span::default(),
        });
    }

    // Check for operator
    if *i >= chars.len() || chars[*i] == '}' {
        if *i < chars.len() {
            *i += 1;
        }
        return WordPart::Param(ParamExpr {
            name,
            op: ParamOp::Normal,
            span: Span::default(),
        });
    }

    let op_char = chars[*i];

    // Validate the operator character
    if !matches!(op_char, ':' | '-' | '=' | '?' | '+' | '%' | '#') {
        // Invalid character after variable name — bad substitution
        while *i < chars.len() && chars[*i] != '}' {
            *i += 1;
        }
        if *i < chars.len() {
            *i += 1;
        }
        return WordPart::Param(ParamExpr {
            name: format!("{}{}", name, op_char),
            op: ParamOp::BadSubst,
            span: Span::default(),
        });
    }

    *i += 1;

    let op = match op_char {
        ':' if *i < chars.len() => {
            let op2 = chars[*i];
            *i += 1;
            let word = read_brace_word(chars, i, in_dquote);
            match op2 {
                '-' => ParamOp::Default { colon: true, word },
                '=' => ParamOp::Assign { colon: true, word },
                '?' => ParamOp::Error { colon: true, word },
                '+' => ParamOp::Alternative { colon: true, word },
                _ => ParamOp::Normal, // shouldn't happen
            }
        }
        '-' => {
            let word = read_brace_word(chars, i, in_dquote);
            ParamOp::Default { colon: false, word }
        }
        '=' => {
            let word = read_brace_word(chars, i, in_dquote);
            ParamOp::Assign { colon: false, word }
        }
        '?' => {
            let word = read_brace_word(chars, i, in_dquote);
            ParamOp::Error { colon: false, word }
        }
        '+' => {
            let word = read_brace_word(chars, i, in_dquote);
            ParamOp::Alternative { colon: false, word }
        }
        // Trim ops: ALWAYS use BASESYNTAX (single quotes are quoting)
        // This matches dash's parsesub() which forces newsyn=BASESYNTAX for % and #
        '%' => {
            if *i < chars.len() && chars[*i] == '%' {
                *i += 1;
                let word = read_brace_word(chars, i, false);
                ParamOp::TrimSuffixLarge(word)
            } else {
                let word = read_brace_word(chars, i, false);
                ParamOp::TrimSuffixSmall(word)
            }
        }
        '#' => {
            if *i < chars.len() && chars[*i] == '#' {
                *i += 1;
                let word = read_brace_word(chars, i, false);
                ParamOp::TrimPrefixLarge(word)
            } else {
                let word = read_brace_word(chars, i, false);
                ParamOp::TrimPrefixSmall(word)
            }
        }
        _ => {
            // Unknown op, skip to }
            while *i < chars.len() && chars[*i] != '}' {
                *i += 1;
            }
            if *i < chars.len() {
                *i += 1;
            }
            ParamOp::Normal
        }
    };

    WordPart::Param(ParamExpr {
        name,
        op,
        span: Span::default(),
    })
}

/// Read the word part of a ${var<op>word} expression up to the closing }.
fn read_brace_word(chars: &[char], i: &mut usize, in_dquote: bool) -> Vec<WordPart> {
    let mut raw = String::new();
    let mut depth = 1u32;

    while *i < chars.len() {
        match chars[*i] {
            '}' => {
                depth -= 1;
                if depth == 0 {
                    *i += 1;
                    break;
                }
                raw.push('}');
                *i += 1;
            }
            '$' if *i + 1 < chars.len() && chars[*i + 1] == '{' => {
                depth += 1;
                raw.push('$');
                raw.push('{');
                *i += 2;
            }
            // Single quotes: quoting in unquoted context, literal in double-quoted
            '\'' if !in_dquote => {
                raw.push('\'');
                *i += 1;
                while *i < chars.len() && chars[*i] != '\'' {
                    raw.push(chars[*i]);
                    *i += 1;
                }
                if *i < chars.len() {
                    raw.push('\'');
                    *i += 1;
                }
            }
            '\\' => {
                raw.push('\\');
                *i += 1;
                if *i < chars.len() {
                    raw.push(chars[*i]);
                    *i += 1;
                }
            }
            c => {
                raw.push(c);
                *i += 1;
            }
        }
    }

    parse_word_parts_impl(&raw, in_dquote)
}

/// Coalesce adjacent Literal parts.
pub(crate) fn coalesce_literals(parts: Vec<WordPart>) -> Vec<WordPart> {
    let mut result = Vec::with_capacity(parts.len());
    for part in parts {
        if let WordPart::Literal(ref s) = part
            && let Some(WordPart::Literal(prev)) = result.last_mut()
        {
            let mut bytes = prev.as_bytes().to_vec();
            bytes.extend_from_slice(s.as_bytes());
            *prev = crate::shell_bytes::ShellBytes::from_vec(bytes);
            continue;
        }
        result.push(part);
    }
    result
}

/// Parse command substitution content by recursively invoking the parser.
pub(crate) fn parse_cmdsubst_content(content: &str) -> Command {
    match Parser::new(content).parse() {
        Ok(program) => {
            if program.commands.is_empty() {
                Command::Simple {
                    assigns: Vec::new(),
                    args: Vec::new(),
                    redirs: Vec::new(),
                    span: Span::default(),
                }
            } else if program.commands.len() == 1 {
                program.commands.into_iter().next().unwrap()
            } else {
                let mut iter = program.commands.into_iter();
                let mut result = iter.next().unwrap();
                for cmd in iter {
                    result = Command::Sequence(Box::new(result), Box::new(cmd));
                }
                result
            }
        }
        Err(_) => Command::Simple {
            assigns: Vec::new(),
            args: Vec::new(),
            redirs: Vec::new(),
            span: Span::default(),
        },
    }
}

#[allow(dead_code)]
pub(crate) fn parse_cmdsubst_content_bytes(content: &[u8]) -> Command {
    match Parser::new_bytes(content).parse() {
        Ok(program) => {
            if program.commands.is_empty() {
                Command::Simple {
                    assigns: Vec::new(),
                    args: Vec::new(),
                    redirs: Vec::new(),
                    span: Span::default(),
                }
            } else if program.commands.len() == 1 {
                program.commands.into_iter().next().unwrap()
            } else {
                let mut iter = program.commands.into_iter();
                let mut result = iter.next().unwrap();
                for cmd in iter {
                    result = Command::Sequence(Box::new(result), Box::new(cmd));
                }
                result
            }
        }
        Err(_) => Command::Simple {
            assigns: Vec::new(),
            args: Vec::new(),
            redirs: Vec::new(),
            span: Span::default(),
        },
    }
}

/// Check if a token can start a command.
fn is_command_start(tok: &Token) -> bool {
    matches!(
        tok,
        Token::Word(_, _)
            | Token::Assignment { .. }
            | Token::If
            | Token::While
            | Token::Until
            | Token::For
            | Token::Case
            | Token::LParen
            | Token::Lbrace
            | Token::Bang
    )
}

/// Get the text representation of a reserved word token.
fn reserved_word_text(tok: &Token) -> &'static str {
    match tok {
        Token::If => "if",
        Token::Then => "then",
        Token::Else => "else",
        Token::Elif => "elif",
        Token::Fi => "fi",
        Token::Do => "do",
        Token::Done => "done",
        Token::Case => "case",
        Token::Esac => "esac",
        Token::While => "while",
        Token::Until => "until",
        Token::For => "for",
        Token::In => "in",
        Token::Lbrace => "{",
        Token::Rbrace => "}",
        Token::Bang => "!",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Program {
        Parser::new(src).parse().unwrap()
    }

    #[test]
    fn simple_command() {
        let prog = parse("echo hello world");
        assert_eq!(prog.commands.len(), 1);
        match &prog.commands[0] {
            Command::Simple { args, .. } => assert_eq!(args.len(), 3),
            other => panic!("expected Simple, got {other:?}"),
        }
    }

    #[test]
    fn pipeline() {
        let prog = parse("ls | grep foo | wc -l");
        match &prog.commands[0] {
            Command::Pipeline { commands, bang, .. } => {
                assert_eq!(commands.len(), 3);
                assert!(!bang);
            }
            other => panic!("expected Pipeline, got {other:?}"),
        }
    }

    #[test]
    fn negated_pipeline() {
        let prog = parse("! grep error log");
        match &prog.commands[0] {
            Command::Pipeline { bang, .. } => assert!(bang),
            other => panic!("expected Pipeline with bang, got {other:?}"),
        }
    }

    #[test]
    fn and_or_list() {
        let prog = parse("a && b || c");
        match &prog.commands[0] {
            Command::Or(_, _) => {} // top-level is Or(And(a,b), c)
            other => panic!("expected Or, got {other:?}"),
        }
    }

    #[test]
    fn sequence() {
        let prog = parse("a; b; c");
        // Should be Sequence(Sequence(a, b), c)
        match &prog.commands[0] {
            Command::Sequence(_, _) => {}
            other => panic!("expected Sequence, got {other:?}"),
        }
    }

    #[test]
    fn background() {
        let prog = parse("sleep 10 &");
        match &prog.commands[0] {
            Command::Background { .. } => {}
            other => panic!("expected Background, got {other:?}"),
        }
    }

    #[test]
    fn if_then_fi() {
        let prog = parse("if true; then echo yes; fi");
        match &prog.commands[0] {
            Command::If { else_part, .. } => assert!(else_part.is_none()),
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn if_then_else_fi() {
        let prog = parse("if true; then echo yes; else echo no; fi");
        match &prog.commands[0] {
            Command::If { else_part, .. } => assert!(else_part.is_some()),
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn while_loop() {
        let prog = parse("while true; do echo loop; done");
        match &prog.commands[0] {
            Command::While { .. } => {}
            other => panic!("expected While, got {other:?}"),
        }
    }

    #[test]
    fn until_loop() {
        let prog = parse("until false; do echo loop; done");
        match &prog.commands[0] {
            Command::Until { .. } => {}
            other => panic!("expected Until, got {other:?}"),
        }
    }

    #[test]
    fn for_loop_with_words() {
        let prog = parse("for x in a b c; do echo $x; done");
        match &prog.commands[0] {
            Command::For { var, words, .. } => {
                assert_eq!(var, "x");
                assert_eq!(words.as_ref().unwrap().len(), 3);
            }
            other => panic!("expected For, got {other:?}"),
        }
    }

    #[test]
    fn for_loop_without_words() {
        let prog = parse("for x; do echo $x; done");
        match &prog.commands[0] {
            Command::For { var, words, .. } => {
                assert_eq!(var, "x");
                assert!(words.is_none());
            }
            other => panic!("expected For, got {other:?}"),
        }
    }

    #[test]
    fn case_statement() {
        let prog = parse("case $x in\na) echo a;;\nb|c) echo bc;;\nesac");
        match &prog.commands[0] {
            Command::Case { arms, .. } => {
                assert_eq!(arms.len(), 2);
                assert_eq!(arms[1].patterns.len(), 2);
            }
            other => panic!("expected Case, got {other:?}"),
        }
    }

    #[test]
    fn subshell() {
        let prog = parse("(echo hello)");
        match &prog.commands[0] {
            Command::Subshell { .. } => {}
            other => panic!("expected Subshell, got {other:?}"),
        }
    }

    #[test]
    fn brace_group() {
        let prog = parse("{ echo hello; }");
        match &prog.commands[0] {
            Command::BraceGroup { .. } => {}
            other => panic!("expected BraceGroup, got {other:?}"),
        }
    }

    #[test]
    fn function_def() {
        let prog = parse("myfunc() { echo hello; }");
        match &prog.commands[0] {
            Command::FuncDef { name, .. } => assert_eq!(name, "myfunc"),
            other => panic!("expected FuncDef, got {other:?}"),
        }
    }

    #[test]
    fn assignment() {
        let prog = parse("FOO=bar");
        match &prog.commands[0] {
            Command::Simple { assigns, args, .. } => {
                assert_eq!(assigns.len(), 1);
                assert_eq!(assigns[0].name, "FOO");
                assert!(args.is_empty());
            }
            other => panic!("expected Simple with assignment, got {other:?}"),
        }
    }

    #[test]
    fn assignment_with_command() {
        let prog = parse("FOO=bar echo $FOO");
        match &prog.commands[0] {
            Command::Simple { assigns, args, .. } => {
                assert_eq!(assigns.len(), 1);
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected Simple, got {other:?}"),
        }
    }

    #[test]
    fn redirections() {
        let prog = parse("echo hello > output.txt");
        match &prog.commands[0] {
            Command::Simple { redirs, .. } => {
                assert_eq!(redirs.len(), 1);
                assert!(matches!(redirs[0].kind, RedirKind::Output(_)));
            }
            other => panic!("expected Simple with redirs, got {other:?}"),
        }
    }

    #[test]
    fn word_parts_literal() {
        let parts = parse_word_parts("hello");
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], WordPart::Literal(s) if s == "hello"));
    }

    #[test]
    fn word_parts_variable() {
        let parts = parse_word_parts("$HOME");
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], WordPart::Param(p) if p.name == "HOME"));
    }

    #[test]
    fn word_parts_brace_default() {
        let parts = parse_word_parts("${var:-default}");
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            WordPart::Param(p) => {
                assert_eq!(p.name, "var");
                assert!(matches!(p.op, ParamOp::Default { colon: true, .. }));
            }
            other => panic!("expected Param, got {other:?}"),
        }
    }

    #[test]
    fn word_parts_length() {
        let parts = parse_word_parts("${#var}");
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            WordPart::Param(p) => {
                assert_eq!(p.name, "var");
                assert!(matches!(p.op, ParamOp::Length));
            }
            other => panic!("expected Param Length, got {other:?}"),
        }
    }

    #[test]
    fn word_parts_single_quoted() {
        let parts = parse_word_parts("'hello world'");
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], WordPart::SingleQuoted(s) if s == "hello world"));
    }

    #[test]
    fn word_parts_double_quoted_with_var() {
        let parts = parse_word_parts("\"hello $name\"");
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            WordPart::DoubleQuoted(inner) => {
                assert_eq!(inner.len(), 2);
                assert!(matches!(&inner[0], WordPart::Literal(s) if s == "hello "));
                assert!(matches!(&inner[1], WordPart::Param(p) if p.name == "name"));
            }
            other => panic!("expected DoubleQuoted, got {other:?}"),
        }
    }

    #[test]
    fn word_parts_tilde() {
        let parts = parse_word_parts("~/bin");
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], WordPart::Tilde(s) if s.is_empty()));
        assert!(matches!(&parts[1], WordPart::Literal(s) if s == "/bin"));
    }

    #[test]
    fn word_parts_cmd_subst() {
        let parts = parse_word_parts("$(date)");
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], WordPart::CmdSubst(_)));
    }

    #[test]
    fn word_parts_arith() {
        let parts = parse_word_parts("$((1+2))");
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], WordPart::Arith(_)));
    }

    #[test]
    fn word_parts_mixed() {
        let parts = parse_word_parts("prefix${var}suffix");
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], WordPart::Literal(s) if s == "prefix"));
        assert!(matches!(&parts[1], WordPart::Param(p) if p.name == "var"));
        assert!(matches!(&parts[2], WordPart::Literal(s) if s == "suffix"));
    }

    #[test]
    fn multiline_program() {
        let prog = parse("echo a\necho b\necho c");
        assert_eq!(prog.commands.len(), 3);
    }

    #[test]
    fn empty_program() {
        let prog = parse("");
        assert!(prog.commands.is_empty());
    }

    #[test]
    fn comments_only() {
        let prog = parse("# this is a comment\n");
        assert!(prog.commands.is_empty());
    }

    #[test]
    fn elif_chain() {
        let prog = parse("if a; then b; elif c; then d; elif e; then f; else g; fi");
        match &prog.commands[0] {
            Command::If { else_part, .. } => {
                let else_cmd = else_part.as_ref().unwrap();
                assert!(matches!(else_cmd.as_ref(), Command::If { .. }));
            }
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn trim_operations() {
        let parts = parse_word_parts("${file%.txt}");
        match &parts[0] {
            WordPart::Param(p) => {
                assert_eq!(p.name, "file");
                assert!(matches!(p.op, ParamOp::TrimSuffixSmall(_)));
            }
            other => panic!("expected Param, got {other:?}"),
        }

        let parts = parse_word_parts("${path##*/}");
        match &parts[0] {
            WordPart::Param(p) => {
                assert_eq!(p.name, "path");
                assert!(matches!(p.op, ParamOp::TrimPrefixLarge(_)));
            }
            other => panic!("expected Param, got {other:?}"),
        }
    }
}
