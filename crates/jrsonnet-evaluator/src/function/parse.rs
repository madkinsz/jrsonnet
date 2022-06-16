use jrsonnet_gcmodule::Trace;
use jrsonnet_interner::IStr;
use jrsonnet_parser::{LocExpr, ParamsDesc};

use super::{
	arglike::ArgsLike,
	builtin::{BuiltinParam, BuiltinParamName},
};
use crate::{
	destructure::destruct,
	error::{Error::*, Result},
	evaluate_named,
	gc::GcHashMap,
	tb, throw,
	val::ThunkValue,
	Context, Pending, State, Thunk, Val,
};

#[derive(Trace)]
struct EvaluateNamedThunk {
	ctx: Pending<Context>,
	name: IStr,
	value: LocExpr,
}

impl ThunkValue for EvaluateNamedThunk {
	type Output = Val;
	fn get(self: Box<Self>, s: State) -> Result<Val> {
		evaluate_named(s, self.ctx.unwrap(), &self.value, self.name)
	}
}

/// Creates correct [context](Context) for function body evaluation returning error on invalid call.
///
/// ## Parameters
/// * `ctx`: used for passed argument expressions' execution and for body execution (if `body_ctx` is not set)
/// * `body_ctx`: used for default parameter values' execution and for body execution (if set)
/// * `params`: function parameters' definition
/// * `args`: passed function arguments
/// * `tailstrict`: if set to `true` function arguments are eagerly executed, otherwise - lazily
pub fn parse_function_call(
	s: State,
	ctx: Context,
	body_ctx: Context,
	params: &ParamsDesc,
	args: &dyn ArgsLike,
	tailstrict: bool,
) -> Result<Context> {
	let mut passed_args = GcHashMap::with_capacity(params.len());
	if args.unnamed_len() > params.len() {
		throw!(TooManyArgsFunctionHas(params.len()))
	}

	let mut filled_named = 0;
	let mut filled_positionals = 0;

	args.unnamed_iter(s.clone(), ctx.clone(), tailstrict, &mut |id, arg| {
		let name = params[id].0.clone();
		destruct(
			&name,
			arg,
			Pending::new_filled(ctx.clone()),
			&mut passed_args,
		)?;
		filled_positionals += 1;
		Ok(())
	})?;

	args.named_iter(s, ctx, tailstrict, &mut |name, value| {
		// FIXME: O(n) for arg existence check
		if !params.iter().any(|p| p.0.name().as_ref() == Some(name)) {
			throw!(UnknownFunctionParameter((name as &str).to_owned()));
		}
		if passed_args.insert(name.clone(), value).is_some() {
			throw!(BindingParameterASecondTime(name.clone()));
		}
		filled_named += 1;
		Ok(())
	})?;

	if filled_named + filled_positionals < params.len() {
		// Some args are unset, but maybe we have defaults for them
		// Default values should be created in newly created context
		let fctx = Context::new_future();
		let mut defaults =
			GcHashMap::with_capacity(params.len() - filled_named - filled_positionals);

		for (idx, param) in params.iter().enumerate().filter(|p| p.1 .1.is_some()) {
			if let Some(name) = param.0.name() {
				if passed_args.contains_key(&name) {
					continue;
				}
			} else if idx < filled_positionals {
				continue;
			}

			destruct(
				&param.0,
				Thunk::new(tb!(EvaluateNamedThunk {
					ctx: fctx.clone(),
					name: param.0.name().unwrap_or_else(|| "<destruct>".into()),
					value: param.1.clone().expect("default exists"),
				})),
				fctx.clone(),
				&mut defaults,
			)?;
			if param.0.name().is_some() {
				filled_named += 1;
			} else {
				filled_positionals += 1;
			}
		}

		// Some args still weren't filled
		if filled_named + filled_positionals != params.len() {
			for param in params.iter().skip(args.unnamed_len()) {
				let mut found = false;
				args.named_names(&mut |name| {
					if Some(name) == param.0.name().as_ref() {
						found = true;
					}
				});
				if !found {
					throw!(FunctionParameterNotBoundInCall(
						param
							.0
							.clone()
							.name()
							.unwrap_or_else(|| "<destruct>".into())
					));
				}
			}
			unreachable!();
		}

		Ok(body_ctx
			.extend(passed_args, None, None, None)
			.extend(defaults, None, None, None)
			.into_future(fctx))
	} else {
		let body_ctx = body_ctx.extend(passed_args, None, None, None);
		Ok(body_ctx)
	}
}

