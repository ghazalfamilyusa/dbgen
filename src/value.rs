//! Values

use chrono::{Duration, NaiveDateTime};
use rand_regex::EncodedString;
use std::{
    cmp::Ordering,
    convert::{TryFrom, TryInto},
    fmt,
};

use crate::{
    array::Array,
    bytes::ByteString,
    error::Error,
    number::{Number, NumberError},
};

/// The string format of an SQL timestamp.
pub const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.f";

/// A scalar value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Null.
    Null,
    /// A number.
    Number(Number),
    /// A string or byte string.
    Bytes(ByteString),
    /// A timestamp. The `NaiveDateTime` field must be in the UTC time zone.
    Timestamp(NaiveDateTime),
    /// A time interval, as multiple of microseconds.
    Interval(i64),
    /// An array of values. The array may be lazily evaluated.
    Array(Array),
}

impl Default for Value {
    fn default() -> Self {
        Self::Null
    }
}

macro_rules! try_or_overflow {
    ($e:expr, $($fmt:tt)+) => {
        if let Some(e) = $e {
            e
        } else {
            return Err(Error::IntegerOverflow(format!($($fmt)+)));
        }
    }
}

macro_rules! try_from_number {
    ($e:expr, $($fmt:tt)+) => {
        match $e {
            Ok(n) => Value::Number(n),
            Err(NumberError::NaN) => Value::Null,
            Err(NumberError::Overflow) => return Err(Error::IntegerOverflow(format!($($fmt)+))),
        }
    }
}

macro_rules! try_from_number_into_interval {
    ($e:expr, $($fmt:tt)+) => {
        match $e.and_then(i64::try_from) {
            Ok(n) => Value::Interval(n),
            Err(NumberError::NaN) => Value::Null,
            Err(NumberError::Overflow) => return Err(Error::IntegerOverflow(format!($($fmt)+))),
        }
    }
}

