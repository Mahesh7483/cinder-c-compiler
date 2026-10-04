//! Evaluation of literal spellings: integer and floating constants, character
//! constants and string literals (with all C11 escape sequences).
//!
//! Shared by the preprocessor (`#if` expressions) and the parser.

/// A problem found while evaluating a literal. `offset`/`len` index the
/// literal's spelling so callers can point a caret at the offending part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LitIssue {
    pub offset: usize,
    pub len: usize,
    pub message: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntLit {
    pub value: u64,
    pub unsigned: bool,
    /// 0 = int, 1 = long, 2 = long long.
    pub longs: u8,
    /// True for decimal constants (affects the type-selection rules of C11 6.4.4.1).
    pub decimal: bool,
}

/// Parse an integer constant spelling such as `0x1FuL`, `017`, `1'000` (no
/// separators in C11), `42LL`.
pub fn parse_int(text: &str) -> Result<IntLit, String> {
    let b = text.as_bytes();
    let (radix, mut i) = if b.len() >= 2 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'X') {
        (16, 2)
    } else if b.len() >= 2 && b[0] == b'0' && (b[1] == b'b' || b[1] == b'B') {
        (2, 2)
    } else if b.len() >= 2 && b[0] == b'0' {
        (8, 1)
    } else {
        (10, 0)
    };
    let digits_start = i;
    let mut value: u128 = 0;
    let mut overflow = false;
    while i < b.len() {
        let d = match b[i] {
            b'0'..=b'9' => (b[i] - b'0') as u32,
            b'a'..=b'f' if radix == 16 => (b[i] - b'a') as u32 + 10,
            b'A'..=b'F' if radix == 16 => (b[i] - b'A') as u32 + 10,
            _ => break,
        };
        if d >= radix {
            return Err(format!("invalid digit '{}' in {} constant", b[i] as char, radix_name(radix)));
        }
        value = value * radix as u128 + d as u128;
        if value > u64::MAX as u128 {
            overflow = true;
            value &= u64::MAX as u128;
        }
        i += 1;
    }
    if i == digits_start && radix != 8 {
        return Err(format!("invalid suffix '{}' on integer constant", &text[i..]));
    }
    let suffix = &text[i..];
    let (mut unsigned, mut longs) = (false, 0u8);
    let sb = suffix.as_bytes();
    let mut k = 0;
    while k < sb.len() {
        match sb[k] {
            b'u' | b'U' if !unsigned => unsigned = true,
            b'l' | b'L' if longs == 0 => {
                if k + 1 < sb.len() && sb[k + 1] == sb[k] {
                    longs = 2;
                    k += 1;
                } else {
                    longs = 1;
                }
            }
            _ => return Err(format!("invalid suffix '{}' on integer constant", suffix)),
        }
        k += 1;
    }
    if overflow {
        return Err("integer literal is too large to be represented in any integer type".to_string());
    }
    Ok(IntLit { value: value as u64, unsigned, longs, decimal: radix == 10 })
}

fn radix_name(r: u32) -> &'static str {
    match r {
        2 => "binary",
        8 => "octal",
        16 => "hexadecimal",
        _ => "decimal",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FloatLit {
    /// The numeric part with any suffix removed.
    pub body: String,
    pub is_float: bool,
    pub is_long_double: bool,
    pub hex: bool,
}

impl FloatLit {
    pub fn value_f64(&self) -> f64 {
        if self.hex {
            parse_hex_float(&self.body).unwrap_or(0.0)
        } else {
            self.body.parse::<f64>().unwrap_or(0.0)
        }
    }

    /// Parsed directly as `f32` so there is no double rounding.
    pub fn value_f32(&self) -> f32 {
        if self.hex {
            parse_hex_float(&self.body).unwrap_or(0.0) as f32
        } else {
            self.body.parse::<f32>().unwrap_or(0.0)
        }
    }
}

/// True if a pp-number spelling is a floating constant (rather than an integer).
pub fn is_float_spelling(text: &str) -> bool {
    let b = text.as_bytes();
    if b.len() >= 2 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'X') {
        text.contains('.') || text.contains('p') || text.contains('P')
    } else {
        text.contains('.') || text.contains('e') || text.contains('E')
    }
}