/// You shouldn't probally use this function, use `jrsonnet_macros::builtin` instead
///
/// ## Parameters
/// * `ctx`: used for passed argument expressions' execution and for body execution (if `body_ctx` is not set)
/// * `params`: function parameters' definition
/// * `args`: passed function arguments
/// * `tailstrict`: if set to `true` function arguments are eagerly executed, otherwise - lazily
pub fn parse_builtin_call(
	s: State,
	ctx: Context,
	params: &[BuiltinParam],
	args: &dyn ArgsLike,
	tailstrict: bool,
) -> Result<GcHashMap<BuiltinParamName, Thunk<Val>>> {
	let mut passed_args = GcHashMap::with_capacity(params.len());
	if args.unnamed_len() > params.len() {
		throw!(TooManyArgsFunctionHas(params.len()))
	}

	let mut filled_args = 0;

	args.unnamed_iter(s.clone(), ctx.clone(), tailstrict, &mut |id, arg| {
		let name = params[id].name.clone();
		passed_args.insert(name, arg);
		filled_args += 1;
		Ok(())
	})?;

	args.named_iter(s, ctx, tailstrict, &mut |name, arg| {
		// FIXME: O(n) for arg existence check
		let p = params
			.iter()
			.find(|p| p.name == name as &str)
			.ok_or_else(|| UnknownFunctionParameter((name as &str).to_owned()))?;
		if passed_args.insert(p.name.clone(), arg).is_some() {
			throw!(BindingParameterASecondTime(name.clone()));
		}
		filled_args += 1;
		Ok(())
	})?;

	if filled_args < params.len() {
		for param in params.iter().filter(|p| p.has_default) {
			if passed_args.contains_key(&param.name) {
				continue;
			}
			filled_args += 1;
		}

		// Some args still wasn't filled
		if filled_args != params.len() {
			for param in params.iter().skip(args.unnamed_len()) {
				let mut found = false;
				args.named_names(&mut |name| {
					if name as &str == &param.name as &str {
						found = true;
					}
				});
				if !found {
					throw!(FunctionParameterNotBoundInCall(param.name.clone().into()));
				}
			}
			unreachable!();
		}
	}
	Ok(passed_args)
}

/// Creates Context, which has all argument default values applied
/// and with unbound values causing error to be returned
pub fn parse_default_function_call(body_ctx: Context, params: &ParamsDesc) -> Result<Context> {
	#[derive(Trace)]
	struct DependsOnUnbound(IStr);
	impl ThunkValue for DependsOnUnbound {
		type Output = Val;
		fn get(self: Box<Self>, _: State) -> Result<Val> {
			Err(FunctionParameterNotBoundInCall(self.0.clone()).into())
		}
	}

	let fctx = Context::new_future();

	let mut bindings = GcHashMap::new();

	for param in params.iter() {
		if let Some(v) = &param.1 {
			destruct(
				&param.0.clone(),
				Thunk::new(tb!(EvaluateNamedThunk {
					ctx: fctx.clone(),
					name: param.0.name().unwrap_or_else(|| "<destruct>".into()),
					value: v.clone(),
				})),
				fctx.clone(),
				&mut bindings,
			)?;
		} else {
			destruct(
				&param.0,
				Thunk::new(tb!(DependsOnUnbound(
					param.0.name().unwrap_or_else(|| "<destruct>".into())
				))),
				fctx.clone(),
				&mut bindings,
			)?;
		}
	}

	Ok(body_ctx
		.extend(bindings, None, None, None)
		.into_future(fctx))
}
