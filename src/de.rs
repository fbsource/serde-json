//! Deserialize JSON data to a Rust data structure.

use crate::error::{Error, ErrorCode, Result};
use crate::lib::str::FromStr;
use crate::lib::*;
use crate::number::Number;
use crate::read::{self, Fused, Reference};
use serde::de::{self, Expected, Unexpected};
use serde::{forward_to_deserialize_any, serde_if_integer128};

#[cfg(feature = "arbitrary_precision")]
use crate::number::NumberDeserializer;

pub use crate::read::{Read, SliceRead, StrRead};

#[cfg(feature = "std")]
pub use crate::read::IoRead;

//////////////////////////////////////////////////////////////////////////////

/// A structure that deserializes JSON into Rust values.
pub struct Deserializer<R> {
    read: R,
    scratch: Vec<u8>,
    remaining_depth: u8,
    #[cfg(feature = "unbounded_depth")]
    disable_recursion_limit: bool,
}

impl<'de, R> Deserializer<R>
where
    R: read::Read<'de>,
{
    /// Create a JSON deserializer from one of the possible serde_json input
    /// sources.
    ///
    /// Typically it is more convenient to use one of these methods instead:
    ///
    ///   - Deserializer::from_str
    ///   - Deserializer::from_bytes
    ///   - Deserializer::from_reader
    pub fn new(read: R) -> Self {
        #[cfg(not(feature = "unbounded_depth"))]
        {
            Deserializer {
                read: read,
                scratch: Vec::new(),
                remaining_depth: 128,
            }
        }

        #[cfg(feature = "unbounded_depth")]
        {
            Deserializer {
                read: read,
                scratch: Vec::new(),
                remaining_depth: 128,
                disable_recursion_limit: false,
            }
        }
    }
}

#[cfg(feature = "std")]
impl<R> Deserializer<read::IoRead<R>>
where
    R: crate::io::Read,
{
    /// Creates a JSON deserializer from an `io::Read`.
    ///
    /// Reader-based deserializers do not support deserializing borrowed types
    /// like `&str`, since the `std::io::Read` trait has no non-copying methods
    /// -- everything it does involves copying bytes out of the data source.
    pub fn from_reader(reader: R) -> Self {
        Deserializer::new(read::IoRead::new(reader))
    }
}

impl<'a> Deserializer<read::SliceRead<'a>> {
    /// Creates a JSON deserializer from a `&[u8]`.
    pub fn from_slice(bytes: &'a [u8]) -> Self {
        Deserializer::new(read::SliceRead::new(bytes))
    }
}

impl<'a> Deserializer<read::StrRead<'a>> {
    /// Creates a JSON deserializer from a `&str`.
    pub fn from_str(s: &'a str) -> Self {
        Deserializer::new(read::StrRead::new(s))
    }
}

macro_rules! overflow {
    ($a:ident * 10 + $b:ident, $c:expr) => {
        $a >= $c / 10 && ($a > $c / 10 || $b > $c % 10)
    };
}

pub(crate) enum ParserNumber {
    F64(f64),
    U64(u64),
    I64(i64),
    #[cfg(feature = "arbitrary_precision")]
    String(String),
}

impl ParserNumber {
    fn visit<'de, V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        match self {
            ParserNumber::F64(x) => visitor.visit_f64(x),
            ParserNumber::U64(x) => visitor.visit_u64(x),
            ParserNumber::I64(x) => visitor.visit_i64(x),
            #[cfg(feature = "arbitrary_precision")]
            ParserNumber::String(x) => visitor.visit_map(NumberDeserializer { number: x.into() }),
        }
    }

    fn invalid_type(self, exp: &dyn Expected) -> Error {
        match self {
            ParserNumber::F64(x) => de::Error::invalid_type(Unexpected::Float(x), exp),
            ParserNumber::U64(x) => de::Error::invalid_type(Unexpected::Unsigned(x), exp),
            ParserNumber::I64(x) => de::Error::invalid_type(Unexpected::Signed(x), exp),
            #[cfg(feature = "arbitrary_precision")]
            ParserNumber::String(_) => de::Error::invalid_type(Unexpected::Other("number"), exp),
        }
    }
}

impl<'de, R: Read<'de>> Deserializer<R> {
    /// The `Deserializer::end` method should be called after a value has been fully deserialized.
    /// This allows the `Deserializer` to validate that the input stream is at the end or that it
    /// only has trailing whitespace.
    pub fn end(&mut self) -> Result<()> {
        match tri!(self.parse_whitespace()) {
            Some(_) => Err(self.peek_error(ErrorCode::TrailingCharacters)),
            None => Ok(()),
        }
    }

    /// Turn a JSON deserializer into an iterator over values of type T.
    pub fn into_iter<T>(self) -> StreamDeserializer<'de, R, T>
    where
        T: de::Deserialize<'de>,
    {
        // This cannot be an implementation of std::iter::IntoIterator because
        // we need the caller to choose what T is.
        let offset = self.read.byte_offset();
        StreamDeserializer {
            de: self,
            offset: offset,
            failed: false,
            output: PhantomData,
            lifetime: PhantomData,
        }
    }

    /// Parse arbitrarily deep JSON structures without any consideration for
    /// overflowing the stack.
    ///
    /// You will want to provide some other way to protect against stack
    /// overflows, such as by wrapping your Deserializer in the dynamically
    /// growing stack adapter provided by the serde_stacker crate. Additionally
    /// you will need to be careful around other recursive operations on the
    /// parsed result which may overflow the stack after deserialization has
    /// completed, including, but not limited to, Display and Debug and Drop
    /// impls.
    ///
    /// *This method is only available if serde_json is built with the
    /// `"unbounded_depth"` feature.*
    ///
    /// # Examples
    ///
    /// ```
    /// use serde::Deserialize;
    /// use serde_json::Value;
    ///
    /// fn main() {
    ///     let mut json = String::new();
    ///     for _ in 0..10000 {
    ///         json = format!("[{}]", json);
    ///     }
    ///
    ///     let mut deserializer = serde_json::Deserializer::from_str(&json);
    ///     deserializer.disable_recursion_limit();
    ///     let deserializer = serde_stacker::Deserializer::new(&mut deserializer);
    ///     let value = Value::deserialize(deserializer).unwrap();
    ///
    ///     carefully_drop_nested_arrays(value);
    /// }
    ///
    /// fn carefully_drop_nested_arrays(value: Value) {
    ///     let mut stack = vec![value];
    ///     while let Some(value) = stack.pop() {
    ///         if let Value::Array(array) = value {
    ///             stack.extend(array);
    ///         }
    ///     }
    /// }
    /// ```
    #[cfg(feature = "unbounded_depth")]
    pub fn disable_recursion_limit(&mut self) {
        self.disable_recursion_limit = true;
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        self.read.peek()
    }

    fn peek_or_null(&mut self) -> Result<u8> {
        Ok(tri!(self.peek()).unwrap_or(b'\x00'))
    }

    fn eat_char(&mut self) {
        self.read.discard();
    }

    fn next_char(&mut self) -> Result<Option<u8>> {
        self.read.next()
    }

    fn next_char_or_null(&mut self) -> Result<u8> {
        Ok(tri!(self.next_char()).unwrap_or(b'\x00'))
    }

    /// Error caused by a byte from next_char().
    #[cold]
    fn error(&self, reason: ErrorCode) -> Error {
        let position = self.read.position();
        Error::syntax(reason, position.line, position.column)
    }

    /// Error caused by a byte from peek().
    #[cold]
    fn peek_error(&self, reason: ErrorCode) -> Error {
        let position = self.read.peek_position();
        Error::syntax(reason, position.line, position.column)
    }

    /// Returns the first non-whitespace byte without consuming it, or `None` if
    /// EOF is encountered.
    fn parse_whitespace(&mut self) -> Result<Option<u8>> {
        loop {
            match tri!(self.peek()) {
                Some(b' ') | Some(b'\n') | Some(b'\t') | Some(b'\r') => {
                    self.eat_char();
                }
                other => {
                    return Ok(other);
                }
            }
        }
    }

    #[cold]
    fn peek_invalid_type(&mut self, exp: &dyn Expected) -> Error {
        let err = match self.peek_or_null().unwrap_or(b'\x00') {
            b'n' => {
                self.eat_char();
                if let Err(err) = self.parse_ident(b"ull") {
                    return err;
                }
                de::Error::invalid_type(Unexpected::Unit, exp)
            }
            b't' => {
                self.eat_char();
                if let Err(err) = self.parse_ident(b"rue") {
                    return err;
                }
                de::Error::invalid_type(Unexpected::Bool(true), exp)
            }
            b'f' => {
                self.eat_char();
                if let Err(err) = self.parse_ident(b"alse") {
                    return err;
                }
                de::Error::invalid_type(Unexpected::Bool(false), exp)
            }
            b'-' => {
                self.eat_char();
                match self.parse_any_number(false) {
                    Ok(n) => n.invalid_type(exp),
                    Err(err) => return err,
                }
            }
            b'0'..=b'9' => match self.parse_any_number(true) {
                Ok(n) => n.invalid_type(exp),
                Err(err) => return err,
            },
            b'"' => {
                self.eat_char();
                self.scratch.clear();
                match self.read.parse_str(&mut self.scratch) {
                    Ok(s) => de::Error::invalid_type(Unexpected::Str(&s), exp),
                    Err(err) => return err,
                }
            }
            b'[' => de::Error::invalid_type(Unexpected::Seq, exp),
            b'{' => de::Error::invalid_type(Unexpected::Map, exp),
            _ => self.peek_error(ErrorCode::ExpectedSomeValue),
        };

        self.fix_position(err)
    }

