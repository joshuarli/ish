use crate::ast::*;
use crate::shell_bytes::ShellBytes;
use crate::error::{ShellError, Span};
use crate::lexer::{
    Lexer, NestingGuard, PendingHereDoc, Token, parts_have_quoting, parts_to_text,
    unbalanced_arith_close,
};

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
    heredoc_bodies: Vec<HereDocBody>,
}

impl Parser {
    pub fn new(source: &str) -> Self {
        Parser {
            lexer: Lexer::new(source),
            heredoc_bodies: Vec::new(),
        }
    }

    pub(crate) fn from_shell_string(source: &str) -> Self {
        Parser {
            lexer: Lexer::from_shell_string(source),
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

    fn peek(&mut self) -> Result<(Token, Span), ShellError> {
        let (tok, span) = self.lexer.next_token()?;
        self.lexer.push_back(tok.clone(), span);
        Ok((tok, span))
    }

    fn next(&mut self) -> Result<(Token, Span), ShellError> {
        self.lexer.next_token()
    }

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

    fn skip_newlines(&mut self) -> Result<(), ShellError> {
        loop {
            let (tok, span) = self.next()?;
            if tok == Token::Newline {
                if !self.lexer.pending_heredocs.is_empty() {
                    let pending: Vec<PendingHereDoc> =
                        std::mem::take(&mut self.lexer.pending_heredocs);
                    for heredoc in &pending {
                        let raw = self.lexer.read_heredoc_body(heredoc)?;
                        self.heredoc_bodies
                            .push(make_heredoc_body(raw, heredoc.quoted)?);
                    }
                }
            } else {
                self.lexer.push_back(tok, span);
                return Ok(());
            }
        }
    }

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

    fn parse_complete_command(&mut self) -> Result<Command, ShellError> {
        self.parse_command_list(false)
    }

    fn parse_compound_list(&mut self) -> Result<Command, ShellError> {
        let (_, span) = self.peek()?;
        let _nesting = NestingGuard::enter(span)?;
        self.parse_command_list(true)
    }

    fn read_pending_heredocs(&mut self, cmd: &mut Command) -> Result<(), ShellError> {
        if !self.lexer.pending_heredocs.is_empty() {
            let pending: Vec<PendingHereDoc> = std::mem::take(&mut self.lexer.pending_heredocs);
            let mut bodies = Vec::new();
            for heredoc in &pending {
                let raw = self.lexer.read_heredoc_body(heredoc)?;
                bodies.push(make_heredoc_body(raw, heredoc.quoted)?);
            }
            Self::patch_heredocs(cmd, &bodies);
        }
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
                    if !self.lexer.pending_heredocs.is_empty() {
                        let pending: Vec<PendingHereDoc> =
                            std::mem::take(&mut self.lexer.pending_heredocs);
                        for heredoc in &pending {
                            let raw = self.lexer.read_heredoc_body(heredoc)?;
                            self.heredoc_bodies
                                .push(make_heredoc_body(raw, heredoc.quoted)?);
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
                self.parse_simple_command_or_func()
            }
        }
    }

    fn parse_simple_command_or_func(&mut self) -> Result<Command, ShellError> {
        let span = self.lexer.span();
        let mut assigns = Vec::new();
        let mut args = Vec::new();
        let mut redirs = Vec::new();

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

        loop {
            let (tok, tok_span) = self.peek()?;
            match &tok {
                Token::Word(parts, had_q) => {
                    let text = parts_to_text(parts);

                    if !had_q
                        && !text.is_empty()
                        && text.len() <= 9
                        && text.chars().all(|c| c.is_ascii_digit())
                    {
                        self.next()?;
                        let (next_tok, next_span) = self.peek()?;
                        if next_tok.is_redir() {
                            let fd: i32 = text.parse().unwrap_or(-1);
                            self.next()?;
                            let mut redir = self.parse_redir_after_op(&next_tok, next_span)?;
                            redir.fd = fd;
                            redirs.push(redir);
                            continue;
                        }
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
                Token::Assignment { name, value } if !args.is_empty() => {
                    self.next()?;
                    let mut parts = vec![WordPart::Literal(format!("{}=", name).into())];
                    parts.extend(value.clone());
                    args.push(Word {
                        parts,
                        span: tok_span,
                    });
                }
                tok if args.is_empty() && !tok.is_redir() => break,
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
                        parts: vec![WordPart::Literal(ShellBytes::from(text))],
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

    fn parse_redir_after_op(&mut self, op: &Token, span: Span) -> Result<Redir, ShellError> {
        let fd = match op {
            Token::Less | Token::DLess | Token::DLessDash | Token::LessAnd | Token::LessGreat => 0,
            _ => 1,
        };

        match op {
            Token::DLess | Token::DLessDash => {
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

                let quoted = had_quoting || parts_have_quoting(&delim_parts);
                let delimiter = parts_to_text(&delim_parts);

                self.lexer.pending_heredocs.push(PendingHereDoc {
                    delimiter: ShellBytes::from_str_lossless(&delimiter),
                    strip_tabs,
                    quoted,
                });

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
                self.lexer.recognize_reserved = false;
                let (word_tok, word_span) = self.next()?;
                self.lexer.recognize_reserved = true;
                let word = match word_tok {
                    Token::Word(parts, _) => Word {
                        parts,
                        span: word_span,
                    },
                    ref tok => {
                        let text = reserved_word_text(tok);
                        if text.is_empty() {
                            return Err(ShellError::Syntax {
                                msg: format!("expected filename, got {word_tok:?}"),
                                span: word_span,
                            });
                        }
                        Word {
                            parts: vec![WordPart::Literal(ShellBytes::from(text))],
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

    fn parse_for(&mut self) -> Result<Command, ShellError> {
        let span = self.expect(&Token::For)?;

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

    fn parse_case(&mut self) -> Result<Command, ShellError> {
        let span = self.expect(&Token::Case)?;

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

    fn parse_case_arm(&mut self) -> Result<CaseArm, ShellError> {
        let span = self.lexer.span();

        let (tok, _) = self.peek()?;
        if tok == Token::LParen {
            self.next()?;
        }

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
                        parts: vec![WordPart::Literal(ShellBytes::from(text))],
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

        let (tok, _) = self.peek()?;
        let body = if tok == Token::SemiSemi || tok == Token::Esac {
            None
        } else {
            Some(self.parse_compound_list()?)
        };

        let (tok, _) = self.peek()?;
        if tok == Token::SemiSemi {
            self.next()?;
            self.skip_newlines()?;
        }

        Ok(CaseArm {
            patterns,
            body,
            span,
        })
    }

    fn parse_redirections(&mut self) -> Result<Vec<Redir>, ShellError> {
        let mut redirs = Vec::new();
        loop {
            let (tok, span) = self.peek()?;
            if tok.is_redir() {
                self.next()?;
                redirs.push(self.parse_redir_after_op(&tok, span)?);
            } else if let Token::Word(parts, false) = &tok {
                let text = parts_to_text(parts);
                if !text.is_empty() && text.len() <= 9 && text.chars().all(|c| c.is_ascii_digit()) {
                    self.next()?;
                    let (next, next_span) = self.peek()?;
                    if next.is_redir() {
                        let fd: i32 = text.parse().unwrap_or(-1);
                        self.next()?;
                        let mut redir = self.parse_redir_after_op(&next, next_span)?;
                        redir.fd = fd;
                        redirs.push(redir);
                    } else {
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

/// Parse a raw shell word while preserving the legacy empty-command recovery.
/// Use [`try_parse_word_parts`] to receive nested syntax errors.
pub fn parse_word_parts(raw: &str) -> Vec<WordPart> {
    parse_word_parts_legacy(&crate::encoding::bytes_to_str(raw.as_bytes()))
}

pub(crate) fn parse_word_parts_shell(raw: &str) -> Result<Vec<WordPart>, ShellError> {
    parse_word_parts_impl(raw, false, NestedCommandErrors::Propagate)
}

/// Parse a raw shell word and return nested syntax errors.
pub fn try_parse_word_parts(raw: &str) -> Result<Vec<WordPart>, ShellError> {
    parse_word_parts_shell(&crate::encoding::bytes_to_str(raw.as_bytes()))
}

/// Parse a raw byte string into word parts while preserving the legacy empty-command recovery.
/// Use [`try_parse_word_parts_bytes`] to receive nested syntax errors.
pub fn parse_word_parts_bytes(raw: &[u8]) -> Vec<WordPart> {
    parse_word_parts_legacy(&crate::encoding::bytes_to_str(raw))
}

/// Parse raw shell bytes into word parts and return nested syntax errors.
pub fn try_parse_word_parts_bytes(raw: &[u8]) -> Result<Vec<WordPart>, ShellError> {
    parse_word_parts_shell(&crate::encoding::bytes_to_str(raw))
}

fn parse_word_parts_legacy(raw: &str) -> Vec<WordPart> {
    parse_word_parts_impl(raw, false, NestedCommandErrors::LegacyEmptyCommand)
        .expect("legacy parser converts nested command errors to empty commands")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NestedCommandErrors {
    Propagate,
    LegacyEmptyCommand,
}

fn make_heredoc_body(raw: String, quoted: bool) -> Result<HereDocBody, ShellError> {
    if quoted {
        Ok(HereDocBody::Literal(ShellBytes::from_str_lossless(&raw)))
    } else {
        Ok(HereDocBody::Parsed(parse_word_parts_heredoc(
            &raw,
            NestedCommandErrors::Propagate,
        )?))
    }
}

fn parse_word_parts_heredoc(
    raw: &str,
    nested_errors: NestedCommandErrors,
) -> Result<Vec<WordPart>, ShellError> {
    let _nesting = NestingGuard::enter(Span::default())?;
    let mut parts = Vec::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '$' => {
                i += 1;
                if let Some(part) = parse_dollar(&chars, &mut i, true, nested_errors)? {
                    parts.push(part);
                } else {
                    parts.push(WordPart::Literal("$".into()));
                }
            }
            '`' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '`' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                let content: String = chars[start..i].iter().collect();
                if i < chars.len() {
                    i += 1;
                }
                let cmd = parse_nested_command_substitution(&content, nested_errors)?;
                parts.push(WordPart::Backtick(Box::new(cmd)));
            }
            '\\' => {
                i += 1;
                if i < chars.len() {
                    if chars[i] == '\n' {
                        i += 1;
                    } else if matches!(chars[i], '$' | '`' | '\\') {
                        parts.push(WordPart::Literal(ShellBytes::from_str_lossless(&chars[i].to_string())));
                        i += 1;
                    } else {
                        let start = i;
                        i += if chars[i] == crate::encoding::ESCAPE && i + 1 < chars.len() { 2 } else { 1 };
                        let literal: String = chars[start..i].iter().collect();
                        parts.push(WordPart::Literal(ShellBytes::from_str_lossless(&format!("\\{literal}"))));
                    }
                }
            }
            _ => {
                let start = i;
                while i < chars.len() && !matches!(chars[i], '$' | '`' | '\\') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                if !text.is_empty() {
                    parts.push(WordPart::Literal(ShellBytes::from_str_lossless(&text)));
                }
            }
        }
    }

    Ok(coalesce_literals(parts))
}

fn parse_word_parts_impl(
    raw: &str,
    in_dquote: bool,
    nested_errors: NestedCommandErrors,
) -> Result<Vec<WordPart>, ShellError> {
    let mut parts = Vec::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '\'' if !in_dquote => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                let content: String = chars[start..i].iter().collect();
                parts.push(WordPart::SingleQuoted(ShellBytes::from_str_lossless(&content)));
                if i < chars.len() {
                    i += 1;
                }
            }
            '"' => {
                i += 1;
                let inner = parse_double_quoted_parts(&chars, &mut i, nested_errors)?;
                parts.push(WordPart::DoubleQuoted(inner));
                if i < chars.len() && chars[i] == '"' {
                    i += 1;
                }
            }
            '$' => {
                i += 1;
                if let Some(part) = parse_dollar(&chars, &mut i, in_dquote, nested_errors)? {
                    parts.push(part);
                } else {
                    parts.push(WordPart::Literal("$".into()));
                }
            }
            '`' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '`' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                let content: String = chars[start..i].iter().collect();
                if i < chars.len() {
                    i += 1;
                }
                let cmd = parse_nested_command_substitution(&content, nested_errors)?;
                parts.push(WordPart::Backtick(Box::new(cmd)));
            }
            '\\' => {
                i += 1;
                if i < chars.len() {
                    if chars[i] == '\n' {
                        i += 1;
                    } else if matches!(chars[i], '$' | '`' | '"' | '\\') {
                        parts.push(WordPart::Literal(ShellBytes::from_str_lossless(&chars[i].to_string())));
                        i += 1;
                    } else {
                        let start = i;
                        i += if chars[i] == crate::encoding::ESCAPE && i + 1 < chars.len() { 2 } else { 1 };
                        let literal: String = chars[start..i].iter().collect();
                        parts.push(WordPart::Literal(ShellBytes::from_str_lossless(&format!("\\{literal}"))));
                    }
                }
            }
            '~' if parts.is_empty() => {
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
                parts.push(WordPart::Tilde(ShellBytes::from_str_lossless(&user)));
            }
            _ => {
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
                    parts.push(WordPart::Literal(ShellBytes::from_str_lossless(&text)));
                }
            }
        }
    }

    Ok(coalesce_literals(parts))
}

fn parse_double_quoted_parts(
    chars: &[char],
    i: &mut usize,
    nested_errors: NestedCommandErrors,
) -> Result<Vec<WordPart>, ShellError> {
    let mut parts = Vec::new();
    let mut literal = String::new();

    while *i < chars.len() && chars[*i] != '"' {
        match chars[*i] {
            '$' => {
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(ShellBytes::from_str_lossless(&std::mem::take(&mut literal))));
                }
                *i += 1;
                if let Some(part) = parse_dollar(chars, i, true, nested_errors)? {
                    parts.push(part);
                } else {
                    literal.push('$');
                }
            }
            '`' => {
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(ShellBytes::from_str_lossless(&std::mem::take(&mut literal))));
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
                let cmd = parse_nested_command_substitution(&content, nested_errors)?;
                parts.push(WordPart::Backtick(Box::new(cmd)));
            }
            '\\' => {
                *i += 1;
                if *i < chars.len() {
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
        parts.push(WordPart::Literal(ShellBytes::from_str_lossless(&literal)));
    }

    Ok(parts)
}

fn parse_dollar(
    chars: &[char],
    i: &mut usize,
    in_dquote: bool,
    nested_errors: NestedCommandErrors,
) -> Result<Option<WordPart>, ShellError> {
    if *i >= chars.len() {
        return Ok(None);
    }

    Ok(Some(match chars[*i] {
        '{' => {
            *i += 1;
            parse_brace_param(chars, i, in_dquote, nested_errors)?
        }
        '(' => {
            *i += 1;
            if *i < chars.len() && chars[*i] == '(' {
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
                        depth = unbalanced_arith_close(depth, Span::default())?;
                        content.push(chars[*i]);
                        *i += 1;
                    } else {
                        content.push(chars[*i]);
                        *i += 1;
                    }
                }
                let text: String = content.into_iter().collect();
                let inner_parts = parse_word_parts_impl(&text, false, nested_errors)?;
                WordPart::Arith(inner_parts)
            } else {
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
                let cmd = parse_nested_command_substitution(&content, nested_errors)?;
                WordPart::CmdSubst(Box::new(cmd))
            }
        }
        c @ ('@' | '*' | '#' | '?' | '-' | '$' | '!') => {
            *i += 1;
            WordPart::Param(ParamExpr {
                name: c.to_string(),
                op: ParamOp::Normal,
                span: Span::default(),
            })
        }
        c @ '0'..='9' => {
            *i += 1;
            WordPart::Param(ParamExpr {
                name: c.to_string(),
                op: ParamOp::Normal,
                span: Span::default(),
            })
        }
        c if c == '_' || c.is_ascii_alphabetic() => {
            let start = *i;
            while *i < chars.len() && (chars[*i] == '_' || chars[*i].is_ascii_alphanumeric()) {
                *i += 1;
            }
            let name: String = chars[start..*i].iter().collect();
            WordPart::Param(ParamExpr {
                name,
                op: ParamOp::Normal,
                span: Span::default(),
            })
        }
        _ => return Ok(None),
    }))
}

fn parse_brace_param(
    chars: &[char],
    i: &mut usize,
    in_dquote: bool,
    nested_errors: NestedCommandErrors,
) -> Result<WordPart, ShellError> {
    if *i >= chars.len() {
        return Ok(WordPart::Literal("${".into()));
    }

    let length =
        *i < chars.len() && chars[*i] == '#' && *i + 1 < chars.len() && chars[*i + 1] != '}';
    if length {
        *i += 1;
    }

    let start = *i;
    if *i < chars.len() && matches!(chars[*i], '@' | '*' | '#' | '?' | '-' | '$' | '!') {
        *i += 1;
    } else if *i < chars.len() && chars[*i].is_ascii_digit() {
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
        while *i < chars.len() && chars[*i] != '}' {
            *i += 1;
        }
        if *i < chars.len() {
            *i += 1;
        }
        return Ok(WordPart::Param(ParamExpr {
            name,
            op: ParamOp::Length,
            span: Span::default(),
        }));
    }

    if *i >= chars.len() || chars[*i] == '}' {
        if *i < chars.len() {
            *i += 1;
        }
        return Ok(WordPart::Param(ParamExpr {
            name,
            op: ParamOp::Normal,
            span: Span::default(),
        }));
    }

    let op_char = chars[*i];

    if !matches!(op_char, ':' | '-' | '=' | '?' | '+' | '%' | '#') {
        while *i < chars.len() && chars[*i] != '}' {
            *i += 1;
        }
        if *i < chars.len() {
            *i += 1;
        }
        return Ok(WordPart::Param(ParamExpr {
            name: format!("{}{}", name, op_char),
            op: ParamOp::BadSubst,
            span: Span::default(),
        }));
    }

    *i += 1;

    let op = match op_char {
        ':' if *i < chars.len() => {
            let op2 = chars[*i];
            *i += 1;
            let word = read_brace_word(chars, i, in_dquote, nested_errors)?;
            match op2 {
                '-' => ParamOp::Default { colon: true, word },
                '=' => ParamOp::Assign { colon: true, word },
                '?' => ParamOp::Error { colon: true, word },
                '+' => ParamOp::Alternative { colon: true, word },
                _ => ParamOp::Normal,
            }
        }
        '-' => {
            let word = read_brace_word(chars, i, in_dquote, nested_errors)?;
            ParamOp::Default { colon: false, word }
        }
        '=' => {
            let word = read_brace_word(chars, i, in_dquote, nested_errors)?;
            ParamOp::Assign { colon: false, word }
        }
        '?' => {
            let word = read_brace_word(chars, i, in_dquote, nested_errors)?;
            ParamOp::Error { colon: false, word }
        }
        '+' => {
            let word = read_brace_word(chars, i, in_dquote, nested_errors)?;
            ParamOp::Alternative { colon: false, word }
        }
        '%' => {
            if *i < chars.len() && chars[*i] == '%' {
                *i += 1;
                let word = read_brace_word(chars, i, false, nested_errors)?;
                ParamOp::TrimSuffixLarge(word)
            } else {
                let word = read_brace_word(chars, i, false, nested_errors)?;
                ParamOp::TrimSuffixSmall(word)
            }
        }
        '#' => {
            if *i < chars.len() && chars[*i] == '#' {
                *i += 1;
                let word = read_brace_word(chars, i, false, nested_errors)?;
                ParamOp::TrimPrefixLarge(word)
            } else {
                let word = read_brace_word(chars, i, false, nested_errors)?;
                ParamOp::TrimPrefixSmall(word)
            }
        }
        _ => {
            while *i < chars.len() && chars[*i] != '}' {
                *i += 1;
            }
            if *i < chars.len() {
                *i += 1;
            }
            ParamOp::Normal
        }
    };

    Ok(WordPart::Param(ParamExpr {
        name,
        op,
        span: Span::default(),
    }))
}

fn read_brace_word(
    chars: &[char],
    i: &mut usize,
    in_dquote: bool,
    nested_errors: NestedCommandErrors,
) -> Result<Vec<WordPart>, ShellError> {
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

    parse_word_parts_impl(&raw, in_dquote, nested_errors)
}

pub(crate) fn coalesce_literals(parts: Vec<WordPart>) -> Vec<WordPart> {
    let mut result = Vec::with_capacity(parts.len());
    for part in parts {
        if let WordPart::Literal(ref s) = part
            && let Some(WordPart::Literal(prev)) = result.last_mut()
        {
            let mut bytes = prev.as_bytes().to_vec();
            bytes.extend_from_slice(s.as_bytes());
            *prev = ShellBytes::from_vec(bytes);
            continue;
        }
        result.push(part);
    }
    result
}

pub(crate) fn parse_cmdsubst_content(content: &str) -> Result<Command, ShellError> {
    let _nesting = NestingGuard::enter(Span::default())?;
    let program = Parser::from_shell_string(content).parse()?;
    let command = if program.commands.is_empty() {
        empty_command_substitution()
    } else if program.commands.len() == 1 {
        program.commands.into_iter().next().unwrap()
    } else {
        let mut iter = program.commands.into_iter();
        let mut result = iter.next().unwrap();
        for cmd in iter {
            result = Command::Sequence(Box::new(result), Box::new(cmd));
        }
        result
    };
    Ok(command)
}

fn parse_nested_command_substitution(
    content: &str,
    nested_errors: NestedCommandErrors,
) -> Result<Command, ShellError> {
    match parse_cmdsubst_content(content) {
        Err(_) if nested_errors == NestedCommandErrors::LegacyEmptyCommand => {
            Ok(empty_command_substitution())
        }
        result => result,
    }
}

fn empty_command_substitution() -> Command {
    Command::Simple {
        assigns: Vec::new(),
        args: Vec::new(),
        redirs: Vec::new(),
        span: Span::default(),
    }
}

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
            Command::Or(_, _) => {}
            other => panic!("expected Or, got {other:?}"),
        }
    }

    #[test]
    fn sequence() {
        let prog = parse("a; b; c");
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
    fn word_parts_propagate_nested_syntax_errors() {
        assert!(matches!(
            try_parse_word_parts("$(if)"),
            Err(ShellError::Syntax { .. })
        ));
    }

    #[test]
    fn word_parts_legacy_api_retains_empty_command_recovery() {
        let parts = parse_word_parts("$(if)");
        assert!(matches!(&parts[0], WordPart::CmdSubst(command)
            if matches!(command.as_ref(), Command::Simple { args, .. } if args.is_empty())));
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

    fn nested_command_substitution(levels: u32) -> String {
        format!(
            "echo {}x{}",
            "$(echo ".repeat(levels as usize),
            ")".repeat(levels as usize)
        )
    }

    fn nested_arithmetic(levels: u32) -> String {
        format!(
            "echo {}1{}",
            "$(( ".repeat(levels as usize),
            " ))".repeat(levels as usize)
        )
    }

    fn nested_brace_defaults(levels: u32) -> String {
        format!(
            "echo {}x{}",
            "${a:-".repeat(levels as usize),
            "}".repeat(levels as usize)
        )
    }

    fn nested_subshells(levels: u32) -> String {
        format!(
            "{}echo x{}",
            "(".repeat(levels as usize),
            ")".repeat(levels as usize)
        )
    }

    fn assert_syntax_error(src: &str) {
        match Parser::new(src).parse() {
            Err(ShellError::Syntax { .. }) => {}
            other => panic!("expected syntax error, got {other:?}"),
        }
    }

    #[test]
    fn command_substitution_nesting_is_bounded() {
        let limit = crate::lexer::MAX_NESTING_DEPTH;
        Parser::new(&nested_command_substitution(limit)).parse().unwrap();
        assert_syntax_error(&nested_command_substitution(limit + 1));
        assert_syntax_error(&nested_command_substitution(100_000));
    }

    #[test]
    fn arithmetic_nesting_is_bounded() {
        let limit = crate::lexer::MAX_NESTING_DEPTH;
        Parser::new(&nested_arithmetic(limit)).parse().unwrap();
        assert_syntax_error(&nested_arithmetic(limit + 1));
        assert_syntax_error(&nested_arithmetic(100_000));
    }

    #[test]
    fn brace_default_nesting_is_bounded() {
        let limit = crate::lexer::MAX_NESTING_DEPTH;
        Parser::new(&nested_brace_defaults(limit)).parse().unwrap();
        assert_syntax_error(&nested_brace_defaults(limit + 1));
        assert_syntax_error(&nested_brace_defaults(100_000));
    }

    #[test]
    fn subshell_nesting_is_bounded() {
        let limit = crate::lexer::MAX_NESTING_DEPTH;
        Parser::new(&nested_subshells(limit)).parse().unwrap();
        assert_syntax_error(&nested_subshells(limit + 1));
        assert_syntax_error(&nested_subshells(100_000));
    }

    #[test]
    fn unbalanced_close_in_arithmetic_is_syntax_error() {
        assert_syntax_error("echo $(( ) )");
        assert_syntax_error("echo \"$(( ) )\"");
        assert_syntax_error("echo $(echo $(( ) ) )");
        assert!(matches!(
            try_parse_word_parts("$(( ) )"),
            Err(ShellError::Syntax { .. })
        ));
    }

    #[test]
    fn nesting_budget_is_released_after_each_parse() {
        let limit = crate::lexer::MAX_NESTING_DEPTH;
        for _ in 0..3 {
            Parser::new(&nested_command_substitution(limit)).parse().unwrap();
            assert_syntax_error(&nested_command_substitution(limit + 1));
        }
    }
}
