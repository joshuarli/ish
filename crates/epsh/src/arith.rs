//! Arithmetic expression evaluator for $((expr)).
//!
//! Implements POSIX shell arithmetic: integer-only, C-like operators.
//! Recursive-descent parser replacing dash's yacc-generated arith_yacc.c.
//!
//! Supported operators (by precedence, low to high):
//!
//! ```text
//!   = *= /= %= += -= <<= >>= &= ^= |=  (assignment)
//!   ?:                                    (ternary)
//!   ||                                    (logical or)
//!   &&                                    (logical and)
//!   |                                     (bitwise or)
//!   ^                                     (bitwise xor)
//!   &                                     (bitwise and)
//!   == !=                                 (equality)
//!   < <= > >=                             (relational)
//!   << >>                                 (shift)
//!   + -                                   (additive)
//!   * / %                                 (multiplicative)
//!   ! ~ + - (unary)                       (unary)
//! ```

use crate::error::ExitStatus;
use crate::var::Variables;

pub fn eval_arith(
    expr: &str,
    vars: &mut Variables,
    exit_status: ExitStatus,
    shell_pid: u32,
) -> Result<i64, String> {
    let tokens = tokenize(expr)?;
    let mut parser = ArithParser {
        tokens,
        pos: 0,
        vars,
        exit_status,
        shell_pid,
        noeval: false,
    };
    let result = parser.parse_expr()?;
    if parser.pos < parser.tokens.len() {
        return Err(format!("unexpected token: {:?}", parser.tokens[parser.pos]));
    }
    Ok(result)
}