    fn deserialize_prim_number<V>(&mut self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let peek = match tri!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'-' => {
                self.eat_char();
                tri!(self.parse_integer(false)).visit(visitor)
            }
            b'0'..=b'9' => tri!(self.parse_integer(true)).visit(visitor),
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    serde_if_integer128! {
        fn scan_integer128(&mut self, buf: &mut String) -> Result<()> {
            match tri!(self.next_char_or_null()) {
                b'0' => {
                    buf.push('0');
                    // There can be only one leading '0'.
                    match tri!(self.peek_or_null()) {
                        b'0'..=b'9' => {
                            Err(self.peek_error(ErrorCode::InvalidNumber))
                        }
                        _ => Ok(()),
                    }
                }
                c @ b'1'..=b'9' => {
                    buf.push(c as char);
                    while let c @ b'0'..=b'9' = tri!(self.peek_or_null()) {
                        self.eat_char();
                        buf.push(c as char);
                    }
                    Ok(())
                }
                _ => {
                    Err(self.error(ErrorCode::InvalidNumber))
                }
            }
        }
    }

    #[cold]
    fn fix_position(&self, err: Error) -> Error {
        err.fix_position(move |code| self.error(code))
    }

    fn parse_ident(&mut self, ident: &[u8]) -> Result<()> {
        for expected in ident {
            match tri!(self.next_char()) {
                None => {
                    return Err(self.error(ErrorCode::EofWhileParsingValue));
                }
                Some(next) => {
                    if next != *expected {
                        return Err(self.error(ErrorCode::ExpectedSomeIdent));
                    }
                }
            }
        }

        Ok(())
    }

    fn parse_integer(&mut self, positive: bool) -> Result<ParserNumber> {
        let start = if positive {
            self.read.byte_offset()
        } else {
            self.read.byte_offset() - 1
        };
        let next = match tri!(self.next_char()) {
            Some(b) => b,
            None => {
                return Err(self.error(ErrorCode::EofWhileParsingValue));
            }
        };

        match next {
            b'0' => {
                // There can be only one leading '0'.
                match tri!(self.peek_or_null()) {
                    b'0'..=b'9' => Err(self.peek_error(ErrorCode::InvalidNumber)),
                    _ => self.parse_number(positive, 0, start),
                }
            }
            c @ b'1'..=b'9' => {
                let mut res = (c - b'0') as u64;

                loop {
                    match tri!(self.peek_or_null()) {
                        c @ b'0'..=b'9' => {
                            self.eat_char();
                            let digit = (c - b'0') as u64;

                            // We need to be careful with overflow. If we can, try to keep the
                            // number as a `u64` until we grow too large. At that point, switch to
                            // parsing the value as a `f64`.
                            if overflow!(res * 10 + digit, u64::max_value()) {
                                return Ok(ParserNumber::F64(tri!(self.parse_long_integer(
                                    positive, res, 1, // res * 10^1
                                    start,
                                ))));
                            }

                            res = res * 10 + digit;
                        }
                        _ => {
                            return self.parse_number(positive, res, start);
                        }
                    }
                }
            }
            _ => Err(self.error(ErrorCode::InvalidNumber)),
        }
    }

    fn parse_long_integer(
        &mut self,
        positive: bool,
        significand: u64,
        mut exponent: i32,
        start: usize,
    ) -> Result<f64> {
        loop {
            match tri!(self.peek_or_null()) {
                b'0'..=b'9' => {
                    self.eat_char();
                    // This could overflow... if your integer is gigabytes long.
                    // Ignore that possibility.
                    exponent += 1;
                }
                b'.' => {
                    return self.parse_decimal(positive, significand, exponent, start);
                }
                b'e' | b'E' => {
                    return self.parse_exponent(positive, significand, exponent, start);
                }
                _ => {
                    return self.f64_from_parts(positive, significand, exponent, start);
                }
            }
        }
    }

    fn parse_number(
        &mut self,
        positive: bool,
        significand: u64,
        start: usize,
    ) -> Result<ParserNumber> {
        Ok(match tri!(self.peek_or_null()) {
            b'.' => ParserNumber::F64(tri!(self.parse_decimal(positive, significand, 0, start))),
            b'e' | b'E' => {
                ParserNumber::F64(tri!(self.parse_exponent(positive, significand, 0, start)))
            }
            _ => {
                if positive {
                    ParserNumber::U64(significand)
                } else {
                    let neg = (significand as i64).wrapping_neg();

                    // Convert into a float if we underflow.
                    if neg > 0 {
                        ParserNumber::F64(-(significand as f64))
                    } else {
                        ParserNumber::I64(neg)
                    }
                }
            }
        })
    }

    fn parse_decimal(
        &mut self,
        positive: bool,
        mut significand: u64,
        mut exponent: i32,
        start: usize,
    ) -> Result<f64> {
        self.eat_char();

        let mut at_least_one_digit = false;
        while let c @ b'0'..=b'9' = tri!(self.peek_or_null()) {
            self.eat_char();
            let digit = (c - b'0') as u64;
            at_least_one_digit = true;

            if overflow!(significand * 10 + digit, u64::max_value()) {
                // The next multiply/add would overflow, so just ignore all
                // further digits.
                while let b'0'..=b'9' = tri!(self.peek_or_null()) {
                    self.eat_char();
                }
                break;
            }

            significand = significand * 10 + digit;
            exponent -= 1;
        }

        if !at_least_one_digit {
            match tri!(self.peek()) {
                Some(_) => return Err(self.peek_error(ErrorCode::InvalidNumber)),
                None => return Err(self.peek_error(ErrorCode::EofWhileParsingValue)),
            }
        }

        match tri!(self.peek_or_null()) {
            b'e' | b'E' => self.parse_exponent(positive, significand, exponent, start),
            _ => self.f64_from_parts(positive, significand, exponent, start),
        }
    }

    fn parse_exponent(
        &mut self,
        positive: bool,
        significand: u64,
        starting_exp: i32,
        start: usize,
    ) -> Result<f64> {
        self.eat_char();

        let positive_exp = match tri!(self.peek_or_null()) {
            b'+' => {
                self.eat_char();
                true
            }
            b'-' => {
                self.eat_char();
                false
            }
            _ => true,
        };

        let next = match tri!(self.next_char()) {
            Some(b) => b,
            None => {
                return Err(self.error(ErrorCode::EofWhileParsingValue));
            }
        };

        // Make sure a digit follows the exponent place.
        let mut exp = match next {
            c @ b'0'..=b'9' => (c - b'0') as i32,
            _ => {
                return Err(self.error(ErrorCode::InvalidNumber));
            }
        };

        while let c @ b'0'..=b'9' = tri!(self.peek_or_null()) {
            self.eat_char();
            let digit = (c - b'0') as i32;

            if overflow!(exp * 10 + digit, i32::max_value()) {
                return self.parse_exponent_overflow(positive, significand, positive_exp);
            }

            exp = exp * 10 + digit;
        }

        let final_exp = if positive_exp {
            starting_exp.saturating_add(exp)
        } else {
            starting_exp.saturating_sub(exp)
        };

        self.f64_from_parts(positive, significand, final_exp, start)
    }

    // This cold code should not be inlined into the middle of the hot
    // exponent-parsing loop above.
    #[cold]
    #[inline(never)]
    fn parse_exponent_overflow(
        &mut self,
        positive: bool,
        significand: u64,
        positive_exp: bool,
    ) -> Result<f64> {
        // Error instead of +/- infinity.
        if significand != 0 && positive_exp {
            return Err(self.error(ErrorCode::NumberOutOfRange));
        }

        while let b'0'..=b'9' = tri!(self.peek_or_null()) {
            self.eat_char();
        }
        Ok(if positive { 0.0 } else { -0.0 })
    }

    fn parse_any_signed_number(&mut self) -> Result<ParserNumber> {
        let peek = match tri!(self.peek()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'-' => {
                self.eat_char();
                self.parse_any_number(false)
            }
            b'0'..=b'9' => self.parse_any_number(true),
            _ => Err(self.peek_error(ErrorCode::InvalidNumber)),
        };

        let value = match tri!(self.peek()) {
            Some(_) => Err(self.peek_error(ErrorCode::InvalidNumber)),
            None => value,
        };

        match value {
            Ok(value) => Ok(value),
            // The de::Error impl creates errors with unknown line and column.
            // Fill in the position here by looking at the current index in the
            // input. There is no way to tell whether this should call `error`
            // or `peek_error` so pick the one that seems correct more often.
            // Worst case, the position is off by one character.
            Err(err) => Err(self.fix_position(err)),
        }
    }

    #[cfg(not(feature = "arbitrary_precision"))]
    fn parse_any_number(&mut self, positive: bool) -> Result<ParserNumber> {
        self.parse_integer(positive)
    }

    #[cfg(feature = "arbitrary_precision")]
    fn parse_any_number(&mut self, positive: bool) -> Result<ParserNumber> {
        let mut buf = String::with_capacity(16);
        if !positive {
            buf.push('-');
        }
        self.scan_integer(&mut buf)?;
        Ok(ParserNumber::String(buf))
    }

    #[cfg(feature = "arbitrary_precision")]
    fn scan_or_eof(&mut self, buf: &mut String) -> Result<u8> {
        match tri!(self.next_char()) {
            Some(b) => {
                buf.push(b as char);
                Ok(b)
            }
            None => Err(self.error(ErrorCode::EofWhileParsingValue)),
        }
    }

    #[cfg(feature = "arbitrary_precision")]
    fn scan_integer(&mut self, buf: &mut String) -> Result<()> {
        match tri!(self.scan_or_eof(buf)) {
            b'0' => {
                // There can be only one leading '0'.
                match tri!(self.peek_or_null()) {
                    b'0'..=b'9' => Err(self.peek_error(ErrorCode::InvalidNumber)),
                    _ => self.scan_number(buf),
                }
            }
            b'1'..=b'9' => loop {
                match tri!(self.peek_or_null()) {
                    c @ b'0'..=b'9' => {
                        self.eat_char();
                        buf.push(c as char);
                    }
                    _ => {
                        return self.scan_number(buf);
                    }
                }
            },
            _ => Err(self.error(ErrorCode::InvalidNumber)),
        }
    }

    #[cfg(feature = "arbitrary_precision")]
    fn scan_number(&mut self, buf: &mut String) -> Result<()> {
        match tri!(self.peek_or_null()) {
            b'.' => self.scan_decimal(buf),
            b'e' | b'E' => self.scan_exponent(buf),
            _ => Ok(()),
        }
    }

    #[cfg(feature = "arbitrary_precision")]
    fn scan_decimal(&mut self, buf: &mut String) -> Result<()> {
        self.eat_char();
        buf.push('.');

        let mut at_least_one_digit = false;
        while let c @ b'0'..=b'9' = tri!(self.peek_or_null()) {
            self.eat_char();
            buf.push(c as char);
            at_least_one_digit = true;
        }

        if !at_least_one_digit {
            match tri!(self.peek()) {
                Some(_) => return Err(self.peek_error(ErrorCode::InvalidNumber)),
                None => return Err(self.peek_error(ErrorCode::EofWhileParsingValue)),
            }
        }

        match tri!(self.peek_or_null()) {
            b'e' | b'E' => self.scan_exponent(buf),
            _ => Ok(()),
        }
    }

    #[cfg(feature = "arbitrary_precision")]
    fn scan_exponent(&mut self, buf: &mut String) -> Result<()> {
        self.eat_char();
        buf.push('e');

        match tri!(self.peek_or_null()) {
            b'+' => {
                self.eat_char();
            }
            b'-' => {
                self.eat_char();
                buf.push('-');
            }
            _ => {}
        }

        // Make sure a digit follows the exponent place.
        match tri!(self.scan_or_eof(buf)) {
            b'0'..=b'9' => {}
            _ => {
                return Err(self.error(ErrorCode::InvalidNumber));
            }
        }

        while let c @ b'0'..=b'9' = tri!(self.peek_or_null()) {
            self.eat_char();
            buf.push(c as char);
        }

        Ok(())
    }

    fn f64_from_parts(
        &mut self,
        positive: bool,
        significand: u64,
        exponent: i32,
        start: usize,
    ) -> Result<f64> {
        if -22 <= exponent && exponent <= 22 && significand <= 9007199254740991 {
            let mut f = significand as f64;
            if exponent < 0 {
                f = f / POW10[-exponent as usize];
            } else {
                f = f * POW10[exponent as usize];
            }
            Ok(if positive { f } else { -f })
        } else if significand == 0 {
            Ok(if positive { 0.0 } else { -0.0 })
        } else if exponent >= -325 && exponent <= 308 {
            let (factor_mantissa, factor_exponent) = POW10_COMPONENTS[(exponent + 325) as usize];
            let mut leading_zeroes = significand.leading_zeros() as u64;
            let f = significand << leading_zeroes;
            let (mut lower, mut upper) = multiply_as_u128(f, factor_mantissa);
            if upper & 0x1FF == 0x1FF && lower.overflowing_add(f).0 < lower {
                let factor_mantissa_low = MANTISSA_128[(exponent + 325) as usize];
                let (product_low, product_middle2) = multiply_as_u128(f, factor_mantissa_low);
                let product_middle1 = lower;
                let mut product_high = upper;
                let product_middle = product_middle1 + product_middle2;
                if product_middle < product_middle1 {
                    product_high += 1;
                }
                if product_middle.overflowing_add(1).0 == 0
                    && product_high & 0x1FF == 0x1FF
                    && product_low.overflowing_add(f).0 < product_low
                {
                    return self.f64_from_parts_slow(start);
                }
                upper = product_high;
                lower = product_middle;
            }
            let upperbit = upper.overflowing_shr(63).0;
            let mut mantissa = upper.overflowing_shr((upperbit + 9) as u32).0;
            leading_zeroes += 1 ^ upperbit;

            if lower == 0 && upper & 0x1FF == 0 && mantissa & 3 == 1 {
                return self.f64_from_parts_slow(start);
            }
            mantissa += mantissa & 1;
            mantissa >>= 1;

            if mantissa >= 1 << 53 {
                mantissa = 1 << 52;
                leading_zeroes -= 1;
            }
            mantissa &= !(1 << 52);
            let real_exponent = factor_exponent as u64 - leading_zeroes;
            // we have to check that real_exponent is in range, otherwise we bail out
            if real_exponent < 1 || real_exponent > 2046 {
                return self.f64_from_parts_slow(start);
            }
            mantissa |= (real_exponent.overflowing_shl(52)).0;
            mantissa |= (!positive as u64) << 63;
            Ok(f64::from_bits(mantissa))
        } else {
            self.f64_from_parts_slow(start)
        }
    }

    #[cold]
    fn f64_from_parts_slow(&mut self, start: usize) -> Result<f64> {
        let buf = self.read.slice_looking_back(start).unwrap();
        match lexical_core::parse_format::<f64>(buf, lexical_core::NumberFormat::JSON) {
            Ok(val) => {
                if val.is_infinite() {
                    Err(self.error(ErrorCode::NumberOutOfRange))
                } else {
                    Ok(val)
                }
            }
            Err(err) => match err.code {
                lexical_core::ErrorCode::ExponentWithoutFraction
                | lexical_core::ErrorCode::MissingExponentSign
                | lexical_core::ErrorCode::EmptyFraction
                | lexical_core::ErrorCode::EmptyMantissa
                | lexical_core::ErrorCode::EmptyExponent
                | lexical_core::ErrorCode::MissingMantissaSign => {
                    Err(self.error(ErrorCode::EofWhileParsingValue))
                }
                _ => Err(self.error(ErrorCode::InvalidNumber)),
            },
        }
    }

    fn parse_object_colon(&mut self) -> Result<()> {
        match tri!(self.parse_whitespace()) {
            Some(b':') => {
                self.eat_char();
                Ok(())
            }
            Some(_) => Err(self.peek_error(ErrorCode::ExpectedColon)),
            None => Err(self.peek_error(ErrorCode::EofWhileParsingObject)),
        }
    }

    fn end_seq(&mut self) -> Result<()> {
        match tri!(self.parse_whitespace()) {
            Some(b']') => {
                self.eat_char();
                Ok(())
            }
            Some(b',') => {
                self.eat_char();
                match self.parse_whitespace() {
                    Ok(Some(b']')) => Err(self.peek_error(ErrorCode::TrailingComma)),
                    _ => Err(self.peek_error(ErrorCode::TrailingCharacters)),
                }
            }
            Some(_) => Err(self.peek_error(ErrorCode::TrailingCharacters)),
            None => Err(self.peek_error(ErrorCode::EofWhileParsingList)),
        }
    }

    fn end_map(&mut self) -> Result<()> {
        match tri!(self.parse_whitespace()) {
            Some(b'}') => {
                self.eat_char();
                Ok(())
            }
            Some(b',') => Err(self.peek_error(ErrorCode::TrailingComma)),
            Some(_) => Err(self.peek_error(ErrorCode::TrailingCharacters)),
            None => Err(self.peek_error(ErrorCode::EofWhileParsingObject)),
        }
    }

    fn ignore_value(&mut self) -> Result<()> {
        self.scratch.clear();
        let mut enclosing = None;

        loop {
            let peek = match tri!(self.parse_whitespace()) {
                Some(b) => b,
                None => {
                    return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
                }
            };

            let frame = match peek {
                b'n' => {
                    self.eat_char();
                    tri!(self.parse_ident(b"ull"));
                    None
                }
                b't' => {
                    self.eat_char();
                    tri!(self.parse_ident(b"rue"));
                    None
                }
                b'f' => {
                    self.eat_char();
                    tri!(self.parse_ident(b"alse"));
                    None
                }
                b'-' => {
                    self.eat_char();
                    tri!(self.ignore_integer());
                    None
                }
                b'0'..=b'9' => {
                    tri!(self.ignore_integer());
                    None
                }
                b'"' => {
                    self.eat_char();
                    tri!(self.read.ignore_str());
                    None
                }
                frame @ b'[' | frame @ b'{' => {
                    self.scratch.extend(enclosing.take());
                    self.eat_char();
                    Some(frame)
                }
                _ => return Err(self.peek_error(ErrorCode::ExpectedSomeValue)),
            };

            let (mut accept_comma, mut frame) = match frame {
                Some(frame) => (false, frame),
                None => match enclosing.take() {
                    Some(frame) => (true, frame),
                    None => match self.scratch.pop() {
                        Some(frame) => (true, frame),
                        None => return Ok(()),
                    },
                },
            };

            loop {
                match tri!(self.parse_whitespace()) {
                    Some(b',') if accept_comma => {
                        self.eat_char();
                        break;
                    }
                    Some(b']') if frame == b'[' => {}
                    Some(b'}') if frame == b'{' => {}
                    Some(_) => {
                        if accept_comma {
                            return Err(self.peek_error(match frame {
                                b'[' => ErrorCode::ExpectedListCommaOrEnd,
                                b'{' => ErrorCode::ExpectedObjectCommaOrEnd,
                                _ => unreachable!(),
                            }));
                        } else {
                            break;
                        }
                    }
                    None => {
                        return Err(self.peek_error(match frame {
                            b'[' => ErrorCode::EofWhileParsingList,
                            b'{' => ErrorCode::EofWhileParsingObject,
                            _ => unreachable!(),
                        }));
                    }
                }

                self.eat_char();
                frame = match self.scratch.pop() {
                    Some(frame) => frame,
                    None => return Ok(()),
                };
                accept_comma = true;
            }

            if frame == b'{' {
                match tri!(self.parse_whitespace()) {
                    Some(b'"') => self.eat_char(),
                    Some(_) => return Err(self.peek_error(ErrorCode::KeyMustBeAString)),
                    None => return Err(self.peek_error(ErrorCode::EofWhileParsingObject)),
                }
                tri!(self.read.ignore_str());
                match tri!(self.parse_whitespace()) {
                    Some(b':') => self.eat_char(),
                    Some(_) => return Err(self.peek_error(ErrorCode::ExpectedColon)),
                    None => return Err(self.peek_error(ErrorCode::EofWhileParsingObject)),
                }
            }

            enclosing = Some(frame);
        }
    }

    fn ignore_integer(&mut self) -> Result<()> {
        match tri!(self.next_char_or_null()) {
            b'0' => {
                // There can be only one leading '0'.
                if let b'0'..=b'9' = tri!(self.peek_or_null()) {
                    return Err(self.peek_error(ErrorCode::InvalidNumber));
                }
            }
            b'1'..=b'9' => {
                while let b'0'..=b'9' = tri!(self.peek_or_null()) {
                    self.eat_char();
                }
            }
            _ => {
                return Err(self.error(ErrorCode::InvalidNumber));
            }
        }

        match tri!(self.peek_or_null()) {
            b'.' => self.ignore_decimal(),
            b'e' | b'E' => self.ignore_exponent(),
            _ => Ok(()),
        }
    }

    fn ignore_decimal(&mut self) -> Result<()> {
        self.eat_char();

        let mut at_least_one_digit = false;
        while let b'0'..=b'9' = tri!(self.peek_or_null()) {
            self.eat_char();
            at_least_one_digit = true;
        }

        if !at_least_one_digit {
            return Err(self.peek_error(ErrorCode::InvalidNumber));
        }

        match tri!(self.peek_or_null()) {
            b'e' | b'E' => self.ignore_exponent(),
            _ => Ok(()),
        }
    }

    fn ignore_exponent(&mut self) -> Result<()> {
        self.eat_char();

        match tri!(self.peek_or_null()) {
            b'+' | b'-' => self.eat_char(),
            _ => {}
        }

        // Make sure a digit follows the exponent place.
        match tri!(self.next_char_or_null()) {
            b'0'..=b'9' => {}
            _ => {
                return Err(self.error(ErrorCode::InvalidNumber));
            }
        }

        while let b'0'..=b'9' = tri!(self.peek_or_null()) {
            self.eat_char();
        }

        Ok(())
    }

    #[cfg(feature = "raw_value")]
    fn deserialize_raw_value<V>(&mut self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.parse_whitespace()?;
        self.read.begin_raw_buffering();
        self.ignore_value()?;
        self.read.end_raw_buffering(visitor)
    }
}

impl FromStr for Number {
    type Err = Error;