pub fn parse_float(text: &str) -> Result<FloatLit, String> {
    let hex = text.len() >= 2 && (text.starts_with("0x") || text.starts_with("0X"));
    let mut end = text.len();
    let mut is_float = false;
    let mut is_long_double = false;
    if let Some(last) = text.chars().last() {
        match last {
            'f' | 'F' if !hex || text.contains(['p', 'P']) => {
                is_float = true;
                end -= 1;
            }
            'l' | 'L' => {
                is_long_double = true;
                end -= 1;
            }
            _ => {}
        }
    }
    let body = &text[..end];
    let valid = if hex {
        parse_hex_float(body).is_some()
    } else {
        !body.is_empty()
            && body.parse::<f64>().is_ok()
            && body.bytes().all(|c| c.is_ascii_digit() || b".eE+-".contains(&c))
    };
    if !valid {
        return Err(format!("invalid suffix or malformed floating constant '{}'", text));
    }
    Ok(FloatLit { body: body.to_string(), is_float, is_long_double, hex })
}

/// Parse a C99 hexadecimal floating constant body such as `0x1.8p3`.
fn parse_hex_float(s: &str) -> Option<f64> {
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    // A hex float requires an exponent.
    let p = s.find(['p', 'P'])?;
    let (mant, exp) = (&s[..p], s[p + 1..].parse::<i32>().ok()?);
    let (ip, fp) = match mant.find('.') {
        Some(p) => (&mant[..p], &mant[p + 1..]),
        None => (mant, ""),
    };
    if ip.is_empty() && fp.is_empty() {
        return None;
    }
    let mut v = 0f64;
    for c in ip.chars() {
        v = v * 16.0 + c.to_digit(16)? as f64;
    }
    let mut scale = 1.0 / 16.0;
    for c in fp.chars() {
        v += c.to_digit(16)? as f64 * scale;
        scale /= 16.0;
    }
    Some(v * 2f64.powi(exp))
}

// ───────────────────────── character / string literals ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrKind {
    /// No prefix: `char` elements.
    Plain,
    /// `u8"..."`.
    Utf8,
    /// `u"..."`: `char16_t` elements.
    Utf16,
    /// `U"..."`: `char32_t` elements.
    Utf32,
    /// `L"..."`: `wchar_t` (32-bit on Linux) elements.
    Wide,
}

/// Split a literal spelling into (kind, body between the quotes, offset of the
/// body within the spelling). `quote` is `"` or `'`.
fn split_literal(text: &str, quote: u8) -> Option<(StrKind, &str, usize)> {
    let b = text.as_bytes();
    let (kind, start) = if text.starts_with("u8") && b.get(2) == Some(&quote) {
        (StrKind::Utf8, 2)
    } else if text.starts_with('u') && b.get(1) == Some(&quote) {
        (StrKind::Utf16, 1)
    } else if text.starts_with('U') && b.get(1) == Some(&quote) {
        (StrKind::Utf32, 1)
    } else if text.starts_with('L') && b.get(1) == Some(&quote) {
        (StrKind::Wide, 1)
    } else if b.first() == Some(&quote) {
        (StrKind::Plain, 0)
    } else {
        return None;
    };
    if b.len() < start + 2 || b[b.len() - 1] != quote {
        return None;
    }
    Some((kind, &text[start + 1..text.len() - 1], start + 1))
}

/// One decoded element of a literal: either a raw byte value from an escape
/// (`\xHH`, octal) or a Unicode scalar.
enum Elem {
    Byte(u32),
    Char(char),
}

