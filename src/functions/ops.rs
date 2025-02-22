//! Numerical and logical functions.

use super::{Arguments, Function, args_1, args_2, iter_args};
use crate::{
    error::Error,
    eval::{C, CompileContext},
    span::{ResultExt, S, Span},
    value::Value,
};
use std::cmp::Ordering;

//------------------------------------------------------------------------------

/// The unary negation SQL function
#[derive(Debug)]
pub struct Neg;

impl Function for Neg {
    fn compile(&self, _: &CompileContext, span: Span, args: Arguments) -> Result<C, S<Error>> {
        let inner = args_1::<Value>(span, args, None)?;
        Ok(C::Constant(inner.sql_neg().span_err(span)?))
    }
}

//------------------------------------------------------------------------------

/// The value comparison (`<`, `=`, `>`, `<=`, `<>`, `>=`) SQL functions.
#[derive(Debug)]
pub struct Compare {
    /// Whether a less-than result is considered TRUE.
    lt: bool,
    /// Whether an equals result is considered TRUE.
    eq: bool,
    /// Whether a greater-than result is considered TRUE.
    gt: bool,
}

/// The `<` SQL function.
pub const LT: Compare = Compare {
    lt: true,
    eq: false,
    gt: false,
};
/// The `=` SQL function.
pub const EQ: Compare = Compare {
    lt: false,
    eq: true,
    gt: false,
};
/// The `>` SQL function.
pub const GT: Compare = Compare {
    lt: false,
    eq: false,
    gt: true,
};
/// The `<=` SQL function.
pub const LE: Compare = Compare {
    lt: true,
    eq: true,
    gt: false,
};
/// The `<>` SQL function.
pub const NE: Compare = Compare {
    lt: true,
    eq: false,
    gt: true,
};
/// The `>=` SQL function.
pub const GE: Compare = Compare {
    lt: false,
    eq: true,
    gt: true,
};

impl Function for Compare {
    fn compile(&self, _: &CompileContext, span: Span, args: Arguments) -> Result<C, S<Error>> {
        if let [lhs, rhs] = &*args {
            Ok(C::Constant(match lhs.inner.sql_cmp(&rhs.inner).span_err(span)? {
                None => Value::Null,
                Some(Ordering::Less) => self.lt.into(),
                Some(Ordering::Equal) => self.eq.into(),
                Some(Ordering::Greater) => self.gt.into(),
            }))
        } else {
            panic!("should have exactly 2 arguments");
        }
    }
}

//------------------------------------------------------------------------------

/// The identity comparison (`IS`, `IS NOT`) SQL functions.
#[derive(Debug)]
pub struct Identical {
    /// Whether an identical result is considered TRUE.
    eq: bool,
}

/// The `IS` SQL function.
pub const IS: Identical = Identical { eq: true };
/// The `IS NOT` SQL function.
pub const IS_NOT: Identical = Identical { eq: false };

impl Function for Identical {
    fn compile(&self, _: &CompileContext, _: Span, args: Arguments) -> Result<C, S<Error>> {
        if let [lhs, rhs] = &*args {
            let is_eq = lhs.inner == rhs.inner;
            Ok(C::Constant((is_eq == self.eq).into()))
        } else {
            panic!("should have exactly 2 arguments");
        }
    }
}

//------------------------------------------------------------------------------

/// The logical `NOT` SQL function.
#[derive(Debug)]
pub struct Not;

impl Function for Not {
    fn compile(&self, _: &CompileContext, span: Span, args: Arguments) -> Result<C, S<Error>> {
        let inner = args_1::<Option<bool>>(span, args, None)?;
        Ok(C::Constant(inner.map(|b| !b).into()))
    }
}

//------------------------------------------------------------------------------

/// The bitwise-NOT `~` SQL function.
#[derive(Debug)]
pub struct BitNot;

impl Function for BitNot {
    fn compile(&self, _: &CompileContext, span: Span, args: Arguments) -> Result<C, S<Error>> {
        let inner = args_1::<i128>(span, args, None)?;
        Ok(C::Constant((!inner).into()))
    }
}

//------------------------------------------------------------------------------

/// The logical `AND`/`OR` SQL functions.
#[derive(Debug)]
pub struct Logic {
    /// The identity value. True means `AND` and false means `OR`.
    identity: bool,
}

/// The logical `AND` SQL function.
pub const AND: Logic = Logic { identity: true };
/// The logical `OR` SQL function.
pub const OR: Logic = Logic { identity: false };

impl Function for Logic {
    fn compile(&self, _: &CompileContext, _: Span, args: Arguments) -> Result<C, S<Error>> {
        let mut result = Some(self.identity);

        for arg in iter_args::<Option<bool>>(args) {
            if let Some(v) = arg? {
                if v == self.identity {
                    continue;
                }
                return Ok(C::Constant(v.into()));
            }
            result = None;
        }
        Ok(C::Constant(result.into()))
    }
}

//------------------------------------------------------------------------------

