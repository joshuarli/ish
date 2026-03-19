//! Minimal recursive-descent expression evaluator.
//!
//! Supports: `+`, `-`, `*`, `/`, `%`, `**` (power), parentheses, unary minus,
//! integers and floats, comparisons (`<`, `>`, `<=`, `>=`, `==`, `!=`).
//!
//! Precedence (low to high): comparison < additive < multiplicative < power < unary.

pub fn eval(expr: &str) -> Result<String, &'static str> {
    let tokens = tokenize(expr)?;
    // Guard against stack overflow from deeply nested parentheses.
    let depth = tokens.iter().filter(|t| matches!(t, Token::LParen)).count();
    if depth > 64 {
        return Err("expression too deeply nested");
    }
    let mut pos = 0;
    let result = parse_comparison(&tokens, &mut pos)?;
    if pos < tokens.len() {
        return Err("unexpected token after expression");
    }
    Ok(format_number(result))
}

fn format_number(n: f64) -> String {
    if n.is_infinite() || n.is_nan() {
        return format!("{n}");
    }
    // If the result is an exact integer (no fractional part), format as integer.
    if n == n.trunc() && n.abs() < (i64::MAX as f64) {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

// -- Tokenizer --

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    StarStar,
    LParen,
    RParen,
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,
}

fn tokenize(expr: &str) -> Result<Vec<Token>, &'static str> {
    let bytes = expr.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' => i += 1,
            b'+' => {
                tokens.push(Token::Plus);
                i += 1;
            }
            b'-' => {
                tokens.push(Token::Minus);
                i += 1;
            }
            b'*' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    tokens.push(Token::StarStar);
                    i += 2;
                } else {
                    tokens.push(Token::Star);
                    i += 1;
                }
            }
            b'/' => {
                tokens.push(Token::Slash);
                i += 1;
            }
            b'%' => {
                tokens.push(Token::Percent);
                i += 1;
            }
            b'(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Le);
                    i += 2;
                } else {
                    tokens.push(Token::Lt);
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Ge);
                    i += 2;
                } else {
                    tokens.push(Token::Gt);
                    i += 1;
                }
            }
            b'=' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Eq);
                    i += 2;
                } else {
                    return Err("unexpected '=', did you mean '=='?");
                }
            }
            b'!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Ne);
                    i += 2;
                } else {
                    return Err("unexpected '!', did you mean '!='?");
                }
            }
            b'0'..=b'9' | b'.' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                let s = &expr[start..i];
                let n: f64 = s.parse().map_err(|_| "invalid number")?;
                tokens.push(Token::Num(n));
            }
            _ => return Err("unexpected character"),
        }
    }

    Ok(tokens)
}

// -- Parser (recursive descent) --

fn parse_comparison(tokens: &[Token], pos: &mut usize) -> Result<f64, &'static str> {
    let mut left = parse_additive(tokens, pos)?;
    while *pos < tokens.len() {
        let cmp = match tokens[*pos] {
            Token::Lt => |a: f64, b: f64| if a < b { 1.0 } else { 0.0 },
            Token::Gt => |a: f64, b: f64| if a > b { 1.0 } else { 0.0 },
            Token::Le => |a: f64, b: f64| if a <= b { 1.0 } else { 0.0 },
            Token::Ge => |a: f64, b: f64| if a >= b { 1.0 } else { 0.0 },
            Token::Eq => |a: f64, b: f64| if a == b { 1.0 } else { 0.0 },
            Token::Ne => |a: f64, b: f64| if a != b { 1.0 } else { 0.0 },
            _ => break,
        };
        *pos += 1;
        let right = parse_additive(tokens, pos)?;
        left = cmp(left, right);
    }
    Ok(left)
}

fn parse_additive(tokens: &[Token], pos: &mut usize) -> Result<f64, &'static str> {
    let mut left = parse_multiplicative(tokens, pos)?;
    while *pos < tokens.len() {
        match tokens[*pos] {
            Token::Plus => {
                *pos += 1;
                left += parse_multiplicative(tokens, pos)?;
            }
            Token::Minus => {
                *pos += 1;
                left -= parse_multiplicative(tokens, pos)?;
            }
            _ => break,
        }
    }
    Ok(left)
}