#[derive(Debug, Clone, PartialEq)]
enum ArithToken {
    Num(i64),
    Var(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    LParen,
    RParen,
    Eq, // ==
    Ne, // !=
    Lt,
    Le, // <=
    Gt,
    Ge,        // >=
    And,       // &&
    Or,        // ||
    BitAnd,    // &
    BitOr,     // |
    BitXor,    // ^
    Shl,       // <<
    Shr,       // >>
    Not,       // !
    BitNot,    // ~
    Assign,    // =
    Question,  // ?
    Colon,     // :
    AddAssign, // +=
    SubAssign, // -=
    MulAssign, // *=
    DivAssign, // /=
    ModAssign, // %=
    ShlAssign, // <<=
    ShrAssign, // >>=
    AndAssign, // &=
    OrAssign,  // |=
    XorAssign, // ^=
}

fn tokenize(expr: &str) -> Result<Vec<ArithToken>, String> {
    let chars: Vec<char> = expr.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            ' ' | '\t' | '\n' => i += 1,
            '0'..='9' => {
                let start = i;
                if chars[i] == '0' && i + 1 < chars.len() {
                    match chars[i + 1] {
                        'x' | 'X' => {
                            // Hex
                            i += 2;
                            while i < chars.len() && chars[i].is_ascii_hexdigit() {
                                i += 1;
                            }
                            let s: String = chars[start..i].iter().collect();
                            let n = i64::from_str_radix(&s[2..], 16)
                                .map_err(|_| format!("invalid hex: {s}"))?;
                            tokens.push(ArithToken::Num(n));
                            continue;
                        }
                        '0'..='7' => {
                            // Octal
                            i += 1;
                            while i < chars.len() && matches!(chars[i], '0'..='7') {
                                i += 1;
                            }
                            let s: String = chars[start + 1..i].iter().collect();
                            let n = i64::from_str_radix(&s, 8)
                                .map_err(|_| format!("invalid octal: {s}"))?;
                            tokens.push(ArithToken::Num(n));
                            continue;
                        }
                        _ => {}
                    }
                }
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                let n = s
                    .parse::<i64>()
                    .map_err(|_| format!("invalid number: {s}"))?;
                tokens.push(ArithToken::Num(n));
            }
            c if c == '_' || c.is_ascii_alphabetic() => {
                let start = i;
                while i < chars.len() && (chars[i] == '_' || chars[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                tokens.push(ArithToken::Var(name));
            }
            '+' => {
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::AddAssign);
                } else {
                    tokens.push(ArithToken::Plus);
                }
            }
            '-' => {
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::SubAssign);
                } else {
                    tokens.push(ArithToken::Minus);
                }
            }
            '*' => {
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::MulAssign);
                } else {
                    tokens.push(ArithToken::Star);
                }
            }
            '/' => {
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::DivAssign);
                } else {
                    tokens.push(ArithToken::Slash);
                }
            }
            '%' => {
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::ModAssign);
                } else {
                    tokens.push(ArithToken::Percent);
                }
            }
            '(' => {
                i += 1;
                tokens.push(ArithToken::LParen);
            }
            ')' => {
                i += 1;
                tokens.push(ArithToken::RParen);
            }
            '=' => {
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::Eq);
                } else {
                    tokens.push(ArithToken::Assign);
                }
            }
            '!' => {
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::Ne);
                } else {
                    tokens.push(ArithToken::Not);
                }
            }
            '<' => {
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::Le);
                } else if i < chars.len() && chars[i] == '<' {
                    i += 1;
                    if i < chars.len() && chars[i] == '=' {
                        i += 1;
                        tokens.push(ArithToken::ShlAssign);
                    } else {
                        tokens.push(ArithToken::Shl);
                    }
                } else {
                    tokens.push(ArithToken::Lt);
                }
            }
            '>' => {
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::Ge);
                } else if i < chars.len() && chars[i] == '>' {
                    i += 1;
                    if i < chars.len() && chars[i] == '=' {
                        i += 1;
                        tokens.push(ArithToken::ShrAssign);
                    } else {
                        tokens.push(ArithToken::Shr);
                    }
                } else {
                    tokens.push(ArithToken::Gt);
                }
            }
            '&' => {
                i += 1;
                if i < chars.len() && chars[i] == '&' {
                    i += 1;
                    tokens.push(ArithToken::And);
                } else if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::AndAssign);
                } else {
                    tokens.push(ArithToken::BitAnd);
                }
            }
            '|' => {
                i += 1;
                if i < chars.len() && chars[i] == '|' {
                    i += 1;
                    tokens.push(ArithToken::Or);
                } else if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::OrAssign);
                } else {
                    tokens.push(ArithToken::BitOr);
                }
            }
            '^' => {
                i += 1;
                if i < chars.len() && chars[i] == '=' {
                    i += 1;
                    tokens.push(ArithToken::XorAssign);
                } else {
                    tokens.push(ArithToken::BitXor);
                }
            }
            '~' => {
                i += 1;
                tokens.push(ArithToken::BitNot);
            }
            '?' => {
                i += 1;
                tokens.push(ArithToken::Question);
            }
            ':' => {
                i += 1;
                tokens.push(ArithToken::Colon);
            }
            c => return Err(format!("unexpected character in arithmetic: '{c}'")),
        }
    }

    Ok(tokens)
}

struct ArithParser<'a> {
    tokens: Vec<ArithToken>,
    pos: usize,
    vars: &'a mut Variables,
    exit_status: ExitStatus,
    shell_pid: u32,
    /// When true, parse expressions but don't execute side effects (assignments).
    /// Used for short-circuit evaluation of ternary and logical operators.
    noeval: bool,
}

