use std::{fmt::Display, rc::Rc};

mod conversions;
pub use conversions::*;
use jrsonnet_gcmodule::Trace;
pub use jrsonnet_types::{ComplexValType, ValType};
use thiserror::Error;

use crate::{
	error::{Error, LocError, Result},
	State, Val,
};

#[derive(Debug, Error, Clone, Trace)]
pub enum TypeError {
	#[error("expected {0}, got {1}")]
	ExpectedGot(ComplexValType, ValType),
	#[error("missing property {0} from {1}")]
	MissingProperty(#[trace(skip)] Rc<str>, ComplexValType),
	#[error("every failed from {0}:\n{1}")]
	UnionFailed(ComplexValType, TypeLocErrorList),
	#[error(
		"number out of bounds: {0} not in {}..{}",
		.1.map(|v|v.to_string()).unwrap_or_else(|| "".to_owned()),
		.2.map(|v|v.to_string()).unwrap_or_else(|| "".to_owned()),
	)]
	BoundsFailed(f64, Option<f64>, Option<f64>),
}
impl From<TypeError> for LocError {
	fn from(e: TypeError) -> Self {
		Error::TypeError(e.into()).into()
	}
}

#[derive(Debug, Clone, Trace)]
pub struct TypeLocError(Box<TypeError>, ValuePathStack);
impl From<TypeError> for TypeLocError {
	fn from(e: TypeError) -> Self {
		Self(Box::new(e), ValuePathStack(Vec::new()))
	}
}
impl From<TypeLocError> for LocError {
	fn from(e: TypeLocError) -> Self {
		Error::TypeError(e).into()
	}
}
impl Display for TypeLocError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)?;
		if !(self.1).0.is_empty() {
			write!(f, " at {}", self.1)?;
		}
		Ok(())
	}
}

#[derive(Debug, Clone, Trace)]
pub struct TypeLocErrorList(Vec<TypeLocError>);
impl Display for TypeLocErrorList {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		use std::fmt::Write;
		let mut out = String::new();
		for (i, err) in self.0.iter().enumerate() {
			if i != 0 {
				writeln!(f)?;
			}
			out.clear();
			write!(out, "{}", err)?;

			for (i, line) in out.lines().enumerate() {
				if line.trim().is_empty() {
					continue;
				}
				if i == 0 {
					write!(f, "  - ")?;
				} else {
					writeln!(f)?;
					write!(f, "    ")?;
				}
				write!(f, "{}", line)?;
			}
		}
		Ok(())
	}
}

fn push_type_description(
	s: State,
	error_reason: impl Fn() -> String,
	path: impl Fn() -> ValuePathItem,
	item: impl Fn() -> Result<()>,
) -> Result<()> {
	s.push_description(error_reason, || match item() {
		Ok(_) => Ok(()),
		Err(mut e) => {
			if let Error::TypeError(e) = &mut e.error_mut() {
				(e.1).0.push(path());
			}
			Err(e)
		}
	})
}

// TODO: check_fast for fast path of union type checking
pub trait CheckType {
	fn check(&self, s: State, value: &Val) -> Result<()>;
}

impl CheckType for ValType {
	fn check(&self, _: State, value: &Val) -> Result<()> {
		let got = value.value_type();
		if got != *self {
			let loc_error: TypeLocError = TypeError::ExpectedGot((*self).into(), got).into();
			return Err(loc_error.into());
		}
		Ok(())
	}
}

#[derive(Clone, Debug, Trace)]
enum ValuePathItem {
	Field(#[trace(skip)] Rc<str>),
	Index(u64),
}
impl Display for ValuePathItem {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Field(name) => write!(f, ".{:?}", name)?,
			Self::Index(idx) => write!(f, "[{}]", idx)?,
		}
		Ok(())
	}
}