    fn from_str(s: &str) -> result::Result<Self, Self::Err> {
        Deserializer::from_str(s)
            .parse_any_signed_number()
            .map(Into::into)
    }
}

fn multiply_as_u128(a: u64, b: u64) -> (u64, u64) {
    let res: u128 = a as u128 * b as u128;
    (res as u64, (res >> 64) as u64)
}

static POW10_COMPONENTS: [(u64, i32); 634] = [
    (0xa5ced43b7e3e9188, 7),
    (0xcf42894a5dce35ea, 10),
    (0x818995ce7aa0e1b2, 14),
    (0xa1ebfb4219491a1f, 17),
    (0xca66fa129f9b60a6, 20),
    (0xfd00b897478238d0, 23),
    (0x9e20735e8cb16382, 27),
    (0xc5a890362fddbc62, 30),
    (0xf712b443bbd52b7b, 33),
    (0x9a6bb0aa55653b2d, 37),
    (0xc1069cd4eabe89f8, 40),
    (0xf148440a256e2c76, 43),
    (0x96cd2a865764dbca, 47),
    (0xbc807527ed3e12bc, 50),
    (0xeba09271e88d976b, 53),
    (0x93445b8731587ea3, 57),
    (0xb8157268fdae9e4c, 60),
    (0xe61acf033d1a45df, 63),
    (0x8fd0c16206306bab, 67),
    (0xb3c4f1ba87bc8696, 70),
    (0xe0b62e2929aba83c, 73),
    (0x8c71dcd9ba0b4925, 77),
    (0xaf8e5410288e1b6f, 80),
    (0xdb71e91432b1a24a, 83),
    (0x892731ac9faf056e, 87),
    (0xab70fe17c79ac6ca, 90),
    (0xd64d3d9db981787d, 93),
    (0x85f0468293f0eb4e, 97),
    (0xa76c582338ed2621, 100),
    (0xd1476e2c07286faa, 103),
    (0x82cca4db847945ca, 107),
    (0xa37fce126597973c, 110),
    (0xcc5fc196fefd7d0c, 113),
    (0xff77b1fcbebcdc4f, 116),
    (0x9faacf3df73609b1, 120),
    (0xc795830d75038c1d, 123),
    (0xf97ae3d0d2446f25, 126),
    (0x9becce62836ac577, 130),
    (0xc2e801fb244576d5, 133),
    (0xf3a20279ed56d48a, 136),
    (0x9845418c345644d6, 140),
    (0xbe5691ef416bd60c, 143),
    (0xedec366b11c6cb8f, 146),
    (0x94b3a202eb1c3f39, 150),
    (0xb9e08a83a5e34f07, 153),
    (0xe858ad248f5c22c9, 156),
    (0x91376c36d99995be, 160),
    (0xb58547448ffffb2d, 163),
    (0xe2e69915b3fff9f9, 166),
    (0x8dd01fad907ffc3b, 170),
    (0xb1442798f49ffb4a, 173),
    (0xdd95317f31c7fa1d, 176),
    (0x8a7d3eef7f1cfc52, 180),
    (0xad1c8eab5ee43b66, 183),
    (0xd863b256369d4a40, 186),
    (0x873e4f75e2224e68, 190),
    (0xa90de3535aaae202, 193),
    (0xd3515c2831559a83, 196),
    (0x8412d9991ed58091, 200),
    (0xa5178fff668ae0b6, 203),
    (0xce5d73ff402d98e3, 206),
    (0x80fa687f881c7f8e, 210),
    (0xa139029f6a239f72, 213),
    (0xc987434744ac874e, 216),
    (0xfbe9141915d7a922, 219),
    (0x9d71ac8fada6c9b5, 223),
    (0xc4ce17b399107c22, 226),
    (0xf6019da07f549b2b, 229),
    (0x99c102844f94e0fb, 233),
    (0xc0314325637a1939, 236),
    (0xf03d93eebc589f88, 239),
    (0x96267c7535b763b5, 243),
    (0xbbb01b9283253ca2, 246),
    (0xea9c227723ee8bcb, 249),
    (0x92a1958a7675175f, 253),
    (0xb749faed14125d36, 256),
    (0xe51c79a85916f484, 259),
    (0x8f31cc0937ae58d2, 263),
    (0xb2fe3f0b8599ef07, 266),
    (0xdfbdcece67006ac9, 269),
    (0x8bd6a141006042bd, 273),
    (0xaecc49914078536d, 276),
    (0xda7f5bf590966848, 279),
    (0x888f99797a5e012d, 283),
    (0xaab37fd7d8f58178, 286),
    (0xd5605fcdcf32e1d6, 289),
    (0x855c3be0a17fcd26, 293),
    (0xa6b34ad8c9dfc06f, 296),
    (0xd0601d8efc57b08b, 299),
    (0x823c12795db6ce57, 303),
    (0xa2cb1717b52481ed, 306),
    (0xcb7ddcdda26da268, 309),
    (0xfe5d54150b090b02, 312),
    (0x9efa548d26e5a6e1, 316),
    (0xc6b8e9b0709f109a, 319),
    (0xf867241c8cc6d4c0, 322),
    (0x9b407691d7fc44f8, 326),
    (0xc21094364dfb5636, 329),
    (0xf294b943e17a2bc4, 332),
    (0x979cf3ca6cec5b5a, 336),
    (0xbd8430bd08277231, 339),
    (0xece53cec4a314ebd, 342),
    (0x940f4613ae5ed136, 346),
    (0xb913179899f68584, 349),
    (0xe757dd7ec07426e5, 352),
    (0x9096ea6f3848984f, 356),
    (0xb4bca50b065abe63, 359),
    (0xe1ebce4dc7f16dfb, 362),
    (0x8d3360f09cf6e4bd, 366),
    (0xb080392cc4349dec, 369),
    (0xdca04777f541c567, 372),
    (0x89e42caaf9491b60, 376),
    (0xac5d37d5b79b6239, 379),
    (0xd77485cb25823ac7, 382),
    (0x86a8d39ef77164bc, 386),
    (0xa8530886b54dbdeb, 389),
    (0xd267caa862a12d66, 392),
    (0x8380dea93da4bc60, 396),
    (0xa46116538d0deb78, 399),
    (0xcd795be870516656, 402),
    (0x806bd9714632dff6, 406),
    (0xa086cfcd97bf97f3, 409),
    (0xc8a883c0fdaf7df0, 412),
    (0xfad2a4b13d1b5d6c, 415),
    (0x9cc3a6eec6311a63, 419),
    (0xc3f490aa77bd60fc, 422),
    (0xf4f1b4d515acb93b, 425),
    (0x991711052d8bf3c5, 429),
    (0xbf5cd54678eef0b6, 432),
    (0xef340a98172aace4, 435),
    (0x9580869f0e7aac0e, 439),
    (0xbae0a846d2195712, 442),
    (0xe998d258869facd7, 445),
    (0x91ff83775423cc06, 449),
    (0xb67f6455292cbf08, 452),
    (0xe41f3d6a7377eeca, 455),
    (0x8e938662882af53e, 459),
    (0xb23867fb2a35b28d, 462),
    (0xdec681f9f4c31f31, 465),
    (0x8b3c113c38f9f37e, 469),
    (0xae0b158b4738705e, 472),
    (0xd98ddaee19068c76, 475),
    (0x87f8a8d4cfa417c9, 479),
    (0xa9f6d30a038d1dbc, 482),
    (0xd47487cc8470652b, 485),
    (0x84c8d4dfd2c63f3b, 489),
    (0xa5fb0a17c777cf09, 492),
    (0xcf79cc9db955c2cc, 495),
    (0x81ac1fe293d599bf, 499),
    (0xa21727db38cb002f, 502),
    (0xca9cf1d206fdc03b, 505),
    (0xfd442e4688bd304a, 508),
    (0x9e4a9cec15763e2e, 512),
    (0xc5dd44271ad3cdba, 515),
    (0xf7549530e188c128, 518),
    (0x9a94dd3e8cf578b9, 522),
    (0xc13a148e3032d6e7, 525),
    (0xf18899b1bc3f8ca1, 528),
    (0x96f5600f15a7b7e5, 532),
    (0xbcb2b812db11a5de, 535),
    (0xebdf661791d60f56, 538),
    (0x936b9fcebb25c995, 542),
    (0xb84687c269ef3bfb, 545),
    (0xe65829b3046b0afa, 548),
    (0x8ff71a0fe2c2e6dc, 552),
    (0xb3f4e093db73a093, 555),
    (0xe0f218b8d25088b8, 558),
    (0x8c974f7383725573, 562),
    (0xafbd2350644eeacf, 565),
    (0xdbac6c247d62a583, 568),
    (0x894bc396ce5da772, 572),
    (0xab9eb47c81f5114f, 575),
    (0xd686619ba27255a2, 578),
    (0x8613fd0145877585, 582),
    (0xa798fc4196e952e7, 585),
    (0xd17f3b51fca3a7a0, 588),
    (0x82ef85133de648c4, 592),
    (0xa3ab66580d5fdaf5, 595),
    (0xcc963fee10b7d1b3, 598),
    (0xffbbcfe994e5c61f, 601),
    (0x9fd561f1fd0f9bd3, 605),
    (0xc7caba6e7c5382c8, 608),
    (0xf9bd690a1b68637b, 611),
    (0x9c1661a651213e2d, 615),
    (0xc31bfa0fe5698db8, 618),
    (0xf3e2f893dec3f126, 621),
    (0x986ddb5c6b3a76b7, 625),
    (0xbe89523386091465, 628),
    (0xee2ba6c0678b597f, 631),
    (0x94db483840b717ef, 635),
    (0xba121a4650e4ddeb, 638),
    (0xe896a0d7e51e1566, 641),
    (0x915e2486ef32cd60, 645),
    (0xb5b5ada8aaff80b8, 648),
    (0xe3231912d5bf60e6, 651),
    (0x8df5efabc5979c8f, 655),
    (0xb1736b96b6fd83b3, 658),
    (0xddd0467c64bce4a0, 661),
    (0x8aa22c0dbef60ee4, 665),
    (0xad4ab7112eb3929d, 668),
    (0xd89d64d57a607744, 671),
    (0x87625f056c7c4a8b, 675),
    (0xa93af6c6c79b5d2d, 678),
    (0xd389b47879823479, 681),
    (0x843610cb4bf160cb, 685),
    (0xa54394fe1eedb8fe, 688),
    (0xce947a3da6a9273e, 691),
    (0x811ccc668829b887, 695),
    (0xa163ff802a3426a8, 698),
    (0xc9bcff6034c13052, 701),
    (0xfc2c3f3841f17c67, 704),
    (0x9d9ba7832936edc0, 708),
    (0xc5029163f384a931, 711),
    (0xf64335bcf065d37d, 714),
    (0x99ea0196163fa42e, 718),
    (0xc06481fb9bcf8d39, 721),
    (0xf07da27a82c37088, 724),
    (0x964e858c91ba2655, 728),
    (0xbbe226efb628afea, 731),
    (0xeadab0aba3b2dbe5, 734),
    (0x92c8ae6b464fc96f, 738),
    (0xb77ada0617e3bbcb, 741),
    (0xe55990879ddcaabd, 744),
    (0x8f57fa54c2a9eab6, 748),
    (0xb32df8e9f3546564, 751),
    (0xdff9772470297ebd, 754),
    (0x8bfbea76c619ef36, 758),
    (0xaefae51477a06b03, 761),
    (0xdab99e59958885c4, 764),
    (0x88b402f7fd75539b, 768),
    (0xaae103b5fcd2a881, 771),
    (0xd59944a37c0752a2, 774),
    (0x857fcae62d8493a5, 778),
    (0xa6dfbd9fb8e5b88e, 781),
    (0xd097ad07a71f26b2, 784),
    (0x825ecc24c873782f, 788),
    (0xa2f67f2dfa90563b, 791),
    (0xcbb41ef979346bca, 794),
    (0xfea126b7d78186bc, 797),
    (0x9f24b832e6b0f436, 801),
    (0xc6ede63fa05d3143, 804),
    (0xf8a95fcf88747d94, 807),
    (0x9b69dbe1b548ce7c, 811),
    (0xc24452da229b021b, 814),
    (0xf2d56790ab41c2a2, 817),
    (0x97c560ba6b0919a5, 821),
    (0xbdb6b8e905cb600f, 824),
    (0xed246723473e3813, 827),
    (0x9436c0760c86e30b, 831),
    (0xb94470938fa89bce, 834),
    (0xe7958cb87392c2c2, 837),
    (0x90bd77f3483bb9b9, 841),
    (0xb4ecd5f01a4aa828, 844),
    (0xe2280b6c20dd5232, 847),
    (0x8d590723948a535f, 851),
    (0xb0af48ec79ace837, 854),
    (0xdcdb1b2798182244, 857),
    (0x8a08f0f8bf0f156b, 861),
    (0xac8b2d36eed2dac5, 864),
    (0xd7adf884aa879177, 867),
    (0x86ccbb52ea94baea, 871),
    (0xa87fea27a539e9a5, 874),
    (0xd29fe4b18e88640e, 877),
    (0x83a3eeeef9153e89, 881),
    (0xa48ceaaab75a8e2b, 884),
    (0xcdb02555653131b6, 887),
    (0x808e17555f3ebf11, 891),
    (0xa0b19d2ab70e6ed6, 894),
    (0xc8de047564d20a8b, 897),
    (0xfb158592be068d2e, 900),
    (0x9ced737bb6c4183d, 904),
    (0xc428d05aa4751e4c, 907),
    (0xf53304714d9265df, 910),
    (0x993fe2c6d07b7fab, 914),
    (0xbf8fdb78849a5f96, 917),
    (0xef73d256a5c0f77c, 920),
    (0x95a8637627989aad, 924),
    (0xbb127c53b17ec159, 927),
    (0xe9d71b689dde71af, 930),
    (0x9226712162ab070d, 934),
    (0xb6b00d69bb55c8d1, 937),
    (0xe45c10c42a2b3b05, 940),
    (0x8eb98a7a9a5b04e3, 944),
    (0xb267ed1940f1c61c, 947),
    (0xdf01e85f912e37a3, 950),
    (0x8b61313bbabce2c6, 954),
    (0xae397d8aa96c1b77, 957),
    (0xd9c7dced53c72255, 960),
    (0x881cea14545c7575, 964),
    (0xaa242499697392d2, 967),
    (0xd4ad2dbfc3d07787, 970),
    (0x84ec3c97da624ab4, 974),
    (0xa6274bbdd0fadd61, 977),
    (0xcfb11ead453994ba, 980),
    (0x81ceb32c4b43fcf4, 984),
    (0xa2425ff75e14fc31, 987),
    (0xcad2f7f5359a3b3e, 990),
    (0xfd87b5f28300ca0d, 993),
    (0x9e74d1b791e07e48, 997),
    (0xc612062576589dda, 1000),
    (0xf79687aed3eec551, 1003),
    (0x9abe14cd44753b52, 1007),
    (0xc16d9a0095928a27, 1010),
    (0xf1c90080baf72cb1, 1013),
    (0x971da05074da7bee, 1017),
    (0xbce5086492111aea, 1020),
    (0xec1e4a7db69561a5, 1023),
    (0x9392ee8e921d5d07, 1027),
    (0xb877aa3236a4b449, 1030),
    (0xe69594bec44de15b, 1033),
    (0x901d7cf73ab0acd9, 1037),
    (0xb424dc35095cd80f, 1040),
    (0xe12e13424bb40e13, 1043),
    (0x8cbccc096f5088cb, 1047),
    (0xafebff0bcb24aafe, 1050),
    (0xdbe6fecebdedd5be, 1053),
    (0x89705f4136b4a597, 1057),
    (0xabcc77118461cefc, 1060),
    (0xd6bf94d5e57a42bc, 1063),
    (0x8637bd05af6c69b5, 1067),
    (0xa7c5ac471b478423, 1070),
    (0xd1b71758e219652b, 1073),
    (0x83126e978d4fdf3b, 1077),
    (0xa3d70a3d70a3d70a, 1080),
    (0xcccccccccccccccc, 1083),
    (0x8000000000000000, 1087),
    (0xa000000000000000, 1090),
    (0xc800000000000000, 1093),
    (0xfa00000000000000, 1096),
    (0x9c40000000000000, 1100),
    (0xc350000000000000, 1103),
    (0xf424000000000000, 1106),
    (0x9896800000000000, 1110),
    (0xbebc200000000000, 1113),
    (0xee6b280000000000, 1116),
    (0x9502f90000000000, 1120),
    (0xba43b74000000000, 1123),
    (0xe8d4a51000000000, 1126),
    (0x9184e72a00000000, 1130),
    (0xb5e620f480000000, 1133),
    (0xe35fa931a0000000, 1136),
    (0x8e1bc9bf04000000, 1140),
    (0xb1a2bc2ec5000000, 1143),
    (0xde0b6b3a76400000, 1146),
    (0x8ac7230489e80000, 1150),
    (0xad78ebc5ac620000, 1153),
    (0xd8d726b7177a8000, 1156),
    (0x878678326eac9000, 1160),
    (0xa968163f0a57b400, 1163),
    (0xd3c21bcecceda100, 1166),
    (0x84595161401484a0, 1170),
    (0xa56fa5b99019a5c8, 1173),
    (0xcecb8f27f4200f3a, 1176),
    (0x813f3978f8940984, 1180),
    (0xa18f07d736b90be5, 1183),
    (0xc9f2c9cd04674ede, 1186),
    (0xfc6f7c4045812296, 1189),
    (0x9dc5ada82b70b59d, 1193),
    (0xc5371912364ce305, 1196),
    (0xf684df56c3e01bc6, 1199),
    (0x9a130b963a6c115c, 1203),
    (0xc097ce7bc90715b3, 1206),
    (0xf0bdc21abb48db20, 1209),
    (0x96769950b50d88f4, 1213),
    (0xbc143fa4e250eb31, 1216),
    (0xeb194f8e1ae525fd, 1219),
    (0x92efd1b8d0cf37be, 1223),
    (0xb7abc627050305ad, 1226),
    (0xe596b7b0c643c719, 1229),
    (0x8f7e32ce7bea5c6f, 1233),
    (0xb35dbf821ae4f38b, 1236),
    (0xe0352f62a19e306e, 1239),
    (0x8c213d9da502de45, 1243),
    (0xaf298d050e4395d6, 1246),
    (0xdaf3f04651d47b4c, 1249),
    (0x88d8762bf324cd0f, 1253),
    (0xab0e93b6efee0053, 1256),
    (0xd5d238a4abe98068, 1259),
    (0x85a36366eb71f041, 1263),
    (0xa70c3c40a64e6c51, 1266),
    (0xd0cf4b50cfe20765, 1269),
    (0x82818f1281ed449f, 1273),
    (0xa321f2d7226895c7, 1276),
    (0xcbea6f8ceb02bb39, 1279),
    (0xfee50b7025c36a08, 1282),
    (0x9f4f2726179a2245, 1286),
    (0xc722f0ef9d80aad6, 1289),
    (0xf8ebad2b84e0d58b, 1292),
    (0x9b934c3b330c8577, 1296),
    (0xc2781f49ffcfa6d5, 1299),
    (0xf316271c7fc3908a, 1302),
    (0x97edd871cfda3a56, 1306),
    (0xbde94e8e43d0c8ec, 1309),
    (0xed63a231d4c4fb27, 1312),
    (0x945e455f24fb1cf8, 1316),
    (0xb975d6b6ee39e436, 1319),
    (0xe7d34c64a9c85d44, 1322),
    (0x90e40fbeea1d3a4a, 1326),
    (0xb51d13aea4a488dd, 1329),
    (0xe264589a4dcdab14, 1332),
    (0x8d7eb76070a08aec, 1336),
    (0xb0de65388cc8ada8, 1339),
    (0xdd15fe86affad912, 1342),
    (0x8a2dbf142dfcc7ab, 1346),
    (0xacb92ed9397bf996, 1349),
    (0xd7e77a8f87daf7fb, 1352),
    (0x86f0ac99b4e8dafd, 1356),
    (0xa8acd7c0222311bc, 1359),
    (0xd2d80db02aabd62b, 1362),
    (0x83c7088e1aab65db, 1366),
    (0xa4b8cab1a1563f52, 1369),
    (0xcde6fd5e09abcf26, 1372),
    (0x80b05e5ac60b6178, 1376),
    (0xa0dc75f1778e39d6, 1379),
    (0xc913936dd571c84c, 1382),
    (0xfb5878494ace3a5f, 1385),
    (0x9d174b2dcec0e47b, 1389),
    (0xc45d1df942711d9a, 1392),
    (0xf5746577930d6500, 1395),
    (0x9968bf6abbe85f20, 1399),
    (0xbfc2ef456ae276e8, 1402),
    (0xefb3ab16c59b14a2, 1405),
    (0x95d04aee3b80ece5, 1409),
    (0xbb445da9ca61281f, 1412),
    (0xea1575143cf97226, 1415),
    (0x924d692ca61be758, 1419),
    (0xb6e0c377cfa2e12e, 1422),
    (0xe498f455c38b997a, 1425),
    (0x8edf98b59a373fec, 1429),
    (0xb2977ee300c50fe7, 1432),
    (0xdf3d5e9bc0f653e1, 1435),
    (0x8b865b215899f46c, 1439),
    (0xae67f1e9aec07187, 1442),
    (0xda01ee641a708de9, 1445),
    (0x884134fe908658b2, 1449),
    (0xaa51823e34a7eede, 1452),
    (0xd4e5e2cdc1d1ea96, 1455),
    (0x850fadc09923329e, 1459),
    (0xa6539930bf6bff45, 1462),
    (0xcfe87f7cef46ff16, 1465),
    (0x81f14fae158c5f6e, 1469),
    (0xa26da3999aef7749, 1472),
    (0xcb090c8001ab551c, 1475),
    (0xfdcb4fa002162a63, 1478),
    (0x9e9f11c4014dda7e, 1482),
    (0xc646d63501a1511d, 1485),
    (0xf7d88bc24209a565, 1488),
    (0x9ae757596946075f, 1492),
    (0xc1a12d2fc3978937, 1495),
    (0xf209787bb47d6b84, 1498),
    (0x9745eb4d50ce6332, 1502),
    (0xbd176620a501fbff, 1505),
    (0xec5d3fa8ce427aff, 1508),
    (0x93ba47c980e98cdf, 1512),
    (0xb8a8d9bbe123f017, 1515),
    (0xe6d3102ad96cec1d, 1518),
    (0x9043ea1ac7e41392, 1522),
    (0xb454e4a179dd1877, 1525),
    (0xe16a1dc9d8545e94, 1528),
    (0x8ce2529e2734bb1d, 1532),
    (0xb01ae745b101e9e4, 1535),
    (0xdc21a1171d42645d, 1538),
    (0x899504ae72497eba, 1542),
    (0xabfa45da0edbde69, 1545),
    (0xd6f8d7509292d603, 1548),
    (0x865b86925b9bc5c2, 1552),
    (0xa7f26836f282b732, 1555),
    (0xd1ef0244af2364ff, 1558),
    (0x8335616aed761f1f, 1562),
    (0xa402b9c5a8d3a6e7, 1565),
    (0xcd036837130890a1, 1568),
    (0x802221226be55a64, 1572),
    (0xa02aa96b06deb0fd, 1575),
    (0xc83553c5c8965d3d, 1578),
    (0xfa42a8b73abbf48c, 1581),
    (0x9c69a97284b578d7, 1585),
    (0xc38413cf25e2d70d, 1588),
    (0xf46518c2ef5b8cd1, 1591),
    (0x98bf2f79d5993802, 1595),
    (0xbeeefb584aff8603, 1598),
    (0xeeaaba2e5dbf6784, 1601),
    (0x952ab45cfa97a0b2, 1605),
    (0xba756174393d88df, 1608),
    (0xe912b9d1478ceb17, 1611),
    (0x91abb422ccb812ee, 1615),
    (0xb616a12b7fe617aa, 1618),
    (0xe39c49765fdf9d94, 1621),
    (0x8e41ade9fbebc27d, 1625),
    (0xb1d219647ae6b31c, 1628),
    (0xde469fbd99a05fe3, 1631),
    (0x8aec23d680043bee, 1635),
    (0xada72ccc20054ae9, 1638),
    (0xd910f7ff28069da4, 1641),
    (0x87aa9aff79042286, 1645),
    (0xa99541bf57452b28, 1648),
    (0xd3fa922f2d1675f2, 1651),
    (0x847c9b5d7c2e09b7, 1655),
    (0xa59bc234db398c25, 1658),
    (0xcf02b2c21207ef2e, 1661),
    (0x8161afb94b44f57d, 1665),
    (0xa1ba1ba79e1632dc, 1668),
    (0xca28a291859bbf93, 1671),
    (0xfcb2cb35e702af78, 1674),
    (0x9defbf01b061adab, 1678),
    (0xc56baec21c7a1916, 1681),
    (0xf6c69a72a3989f5b, 1684),
    (0x9a3c2087a63f6399, 1688),
    (0xc0cb28a98fcf3c7f, 1691),
    (0xf0fdf2d3f3c30b9f, 1694),
    (0x969eb7c47859e743, 1698),
    (0xbc4665b596706114, 1701),
    (0xeb57ff22fc0c7959, 1704),
    (0x9316ff75dd87cbd8, 1708),
    (0xb7dcbf5354e9bece, 1711),
    (0xe5d3ef282a242e81, 1714),
    (0x8fa475791a569d10, 1718),
    (0xb38d92d760ec4455, 1721),
    (0xe070f78d3927556a, 1724),
    (0x8c469ab843b89562, 1728),
    (0xaf58416654a6babb, 1731),
    (0xdb2e51bfe9d0696a, 1734),
    (0x88fcf317f22241e2, 1738),
    (0xab3c2fddeeaad25a, 1741),
    (0xd60b3bd56a5586f1, 1744),
    (0x85c7056562757456, 1748),
    (0xa738c6bebb12d16c, 1751),
    (0xd106f86e69d785c7, 1754),
    (0x82a45b450226b39c, 1758),
    (0xa34d721642b06084, 1761),
    (0xcc20ce9bd35c78a5, 1764),
    (0xff290242c83396ce, 1767),
    (0x9f79a169bd203e41, 1771),
    (0xc75809c42c684dd1, 1774),
    (0xf92e0c3537826145, 1777),
    (0x9bbcc7a142b17ccb, 1781),
    (0xc2abf989935ddbfe, 1784),
    (0xf356f7ebf83552fe, 1787),
    (0x98165af37b2153de, 1791),
    (0xbe1bf1b059e9a8d6, 1794),
    (0xeda2ee1c7064130c, 1797),
    (0x9485d4d1c63e8be7, 1801),
    (0xb9a74a0637ce2ee1, 1804),
    (0xe8111c87c5c1ba99, 1807),
    (0x910ab1d4db9914a0, 1811),
    (0xb54d5e4a127f59c8, 1814),
    (0xe2a0b5dc971f303a, 1817),
    (0x8da471a9de737e24, 1821),
    (0xb10d8e1456105dad, 1824),
    (0xdd50f1996b947518, 1827),
    (0x8a5296ffe33cc92f, 1831),
    (0xace73cbfdc0bfb7b, 1834),
    (0xd8210befd30efa5a, 1837),
    (0x8714a775e3e95c78, 1841),
    (0xa8d9d1535ce3b396, 1844),
    (0xd31045a8341ca07c, 1847),
    (0x83ea2b892091e44d, 1851),
    (0xa4e4b66b68b65d60, 1854),
    (0xce1de40642e3f4b9, 1857),
    (0x80d2ae83e9ce78f3, 1861),
    (0xa1075a24e4421730, 1864),
    (0xc94930ae1d529cfc, 1867),
    (0xfb9b7cd9a4a7443c, 1870),
    (0x9d412e0806e88aa5, 1874),
    (0xc491798a08a2ad4e, 1877),
    (0xf5b5d7ec8acb58a2, 1880),
    (0x9991a6f3d6bf1765, 1884),
    (0xbff610b0cc6edd3f, 1887),
    (0xeff394dcff8a948e, 1890),
    (0x95f83d0a1fb69cd9, 1894),
    (0xbb764c4ca7a4440f, 1897),
    (0xea53df5fd18d5513, 1900),
    (0x92746b9be2f8552c, 1904),
    (0xb7118682dbb66a77, 1907),
    (0xe4d5e82392a40515, 1910),
    (0x8f05b1163ba6832d, 1914),
    (0xb2c71d5bca9023f8, 1917),
    (0xdf78e4b2bd342cf6, 1920),
    (0x8bab8eefb6409c1a, 1924),
    (0xae9672aba3d0c320, 1927),
    (0xda3c0f568cc4f3e8, 1930),
    (0x8865899617fb1871, 1934),
    (0xaa7eebfb9df9de8d, 1937),
    (0xd51ea6fa85785631, 1940),
    (0x8533285c936b35de, 1944),
    (0xa67ff273b8460356, 1947),
    (0xd01fef10a657842c, 1950),
    (0x8213f56a67f6b29b, 1954),
    (0xa298f2c501f45f42, 1957),
    (0xcb3f2f7642717713, 1960),
    (0xfe0efb53d30dd4d7, 1963),
    (0x9ec95d1463e8a506, 1967),
    (0xc67bb4597ce2ce48, 1970),
    (0xf81aa16fdc1b81da, 1973),
    (0x9b10a4e5e9913128, 1977),
    (0xc1d4ce1f63f57d72, 1980),
    (0xf24a01a73cf2dccf, 1983),
    (0x976e41088617ca01, 1987),
    (0xbd49d14aa79dbc82, 1990),
    (0xec9c459d51852ba2, 1993),
    (0x93e1ab8252f33b45, 1997),
    (0xb8da1662e7b00a17, 2000),
    (0xe7109bfba19c0c9d, 2003),
    (0x906a617d450187e2, 2007),
    (0xb484f9dc9641e9da, 2010),
    (0xe1a63853bbd26451, 2013),
    (0x8d07e33455637eb2, 2017),
    (0xb049dc016abc5e5f, 2020),
    (0xdc5c5301c56b75f7, 2023),
    (0x89b9b3e11b6329ba, 2027),
    (0xac2820d9623bf429, 2030),
    (0xd732290fbacaf133, 2033),
    (0x867f59a9d4bed6c0, 2037),
    (0xa81f301449ee8c70, 2040),
    (0xd226fc195c6a2f8c, 2043),
    (0x83585d8fd9c25db7, 2047),
    (0xa42e74f3d032f525, 2050),
    (0xcd3a1230c43fb26f, 2053),
    (0x80444b5e7aa7cf85, 2057),
    (0xa0555e361951c366, 2060),
    (0xc86ab5c39fa63440, 2063),
    (0xfa856334878fc150, 2066),
    (0x9c935e00d4b9d8d2, 2070),
    (0xc3b8358109e84f07, 2073),
    (0xf4a642e14c6262c8, 2076),
    (0x98e7e9cccfbd7dbd, 2080),
    (0xbf21e44003acdd2c, 2083),
    (0xeeea5d5004981478, 2086),
    (0x95527a5202df0ccb, 2090),
    (0xbaa718e68396cffd, 2093),
    (0xe950df20247c83fd, 2096),
    (0x91d28b7416cdd27e, 2100),
    (0xb6472e511c81471d, 2103),
    (0xe3d8f9e563a198e5, 2106),
    (0x8e679c2f5e44ff8f, 2110),
];

const MANTISSA_128: [u64; 634] = [
    0x419ea3bd35385e2d,
    0x52064cac828675b9,
    0x7343efebd1940993,
    0x1014ebe6c5f90bf8,
    0xd41a26e077774ef6,
    0x8920b098955522b4,
    0x55b46e5f5d5535b0,
    0xeb2189f734aa831d,
    0xa5e9ec7501d523e4,
    0x47b233c92125366e,
    0x999ec0bb696e840a,
    0xc00670ea43ca250d,
    0x380406926a5e5728,
    0xc605083704f5ecf2,
    0xf7864a44c633682e,
    0x7ab3ee6afbe0211d,
    0x5960ea05bad82964,
    0x6fb92487298e33bd,
    0xa5d3b6d479f8e056,
    0x8f48a4899877186c,
    0x331acdabfe94de87,
    0x9ff0c08b7f1d0b14,
    0x7ecf0ae5ee44dd9,
    0xc9e82cd9f69d6150,
    0xbe311c083a225cd2,
    0x6dbd630a48aaf406,
    0x92cbbccdad5b108,
    0x25bbf56008c58ea5,
    0xaf2af2b80af6f24e,
    0x1af5af660db4aee1,
    0x50d98d9fc890ed4d,
    0xe50ff107bab528a0,
    0x1e53ed49a96272c8,
    0x25e8e89c13bb0f7a,
    0x77b191618c54e9ac,
    0xd59df5b9ef6a2417,
    0x4b0573286b44ad1d,
    0x4ee367f9430aec32,
    0x229c41f793cda73f,
    0x6b43527578c1110f,
    0x830a13896b78aaa9,
    0x23cc986bc656d553,
    0x2cbfbe86b7ec8aa8,
    0x7bf7d71432f3d6a9,
    0xdaf5ccd93fb0cc53,
    0xd1b3400f8f9cff68,
    0x23100809b9c21fa1,
    0xabd40a0c2832a78a,
    0x16c90c8f323f516c,
    0xae3da7d97f6792e3,
    0x99cd11cfdf41779c,
    0x40405643d711d583,
    0x482835ea666b2572,
    0xda3243650005eecf,
    0x90bed43e40076a82,
    0x5a7744a6e804a291,
    0x711515d0a205cb36,
    0xd5a5b44ca873e03,
    0xe858790afe9486c2,
    0x626e974dbe39a872,
    0xfb0a3d212dc8128f,
    0x7ce66634bc9d0b99,
    0x1c1fffc1ebc44e80,
    0xa327ffb266b56220,
    0x4bf1ff9f0062baa8,
    0x6f773fc3603db4a9,
    0xcb550fb4384d21d3,
    0x7e2a53a146606a48,
    0x2eda7444cbfc426d,
    0xfa911155fefb5308,
    0x793555ab7eba27ca,
    0x4bc1558b2f3458de,
    0x9eb1aaedfb016f16,
    0x465e15a979c1cadc,
    0xbfacd89ec191ec9,
    0xcef980ec671f667b,
    0x82b7e12780e7401a,
    0xd1b2ecb8b0908810,
    0x861fa7e6dcb4aa15,
    0x67a791e093e1d49a,
    0xe0c8bb2c5c6d24e0,
    0x58fae9f773886e18,
    0xaf39a475506a899e,
    0x6d8406c952429603,
    0xc8e5087ba6d33b83,
    0xfb1e4a9a90880a64,
    0x5cf2eea09a55067f,
    0xf42faa48c0ea481e,
    0xf13b94daf124da26,
    0x76c53d08d6b70858,
    0x54768c4b0c64ca6e,
    0xa9942f5dcf7dfd09,
    0xd3f93b35435d7c4c,
    0xc47bc5014a1a6daf,
    0x359ab6419ca1091b,
    0xc30163d203c94b62,
    0x79e0de63425dcf1d,
    0x985915fc12f542e4,
    0x3e6f5b7b17b2939d,
    0xa705992ceecf9c42,
    0x50c6ff782a838353,
    0xa4f8bf5635246428,
    0x871b7795e136be99,
    0x28e2557b59846e3f,
    0x331aeada2fe589cf,
    0x3ff0d2c85def7621,
    0xfed077a756b53a9,
    0xd3e8495912c62894,
    0x64712dd7abbbd95c,
    0xbd8d794d96aacfb3,
    0xecf0d7a0fc5583a0,
    0xf41686c49db57244,
    0x311c2875c522ced5,
    0x7d633293366b828b,
    0xae5dff9c02033197,
    0xd9f57f830283fdfc,
    0xd072df63c324fd7b,
    0x4247cb9e59f71e6d,
    0x52d9be85f074e608,
    0x67902e276c921f8b,
    0xba1cd8a3db53b6,
    0x80e8a40eccd228a4,
    0x6122cd128006b2cd,
    0x796b805720085f81,
    0xcbe3303674053bb0,
    0xbedbfc4411068a9c,
    0xee92fb5515482d44,
    0x751bdd152d4d1c4a,
    0xd262d45a78a0635d,
    0x86fb897116c87c34,
    0xd45d35e6ae3d4da0,
    0x8974836059cca109,
    0x2bd1a438703fc94b,
    0x7b6306a34627ddcf,
    0x1a3bc84c17b1d542,
    0x20caba5f1d9e4a93,
    0x547eb47b7282ee9c,
    0xe99e619a4f23aa43,
    0x6405fa00e2ec94d4,
    0xde83bc408dd3dd04,
    0x9624ab50b148d445,
    0x3badd624dd9b0957,
    0xe54ca5d70a80e5d6,
    0x5e9fcf4ccd211f4c,
    0x7647c3200069671f,
    0x29ecd9f40041e073,
    0xf468107100525890,
    0x7182148d4066eeb4,
    0xc6f14cd848405530,
    0xb8ada00e5a506a7c,
    0xa6d90811f0e4851c,
    0x908f4a166d1da663,
    0x9a598e4e043287fe,
    0x40eff1e1853f29fd,
    0xd12bee59e68ef47c,
    0x82bb74f8301958ce,
    0xe36a52363c1faf01,
    0xdc44e6c3cb279ac1,
    0x29ab103a5ef8c0b9,
    0x7415d448f6b6f0e7,
    0x111b495b3464ad21,
    0xcab10dd900beec34,
    0x3d5d514f40eea742,
    0xcb4a5a3112a5112,
    0x47f0e785eaba72ab,
    0x59ed216765690f56,
    0x306869c13ec3532c,
    0x1e414218c73a13fb,
    0xe5d1929ef90898fa,
    0xdf45f746b74abf39,
    0x6b8bba8c328eb783,
    0x66ea92f3f326564,
    0xc80a537b0efefebd,
    0xbd06742ce95f5f36,
    0x2c48113823b73704,
    0xf75a15862ca504c5,
    0x9a984d73dbe722fb,
    0xc13e60d0d2e0ebba,
    0x318df905079926a8,
    0xfdf17746497f7052,
    0xfeb6ea8bedefa633,
    0xfe64a52ee96b8fc0,
    0x3dfdce7aa3c673b0,
    0x6bea10ca65c084e,
    0x486e494fcff30a62,
    0x5a89dba3c3efccfa,
    0xf89629465a75e01c,
    0xf6bbb397f1135823,
    0x746aa07ded582e2c,
    0xa8c2a44eb4571cdc,
    0x92f34d62616ce413,
    0x77b020baf9c81d17,
    0xace1474dc1d122e,
    0xd819992132456ba,
    0x10e1fff697ed6c69,
    0xca8d3ffa1ef463c1,
    0xbd308ff8a6b17cb2,
    0xac7cb3f6d05ddbde,
    0x6bcdf07a423aa96b,
    0x86c16c98d2c953c6,
    0xe871c7bf077ba8b7,
    0x11471cd764ad4972,
    0xd598e40d3dd89bcf,
    0x4aff1d108d4ec2c3,
    0xcedf722a585139ba,
    0xc2974eb4ee658828,
    0x733d226229feea32,
    0x806357d5a3f525f,
    0xca07c2dcb0cf26f7,
    0xfc89b393dd02f0b5,
    0xbbac2078d443ace2,
    0xd54b944b84aa4c0d,
    0xa9e795e65d4df11,
    0x4d4617b5ff4a16d5,
    0x504bced1bf8e4e45,
    0xe45ec2862f71e1d6,
    0x5d767327bb4e5a4c,
    0x3a6a07f8d510f86f,
    0x890489f70a55368b,
    0x2b45ac74ccea842e,
    0x3b0b8bc90012929d,
    0x9ce6ebb40173744,
    0xcc420a6a101d0515,
    0x9fa946824a12232d,
    0x47939822dc96abf9,
    0x59787e2b93bc56f7,
    0x57eb4edb3c55b65a,
    0xede622920b6b23f1,
    0xe95fab368e45eced,
    0x11dbcb0218ebb414,
    0xd652bdc29f26a119,
    0x4be76d3346f0495f,
    0x6f70a4400c562ddb,
    0xcb4ccd500f6bb952,
    0x7e2000a41346a7a7,
    0x8ed400668c0c28c8,
    0x728900802f0f32fa,
    0x4f2b40a03ad2ffb9,
    0xe2f610c84987bfa8,
    0xdd9ca7d2df4d7c9,
    0x91503d1c79720dbb,
    0x75a44c6397ce912a,
    0xc986afbe3ee11aba,
    0xfbe85badce996168,
    0xfae27299423fb9c3,
    0xdccd879fc967d41a,
    0x5400e987bbc1c920,
    0x290123e9aab23b68,
    0xf9a0b6720aaf6521,
    0xf808e40e8d5b3e69,
    0xb60b1d1230b20e04,
    0xb1c6f22b5e6f48c2,
    0x1e38aeb6360b1af3,
    0x25c6da63c38de1b0,
    0x579c487e5a38ad0e,
    0x2d835a9df0c6d851,
    0xf8e431456cf88e65,
    0x1b8e9ecb641b58ff,
    0xe272467e3d222f3f,
    0x5b0ed81dcc6abb0f,
    0x98e947129fc2b4e9,
    0x3f2398d747b36224,
    0x8eec7f0d19a03aad,
    0x1953cf68300424ac,
    0x5fa8c3423c052dd7,
    0x3792f412cb06794d,
    0xe2bbd88bbee40bd0,
    0x5b6aceaeae9d0ec4,
    0xf245825a5a445275,
    0xeed6e2f0f0d56712,
    0x55464dd69685606b,
    0xaa97e14c3c26b886,
    0xd53dd99f4b3066a8,
    0xe546a8038efe4029,
    0xde98520472bdd033,
    0x963e66858f6d4440,
    0xdde7001379a44aa8,
    0x5560c018580d5d52,
    0xaab8f01e6e10b4a6,
    0xcab3961304ca70e8,
    0x3d607b97c5fd0d22,
    0x8cb89a7db77c506a,
    0x77f3608e92adb242,
    0x55f038b237591ed3,
    0x6b6c46dec52f6688,
    0x2323ac4b3b3da015,
    0xabec975e0a0d081a,
    0x96e7bd358c904a21,
    0x7e50d64177da2e54,
    0xdde50bd1d5d0b9e9,
    0x955e4ec64b44e864,
    0xbd5af13bef0b113e,
    0xecb1ad8aeacdd58e,
    0x67de18eda5814af2,
    0x80eacf948770ced7,
    0xa1258379a94d028d,
    0x96ee45813a04330,
    0x8bca9d6e188853fc,
    0x775ea264cf55347d,
    0x95364afe032a819d,
    0x3a83ddbd83f52204,
    0xc4926a9672793542,
    0x75b7053c0f178293,
    0x5324c68b12dd6338,
    0xd3f6fc16ebca5e03,
    0x88f4bb1ca6bcf584,
    0x2b31e9e3d06c32e5,
    0x3aff322e62439fcf,
    0x9befeb9fad487c2,
    0x4c2ebe687989a9b3,
    0xf9d37014bf60a10,
    0x538484c19ef38c94,
    0x2865a5f206b06fb9,
    0xf93f87b7442e45d3,
    0xf78f69a51539d748,
    0xb573440e5a884d1b,
    0x31680a88f8953030,
    0xfdc20d2b36ba7c3d,
    0x3d32907604691b4c,
    0xa63f9a49c2c1b10f,
    0xfcf80dc33721d53,
    0xd3c36113404ea4a8,
    0x645a1cac083126e9,
    0x3d70a3d70a3d70a3,
    0xcccccccccccccccc,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x0,
    0x4000000000000000,
    0x5000000000000000,
    0xa400000000000000,
    0x4d00000000000000,
    0xf020000000000000,
    0x6c28000000000000,
    0xc732000000000000,
    0x3c7f400000000000,
    0x4b9f100000000000,
    0x1e86d40000000000,
    0x1314448000000000,
    0x17d955a000000000,
    0x5dcfab0800000000,
    0x5aa1cae500000000,
    0xf14a3d9e40000000,
    0x6d9ccd05d0000000,
    0xe4820023a2000000,
    0xdda2802c8a800000,
    0xd50b2037ad200000,
    0x4526f422cc340000,
    0x9670b12b7f410000,
    0x3c0cdd765f114000,
    0xa5880a69fb6ac800,
    0x8eea0d047a457a00,
    0x72a4904598d6d880,
    0x47a6da2b7f864750,
    0x999090b65f67d924,
    0xfff4b4e3f741cf6d,
    0xbff8f10e7a8921a4,
    0xaff72d52192b6a0d,
    0x9bf4f8a69f764490,
    0x2f236d04753d5b4,
    0x1d762422c946590,
    0x424d3ad2b7b97ef5,
    0xd2e0898765a7deb2,
    0x63cc55f49f88eb2f,
    0x3cbf6b71c76b25fb,
    0x8bef464e3945ef7a,
    0x97758bf0e3cbb5ac,
    0x3d52eeed1cbea317,
    0x4ca7aaa863ee4bdd,
    0x8fe8caa93e74ef6a,
    0xb3e2fd538e122b44,
    0x60dbbca87196b616,
    0xbc8955e946fe31cd,
    0x6babab6398bdbe41,
    0xc696963c7eed2dd1,
    0xfc1e1de5cf543ca2,
    0x3b25a55f43294bcb,
    0x49ef0eb713f39ebe,
    0x6e3569326c784337,
    0x49c2c37f07965404,
    0xdc33745ec97be906,
    0x69a028bb3ded71a3,
    0xc40832ea0d68ce0c,
    0xf50a3fa490c30190,
    0x792667c6da79e0fa,
    0x577001b891185938,
    0xed4c0226b55e6f86,
    0x544f8158315b05b4,
    0x696361ae3db1c721,
    0x3bc3a19cd1e38e9,
    0x4ab48a04065c723,
    0x62eb0d64283f9c76,
    0x3ba5d0bd324f8394,
    0xca8f44ec7ee36479,
    0x7e998b13cf4e1ecb,
    0x9e3fedd8c321a67e,
    0xc5cfe94ef3ea101e,
    0xbba1f1d158724a12,
    0x2a8a6e45ae8edc97,
    0xf52d09d71a3293bd,
    0x593c2626705f9c56,
    0x6f8b2fb00c77836c,
    0xb6dfb9c0f956447,
    0x4724bd4189bd5eac,
    0x58edec91ec2cb657,
    0x2f2967b66737e3ed,
    0xbd79e0d20082ee74,
    0xecd8590680a3aa11,
    0xe80e6f4820cc9495,
    0x3109058d147fdcdd,
    0xbd4b46f0599fd415,
    0x6c9e18ac7007c91a,
    0x3e2cf6bc604ddb0,
    0x84db8346b786151c,
    0xe612641865679a63,
    0x4fcb7e8f3f60c07e,
    0xe3be5e330f38f09d,
    0x5cadf5bfd3072cc5,
    0x73d9732fc7c8f7f6,
    0x2867e7fddcdd9afa,
    0xb281e1fd541501b8,
    0x1f225a7ca91a4226,
    0x3375788de9b06958,
    0x52d6b1641c83ae,
    0xc0678c5dbd23a49a,
    0xf840b7ba963646e0,
    0xb650e5a93bc3d898,
    0xa3e51f138ab4cebe,
    0xc66f336c36b10137,
    0xb80b0047445d4184,
    0xa60dc059157491e5,
    0x87c89837ad68db2f,
    0x29babe4598c311fb,
    0xf4296dd6fef3d67a,
    0x1899e4a65f58660c,
    0x5ec05dcff72e7f8f,
    0x76707543f4fa1f73,
    0x6a06494a791c53a8,
    0x487db9d17636892,
    0x45a9d2845d3c42b6,
    0xb8a2392ba45a9b2,
    0x8e6cac7768d7141e,
    0x3207d795430cd926,
    0x7f44e6bd49e807b8,
    0x5f16206c9c6209a6,
    0x36dba887c37a8c0f,
    0xc2494954da2c9789,
    0xf2db9baa10b7bd6c,
    0x6f92829494e5acc7,
    0xcb772339ba1f17f9,
    0xff2a760414536efb,
    0xfef5138519684aba,
    0x7eb258665fc25d69,
    0xef2f773ffbd97a61,
    0xaafb550ffacfd8fa,
    0x95ba2a53f983cf38,
    0xdd945a747bf26183,
    0x94f971119aeef9e4,
    0x7a37cd5601aab85d,
    0xac62e055c10ab33a,
    0x577b986b314d6009,
    0xed5a7e85fda0b80b,
    0x14588f13be847307,
    0x596eb2d8ae258fc8,
    0x6fca5f8ed9aef3bb,
    0x25de7bb9480d5854,
    0xaf561aa79a10ae6a,
    0x1b2ba1518094da04,
    0x90fb44d2f05d0842,
    0x353a1607ac744a53,
    0x42889b8997915ce8,
    0x69956135febada11,
    0x43fab9837e699095,
    0x94f967e45e03f4bb,
    0x1d1be0eebac278f5,
    0x6462d92a69731732,
    0x7d7b8f7503cfdcfe,
    0x5cda735244c3d43e,
    0x3a0888136afa64a7,
    0x88aaa1845b8fdd0,
    0x8aad549e57273d45,
    0x36ac54e2f678864b,
    0x84576a1bb416a7dd,
    0x656d44a2a11c51d5,
    0x9f644ae5a4b1b325,
    0x873d5d9f0dde1fee,
    0xa90cb506d155a7ea,
    0x9a7f12442d588f2,
    0xc11ed6d538aeb2f,
    0x8f1668c8a86da5fa,
    0xf96e017d694487bc,
    0x37c981dcc395a9ac,
    0x85bbe253f47b1417,
    0x93956d7478ccec8e,
    0x387ac8d1970027b2,
    0x6997b05fcc0319e,
    0x441fece3bdf81f03,
    0xd527e81cad7626c3,
    0x8a71e223d8d3b074,
    0xf6872d5667844e49,
    0xb428f8ac016561db,
    0xe13336d701beba52,
    0xecc0024661173473,
    0x27f002d7f95d0190,
    0x31ec038df7b441f4,
    0x7e67047175a15271,
    0xf0062c6e984d386,
    0x52c07b78a3e60868,
    0xa7709a56ccdf8a82,
    0x88a66076400bb691,
    0x6acff893d00ea435,
    0x583f6b8c4124d43,
    0xc3727a337a8b704a,
    0x744f18c0592e4c5c,
    0x1162def06f79df73,
    0x8addcb5645ac2ba8,
    0x6d953e2bd7173692,
    0xc8fa8db6ccdd0437,
    0x1d9c9892400a22a2,
    0x2503beb6d00cab4b,
    0x2e44ae64840fd61d,
    0x5ceaecfed289e5d2,
    0x7425a83e872c5f47,
    0xd12f124e28f77719,
    0x82bd6b70d99aaa6f,
    0x636cc64d1001550b,
    0x3c47f7e05401aa4e,
    0x65acfaec34810a71,
    0x7f1839a741a14d0d,
    0x1ede48111209a050,
    0x934aed0aab460432,
    0xf81da84d5617853f,
    0x36251260ab9d668e,
    0xc1d72b7c6b426019,
    0xb24cf65b8612f81f,
    0xdee033f26797b627,
    0x169840ef017da3b1,
    0x8e1f289560ee864e,
    0xf1a6f2bab92a27e2,
    0xae10af696774b1db,
    0xacca6da1e0a8ef29,
    0x17fd090a58d32af3,
    0xddfc4b4cef07f5b0,
    0x4abdaf101564f98e,
    0x9d6d1ad41abe37f1,
    0x84c86189216dc5ed,
    0x32fd3cf5b4e49bb4,
    0x3fbc8c33221dc2a1,
    0xfabaf3feaa5334a,
    0x29cb4d87f2a7400e,
    0x743e20e9ef511012,
    0x914da9246b255416,
    0x1ad089b6c2f7548e,
    0xa184ac2473b529b1,
    0xc9e5d72d90a2741e,
    0x7e2fa67c7a658892,
    0xddbb901b98feeab7,
    0x552a74227f3ea565,
    0xd53a88958f87275f,
    0x8a892abaf368f137,
    0x2d2b7569b0432d85,
    0x9c3b29620e29fc73,
    0x8349f3ba91b47b8f,
    0x241c70a936219a73,
    0xed238cd383aa0110,
    0xf4363804324a40aa,
    0xb143c6053edcd0d5,
    0xdd94b7868e94050a,
    0xca7cf2b4191c8326,
    0xfd1c2f611f63a3f0,
    0xbc633b39673c8cec,
    0xd5be0503e085d813,
    0x4b2d8644d8a74e18,
    0xddf8e7d60ed1219e,
    0xcabb90e5c942b503,
    0x3d6a751f3b936243,
    0xcc512670a783ad4,
    0x27fb2b80668b24c5,
    0xb1f9f660802dedf6,
    0x5e7873f8a0396973,
    0xdb0b487b6423e1e8,
    0x91ce1a9a3d2cda62,
    0x7641a140cc7810fb,
    0xa9e904c87fcb0a9d,
    0x546345fa9fbdcd44,
    0xa97c177947ad4095,
    0x49ed8eabcccc485d,
    0x5c68f256bfff5a74,
    0x73832eec6fff3111,
    0xc831fd53c5ff7eab,
    0xba3e7ca8b77f5e55,
    0x28ce1bd2e55f35eb,
    0x7980d163cf5b81b3,
    0xd7e105bcc332621f,
    0x8dd9472bf3fefaa7,
    0xb14f98f6f0feb951,
    0x6ed1bf9a569f33d3,
    0xa862f80ec4700c8,
    0xcd27bb612758c0fa,
    0x8038d51cb897789c,
    0xe0470a63e6bd56c3,
    0x1858ccfce06cac74,
    0xf37801e0c43ebc8,
    0xd30560258f54e6ba,
    0x47c6b82ef32a2069,
    0x4cdc331d57fa5441,
    0xe0133fe4adf8e952,
    0x58180fddd97723a6,
    0x570f09eaa7ea7648,
];

static POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

macro_rules! deserialize_prim_number {
    ($method:ident) => {
        fn $method<V>(self, visitor: V) -> Result<V::Value>
        where
            V: de::Visitor<'de>,
        {
            self.deserialize_prim_number(visitor)
        }
    };
}

#[cfg(not(feature = "unbounded_depth"))]
macro_rules! if_checking_recursion_limit {
    ($($body:tt)*) => {
        $($body)*
    };
}

#[cfg(feature = "unbounded_depth")]
macro_rules! if_checking_recursion_limit {
    ($this:ident $($body:tt)*) => {
        if !$this.disable_recursion_limit {
            $this $($body)*
        }
    };
}

macro_rules! check_recursion {
    ($this:ident $($body:tt)*) => {
        if_checking_recursion_limit! {
            $this.remaining_depth -= 1;
            if $this.remaining_depth == 0 {
                return Err($this.peek_error(ErrorCode::RecursionLimitExceeded));
            }
        }

        $this $($body)*

        if_checking_recursion_limit! {
            $this.remaining_depth += 1;
        }
    };
}

impl<'de, 'a, R: Read<'de>> de::Deserializer<'de> for &'a mut Deserializer<R> {
    type Error = Error;

