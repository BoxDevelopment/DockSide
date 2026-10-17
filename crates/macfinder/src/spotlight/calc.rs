//! Spotlight's inline calculator: + - * / % ^, parentheses, unary minus, decimals.

/// Evaluates `input` if it looks like arithmetic (has a digit and an operator).
pub fn eval(input: &str) -> Option<f64> {
    let s: Vec<char> = input.chars().filter(|c| !c.is_whitespace()).collect();
    let has_digit = s.iter().any(|c| c.is_ascii_digit());
    let has_op = s.iter().skip(1).any(|c| "+-*/%^x×÷".contains(*c));
    if !has_digit || !has_op {
        return None;
    }
    let mut p = Parser { s, i: 0 };
    let v = p.expr()?;
    (p.i == p.s.len() && v.is_finite()).then_some(v)
}

/// Formats like a calculator: no trailing ".0", at most 10 decimals.
pub fn format(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.10}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

struct Parser {
    s: Vec<char>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(c) = self.peek() {
            match c {
                '+' => {
                    self.i += 1;
                    v += self.term()?;
                }
                '-' => {
                    self.i += 1;
                    v -= self.term()?;
                }
                _ => break,
            }
        }
        Some(v)
    }

    fn term(&mut self) -> Option<f64> {
        let mut v = self.power()?;
        while let Some(c) = self.peek() {
            match c {
                '*' | 'x' | '×' => {
                    self.i += 1;
                    v *= self.power()?;
                }
                '/' | '÷' => {
                    self.i += 1;
                    v /= self.power()?;
                }
                '%' => {
                    self.i += 1;
                    v %= self.power()?;
                }
                _ => break,
            }
        }
        Some(v)
    }

    /// Right-associative exponent.
    fn power(&mut self) -> Option<f64> {
        let base = self.unary()?;
        if self.peek() == Some('^') {
            self.i += 1;
            return Some(base.powf(self.power()?));
        }
        Some(base)
    }

    fn unary(&mut self) -> Option<f64> {
        match self.peek()? {
            '-' => {
                self.i += 1;
                Some(-self.unary()?)
            }
            '+' => {
                self.i += 1;
                self.unary()
            }
            _ => self.atom(),
        }
    }

    fn atom(&mut self) -> Option<f64> {
        if self.peek()? == '(' {
            self.i += 1;
            let v = self.expr()?;
            if self.peek()? != ')' {
                return None;
            }
            self.i += 1;
            return Some(v);
        }
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '.' || c == ',') {
            self.i += 1;
        }
        let num: String = self.s[start..self.i].iter().filter(|c| **c != ',').collect();
        num.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        assert_eq!(eval("2+2"), Some(4.0));
        assert_eq!(eval("2+3*4"), Some(14.0));
        assert_eq!(eval("(2+3)*4"), Some(20.0));
        assert_eq!(eval("2^3^2"), Some(512.0));
        assert_eq!(eval("-3 + 5"), Some(2.0));
        assert_eq!(eval("1,000 * 2"), Some(2000.0));
        assert_eq!(eval("10 / 4"), Some(2.5));
    }

    #[test]
    fn not_math() {
        assert_eq!(eval("notepad"), None);
        assert_eq!(eval("42"), None);
        assert_eq!(eval("2+"), None);
        assert_eq!(eval("1/0"), None);
    }

    #[test]
    fn formatting() {
        assert_eq!(format(4.0), "4");
        assert_eq!(format(2.5), "2.5");
        assert_eq!(format(1.0 / 3.0), "0.3333333333");
    }
}