fn decode_elems(body: &str, base: usize, issues: &mut Vec<LitIssue>) -> Vec<Elem> {
    let mut out = Vec::new();
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            let ch = body[i..].chars().next().unwrap();
            out.push(Elem::Char(ch));
            i += ch.len_utf8();
            continue;
        }
        let esc_start = i;
        i += 1;
        if i >= b.len() {
            issues.push(LitIssue {
                offset: base + esc_start,
                len: 1,
                message: "incomplete escape sequence".into(),
                is_error: true,
            });
            break;
        }
        let c = b[i];
        i += 1;
        let simple = |v: u8| Elem::Byte(v as u32);
        match c {
            b'n' => out.push(simple(b'\n')),
            b't' => out.push(simple(b'\t')),
            b'r' => out.push(simple(b'\r')),
            b'a' => out.push(simple(7)),
            b'b' => out.push(simple(8)),
            b'f' => out.push(simple(12)),
            b'v' => out.push(simple(11)),
            b'e' | b'E' => out.push(simple(27)),
            b'\\' => out.push(simple(b'\\')),
            b'\'' => out.push(simple(b'\'')),
            b'"' => out.push(simple(b'"')),
            b'?' => out.push(simple(b'?')),
            b'0'..=b'7' => {
                let mut v = (c - b'0') as u32;
                let mut n = 1;
                while n < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    v = v * 8 + (b[i] - b'0') as u32;
                    i += 1;
                    n += 1;
                }
                out.push(Elem::Byte(v));
            }
            b'x' => {
                let st = i;
                let mut v: u32 = 0;
                while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                    v = v.wrapping_mul(16).wrapping_add((b[i] as char).to_digit(16).unwrap());
                    i += 1;
                }
                if i == st {
                    issues.push(LitIssue {
                        offset: base + esc_start,
                        len: 2,
                        message: "\\x used with no following hex digits".into(),
                        is_error: true,
                    });
                }
                out.push(Elem::Byte(v));
            }
            b'u' | b'U' => {
                let want = if c == b'u' { 4 } else { 8 };
                let st = i;
                while i < b.len() && i - st < want && (b[i] as char).is_ascii_hexdigit() {
                    i += 1;
                }
                if i - st != want {
                    issues.push(LitIssue {
                        offset: base + esc_start,
                        len: i - esc_start,
                        message: "incomplete universal character name".into(),
                        is_error: true,
                    });
                    continue;
                }
                let cp = u32::from_str_radix(&body[st..i], 16).unwrap();
                match char::from_u32(cp) {
                    Some(ch) => out.push(Elem::Char(ch)),
                    None => issues.push(LitIssue {
                        offset: base + esc_start,
                        len: i - esc_start,
                        message: "invalid universal character".into(),
                        is_error: true,
                    }),
                }
            }
            other => {
                issues.push(LitIssue {
                    offset: base + esc_start,
                    len: 2,
                    message: format!("unknown escape sequence '\\{}'", other as char),
                    is_error: false,
                });
                out.push(Elem::Byte(other as u32));
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharLit {
    pub kind: StrKind,
    /// Value as the *type* the constant has: `int` for plain (sign-extended
    /// from `char`), `wchar_t`/`char16_t`/`char32_t` otherwise.
    pub value: i64,
    pub multichar: bool,
}

pub fn parse_char(text: &str, issues: &mut Vec<LitIssue>) -> Option<CharLit> {
    let (kind, body, base) = split_literal(text, b'\'')?;
    let elems = decode_elems(body, base, issues);
    if elems.is_empty() {
        issues.push(LitIssue {
            offset: 0,
            len: text.len(),
            message: "empty character constant".into(),
            is_error: true,
        });
        return Some(CharLit { kind, value: 0, multichar: false });
    }
    let multichar = elems.len() > 1;
    let value = match kind {
        StrKind::Plain | StrKind::Utf8 => {
            // Each element contributes a byte (UTF-8 for non-ASCII chars); a
            // multi-character constant packs bytes big-endian into an int.
            let mut bytes: Vec<u8> = Vec::new();
            for e in &elems {
                match e {
                    Elem::Byte(v) => bytes.push(*v as u8),
                    Elem::Char(c) => {
                        let mut buf = [0u8; 4];
                        bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            if bytes.len() == 1 {
                bytes[0] as i8 as i64
            } else {
                let mut v: u32 = 0;
                for &x in bytes.iter().take(4) {
                    v = (v << 8) | x as u32;
                }
                v as i32 as i64
            }
        }
        StrKind::Wide => elems_first_code(&elems) as i32 as i64,
        StrKind::Utf16 => (elems_first_code(&elems) & 0xFFFF) as i64,
        StrKind::Utf32 => elems_first_code(&elems) as i64,
    };
    Some(CharLit { kind, value, multichar })
}

fn elems_first_code(elems: &[Elem]) -> u32 {
    match &elems[0] {
        Elem::Byte(v) => *v,
        Elem::Char(c) => *c as u32,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrLit {
    pub kind: StrKind,
    /// Code units, *without* the terminating NUL. For `Plain`/`Utf8` each is a byte.
    pub units: Vec<u32>,
}

pub fn parse_string(text: &str, issues: &mut Vec<LitIssue>) -> Option<StrLit> {
    let (kind, body, base) = split_literal(text, b'"')?;
    let elems = decode_elems(body, base, issues);
    let mut units: Vec<u32> = Vec::with_capacity(elems.len());
    for e in elems {
        match (kind, e) {
            (StrKind::Plain | StrKind::Utf8, Elem::Byte(v)) => units.push(v & 0xFF),
            (StrKind::Plain | StrKind::Utf8, Elem::Char(c)) => {
                let mut buf = [0u8; 4];
                units.extend(c.encode_utf8(&mut buf).bytes().map(|x| x as u32));
            }
            (StrKind::Wide | StrKind::Utf32, Elem::Byte(v)) => units.push(v),
            (StrKind::Wide | StrKind::Utf32, Elem::Char(c)) => units.push(c as u32),
            (StrKind::Utf16, Elem::Byte(v)) => units.push(v & 0xFFFF),
            (StrKind::Utf16, Elem::Char(c)) => {
                let mut buf = [0u16; 2];
                units.extend(c.encode_utf16(&mut buf).iter().map(|&x| x as u32));
            }
        }
    }
    Some(StrLit { kind, units })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers() {
        let v = parse_int("42").unwrap();
        assert_eq!((v.value, v.unsigned, v.longs, v.decimal), (42, false, 0, true));
        let v = parse_int("0x1Fu").unwrap();
        assert_eq!((v.value, v.unsigned, v.decimal), (31, true, false));
        let v = parse_int("017").unwrap();
        assert_eq!((v.value, v.decimal), (15, false));
        let v = parse_int("0b101").unwrap();
        assert_eq!(v.value, 5);
        let v = parse_int("18446744073709551615ULL").unwrap();
        assert_eq!((v.value, v.unsigned, v.longs), (u64::MAX, true, 2));
        let v = parse_int("0").unwrap();
        assert_eq!(v.value, 0);
        let v = parse_int("5lu").unwrap();
        assert_eq!((v.unsigned, v.longs), (true, 1));
    }

    #[test]
    fn integer_errors() {
        assert!(parse_int("09").is_err());
        assert!(parse_int("0x").is_err());
        assert!(parse_int("12abc").is_err());
        assert!(parse_int("1lul").is_err());
        assert!(parse_int("18446744073709551616").is_err());
        assert!(parse_int("1lL").is_err());
    }

    #[test]
    fn floats() {
        assert!(is_float_spelling("1.5"));
        assert!(is_float_spelling("1e10"));
        assert!(!is_float_spelling("0x1e10"));
        assert!(is_float_spelling("0x1p3"));
        let f = parse_float("1.5f").unwrap();
        assert!(f.is_float);
        assert_eq!(f.value_f32(), 1.5);
        let f = parse_float(".25").unwrap();
        assert_eq!(f.value_f64(), 0.25);
        let f = parse_float("1.").unwrap();
        assert_eq!(f.value_f64(), 1.0);
        let f = parse_float("0x1.8p1").unwrap();
        assert_eq!(f.value_f64(), 3.0);
        let f = parse_float("1e-3").unwrap();
        assert_eq!(f.value_f64(), 0.001);
        let f = parse_float("2.0L").unwrap();
        assert!(f.is_long_double);
        assert!(parse_float("1.5q").is_err());
    }

    #[test]
    fn chars() {
        let mut iss = Vec::new();
        assert_eq!(parse_char("'a'", &mut iss).unwrap().value, 97);
        assert_eq!(parse_char("'\\n'", &mut iss).unwrap().value, 10);
        assert_eq!(parse_char("'\\0'", &mut iss).unwrap().value, 0);
        assert_eq!(parse_char("'\\x41'", &mut iss).unwrap().value, 65);
        assert_eq!(parse_char("'\\101'", &mut iss).unwrap().value, 65);
        assert_eq!(parse_char("'\\377'", &mut iss).unwrap().value, -1);
        assert_eq!(parse_char("L'a'", &mut iss).unwrap().value, 97);
        assert_eq!(parse_char("u'\\u00e9'", &mut iss).unwrap().value, 0xe9);
        let m = parse_char("'ab'", &mut iss).unwrap();
        assert_eq!(m.value, 0x6162);
        assert!(m.multichar);
        assert!(iss.is_empty());
        let mut iss = Vec::new();
        parse_char("''", &mut iss);
        assert!(iss[0].is_error);
    }

    #[test]
    fn strings() {
        let mut iss = Vec::new();
        let s = parse_string("\"hi\\n\"", &mut iss).unwrap();
        assert_eq!(s.units, vec![104, 105, 10]);
        let s = parse_string("\"\\x41\\102\\0\"", &mut iss).unwrap();
        assert_eq!(s.units, vec![65, 66, 0]);
        let s = parse_string("\"é\"", &mut iss).unwrap();
        assert_eq!(s.units, vec![0xC3, 0xA9]);
        let s = parse_string("L\"é\"", &mut iss).unwrap();
        assert_eq!((s.kind, s.units), (StrKind::Wide, vec![0xE9]));
        let s = parse_string("u\"\\U0001F600\"", &mut iss).unwrap();
        assert_eq!(s.units, vec![0xD83D, 0xDE00]);
        let s = parse_string("u8\"a\"", &mut iss).unwrap();
        assert_eq!(s.kind, StrKind::Utf8);
        assert!(iss.is_empty());
    }

    #[test]
    fn unknown_escape_is_a_warning_not_an_error() {
        let mut iss = Vec::new();
        let s = parse_string("\"a\\qb\"", &mut iss).unwrap();
        assert_eq!(s.units, vec![97, b'q' as u32, 98]);
        assert_eq!(iss.len(), 1);
        assert!(!iss[0].is_error);
        assert_eq!(iss[0].offset, 2);
    }
}