fn parse_multiplicative(tokens: &[Token], pos: &mut usize) -> Result<f64, &'static str> {
    let mut left = parse_power(tokens, pos)?;
    while *pos < tokens.len() {
        match tokens[*pos] {
            Token::Star => {
                *pos += 1;
                left *= parse_power(tokens, pos)?;
            }
            Token::Slash => {
                *pos += 1;
                let right = parse_power(tokens, pos)?;
                if right == 0.0 {
                    return Err("division by zero");
                }
                left /= right;
            }
            Token::Percent => {
                *pos += 1;
                let right = parse_power(tokens, pos)?;
                if right == 0.0 {
                    return Err("division by zero");
                }
                left %= right;
            }
            _ => break,
        }
    }
    Ok(left)
}

fn parse_power(tokens: &[Token], pos: &mut usize) -> Result<f64, &'static str> {
    let base = parse_unary(tokens, pos)?;
    if *pos < tokens.len() && tokens[*pos] == Token::StarStar {
        *pos += 1;
        // Right-associative: parse_power recurses
        let exp = parse_power(tokens, pos)?;
        Ok(base.powf(exp))
    } else {
        Ok(base)
    }
}

fn parse_unary(tokens: &[Token], pos: &mut usize) -> Result<f64, &'static str> {
    if *pos < tokens.len() && tokens[*pos] == Token::Minus {
        *pos += 1;
        let val = parse_unary(tokens, pos)?;
        Ok(-val)
    } else if *pos < tokens.len() && tokens[*pos] == Token::Plus {
        *pos += 1;
        parse_unary(tokens, pos)
    } else {
        parse_primary(tokens, pos)
    }
}

fn parse_primary(tokens: &[Token], pos: &mut usize) -> Result<f64, &'static str> {
    if *pos >= tokens.len() {
        return Err("unexpected end of expression");
    }
    match &tokens[*pos] {
        Token::Num(n) => {
            let val = *n;
            *pos += 1;
            Ok(val)
        }
        Token::LParen => {
            *pos += 1;
            let val = parse_comparison(tokens, pos)?;
            if *pos >= tokens.len() || tokens[*pos] != Token::RParen {
                return Err("unclosed parenthesis");
            }
            *pos += 1;
            Ok(val)
        }
        _ => Err("expected number or '('"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_arithmetic() {
        assert_eq!(eval("2 + 3").unwrap(), "5");
        assert_eq!(eval("10 - 4").unwrap(), "6");
        assert_eq!(eval("3 * 4").unwrap(), "12");
        assert_eq!(eval("15 / 4").unwrap(), "3.75");
        assert_eq!(eval("7 % 3").unwrap(), "1");
    }

    #[test]
    fn precedence() {
        assert_eq!(eval("2 + 3 * 4").unwrap(), "14");
        assert_eq!(eval("(2 + 3) * 4").unwrap(), "20");
    }

    #[test]
    fn power() {
        assert_eq!(eval("2 ** 10").unwrap(), "1024");
        // Right-associative: 2 ** (3 ** 2) = 2 ** 9 = 512
        assert_eq!(eval("2 ** 3 ** 2").unwrap(), "512");
    }

    #[test]
    fn unary_minus() {
        assert_eq!(eval("-5").unwrap(), "-5");
        assert_eq!(eval("-5 + 3").unwrap(), "-2");
        assert_eq!(eval("-(2 + 3)").unwrap(), "-5");
    }

    #[test]
    fn comparisons() {
        assert_eq!(eval("3 > 2").unwrap(), "1");
        assert_eq!(eval("2 > 3").unwrap(), "0");
        assert_eq!(eval("2 == 2").unwrap(), "1");
        assert_eq!(eval("2 != 3").unwrap(), "1");
        assert_eq!(eval("3 <= 3").unwrap(), "1");
        assert_eq!(eval("4 >= 5").unwrap(), "0");
    }

    #[test]
    fn floats() {
        assert_eq!(eval("1.5 + 2.5").unwrap(), "4");
        assert_eq!(eval("1.5 * 2").unwrap(), "3");
    }

    #[test]
    fn division_by_zero() {
        assert!(eval("1 / 0").is_err());
        assert!(eval("1 % 0").is_err());
    }

    #[test]
    fn integer_formatting() {
        assert_eq!(eval("6 / 2").unwrap(), "3");
        assert_eq!(eval("0").unwrap(), "0");
    }

    #[test]
    fn deep_nesting_rejected() {
        let expr = "(".repeat(100) + "1" + &")".repeat(100);
        assert!(eval(&expr).is_err());
    }

    #[test]
    fn reasonable_nesting_ok() {
        assert_eq!(eval("((((1 + 2))))").unwrap(), "3");
    }
}