impl<'a> ArithParser<'a> {
    fn peek(&self) -> Option<&ArithToken> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&ArithToken> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: &ArithToken) -> Result<(), String> {
        match self.advance() {
            Some(tok) if tok == expected => Ok(()),
            Some(tok) => Err(format!("expected {expected:?}, got {tok:?}")),
            None => Err(format!("expected {expected:?}, got end of expression")),
        }
    }

    fn get_var(&self, name: &str) -> i64 {
        // Fast path: check integer cache first (avoids string→int parse)
        if let Some(n) = self.vars.get_int(name) {
            return n;
        }
        // Slow path: special params ($?, $$, $#) and non-cached vars
        self.vars
            .get_special(name, self.exit_status, self.shell_pid, "", None)
            .or_else(|| self.vars.get(name).map(String::from))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
    }

    fn set_var(&mut self, name: &str, value: i64) {
        if !self.noeval {
            // Use set_int to avoid i64→String→parse roundtrip
            let _ = self.vars.set_int(name, value);
        }
    }

    // ── Precedence climbing ──────────────────────────────────────

    fn parse_expr(&mut self) -> Result<i64, String> {
        self.parse_assignment()
    }

    fn parse_assignment(&mut self) -> Result<i64, String> {
        // Check for var = expr, var += expr, etc.
        if let Some(ArithToken::Var(name)) = self.peek().cloned() {
            let saved_pos = self.pos;
            self.pos += 1;

            if let Some(op) = self.peek().cloned() {
                match op {
                    ArithToken::Assign => {
                        self.pos += 1;
                        let val = self.parse_assignment()?;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    ArithToken::AddAssign => {
                        self.pos += 1;
                        let right = self.parse_assignment()?;
                        let val = self.get_var(&name) + right;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    ArithToken::SubAssign => {
                        self.pos += 1;
                        let right = self.parse_assignment()?;
                        let val = self.get_var(&name) - right;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    ArithToken::MulAssign => {
                        self.pos += 1;
                        let right = self.parse_assignment()?;
                        let val = self.get_var(&name) * right;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    ArithToken::DivAssign => {
                        self.pos += 1;
                        let right = self.parse_assignment()?;
                        if right == 0 {
                            return Err("division by zero".into());
                        }
                        let val = self.get_var(&name) / right;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    ArithToken::ModAssign => {
                        self.pos += 1;
                        let right = self.parse_assignment()?;
                        if right == 0 {
                            return Err("division by zero".into());
                        }
                        let val = self.get_var(&name) % right;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    ArithToken::ShlAssign => {
                        self.pos += 1;
                        let right = self.parse_assignment()?;
                        let val = self.get_var(&name) << right;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    ArithToken::ShrAssign => {
                        self.pos += 1;
                        let right = self.parse_assignment()?;
                        let val = self.get_var(&name) >> right;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    ArithToken::AndAssign => {
                        self.pos += 1;
                        let right = self.parse_assignment()?;
                        let val = self.get_var(&name) & right;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    ArithToken::OrAssign => {
                        self.pos += 1;
                        let right = self.parse_assignment()?;
                        let val = self.get_var(&name) | right;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    ArithToken::XorAssign => {
                        self.pos += 1;
                        let right = self.parse_assignment()?;
                        let val = self.get_var(&name) ^ right;
                        self.set_var(&name, val);
                        return Ok(val);
                    }
                    _ => {}
                }
            }
            // Not an assignment — backtrack
            self.pos = saved_pos;
        }
        self.parse_ternary()
    }

    fn parse_ternary(&mut self) -> Result<i64, String> {
        let cond = self.parse_or()?;
        if self.peek() == Some(&ArithToken::Question) {
            self.pos += 1;
            // Short-circuit: only evaluate the taken branch (dash: noeval | !a / noeval | !!a)
            let saved = self.noeval;
            self.noeval = saved || cond == 0; // noeval then-branch if cond is false
            let then_val = self.parse_expr()?;
            self.expect(&ArithToken::Colon)?;
            self.noeval = saved || cond != 0; // noeval else-branch if cond is true
            let else_val = self.parse_expr()?;
            self.noeval = saved;
            Ok(if cond != 0 { then_val } else { else_val })
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self) -> Result<i64, String> {
        let mut left = self.parse_and()?;
        while self.peek() == Some(&ArithToken::Or) {
            self.pos += 1;
            // Short-circuit: if left is true, don't evaluate right side effects
            let saved = self.noeval;
            self.noeval = saved || left != 0;
            let right = self.parse_and()?;
            self.noeval = saved;
            left = if left != 0 || right != 0 { 1 } else { 0 };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<i64, String> {
        let mut left = self.parse_binop(1)?;
        while self.peek() == Some(&ArithToken::And) {
            self.pos += 1;
            // Short-circuit: if left is false, don't evaluate right side effects
            let saved = self.noeval;
            self.noeval = saved || left == 0;
            let right = self.parse_binop(1)?;
            self.noeval = saved;
            left = if left != 0 && right != 0 { 1 } else { 0 };
        }
        Ok(left)
    }

    /// Apply a binary operator token to two operands. Returns None if the
    /// token is not a binary operator at this precedence level.
    fn apply_binop(tok: &ArithToken, left: i64, right: i64) -> Result<Option<i64>, String> {
        Ok(Some(match tok {
            ArithToken::BitOr => left | right,
            ArithToken::BitXor => left ^ right,
            ArithToken::BitAnd => left & right,
            ArithToken::Eq => {
                if left == right {
                    1
                } else {
                    0
                }
            }
            ArithToken::Ne => {
                if left != right {
                    1
                } else {
                    0
                }
            }
            ArithToken::Lt => {
                if left < right {
                    1
                } else {
                    0
                }
            }
            ArithToken::Le => {
                if left <= right {
                    1
                } else {
                    0
                }
            }
            ArithToken::Gt => {
                if left > right {
                    1
                } else {
                    0
                }
            }
            ArithToken::Ge => {
                if left >= right {
                    1
                } else {
                    0
                }
            }
            ArithToken::Shl => left << right,
            ArithToken::Shr => left >> right,
            ArithToken::Plus => left + right,
            ArithToken::Minus => left - right,
            ArithToken::Star => left * right,
            ArithToken::Slash => {
                if right == 0 {
                    return Err("division by zero".into());
                }
                left / right
            }
            ArithToken::Percent => {
                if right == 0 {
                    return Err("division by zero".into());
                }
                left % right
            }
            _ => return Ok(None),
        }))
    }

    /// Precedence level for binary operators (higher = tighter binding).
    fn precedence(tok: &ArithToken) -> Option<u8> {
        Some(match tok {
            ArithToken::BitOr => 1,
            ArithToken::BitXor => 2,
            ArithToken::BitAnd => 3,
            ArithToken::Eq | ArithToken::Ne => 4,
            ArithToken::Lt | ArithToken::Le | ArithToken::Gt | ArithToken::Ge => 5,
            ArithToken::Shl | ArithToken::Shr => 6,
            ArithToken::Plus | ArithToken::Minus => 7,
            ArithToken::Star | ArithToken::Slash | ArithToken::Percent => 8,
            _ => return None,
        })
    }

    /// Parse binary operators using precedence climbing.
    fn parse_binop(&mut self, min_prec: u8) -> Result<i64, String> {
        let mut left = self.parse_unary()?;
        while let Some(tok) = self.peek().cloned()
            && let Some(prec) = Self::precedence(&tok)
            && prec >= min_prec
        {
            self.pos += 1;
            let right = self.parse_binop(prec + 1)?;
            left = Self::apply_binop(&tok, left, right)?.unwrap();
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<i64, String> {
        match self.peek() {
            Some(&ArithToken::Plus) => {
                self.pos += 1;
                self.parse_unary()
            }
            Some(&ArithToken::Minus) => {
                self.pos += 1;
                Ok(-self.parse_unary()?)
            }
            Some(&ArithToken::Not) => {
                self.pos += 1;
                let val = self.parse_unary()?;
                Ok(if val == 0 { 1 } else { 0 })
            }
            Some(&ArithToken::BitNot) => {
                self.pos += 1;
                Ok(!self.parse_unary()?)
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<i64, String> {
        match self.advance().cloned() {
            Some(ArithToken::Num(n)) => Ok(n),
            Some(ArithToken::Var(name)) => Ok(self.get_var(&name)),
            Some(ArithToken::LParen) => {
                let val = self.parse_expr()?;
                self.expect(&ArithToken::RParen)?;
                Ok(val)
            }
            Some(tok) => Err(format!("unexpected token: {tok:?}")),
            None => Err("unexpected end of expression".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> i64 {
        let mut vars = Variables::new();
        eval_arith(expr, &mut vars, ExitStatus::SUCCESS, 1).unwrap()
    }

    fn eval_with_vars(expr: &str, vars: &mut Variables) -> i64 {
        eval_arith(expr, vars, ExitStatus::SUCCESS, 1).unwrap()
    }

    #[test]
    fn basic_arithmetic() {
        assert_eq!(eval("1 + 2"), 3);
        assert_eq!(eval("10 - 3"), 7);
        assert_eq!(eval("4 * 5"), 20);
        assert_eq!(eval("15 / 3"), 5);
        assert_eq!(eval("17 % 5"), 2);
    }

    #[test]
    fn precedence() {
        assert_eq!(eval("2 + 3 * 4"), 14);
        assert_eq!(eval("(2 + 3) * 4"), 20);
        assert_eq!(eval("10 - 2 * 3"), 4);
    }

    #[test]
    fn unary() {
        assert_eq!(eval("-5"), -5);
        assert_eq!(eval("+5"), 5);
        assert_eq!(eval("!0"), 1);
        assert_eq!(eval("!1"), 0);
        assert_eq!(eval("~0"), -1);
    }

    #[test]
    fn comparison() {
        assert_eq!(eval("1 == 1"), 1);
        assert_eq!(eval("1 == 2"), 0);
        assert_eq!(eval("1 != 2"), 1);
        assert_eq!(eval("1 < 2"), 1);
        assert_eq!(eval("2 <= 2"), 1);
        assert_eq!(eval("3 > 2"), 1);
        assert_eq!(eval("2 >= 3"), 0);
    }

    #[test]
    fn logical() {
        assert_eq!(eval("1 && 1"), 1);
        assert_eq!(eval("1 && 0"), 0);
        assert_eq!(eval("0 || 1"), 1);
        assert_eq!(eval("0 || 0"), 0);
    }

    #[test]
    fn bitwise() {
        assert_eq!(eval("5 & 3"), 1);
        assert_eq!(eval("5 | 3"), 7);
        assert_eq!(eval("5 ^ 3"), 6);
        assert_eq!(eval("1 << 3"), 8);
        assert_eq!(eval("16 >> 2"), 4);
    }

    #[test]
    fn ternary() {
        assert_eq!(eval("1 ? 10 : 20"), 10);
        assert_eq!(eval("0 ? 10 : 20"), 20);
    }

    #[test]
    fn variables() {
        let mut vars = Variables::new();
        vars.set("x", "10").unwrap();
        assert_eq!(eval_with_vars("x + 5", &mut vars), 15);
    }

    #[test]
    fn assignment() {
        let mut vars = Variables::new();
        assert_eq!(eval_with_vars("x = 42", &mut vars), 42);
        assert_eq!(vars.get("x"), Some("42"));
    }

    #[test]
    fn compound_assignment() {
        let mut vars = Variables::new();
        vars.set("x", "10").unwrap();
        assert_eq!(eval_with_vars("x += 5", &mut vars), 15);
        assert_eq!(vars.get("x"), Some("15"));
    }

    #[test]
    fn hex_octal() {
        assert_eq!(eval("0xff"), 255);
        assert_eq!(eval("0xFF"), 255);
        assert_eq!(eval("010"), 8);
        assert_eq!(eval("0x10"), 16);
    }

    #[test]
    fn division_by_zero() {
        let mut vars = Variables::new();
        assert!(eval_arith("1 / 0", &mut vars, ExitStatus::SUCCESS, 1).is_err());
        assert!(eval_arith("1 % 0", &mut vars, ExitStatus::SUCCESS, 1).is_err());
    }

    #[test]
    fn nested_parens() {
        assert_eq!(eval("((1 + 2) * (3 + 4))"), 21);
    }

    #[test]
    fn complex_expr() {
        assert_eq!(eval("2 + 3 * 4 - 1"), 13);
        assert_eq!(eval("(1 + 2) * 3 == 9"), 1);
        assert_eq!(eval("10 > 5 && 3 < 7"), 1);
    }
}