#[derive(Clone, Debug, Trace)]
struct ValuePathStack(Vec<ValuePathItem>);
impl Display for ValuePathStack {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "self")?;
		for elem in self.0.iter().rev() {
			write!(f, "{}", elem)?;
		}
		Ok(())
	}
}

impl CheckType for ComplexValType {
	#[allow(clippy::too_many_lines)]
	fn check(&self, s: State, value: &Val) -> Result<()> {
		match self {
			Self::Any => Ok(()),
			Self::Simple(t) => t.check(s, value),
			Self::Char => match value {
				Val::Str(s) if s.len() == 1 || s.chars().count() == 1 => Ok(()),
				v => Err(TypeError::ExpectedGot(self.clone(), v.value_type()).into()),
			},
			Self::BoundedNumber(from, to) => {
				if let Val::Num(n) = value {
					if from.map(|from| from > *n).unwrap_or(false)
						|| to.map(|to| to < *n).unwrap_or(false)
					{
						return Err(TypeError::BoundsFailed(*n, *from, *to).into());
					}
					Ok(())
				} else {
					Err(TypeError::ExpectedGot(self.clone(), value.value_type()).into())
				}
			}
			Self::Array(elem_type) => match value {
				Val::Arr(a) => {
					for (i, item) in a.iter(s.clone()).enumerate() {
						push_type_description(
							s.clone(),
							|| format!("array index {}", i),
							|| ValuePathItem::Index(i as u64),
							|| elem_type.check(s.clone(), &item.clone()?),
						)?;
					}
					Ok(())
				}
				v => Err(TypeError::ExpectedGot(self.clone(), v.value_type()).into()),
			},
			Self::ArrayRef(elem_type) => match value {
				Val::Arr(a) => {
					for (i, item) in a.iter(s.clone()).enumerate() {
						push_type_description(
							s.clone(),
							|| format!("array index {}", i),
							|| ValuePathItem::Index(i as u64),
							|| elem_type.check(s.clone(), &item.clone()?),
						)?;
					}
					Ok(())
				}
				v => Err(TypeError::ExpectedGot(self.clone(), v.value_type()).into()),
			},
			Self::ObjectRef(elems) => match value {
				Val::Obj(obj) => {
					for (k, v) in elems.iter() {
						if let Some(got_v) = obj.get(s.clone(), (*k).into())? {
							push_type_description(
								s.clone(),
								|| format!("property {}", k),
								|| ValuePathItem::Field((*k).into()),
								|| v.check(s.clone(), &got_v),
							)?;
						} else {
							return Err(
								TypeError::MissingProperty((*k).into(), self.clone()).into()
							);
						}
					}
					Ok(())
				}
				v => Err(TypeError::ExpectedGot(self.clone(), v.value_type()).into()),
			},
			Self::Union(types) => {
				let mut errors = Vec::new();
				for ty in types.iter() {
					match ty.check(s.clone(), value) {
						Ok(()) => {
							return Ok(());
						}
						Err(e) => match e.error() {
							Error::TypeError(e) => errors.push(e.clone()),
							_ => return Err(e),
						},
					}
				}
				Err(TypeError::UnionFailed(self.clone(), TypeLocErrorList(errors)).into())
			}
			Self::UnionRef(types) => {
				let mut errors = Vec::new();
				for ty in types.iter() {
					match ty.check(s.clone(), value) {
						Ok(()) => {
							return Ok(());
						}
						Err(e) => match e.error() {
							Error::TypeError(e) => errors.push(e.clone()),
							_ => return Err(e),
						},
					}
				}
				Err(TypeError::UnionFailed(self.clone(), TypeLocErrorList(errors)).into())
			}
			Self::Sum(types) => {
				for ty in types.iter() {
					ty.check(s.clone(), value)?;
				}
				Ok(())
			}
			Self::SumRef(types) => {
				for ty in types.iter() {
					ty.check(s.clone(), value)?;
				}
				Ok(())
			}
		}
	}
}
