//! Small Java-like expression parser used by evaluate, conditional breakpoints
//! and watches.
//!
//! Supported: literals, local names, one-level field access, array indexing
//! (`arr[i]`), method calls (`obj.m(a)`, `this.m()`, `Classe.stat(a)`),
//! parentheses, unary `! - +`, arithmetic, comparisons and boolean operators.
//! `new`, casts and lambdas are intentionally rejected; method calls are
//! executed through JDWP invocation on the paused JVM (side effects possible).

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Literal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Char(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnaryOp {
    Not,
    Negate,
    Plus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Literal(Literal),
    Name(String),
    Field(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Call {
        receiver: Option<Box<Expr>>,
        method: String,
        args: Vec<Expr>,
    },
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Int(i64),
    Float(f64),
    String(String),
    Char(char),
    True,
    False,
    Null,
    Dot,
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    Comma,
    Not,
    Minus,
    Plus,
    Star,
    Slash,
    Percent,
    EqualEqual,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    AndAnd,
    OrOr,
    End,
}

pub(crate) fn parse(input: &str) -> Result<Expr, String> {
    if input.trim().is_empty() {
        return Err("expressão vazia".to_owned());
    }
    if input.chars().count() > 500 {
        return Err("expressão longa demais (máximo 500 caracteres)".to_owned());
    }
    let tokens = tokenize(input)?;
    let mut parser = Parser {
        tokens,
        index: 0,
        depth: 0,
    };
    let expr = parser.parse_or()?;
    if parser.peek() != &Token::End {
        return Err(format!(
            "token inesperado após a expressão: {:?}",
            parser.peek()
        ));
    }
    Ok(expr)
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let characters: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character.is_whitespace() {
            index += 1;
            continue;
        }
        match character {
            '(' => {
                tokens.push(Token::LeftParen);
                index += 1;
            }
            ')' => {
                tokens.push(Token::RightParen);
                index += 1;
            }
            '[' => {
                tokens.push(Token::LeftBracket);
                index += 1;
            }
            ']' => {
                tokens.push(Token::RightBracket);
                index += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                index += 1;
            }
            '.' => {
                tokens.push(Token::Dot);
                index += 1;
            }
            '+' => {
                tokens.push(Token::Plus);
                index += 1;
            }
            '-' => {
                tokens.push(Token::Minus);
                index += 1;
            }
            '*' => {
                tokens.push(Token::Star);
                index += 1;
            }
            '/' => {
                tokens.push(Token::Slash);
                index += 1;
            }
            '%' => {
                tokens.push(Token::Percent);
                index += 1;
            }
            '!' => {
                if characters.get(index + 1) == Some(&'=') {
                    tokens.push(Token::NotEqual);
                    index += 2;
                } else {
                    tokens.push(Token::Not);
                    index += 1;
                }
            }
            '=' => {
                if characters.get(index + 1) == Some(&'=') {
                    tokens.push(Token::EqualEqual);
                    index += 2;
                } else {
                    return Err("use `==` para comparação".to_owned());
                }
            }
            '<' => {
                if characters.get(index + 1) == Some(&'=') {
                    tokens.push(Token::LessEqual);
                    index += 2;
                } else {
                    tokens.push(Token::Less);
                    index += 1;
                }
            }
            '>' => {
                if characters.get(index + 1) == Some(&'=') {
                    tokens.push(Token::GreaterEqual);
                    index += 2;
                } else {
                    tokens.push(Token::Greater);
                    index += 1;
                }
            }
            '&' => {
                if characters.get(index + 1) == Some(&'&') {
                    tokens.push(Token::AndAnd);
                    index += 2;
                } else {
                    return Err("use `&&` para conjunção".to_owned());
                }
            }
            '|' => {
                if characters.get(index + 1) == Some(&'|') {
                    tokens.push(Token::OrOr);
                    index += 2;
                } else {
                    return Err("use `||` para disjunção".to_owned());
                }
            }
            '"' => {
                let (value, next) = lex_string(&characters, index + 1, '"')?;
                tokens.push(Token::String(value));
                index = next;
            }
            '\'' => {
                let (value, next) = lex_string(&characters, index + 1, '\'')?;
                let mut chars = value.chars();
                let Some(character) = chars.next() else {
                    return Err("literal de char vazio".to_owned());
                };
                if chars.next().is_some() {
                    return Err("literal de char deve ter um caractere".to_owned());
                }
                tokens.push(Token::Char(character));
                index = next;
            }
            character if character.is_ascii_digit() => {
                let start = index;
                while index < characters.len()
                    && (characters[index].is_ascii_digit() || characters[index] == '.')
                {
                    index += 1;
                }
                let text: String = characters[start..index].iter().collect();
                if let Ok(value) = text.parse::<i64>() {
                    tokens.push(Token::Int(value));
                } else if let Ok(value) = text.parse::<f64>() {
                    tokens.push(Token::Float(value));
                } else {
                    return Err(format!("número inválido: {text}"));
                }
            }
            character if is_identifier_start(character) => {
                let start = index;
                index += 1;
                while index < characters.len() && is_identifier_part(characters[index]) {
                    index += 1;
                }
                let text: String = characters[start..index].iter().collect();
                tokens.push(match text.as_str() {
                    "true" => Token::True,
                    "false" => Token::False,
                    "null" => Token::Null,
                    _ => Token::Ident(text),
                });
            }
            other => return Err(format!("caractere inesperado na expressão: {other}")),
        }
    }
    tokens.push(Token::End);
    Ok(tokens)
}

fn lex_string(
    characters: &[char],
    mut index: usize,
    quote: char,
) -> Result<(String, usize), String> {
    let mut value = String::new();
    while index < characters.len() {
        let character = characters[index];
        if character == quote {
            return Ok((value, index + 1));
        }
        if character == '\\' {
            index += 1;
            let Some(escaped) = characters.get(index).copied() else {
                return Err("escape incompleto na string".to_owned());
            };
            value.push(match escaped {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '"' => '"',
                '\'' => '\'',
                other => other,
            });
            index += 1;
            continue;
        }
        value.push(character);
        index += 1;
    }
    Err("string não terminada".to_owned())
}

fn is_identifier_start(character: char) -> bool {
    character.is_alphabetic() || character == '_' || character == '$'
}

fn is_identifier_part(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        self.tokens.get(self.index).unwrap_or(&Token::End)
    }

    fn next(&mut self) -> Token {
        let token = self.peek().clone();
        self.index += 1;
        token
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.peek() == &Token::OrOr {
            self.next();
            let right = self.parse_and()?;
            left = Expr::Binary(BinaryOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_equality()?;
        while self.peek() == &Token::AndAnd {
            self.next();
            let right = self.parse_equality()?;
            left = Expr::Binary(BinaryOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_relational()?;
        loop {
            let operator = match self.peek() {
                Token::EqualEqual => BinaryOp::Equal,
                Token::NotEqual => BinaryOp::NotEqual,
                _ => break,
            };
            self.next();
            let right = self.parse_relational()?;
            left = Expr::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_relational(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_additive()?;
        loop {
            let operator = match self.peek() {
                Token::Less => BinaryOp::Less,
                Token::LessEqual => BinaryOp::LessEqual,
                Token::Greater => BinaryOp::Greater,
                Token::GreaterEqual => BinaryOp::GreaterEqual,
                _ => break,
            };
            self.next();
            let right = self.parse_additive()?;
            left = Expr::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let operator = match self.peek() {
                Token::Plus => BinaryOp::Add,
                Token::Minus => BinaryOp::Subtract,
                _ => break,
            };
            self.next();
            let right = self.parse_multiplicative()?;
            left = Expr::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            let operator = match self.peek() {
                Token::Star => BinaryOp::Multiply,
                Token::Slash => BinaryOp::Divide,
                Token::Percent => BinaryOp::Remainder,
                _ => break,
            };
            self.next();
            let right = self.parse_unary()?;
            left = Expr::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        let operator = match self.peek() {
            Token::Not => Some(UnaryOp::Not),
            Token::Minus => Some(UnaryOp::Negate),
            Token::Plus => Some(UnaryOp::Plus),
            _ => None,
        };
        if let Some(operator) = operator {
            self.next();
            let value = self.parse_unary()?;
            return Ok(Expr::Unary(operator, Box::new(value)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut value = self.parse_primary()?;
        // Direct call: `name(args)` — receiver is the implicit `this`.
        if self.peek() == &Token::LeftParen {
            let Expr::Name(name) = value else {
                return Err("chamada de método exige um nome ou `objeto.método`".to_owned());
            };
            let args = self.parse_args()?;
            value = Expr::Call {
                receiver: None,
                method: name,
                args,
            };
        }
        loop {
            match self.peek() {
                Token::Dot => {
                    self.next();
                    let Token::Ident(field) = self.next() else {
                        return Err("esperado nome de campo após `.`".to_owned());
                    };
                    if self.peek() == &Token::LeftParen {
                        let args = self.parse_args()?;
                        value = Expr::Call {
                            receiver: Some(Box::new(value)),
                            method: field,
                            args,
                        };
                    } else {
                        value = Expr::Field(Box::new(value), field);
                    }
                }
                Token::LeftBracket => {
                    self.next();
                    let index = self.parse_or()?;
                    if self.next() != Token::RightBracket {
                        return Err("colchete não fechado".to_owned());
                    }
                    value = Expr::Index(Box::new(value), Box::new(index));
                }
                _ => break,
            }
        }
        Ok(value)
    }

    /// Consume `(`, a comma-separated argument list, and `)`.
    fn parse_args(&mut self) -> Result<Vec<Expr>, String> {
        if self.next() != Token::LeftParen {
            return Err("esperado `(` nos argumentos".to_owned());
        }
        if self.peek() == &Token::RightParen {
            self.next();
            return Ok(Vec::new());
        }
        let mut args = Vec::new();
        loop {
            args.push(self.parse_or()?);
            if args.len() > 8 {
                return Err("máximo de 8 argumentos na chamada".to_owned());
            }
            match self.next() {
                Token::Comma => continue,
                Token::RightParen => break,
                other => {
                    return Err(format!(
                        "esperado `,` ou `)` nos argumentos, encontrei {other:?}"
                    ));
                }
            }
        }
        Ok(args)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        self.depth += 1;
        if self.depth > 64 {
            return Err("expressão aninhada demais".to_owned());
        }
        let result = match self.next() {
            Token::Int(value) => Expr::Literal(Literal::Int(value)),
            Token::Float(value) => Expr::Literal(Literal::Float(value)),
            Token::String(value) => Expr::Literal(Literal::String(value)),
            Token::Char(value) => Expr::Literal(Literal::Char(value)),
            Token::True => Expr::Literal(Literal::Bool(true)),
            Token::False => Expr::Literal(Literal::Bool(false)),
            Token::Null => Expr::Literal(Literal::Null),
            Token::Ident(name) => Expr::Name(name),
            Token::LeftParen => {
                let value = self.parse_or()?;
                if self.next() != Token::RightParen {
                    return Err("parêntese não fechado".to_owned());
                }
                value
            }
            other => return Err(format!("expressão inválida perto de {other:?}")),
        };
        self.depth -= 1;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_precedence() {
        let expr = parse("i + 2 * 3 == 7 && !done").unwrap();
        assert_eq!(
            expr,
            Expr::Binary(
                BinaryOp::And,
                Box::new(Expr::Binary(
                    BinaryOp::Equal,
                    Box::new(Expr::Binary(
                        BinaryOp::Add,
                        Box::new(Expr::Name("i".to_owned())),
                        Box::new(Expr::Binary(
                            BinaryOp::Multiply,
                            Box::new(Expr::Literal(Literal::Int(2))),
                            Box::new(Expr::Literal(Literal::Int(3))),
                        )),
                    )),
                    Box::new(Expr::Literal(Literal::Int(7))),
                )),
                Box::new(Expr::Unary(
                    UnaryOp::Not,
                    Box::new(Expr::Name("done".to_owned())),
                )),
            )
        );
    }

    #[test]
    fn parses_field_access_and_literals() {
        let expr = parse("this.user.name == \"Ada\"").unwrap();
        assert!(matches!(expr, Expr::Binary(BinaryOp::Equal, _, _)));
        assert!(parse("'a'").is_ok());
        assert!(parse("null").is_ok());
        assert!(parse("1.5").is_ok());
    }

    #[test]
    fn parses_calls_and_index() {
        assert!(matches!(parse("list.get(0)").unwrap(), Expr::Call { .. }));
        assert!(matches!(parse("nums[i + 1]").unwrap(), Expr::Index(_, _)));
        assert!(matches!(
            parse("user.getName().length()").unwrap(),
            Expr::Call { .. }
        ));
        assert!(matches!(
            parse("Math.max(a, b)").unwrap(),
            Expr::Call { .. }
        ));
        assert!(parse("size()").is_ok());
    }

    #[test]
    fn rejects_bad_syntax() {
        assert!(parse("i = 1").is_err());
        assert!(parse("i &&").is_err());
        assert!(parse("").is_err());
        assert!(parse("a[0").is_err());
        assert!(parse("a(b,)").is_err());
        assert!(parse("(int) x").is_err());
    }
}