fn try_partial_cmp_by<I, J, F>(a: I, b: J, mut f: F) -> Result<Option<Ordering>, Error>
where
    I: IntoIterator,
    J: IntoIterator<Item = I::Item>,
    F: FnMut(I::Item, I::Item) -> Result<Option<Ordering>, Error>,
{
    let mut a = a.into_iter();
    let mut b = b.into_iter();
    loop {
        match (a.next(), b.next()) {
            (Some(aa), Some(bb)) => match f(aa, bb) {
                Ok(Some(Ordering::Equal)) => {}
                res => return res,
            },
            (aa, bb) => return Ok(aa.is_some().partial_cmp(&bb.is_some())),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::format::Options;
        let mut writer = Vec::new();
        Options::default()
            .write_sql_value(&mut writer, self)
            .map_err(|_| fmt::Error)?;
        let s = String::from_utf8(writer).map_err(|_| fmt::Error)?;
        f.write_str(&s)
    }
}

impl Value {
    /// Creates a timestamp value.
    pub fn new_timestamp(ts: NaiveDateTime) -> Self {
        Self::Timestamp(ts)
    }

    /// Creates a finite floating point value.
    pub(crate) fn from_finite_f64(v: f64) -> Self {
        Self::Number(Number::from_finite_f64(v))
    }

    /// Compares two values using the rules common among SQL implementations.
    ///
    /// * Comparing with NULL always return `None`.
    /// * Numbers and intervals are ordered by value.
    /// * Timestamps are ordered by its UTC value.
    /// * Strings are ordered by UTF-8 binary collation.
    /// * Arrays are ordered lexicographically.
    /// * Comparing between different types are inconsistent among database
    ///     engines, thus this function will just error with `InvalidArguments`.
    pub fn sql_cmp(&self, other: &Self) -> Result<Option<Ordering>, Error> {
        Ok(match (self, other) {
            (Self::Null, _) | (_, Self::Null) => None,
            (Self::Number(a), Self::Number(b)) => a.partial_cmp(b),
            (Self::Bytes(a), Self::Bytes(b)) => a.partial_cmp(b),
            (Self::Timestamp(a), Self::Timestamp(b)) => a.partial_cmp(b),
            (Self::Interval(a), Self::Interval(b)) => a.partial_cmp(b),
            (Self::Array(a), Self::Array(b)) => try_partial_cmp_by(a.iter(), b.iter(), |x, y| x.sql_cmp(&y))?,
            _ => {
                return Err(Error::InvalidArguments(format!("cannot compare {self} with {other}")));
            }
        })
    }

    /// Compares this value with the zero value of its own type.
    pub fn sql_sign(&self) -> Ordering {
        match self {
            Self::Null => Ordering::Equal,
            Self::Number(a) => a.sql_sign(),
            Self::Bytes(a) => true.cmp(&a.is_empty()),
            Self::Timestamp(..) => Ordering::Greater,
            Self::Interval(a) => a.cmp(&0),
            Self::Array(a) => true.cmp(&a.is_empty()),
        }
    }

    /// Negates the value.
    pub fn sql_neg(&self) -> Result<Self, Error> {
        Ok(match self {
            Self::Number(inner) => Self::Number(inner.neg()),
            Self::Interval(inner) => Self::Interval(try_or_overflow!(inner.checked_neg(), "-{inner}us")),
            _ => return Err(Error::InvalidArguments(format!("cannot negate {self}"))),
        })
    }

    /// Adds two values using the rules common among SQL implementations.
    pub fn sql_add(&self, other: &Self) -> Result<Self, Error> {
        Ok(match (self, other) {
            (Self::Number(lhs), Self::Number(rhs)) => try_from_number!(lhs.add(*rhs), "{} + {}", lhs, rhs),
            (Self::Timestamp(ts), Self::Interval(dur)) | (Self::Interval(dur), Self::Timestamp(ts)) => Self::Timestamp(
                try_or_overflow!(ts.checked_add_signed(Duration::microseconds(*dur)), "{ts} + {dur}us"),
            ),
            (Self::Interval(a), Self::Interval(b)) => Self::Interval(try_or_overflow!(a.checked_add(*b), "{a} + {b}")),
            _ => {
                return Err(Error::InvalidArguments(format!("cannot add {self} to {other}")));
            }
        })
    }

    /// Subtracts two values using the rules common among SQL implementations.
    pub fn sql_sub(&self, other: &Self) -> Result<Self, Error> {
        Ok(match (self, other) {
            (Self::Number(lhs), Self::Number(rhs)) => try_from_number!(lhs.sub(*rhs), "{} - {}", lhs, rhs),
            (Self::Timestamp(lhs), Self::Timestamp(rhs)) => Self::Interval(try_or_overflow!(
                lhs.signed_duration_since(*rhs).num_microseconds(),
                "{lhs} - {rhs}"
            )),
            (Self::Timestamp(ts), Self::Interval(dur)) => Self::Timestamp(try_or_overflow!(
                ts.checked_sub_signed(Duration::microseconds(*dur)),
                "{ts} - {dur}us"
            )),
            (Self::Interval(a), Self::Interval(b)) => Self::Interval(try_or_overflow!(a.checked_sub(*b), "{a} - {b}")),
            _ => {
                return Err(Error::InvalidArguments(format!("cannot subtract {self} from {other}")));
            }
        })
    }

    /// Multiplies two values using the rules common among SQL implementations.
    pub fn sql_mul(&self, other: &Self) -> Result<Self, Error> {
        Ok(match (self, other) {
            (Self::Number(lhs), Self::Number(rhs)) => try_from_number!(lhs.mul(*rhs), "{lhs} * {rhs}"),
            (Self::Number(m), Self::Interval(dur)) | (Self::Interval(dur), Self::Number(m)) => {
                try_from_number_into_interval!(Number::from(*dur).mul(*m), "interval {dur} microsecond * {m}")
            }
            _ => {
                return Err(Error::InvalidArguments(format!("cannot multiply {self} with {other}")));
            }
        })
    }

    /// Divides two values using the rules common among SQL implementations.
    pub fn sql_float_div(&self, other: &Self) -> Result<Self, Error> {
        Ok(match (self, other) {
            (Self::Number(lhs), Self::Number(rhs)) => try_from_number!(lhs.float_div(*rhs), "{lhs} / {rhs}"),
            (Self::Interval(lhs), Self::Interval(rhs)) => {
                try_from_number!(Number::from(*lhs).float_div(Number::from(*rhs)), "{lhs}us / {rhs}us")
            }
            (Self::Interval(dur), Self::Number(d)) => {
                try_from_number_into_interval!(Number::from(*dur).float_div(*d), "interval {dur} microsecond / {d}")
            }
            _ => {
                return Err(Error::InvalidArguments(format!("cannot divide {self} by {other}")));
            }
        })
    }

    /// Divides two values using the rules common among SQL implementations.
    pub fn sql_div(&self, other: &Self) -> Result<Self, Error> {
        Ok(match (self, other) {
            (Self::Number(lhs), Self::Number(rhs)) => try_from_number!(lhs.div(*rhs), "div({lhs}, {rhs})"),
            (Self::Interval(lhs), Self::Interval(rhs)) => {
                try_from_number!(Number::from(*lhs).div(Number::from(*rhs)), "div({lhs}us, {rhs}us)")
            }
            _ => return Err(Error::InvalidArguments(format!("cannot divide {self} by {other}"))),
        })
    }

    /// Computes the remainder when dividing two values using the rules common among SQL implementations.
    pub fn sql_rem(&self, other: &Self) -> Result<Self, Error> {
        Ok(match (self, other) {
            (Self::Number(lhs), Self::Number(rhs)) => try_from_number!(lhs.rem(*rhs), "mod({lhs}, {rhs})"),
            (Self::Interval(_), Self::Interval(0)) => Self::Null,
            (Self::Interval(_), Self::Interval(-1)) => Self::Interval(0),
            (Self::Interval(lhs), Self::Interval(rhs)) => Self::Interval(lhs % rhs),
            _ => {
                return Err(Error::InvalidArguments(format!(
                    "cannot compute remainder of {self} by {other}"
                )));
            }
        })
    }

    /// Concatenates multiple values into a string.
    pub fn sql_concat<'a>(values: impl Iterator<Item = &'a Self>) -> Result<Self, Error> {
        use std::fmt::Write;

        let mut res = ByteString::default();
        for item in values {
            match item {
                Self::Null => return Ok(Self::Null),
                Self::Number(n) => res.extend_number(n),
                Self::Bytes(b) => res.extend_byte_string(b),
                Self::Timestamp(timestamp) => {
                    write!(res, "{}", timestamp.format(TIMESTAMP_FORMAT)).unwrap();
                }
                Self::Interval(interval) => write!(res, "INTERVAL {interval} MICROSECOND").unwrap(),
                Self::Array(_) => {
                    return Err(Error::InvalidArguments(
                        "cannot concatenate arrays using || operator".to_owned(),
                    ));
                }
            }
        }
        Ok(Self::Bytes(res))
    }

    /// Checks whether this value is truthy in SQL sense.
    ///
    /// All nonzero numbers are considered "true", and both NULL and zero are
    /// considered "false". All other types cause the `InvalidArguments` error.
    pub fn is_sql_true(&self) -> Result<bool, Error> {
        match self {
            Self::Null => Ok(false),
            Self::Number(n) => Ok(n.sql_sign() != Ordering::Equal),
            _ => Err(Error::InvalidArguments(format!("truth value of {self} is undefined"))),
        }
    }

    fn to_unexpected_value_type_error(&self, expected: &'static str) -> Error {
        Error::UnexpectedValueType {
            expected,
            value: self.to_string(),
        }
    }
}