/// The arithmetic (`+`, `-`, `*`, `/`) SQL functions.
#[derive(Debug)]
pub enum Arith {
    /// Addition (`+`)
    Add,
    /// Subtraction (`-`)
    Sub,
    /// Multiplication (`*`)
    Mul,
    /// Floating-point division (`/`)
    FloatDiv,
}

impl Function for Arith {
    fn compile(&self, _: &CompileContext, _: Span, args: Arguments) -> Result<C, S<Error>> {
        let func = match self {
            Self::Add => Value::sql_add,
            Self::Sub => Value::sql_sub,
            Self::Mul => Value::sql_mul,
            Self::FloatDiv => Value::sql_float_div,
        };

        let result = args.into_iter().try_fold(None, |accum, cur| -> Result<_, S<Error>> {
            Ok(Some(if let Some(prev) = accum {
                func(&prev, &cur.inner).span_err(cur.span)?
            } else {
                cur.inner
            }))
        });
        Ok(C::Constant(result?.expect("at least 1 argument")))
    }
}

//------------------------------------------------------------------------------

/// The bitwise binary (`&`, `|`, `^`) SQL functions.
#[derive(Debug)]
pub enum Bitwise {
    /// Bitwise-AND (`&`)
    And,
    /// Bitwise-OR (`|`)
    Or,
    /// Bitwise-XOR (`^`)
    Xor,
}

impl Function for Bitwise {
    fn compile(&self, _: &CompileContext, _: Span, args: Arguments) -> Result<C, S<Error>> {
        use std::ops::{BitAnd, BitOr, BitXor};

        let (func, init): (fn(i128, i128) -> i128, _) = match self {
            Self::And => (i128::bitand, -1),
            Self::Or => (i128::bitor, 0),
            Self::Xor => (i128::bitxor, 0),
        };

        let result = iter_args::<i128>(args).try_fold(init, |a, b| b.map(|bb| func(a, bb)))?;
        Ok(C::Constant(result.into()))
    }
}

//------------------------------------------------------------------------------

/// The extremum (`least`, `greatest`) SQL functions.
#[derive(Debug)]
pub struct Extremum {
    /// The order to drive the extremum.
    order: Ordering,
}

/// The `greatest` SQL function.
pub const GREATEST: Extremum = Extremum {
    order: Ordering::Greater,
};
/// The `least` SQL function.
pub const LEAST: Extremum = Extremum { order: Ordering::Less };

impl Function for Extremum {
    fn compile(&self, _: &CompileContext, _: Span, args: Arguments) -> Result<C, S<Error>> {
        let mut res = Value::Null;
        for value in args {
            let should_replace = if let Some(order) = value.inner.sql_cmp(&res).span_err(value.span)? {
                order == self.order
            } else {
                res == Value::Null
            };
            if should_replace {
                res = value.inner;
            }
        }
        Ok(C::Constant(res))
    }
}

//------------------------------------------------------------------------------

/// The `round` SQL function.
#[derive(Debug)]
pub struct Round;

impl Function for Round {
    fn compile(&self, _: &CompileContext, span: Span, args: Arguments) -> Result<C, S<Error>> {
        let (value, digits) = args_2::<f64, i32>(span, args, None, Some(0))?;
        let scale = 10.0_f64.powi(digits);
        let result = if scale.is_finite() {
            (value * scale).round() / scale
        } else {
            value
        };
        Ok(C::Constant(Value::from_finite_f64(result)))
    }
}

//------------------------------------------------------------------------------

/// The `div` SQL function.
#[derive(Debug)]
pub struct Div;

impl Function for Div {
    fn compile(&self, _: &CompileContext, span: Span, args: Arguments) -> Result<C, S<Error>> {
        let (n, d) = args_2::<Value, Value>(span, args, None, None)?;
        Ok(C::Constant(n.sql_div(&d).span_err(span)?))
    }
}

/// The `mod` SQL function.
#[derive(Debug)]
pub struct Mod;

impl Function for Mod {
    fn compile(&self, _: &CompileContext, span: Span, args: Arguments) -> Result<C, S<Error>> {
        let (n, d) = args_2::<Value, Value>(span, args, None, None)?;
        Ok(C::Constant(n.sql_rem(&d).span_err(span)?))
    }
}

//------------------------------------------------------------------------------

/// The `coalesce` SQL function.
#[derive(Debug)]
pub struct Coalesce;

impl Function for Coalesce {
    fn compile(&self, _: &CompileContext, _: Span, args: Arguments) -> Result<C, S<Error>> {
        let res = args
            .into_iter()
            .map(|v| v.inner)
            .find(|v| *v != Value::Null)
            .unwrap_or(Value::Null);
        Ok(C::Constant(res))
    }
}

//------------------------------------------------------------------------------

/// The statement terminator `;`.
#[derive(Debug)]
pub struct Last;

impl Function for Last {
    fn compile(&self, _: &CompileContext, _: Span, mut args: Arguments) -> Result<C, S<Error>> {
        Ok(C::Constant(args.pop().expect("at least one expression").inner))
    }
}