    #[inline]
    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let peek = match tri!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'n' => {
                self.eat_char();
                tri!(self.parse_ident(b"ull"));
                visitor.visit_unit()
            }
            b't' => {
                self.eat_char();
                tri!(self.parse_ident(b"rue"));
                visitor.visit_bool(true)
            }
            b'f' => {
                self.eat_char();
                tri!(self.parse_ident(b"alse"));
                visitor.visit_bool(false)
            }
            b'-' => {
                self.eat_char();
                tri!(self.parse_any_number(false)).visit(visitor)
            }
            b'0'..=b'9' => tri!(self.parse_any_number(true)).visit(visitor),
            b'"' => {
                self.eat_char();
                self.scratch.clear();
                match tri!(self.read.parse_str(&mut self.scratch)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            }
            b'[' => {
                check_recursion! {
                    self.eat_char();
                    let ret = visitor.visit_seq(SeqAccess::new(self));
                }

                match (ret, self.end_seq()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            b'{' => {
                check_recursion! {
                    self.eat_char();
                    let ret = visitor.visit_map(MapAccess::new(self));
                }

                match (ret, self.end_map()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            _ => Err(self.peek_error(ErrorCode::ExpectedSomeValue)),
        };

        match value {
            Ok(value) => Ok(value),
            // The de::Error impl creates errors with unknown line and column.
            // Fill in the position here by looking at the current index in the
            // input. There is no way to tell whether this should call `error`
            // or `peek_error` so pick the one that seems correct more often.
            // Worst case, the position is off by one character.
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let peek = match tri!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b't' => {
                self.eat_char();
                tri!(self.parse_ident(b"rue"));
                visitor.visit_bool(true)
            }
            b'f' => {
                self.eat_char();
                tri!(self.parse_ident(b"alse"));
                visitor.visit_bool(false)
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    deserialize_prim_number!(deserialize_i8);
    deserialize_prim_number!(deserialize_i16);
    deserialize_prim_number!(deserialize_i32);
    deserialize_prim_number!(deserialize_i64);
    deserialize_prim_number!(deserialize_u8);
    deserialize_prim_number!(deserialize_u16);
    deserialize_prim_number!(deserialize_u32);
    deserialize_prim_number!(deserialize_u64);
    deserialize_prim_number!(deserialize_f32);
    deserialize_prim_number!(deserialize_f64);

    serde_if_integer128! {
        fn deserialize_i128<V>(self, visitor: V) -> Result<V::Value>
        where
            V: de::Visitor<'de>,
        {
            let mut buf = String::new();

            match tri!(self.parse_whitespace()) {
                Some(b'-') => {
                    self.eat_char();
                    buf.push('-');
                }
                Some(_) => {}
                None => {
                    return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
                }
            };

            tri!(self.scan_integer128(&mut buf));

            let value = match buf.parse() {
                Ok(int) => visitor.visit_i128(int),
                Err(_) => {
                    return Err(self.error(ErrorCode::NumberOutOfRange));
                }
            };

            match value {
                Ok(value) => Ok(value),
                Err(err) => Err(self.fix_position(err)),
            }
        }

        fn deserialize_u128<V>(self, visitor: V) -> Result<V::Value>
        where
            V: de::Visitor<'de>,
        {
            match tri!(self.parse_whitespace()) {
                Some(b'-') => {
                    return Err(self.peek_error(ErrorCode::NumberOutOfRange));
                }
                Some(_) => {}
                None => {
                    return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
                }
            }

            let mut buf = String::new();
            tri!(self.scan_integer128(&mut buf));

            let value = match buf.parse() {
                Ok(int) => visitor.visit_u128(int),
                Err(_) => {
                    return Err(self.error(ErrorCode::NumberOutOfRange));
                }
            };

            match value {
                Ok(value) => Ok(value),
                Err(err) => Err(self.fix_position(err)),
            }
        }
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let peek = match tri!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'"' => {
                self.eat_char();
                self.scratch.clear();
                match tri!(self.read.parse_str(&mut self.scratch)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    /// Parses a JSON string as bytes. Note that this function does not check
    /// whether the bytes represent a valid UTF-8 string.
    ///
    /// The relevant part of the JSON specification is Section 8.2 of [RFC
    /// 7159]:
    ///
    /// > When all the strings represented in a JSON text are composed entirely
    /// > of Unicode characters (however escaped), then that JSON text is
    /// > interoperable in the sense that all software implementations that
    /// > parse it will agree on the contents of names and of string values in
    /// > objects and arrays.
    /// >
    /// > However, the ABNF in this specification allows member names and string
    /// > values to contain bit sequences that cannot encode Unicode characters;
    /// > for example, "\uDEAD" (a single unpaired UTF-16 surrogate). Instances
    /// > of this have been observed, for example, when a library truncates a
    /// > UTF-16 string without checking whether the truncation split a
    /// > surrogate pair.  The behavior of software that receives JSON texts
    /// > containing such values is unpredictable; for example, implementations
    /// > might return different values for the length of a string value or even
    /// > suffer fatal runtime exceptions.
    ///
    /// [RFC 7159]: https://tools.ietf.org/html/rfc7159
    ///
    /// The behavior of serde_json is specified to fail on non-UTF-8 strings
    /// when deserializing into Rust UTF-8 string types such as String, and
    /// succeed with non-UTF-8 bytes when deserializing using this method.
    ///
    /// Escape sequences are processed as usual, and for `\uXXXX` escapes it is
    /// still checked if the hex number represents a valid Unicode code point.
    ///
    /// # Examples
    ///
    /// You can use this to parse JSON strings containing invalid UTF-8 bytes.
    ///
    /// ```
    /// use serde_bytes::ByteBuf;
    ///
    /// fn look_at_bytes() -> Result<(), serde_json::Error> {
    ///     let json_data = b"\"some bytes: \xe5\x00\xe5\"";
    ///     let bytes: ByteBuf = serde_json::from_slice(json_data)?;
    ///
    ///     assert_eq!(b'\xe5', bytes[12]);
    ///     assert_eq!(b'\0', bytes[13]);
    ///     assert_eq!(b'\xe5', bytes[14]);
    ///
    ///     Ok(())
    /// }
    /// #
    /// # look_at_bytes().unwrap();
    /// ```
    ///
    /// Backslash escape sequences like `\n` are still interpreted and required
    /// to be valid, and `\u` escape sequences are required to represent valid
    /// Unicode code points.
    ///
    /// ```
    /// use serde_bytes::ByteBuf;
    ///
    /// fn look_at_bytes() {
    ///     let json_data = b"\"invalid unicode surrogate: \\uD801\"";
    ///     let parsed: Result<ByteBuf, _> = serde_json::from_slice(json_data);
    ///
    ///     assert!(parsed.is_err());
    ///
    ///     let expected_msg = "unexpected end of hex escape at line 1 column 35";
    ///     assert_eq!(expected_msg, parsed.unwrap_err().to_string());
    /// }
    /// #
    /// # look_at_bytes();
    /// ```
    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let peek = match tri!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'"' => {
                self.eat_char();
                self.scratch.clear();
                match tri!(self.read.parse_str_raw(&mut self.scratch)) {
                    Reference::Borrowed(b) => visitor.visit_borrowed_bytes(b),
                    Reference::Copied(b) => visitor.visit_bytes(b),
                }
            }
            b'[' => self.deserialize_seq(visitor),
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    #[inline]
    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_bytes(visitor)
    }

    /// Parses a `null` as a None, and any other values as a `Some(...)`.
    #[inline]
    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        match tri!(self.parse_whitespace()) {
            Some(b'n') => {
                self.eat_char();
                tri!(self.parse_ident(b"ull"));
                visitor.visit_none()
            }
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let peek = match tri!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'n' => {
                self.eat_char();
                tri!(self.parse_ident(b"ull"));
                visitor.visit_unit()
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_unit_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_unit(visitor)
    }

    /// Parses a newtype struct as the underlying value.
    #[inline]
    fn deserialize_newtype_struct<V>(self, name: &str, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        #[cfg(feature = "raw_value")]
        {
            if name == crate::raw::TOKEN {
                return self.deserialize_raw_value(visitor);
            }
        }

        let _ = name;
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let peek = match tri!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'[' => {
                check_recursion! {
                    self.eat_char();
                    let ret = visitor.visit_seq(SeqAccess::new(self));
                }

                match (ret, self.end_seq()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let peek = match tri!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'{' => {
                check_recursion! {
                    self.eat_char();
                    let ret = visitor.visit_map(MapAccess::new(self));
                }

                match (ret, self.end_map()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let peek = match tri!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'[' => {
                check_recursion! {
                    self.eat_char();
                    let ret = visitor.visit_seq(SeqAccess::new(self));
                }

                match (ret, self.end_seq()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            b'{' => {
                check_recursion! {
                    self.eat_char();
                    let ret = visitor.visit_map(MapAccess::new(self));
                }

                match (ret, self.end_map()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    /// Parses an enum as an object like `{"$KEY":$VALUE}`, where $VALUE is either a straight
    /// value, a `[..]`, or a `{..}`.
    #[inline]
    fn deserialize_enum<V>(
        self,
        _name: &str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        match tri!(self.parse_whitespace()) {
            Some(b'{') => {
                check_recursion! {
                    self.eat_char();
                    let value = tri!(visitor.visit_enum(VariantAccess::new(self)));
                }

                match tri!(self.parse_whitespace()) {
                    Some(b'}') => {
                        self.eat_char();
                        Ok(value)
                    }
                    Some(_) => Err(self.error(ErrorCode::ExpectedSomeValue)),
                    None => Err(self.error(ErrorCode::EofWhileParsingObject)),
                }
            }
            Some(b'"') => visitor.visit_enum(UnitVariantAccess::new(self)),
            Some(_) => Err(self.peek_error(ErrorCode::ExpectedSomeValue)),
            None => Err(self.peek_error(ErrorCode::EofWhileParsingValue)),
        }
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        tri!(self.ignore_value());
        visitor.visit_unit()
    }
}

struct SeqAccess<'a, R: 'a> {
    de: &'a mut Deserializer<R>,
    first: bool,
}

impl<'a, R: 'a> SeqAccess<'a, R> {
    fn new(de: &'a mut Deserializer<R>) -> Self {
        SeqAccess {
            de: de,
            first: true,
        }
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::SeqAccess<'de> for SeqAccess<'a, R> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>>
    where
        T: de::DeserializeSeed<'de>,
    {
        let peek = match tri!(self.de.parse_whitespace()) {
            Some(b']') => {
                return Ok(None);
            }
            Some(b',') if !self.first => {
                self.de.eat_char();
                tri!(self.de.parse_whitespace())
            }
            Some(b) => {
                if self.first {
                    self.first = false;
                    Some(b)
                } else {
                    return Err(self.de.peek_error(ErrorCode::ExpectedListCommaOrEnd));
                }
            }
            None => {
                return Err(self.de.peek_error(ErrorCode::EofWhileParsingList));
            }
        };

        match peek {
            Some(b']') => Err(self.de.peek_error(ErrorCode::TrailingComma)),
            Some(_) => Ok(Some(tri!(seed.deserialize(&mut *self.de)))),
            None => Err(self.de.peek_error(ErrorCode::EofWhileParsingValue)),
        }
    }
}

struct MapAccess<'a, R: 'a> {
    de: &'a mut Deserializer<R>,
    first: bool,
}

impl<'a, R: 'a> MapAccess<'a, R> {
    fn new(de: &'a mut Deserializer<R>) -> Self {
        MapAccess {
            de: de,
            first: true,
        }
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::MapAccess<'de> for MapAccess<'a, R> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>>
    where
        K: de::DeserializeSeed<'de>,
    {
        let peek = match tri!(self.de.parse_whitespace()) {
            Some(b'}') => {
                return Ok(None);
            }
            Some(b',') if !self.first => {
                self.de.eat_char();
                tri!(self.de.parse_whitespace())
            }
            Some(b) => {
                if self.first {
                    self.first = false;
                    Some(b)
                } else {
                    return Err(self.de.peek_error(ErrorCode::ExpectedObjectCommaOrEnd));
                }
            }
            None => {
                return Err(self.de.peek_error(ErrorCode::EofWhileParsingObject));
            }
        };

        match peek {
            Some(b'"') => seed.deserialize(MapKey { de: &mut *self.de }).map(Some),
            Some(b'}') => Err(self.de.peek_error(ErrorCode::TrailingComma)),
            Some(_) => Err(self.de.peek_error(ErrorCode::KeyMustBeAString)),
            None => Err(self.de.peek_error(ErrorCode::EofWhileParsingValue)),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value>
    where
        V: de::DeserializeSeed<'de>,
    {
        tri!(self.de.parse_object_colon());

        seed.deserialize(&mut *self.de)
    }
}

struct VariantAccess<'a, R: 'a> {
    de: &'a mut Deserializer<R>,
}

impl<'a, R: 'a> VariantAccess<'a, R> {
    fn new(de: &'a mut Deserializer<R>) -> Self {
        VariantAccess { de: de }
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::EnumAccess<'de> for VariantAccess<'a, R> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self)>
    where
        V: de::DeserializeSeed<'de>,
    {
        let val = tri!(seed.deserialize(&mut *self.de));
        tri!(self.de.parse_object_colon());
        Ok((val, self))
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::VariantAccess<'de> for VariantAccess<'a, R> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        de::Deserialize::deserialize(self.de)
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value>
    where
        T: de::DeserializeSeed<'de>,
    {
        seed.deserialize(self.de)
    }

    fn tuple_variant<V>(self, _len: usize, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        de::Deserializer::deserialize_seq(self.de, visitor)
    }

    fn struct_variant<V>(self, fields: &'static [&'static str], visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        de::Deserializer::deserialize_struct(self.de, "", fields, visitor)
    }
}

struct UnitVariantAccess<'a, R: 'a> {
    de: &'a mut Deserializer<R>,
}

impl<'a, R: 'a> UnitVariantAccess<'a, R> {
    fn new(de: &'a mut Deserializer<R>) -> Self {
        UnitVariantAccess { de: de }
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::EnumAccess<'de> for UnitVariantAccess<'a, R> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self)>
    where
        V: de::DeserializeSeed<'de>,
    {
        let variant = tri!(seed.deserialize(&mut *self.de));
        Ok((variant, self))
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::VariantAccess<'de> for UnitVariantAccess<'a, R> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, _seed: T) -> Result<T::Value>
    where
        T: de::DeserializeSeed<'de>,
    {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"newtype variant",
        ))
    }

    fn tuple_variant<V>(self, _len: usize, _visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"tuple variant",
        ))
    }

    fn struct_variant<V>(self, _fields: &'static [&'static str], _visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"struct variant",
        ))
    }
}

/// Only deserialize from this after peeking a '"' byte! Otherwise it may
/// deserialize invalid JSON successfully.
struct MapKey<'a, R: 'a> {
    de: &'a mut Deserializer<R>,
}

macro_rules! deserialize_integer_key {
    ($method:ident => $visit:ident) => {
        fn $method<V>(self, visitor: V) -> Result<V::Value>
        where
            V: de::Visitor<'de>,
        {
            self.de.eat_char();
            self.de.scratch.clear();
            let string = tri!(self.de.read.parse_str(&mut self.de.scratch));
            match (string.parse(), string) {
                (Ok(integer), _) => visitor.$visit(integer),
                (Err(_), Reference::Borrowed(s)) => visitor.visit_borrowed_str(s),
                (Err(_), Reference::Copied(s)) => visitor.visit_str(s),
            }
        }
    };
}

impl<'de, 'a, R> de::Deserializer<'de> for MapKey<'a, R>
where
    R: Read<'de>,
{
    type Error = Error;

    #[inline]
    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.de.eat_char();
        self.de.scratch.clear();
        match tri!(self.de.read.parse_str(&mut self.de.scratch)) {
            Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
            Reference::Copied(s) => visitor.visit_str(s),
        }
    }

    deserialize_integer_key!(deserialize_i8 => visit_i8);
    deserialize_integer_key!(deserialize_i16 => visit_i16);
    deserialize_integer_key!(deserialize_i32 => visit_i32);
    deserialize_integer_key!(deserialize_i64 => visit_i64);
    deserialize_integer_key!(deserialize_u8 => visit_u8);
    deserialize_integer_key!(deserialize_u16 => visit_u16);
    deserialize_integer_key!(deserialize_u32 => visit_u32);
    deserialize_integer_key!(deserialize_u64 => visit_u64);

    serde_if_integer128! {
        deserialize_integer_key!(deserialize_i128 => visit_i128);
        deserialize_integer_key!(deserialize_u128 => visit_u128);
    }

    #[inline]
    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        // Map keys cannot be null.
        visitor.visit_some(self)
    }

    #[inline]
    fn deserialize_newtype_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    #[inline]
    fn deserialize_enum<V>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.de.deserialize_enum(name, variants, visitor)
    }

    #[inline]
    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.de.deserialize_bytes(visitor)
    }

    #[inline]
    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.de.deserialize_bytes(visitor)
    }

    forward_to_deserialize_any! {
        bool f32 f64 char str string unit unit_struct seq tuple tuple_struct map
        struct identifier ignored_any
    }
}

//////////////////////////////////////////////////////////////////////////////

/// Iterator that deserializes a stream into multiple JSON values.
///
/// A stream deserializer can be created from any JSON deserializer using the
/// `Deserializer::into_iter` method.
///
/// The data can consist of any JSON value. Values need to be a self-delineating value e.g.
/// arrays, objects, or strings, or be followed by whitespace or a self-delineating value.
///
/// ```
/// use serde_json::{Deserializer, Value};
///
/// fn main() {
///     let data = "{\"k\": 3}1\"cool\"\"stuff\" 3{}  [0, 1, 2]";
///
///     let stream = Deserializer::from_str(data).into_iter::<Value>();
///
///     for value in stream {
///         println!("{}", value.unwrap());
///     }
/// }
/// ```
pub struct StreamDeserializer<'de, R, T> {
    de: Deserializer<R>,
    offset: usize,
    failed: bool,
    output: PhantomData<T>,
    lifetime: PhantomData<&'de ()>,
}

impl<'de, R, T> StreamDeserializer<'de, R, T>
where
    R: read::Read<'de>,
    T: de::Deserialize<'de>,
{
    /// Create a JSON stream deserializer from one of the possible serde_json
    /// input sources.
    ///
    /// Typically it is more convenient to use one of these methods instead:
    ///
    ///   - Deserializer::from_str(...).into_iter()
    ///   - Deserializer::from_bytes(...).into_iter()
    ///   - Deserializer::from_reader(...).into_iter()
    pub fn new(read: R) -> Self {
        let offset = read.byte_offset();
        StreamDeserializer {
            de: Deserializer::new(read),
            offset: offset,
            failed: false,
            output: PhantomData,
            lifetime: PhantomData,
        }
    }

    /// Returns the number of bytes so far deserialized into a successful `T`.
    ///
    /// If a stream deserializer returns an EOF error, new data can be joined to
    /// `old_data[stream.byte_offset()..]` to try again.
    ///
    /// ```
    /// let data = b"[0] [1] [";
    ///
    /// let de = serde_json::Deserializer::from_slice(data);
    /// let mut stream = de.into_iter::<Vec<i32>>();
    /// assert_eq!(0, stream.byte_offset());
    ///
    /// println!("{:?}", stream.next()); // [0]
    /// assert_eq!(3, stream.byte_offset());
    ///
    /// println!("{:?}", stream.next()); // [1]
    /// assert_eq!(7, stream.byte_offset());
    ///
    /// println!("{:?}", stream.next()); // error
    /// assert_eq!(8, stream.byte_offset());
    ///
    /// // If err.is_eof(), can join the remaining data to new data and continue.
    /// let remaining = &data[stream.byte_offset()..];
    /// ```
    ///
    /// *Note:* In the future this method may be changed to return the number of
    /// bytes so far deserialized into a successful T *or* syntactically valid
    /// JSON skipped over due to a type error. See [serde-rs/json#70] for an
    /// example illustrating this.
    ///
    /// [serde-rs/json#70]: https://github.com/serde-rs/json/issues/70
    pub fn byte_offset(&self) -> usize {
        self.offset
    }

    fn peek_end_of_value(&mut self) -> Result<()> {
        match tri!(self.de.peek()) {
            Some(b' ') | Some(b'\n') | Some(b'\t') | Some(b'\r') | Some(b'"') | Some(b'[')
            | Some(b']') | Some(b'{') | Some(b'}') | Some(b',') | Some(b':') | None => Ok(()),
            Some(_) => {
                let position = self.de.read.peek_position();
                Err(Error::syntax(
                    ErrorCode::TrailingCharacters,
                    position.line,
                    position.column,
                ))
            }
        }
    }
}

impl<'de, R, T> Iterator for StreamDeserializer<'de, R, T>
where
    R: Read<'de>,
    T: de::Deserialize<'de>,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Result<T>> {
        if R::should_early_return_if_failed && self.failed {
            return None;
        }

        // skip whitespaces, if any
        // this helps with trailing whitespaces, since whitespaces between
        // values are handled for us.
        match self.de.parse_whitespace() {
            Ok(None) => {
                self.offset = self.de.read.byte_offset();
                None
            }
            Ok(Some(b)) => {
                // If the value does not have a clear way to show the end of the value
                // (like numbers, null, true etc.) we have to look for whitespace or
                // the beginning of a self-delineated value.
                let self_delineated_value = match b {
                    b'[' | b'"' | b'{' => true,
                    _ => false,
                };
                self.offset = self.de.read.byte_offset();
                let result = de::Deserialize::deserialize(&mut self.de);

                Some(match result {
                    Ok(value) => {
                        self.offset = self.de.read.byte_offset();
                        if self_delineated_value {
                            Ok(value)
                        } else {
                            self.peek_end_of_value().map(|_| value)
                        }
                    }
                    Err(e) => {
                        self.de.read.set_failed(&mut self.failed);
                        Err(e)
                    }
                })
            }
            Err(e) => {
                self.de.read.set_failed(&mut self.failed);
                Some(Err(e))
            }
        }
    }
}

impl<'de, R, T> FusedIterator for StreamDeserializer<'de, R, T>
where
    R: Read<'de> + Fused,
    T: de::Deserialize<'de>,
{
}

//////////////////////////////////////////////////////////////////////////////

fn from_trait<'de, R, T>(read: R) -> Result<T>
where
    R: Read<'de>,
    T: de::Deserialize<'de>,
{
    let mut de = Deserializer::new(read);
    let value = tri!(de::Deserialize::deserialize(&mut de));

    // Make sure the whole stream has been consumed.
    tri!(de.end());
    Ok(value)
}

/// Deserialize an instance of type `T` from an IO stream of JSON.
///
/// The content of the IO stream is deserialized directly from the stream
/// without being buffered in memory by serde_json.
///
/// When reading from a source against which short reads are not efficient, such
/// as a [`File`], you will want to apply your own buffering because serde_json
/// will not buffer the input. See [`std::io::BufReader`].
///
/// It is expected that the input stream ends after the deserialized object.
/// If the stream does not end, such as in the case of a persistent socket connection,
/// this function will not return. It is possible instead to deserialize from a prefix of an input
/// stream without looking for EOF by managing your own [`Deserializer`].
///
/// Note that counter to intuition, this function is usually slower than
/// reading a file completely into memory and then applying [`from_str`]
/// or [`from_slice`] on it. See [issue #160].
///
/// [`File`]: https://doc.rust-lang.org/std/fs/struct.File.html
/// [`std::io::BufReader`]: https://doc.rust-lang.org/std/io/struct.BufReader.html
/// [`from_str`]: ./fn.from_str.html
/// [`from_slice`]: ./fn.from_slice.html
/// [issue #160]: https://github.com/serde-rs/json/issues/160
///
/// # Example
///
/// Reading the contents of a file.
///
/// ```
/// use serde::Deserialize;
///
/// use std::error::Error;
/// use std::fs::File;
/// use std::io::BufReader;
/// use std::path::Path;
///
/// #[derive(Deserialize, Debug)]
/// struct User {
///     fingerprint: String,
///     location: String,
/// }
///
/// fn read_user_from_file<P: AsRef<Path>>(path: P) -> Result<User, Box<Error>> {
///     // Open the file in read-only mode with buffer.
///     let file = File::open(path)?;
///     let reader = BufReader::new(file);
///
///     // Read the JSON contents of the file as an instance of `User`.
///     let u = serde_json::from_reader(reader)?;
///
///     // Return the `User`.
///     Ok(u)
/// }
///
/// fn main() {
/// # }
/// # fn fake_main() {
///     let u = read_user_from_file("test.json").unwrap();
///     println!("{:#?}", u);
/// }
/// ```
///
/// Reading from a persistent socket connection.
///
/// ```
/// use serde::Deserialize;
///
/// use std::error::Error;
/// use std::net::{TcpListener, TcpStream};
///
/// #[derive(Deserialize, Debug)]
/// struct User {
///     fingerprint: String,
///     location: String,
/// }
///
/// fn read_user_from_stream(tcp_stream: TcpStream) -> Result<User, Box<dyn Error>> {
///     let mut de = serde_json::Deserializer::from_reader(tcp_stream);
///     let u = User::deserialize(&mut de)?;
///
///     Ok(u)
/// }
///
/// fn main() {
/// # }
/// # fn fake_main() {
///     let listener = TcpListener::bind("127.0.0.1:4000").unwrap();
///
///     for stream in listener.incoming() {
///         println!("{:#?}", read_user_from_stream(stream.unwrap()));
///     }
/// }
/// ```
///
/// # Errors
///
/// This conversion can fail if the structure of the input does not match the
/// structure expected by `T`, for example if `T` is a struct type but the input
/// contains something other than a JSON map. It can also fail if the structure
/// is correct but `T`'s implementation of `Deserialize` decides that something
/// is wrong with the data, for example required struct fields are missing from
/// the JSON map or some number is too big to fit in the expected primitive
/// type.
#[cfg(feature = "std")]
pub fn from_reader<R, T>(rdr: R) -> Result<T>
where
    R: crate::io::Read,
    T: de::DeserializeOwned,
{
    from_trait(read::IoRead::new(rdr))
}

/// Deserialize an instance of type `T` from bytes of JSON text.
///
/// # Example
///
/// ```
/// use serde::Deserialize;
///
/// #[derive(Deserialize, Debug)]
/// struct User {
///     fingerprint: String,
///     location: String,
/// }
///
/// fn main() {
///     // The type of `j` is `&[u8]`
///     let j = b"
///         {
///             \"fingerprint\": \"0xF9BA143B95FF6D82\",
///             \"location\": \"Menlo Park, CA\"
///         }";
///
///     let u: User = serde_json::from_slice(j).unwrap();
///     println!("{:#?}", u);
/// }
/// ```
///
/// # Errors
///
/// This conversion can fail if the structure of the input does not match the
/// structure expected by `T`, for example if `T` is a struct type but the input
/// contains something other than a JSON map. It can also fail if the structure
/// is correct but `T`'s implementation of `Deserialize` decides that something
/// is wrong with the data, for example required struct fields are missing from
/// the JSON map or some number is too big to fit in the expected primitive
/// type.
pub fn from_slice<'a, T>(v: &'a [u8]) -> Result<T>
where
    T: de::Deserialize<'a>,
{
    from_trait(read::SliceRead::new(v))
}

/// Deserialize an instance of type `T` from a string of JSON text.
///
/// # Example
///
/// ```
/// use serde::Deserialize;
///
/// #[derive(Deserialize, Debug)]
/// struct User {
///     fingerprint: String,
///     location: String,
/// }
///
/// fn main() {
///     // The type of `j` is `&str`
///     let j = "
///         {
///             \"fingerprint\": \"0xF9BA143B95FF6D82\",
///             \"location\": \"Menlo Park, CA\"
///         }";
///
///     let u: User = serde_json::from_str(j).unwrap();
///     println!("{:#?}", u);
/// }
/// ```
///
/// # Errors
///
/// This conversion can fail if the structure of the input does not match the
/// structure expected by `T`, for example if `T` is a struct type but the input
/// contains something other than a JSON map. It can also fail if the structure
/// is correct but `T`'s implementation of `Deserialize` decides that something
/// is wrong with the data, for example required struct fields are missing from
/// the JSON map or some number is too big to fit in the expected primitive
/// type.
pub fn from_str<'a, T>(s: &'a str) -> Result<T>
where
    T: de::Deserialize<'a>,
{
    from_trait(read::StrRead::new(s))
}