macro_rules! impl_try_from_value {
    ($T:ty, $name:expr) => {
        impl TryFrom<Value> for $T {
            type Error = Error;

            fn try_from(value: Value) -> Result<Self, Self::Error> {
                if let Value::Number(n) = value {
                    #[allow(irrefutable_let_patterns)]
                    if let Ok(v) = n.try_into() {
                        return Ok(v);
                    }
                }
                Err(value.to_unexpected_value_type_error($name))
            }
        }

        impl TryFrom<Value> for Option<$T> {
            type Error = Error;

            fn try_from(value: Value) -> Result<Self, Self::Error> {
                match value {
                    Value::Null => return Ok(None),
                    Value::Number(n) =>
                    {
                        #[allow(irrefutable_let_patterns)]
                        if let Ok(v) = n.try_into() {
                            return Ok(Some(v));
                        }
                    }
                    _ => {}
                }
                Err(value.to_unexpected_value_type_error(concat!("nullable ", $name)))
            }
        }
    };
}

impl_try_from_value!(u8, "8-bit unsigned integer");
impl_try_from_value!(u16, "16-bit unsigned integer");
impl_try_from_value!(u32, "32-bit unsigned integer");
impl_try_from_value!(u64, "64-bit unsigned integer");
impl_try_from_value!(usize, "unsigned integer");
impl_try_from_value!(i8, "8-bit signed integer");
impl_try_from_value!(i16, "16-bit signed integer");
impl_try_from_value!(i32, "32-bit signed integer");
impl_try_from_value!(i64, "64-bit signed integer");
impl_try_from_value!(i128, "signed integer");
impl_try_from_value!(isize, "signed integer");
impl_try_from_value!(f64, "floating point number");

impl TryFrom<Value> for Number {
    type Error = Error;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::Number(n) => Ok(n),
            _ => Err(value.to_unexpected_value_type_error("number")),
        }
    }
}

impl TryFrom<Value> for ByteString {
    type Error = Error;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(value.to_unexpected_value_type_error("byte string")),
        }
    }
}

impl TryFrom<Value> for String {
    type Error = Error;

    fn try_from(mut value: Value) -> Result<Self, Self::Error> {
        if let Value::Bytes(bytes) = value {
            match bytes.try_into() {
                Ok(s) => return Ok(s),
                Err(e) => value = Value::Bytes(e.0),
            }
        }
        Err(value.to_unexpected_value_type_error("string"))
    }
}

impl TryFrom<Value> for Vec<u8> {
    type Error = Error;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::Bytes(bytes) => Ok(bytes.into_bytes()),
            _ => Err(value.to_unexpected_value_type_error("bytes")),
        }
    }
}

impl TryFrom<Value> for Option<bool> {
    type Error = Error;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::Null => Ok(None),
            Value::Number(n) => Ok(Some(n.sql_sign() != Ordering::Equal)),
            _ => Err(value.to_unexpected_value_type_error("nullable boolean")),
        }
    }
}

impl TryFrom<Value> for Array {
    type Error = Error;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::Array(v) => Ok(v),
            _ => Err(value.to_unexpected_value_type_error("array")),
        }
    }
}

impl<T: Into<Number>> From<T> for Value {
    fn from(value: T) -> Self {
        Self::Number(value.into())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Bytes(value.into())
    }
}

impl From<Vec<u8>> for Value {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Bytes(bytes.into())
    }
}

impl From<ByteString> for Value {
    fn from(b: ByteString) -> Self {
        Self::Bytes(b)
    }
}

impl From<EncodedString> for Value {
    fn from(result: EncodedString) -> Self {
        Self::Bytes(result.into())
    }
}

impl<T: Into<Self>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, T::into)
    }
}
