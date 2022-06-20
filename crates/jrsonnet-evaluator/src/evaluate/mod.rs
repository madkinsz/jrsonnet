use std::{cmp::Ordering, rc::Rc};

use jrsonnet_gcmodule::{Cc, Trace};
use jrsonnet_interner::IStr;
use jrsonnet_parser::{
	ArgsDesc, AssertStmt, BindSpec, CompSpec, Expr, FieldMember, FieldName, ForSpecData,
	IfSpecData, LiteralType, LocExpr, Member, ObjBody, ParamsDesc,
};
use jrsonnet_types::ValType;

use crate::{
	destructure::evaluate_dest,
	error::Error::*,
	evaluate::operator::{evaluate_add_op, evaluate_binary_op_special, evaluate_unary_op},
	function::{CallLocation, FuncDesc, FuncVal},
	stdlib::{std_slice, BUILTINS},
	tb, throw,
	typed::Typed,
	val::{ArrValue, CachedUnbound, Thunk, ThunkValue},
	Context, GcHashMap, ObjValue, ObjValueBuilder, ObjectAssertion, Pending, Result, State,
	Unbound, Val,
};
pub mod destructure;
pub mod operator;

pub fn evaluate_method(ctx: Context, name: IStr, params: ParamsDesc, body: LocExpr) -> Val {
	Val::Func(FuncVal::Normal(Cc::new(FuncDesc {
		name,
		ctx,
		params,
		body,
	})))
}

pub fn evaluate_field_name(s: State, ctx: Context, field_name: &FieldName) -> Result<Option<IStr>> {
	Ok(match field_name {
		FieldName::Fixed(n) => Some(n.clone()),
		FieldName::Dyn(expr) => s.push(
			CallLocation::new(&expr.1),
			|| "evaluating field name".to_string(),
			|| {
				let value = evaluate(s.clone(), ctx, expr)?;
				if matches!(value, Val::Null) {
					Ok(None)
				} else {
					Ok(Some(IStr::from_untyped(value, s.clone())?))
				}
			},
		)?,
	})
}

pub fn evaluate_comp(
	s: State,
	ctx: Context,
	specs: &[CompSpec],
	callback: &mut impl FnMut(Context) -> Result<()>,
) -> Result<()> {
	match specs.get(0) {
		None => callback(ctx)?,
		Some(CompSpec::IfSpec(IfSpecData(cond))) => {
			if bool::from_untyped(evaluate(s.clone(), ctx.clone(), cond)?, s.clone())? {
				evaluate_comp(s, ctx, &specs[1..], callback)?;
			}
		}
		Some(CompSpec::ForSpec(ForSpecData(var, expr))) => {
			match evaluate(s.clone(), ctx.clone(), expr)? {
				Val::Arr(list) => {
					for item in list.iter(s.clone()) {
						evaluate_comp(
							s.clone(),
							ctx.clone().with_var(var.clone(), item?.clone()),
							&specs[1..],
							callback,
						)?;
					}
				}
				_ => throw!(InComprehensionCanOnlyIterateOverArray),
			}
		}
	}
	Ok(())
}

trait CloneableUnbound<T>: Unbound<Bound = T> + Clone {}

fn evaluate_object_locals(
	fctx: Pending<Context>,
	locals: Rc<Vec<BindSpec>>,
) -> impl CloneableUnbound<Context> {
	#[derive(Trace, Clone)]
	struct UnboundLocals {
		fctx: Pending<Context>,
		locals: Rc<Vec<BindSpec>>,
	}
	impl CloneableUnbound<Context> for UnboundLocals {}
	impl Unbound for UnboundLocals {
		type Bound = Context;

		fn bind(
			&self,
			_s: State,
			sup: Option<ObjValue>,
			this: Option<ObjValue>,
		) -> Result<Context> {
			let fctx = Context::new_future();
			let mut new_bindings = GcHashMap::new();
			for b in self.locals.iter() {
				evaluate_dest(b, fctx.clone(), &mut new_bindings)?;
			}

			let ctx = self.fctx.unwrap();
			let new_dollar = ctx.dollar().clone().or_else(|| this.clone());

			let ctx = ctx
				.extend(new_bindings, new_dollar, sup, this)
				.into_future(fctx);

			Ok(ctx)
		}
	}

	UnboundLocals { fctx, locals }
}

#[allow(clippy::too_many_lines)]
pub fn evaluate_member_list_object(s: State, ctx: Context, members: &[Member]) -> Result<ObjValue> {
	let mut builder = ObjValueBuilder::new();
	let locals = Rc::new(
		members
			.iter()
			.filter_map(|m| match m {
				Member::BindStmt(bind) => Some(bind.clone()),
				_ => None,
			})
			.collect::<Vec<_>>(),
	);

	let fctx = Context::new_future();

	// We have single context for all fields, so we can cache binds
	let uctx = CachedUnbound::new(evaluate_object_locals(fctx.clone(), locals));

	for member in members.iter() {
		match member {
			Member::Field(FieldMember {
				name,
				plus,
				params: None,
				visibility,
				value,
			}) => {
				#[derive(Trace)]
				struct UnboundValue<B: Trace> {
					uctx: B,
					value: LocExpr,
					name: IStr,
				}
				impl<B: Unbound<Bound = Context>> Unbound for UnboundValue<B> {
					type Bound = Thunk<Val>;
					fn bind(
						&self,
						s: State,
						sup: Option<ObjValue>,
						this: Option<ObjValue>,
					) -> Result<Thunk<Val>> {
						Ok(Thunk::evaluated(evaluate_named(
							s.clone(),
							self.uctx.bind(s, sup, this)?,
							&self.value,
							self.name.clone(),
						)?))
					}
				}

				let name = evaluate_field_name(s.clone(), ctx.clone(), name)?;
				let name = if let Some(name) = name {
					name
				} else {
					continue;
				};

				builder
					.member(name.clone())
					.with_add(*plus)
					.with_visibility(*visibility)
					.with_location(value.1.clone())
					.bindable(
						s.clone(),
						tb!(UnboundValue {
							uctx: uctx.clone(),
							value: value.clone(),
							name: name.clone()
						}),
					)?;
			}
			Member::Field(FieldMember {
				name,
				params: Some(params),
				value,
				..
			}) => {
				#[derive(Trace)]
				struct UnboundMethod<B: Trace> {
					uctx: B,
					value: LocExpr,
					params: ParamsDesc,
					name: IStr,
				}
				impl<B: Unbound<Bound = Context>> Unbound for UnboundMethod<B> {
					type Bound = Thunk<Val>;
					fn bind(
						&self,
						s: State,
						sup: Option<ObjValue>,
						this: Option<ObjValue>,
					) -> Result<Thunk<Val>> {
						Ok(Thunk::evaluated(evaluate_method(
							self.uctx.bind(s, sup, this)?,
							self.name.clone(),
							self.params.clone(),
							self.value.clone(),
						)))
					}
				}

				let name = if let Some(name) = evaluate_field_name(s.clone(), ctx.clone(), name)? {
					name
				} else {
					continue;
				};

				builder
					.member(name.clone())
					.hide()
					.with_location(value.1.clone())
					.bindable(
						s.clone(),
						tb!(UnboundMethod {
							uctx: uctx.clone(),
							value: value.clone(),
							params: params.clone(),
							name: name.clone()
						}),
					)?;
			}
			Member::BindStmt(_) => {}
			Member::AssertStmt(stmt) => {
				#[derive(Trace)]
				struct ObjectAssert<B: Trace> {
					uctx: B,
					assert: AssertStmt,
				}
				impl<B: Unbound<Bound = Context>> ObjectAssertion for ObjectAssert<B> {
					fn run(
						&self,
						s: State,
						sup: Option<ObjValue>,
						this: Option<ObjValue>,
					) -> Result<()> {
						let ctx = self.uctx.bind(s.clone(), sup, this)?;
						evaluate_assert(s, ctx, &self.assert)
					}
				}
				builder.assert(tb!(ObjectAssert {
					uctx: uctx.clone(),
					assert: stmt.clone(),
				}));
			}
		}
	}
	let this = builder.build();
	let _ctx = ctx
		.extend(GcHashMap::new(), None, None, Some(this.clone()))
		.into_future(fctx);
	Ok(this)
}

pub fn evaluate_object(s: State, ctx: Context, object: &ObjBody) -> Result<ObjValue> {
	Ok(match object {
		ObjBody::MemberList(members) => evaluate_member_list_object(s, ctx, members)?,
		ObjBody::ObjComp(obj) => {
			let mut builder = ObjValueBuilder::new();
			let locals = Rc::new(
				obj.pre_locals
					.iter()
					.chain(obj.post_locals.iter())
					.cloned()
					.collect::<Vec<_>>(),
			);
			let mut ctxs = vec![];
			evaluate_comp(s.clone(), ctx, &obj.compspecs, &mut |ctx| {
				let key = evaluate(s.clone(), ctx.clone(), &obj.key)?;
				let fctx = Context::new_future();
				ctxs.push((ctx, fctx.clone()));
				let uctx = evaluate_object_locals(fctx, locals.clone());

				match key {
					Val::Null => {}
					Val::Str(n) => {
						#[derive(Trace)]
						struct UnboundValue<B: Trace> {
							uctx: B,
							value: LocExpr,
						}
						impl<B: Unbound<Bound = Context>> Unbound for UnboundValue<B> {
							type Bound = Thunk<Val>;
							fn bind(
								&self,
								s: State,
								sup: Option<ObjValue>,
								this: Option<ObjValue>,
							) -> Result<Thunk<Val>> {
								Ok(Thunk::evaluated(evaluate(
									s.clone(),
									self.uctx.bind(s, sup, this.clone())?.extend(
										GcHashMap::new(),
										None,
										None,
										this,
									),
									&self.value,
								)?))
							}
						}
						builder
							.member(n)
							.with_location(obj.value.1.clone())
							.with_add(obj.plus)
							.bindable(
								s.clone(),
								tb!(UnboundValue {
									uctx,
									value: obj.value.clone(),
								}),
							)?;
					}
					v => throw!(FieldMustBeStringGot(v.value_type())),
				}

				Ok(())
			})?;

			let this = builder.build();
			for (ctx, fctx) in ctxs {
				let _ctx = ctx
					.extend(GcHashMap::new(), None, None, Some(this.clone()))
					.into_future(fctx);
			}
			this
		}
	})
}

pub fn evaluate_apply(
	s: State,
	ctx: Context,
	value: &LocExpr,
	args: &ArgsDesc,
	loc: CallLocation,
	tailstrict: bool,
) -> Result<Val> {
	let value = evaluate(s.clone(), ctx.clone(), value)?;
	Ok(match value {
		Val::Func(f) => {
			let body = || f.evaluate(s.clone(), ctx, loc, args, tailstrict);
			if tailstrict {
				body()?
			} else {
				s.push(loc, || format!("function <{}> call", f.name()), body)?
			}
		}
		v => throw!(OnlyFunctionsCanBeCalledGot(v.value_type())),
	})
}

pub fn evaluate_assert(s: State, ctx: Context, assertion: &AssertStmt) -> Result<()> {
	let value = &assertion.0;
	let msg = &assertion.1;
	let assertion_result = s.push(
		CallLocation::new(&value.1),
		|| "assertion condition".to_owned(),
		|| bool::from_untyped(evaluate(s.clone(), ctx.clone(), value)?, s.clone()),
	)?;
	if !assertion_result {
		s.push(
			CallLocation::new(&value.1),
			|| "assertion failure".to_owned(),
			|| {
				if let Some(msg) = msg {
					throw!(AssertionFailed(
						evaluate(s.clone(), ctx, msg)?.to_string(s.clone())?
					));
				}
				throw!(AssertionFailed(Val::Null.to_string(s.clone())?));
			},
		)?;
	}
	Ok(())
}

pub fn evaluate_named(s: State, ctx: Context, expr: &LocExpr, name: IStr) -> Result<Val> {
	use Expr::*;
	let LocExpr(raw_expr, _loc) = expr;
	Ok(match &**raw_expr {
		Function(params, body) => evaluate_method(ctx, name, params.clone(), body.clone()),
		_ => evaluate(s, ctx, expr)?,
	})
}

#[allow(clippy::too_many_lines)]
pub fn evaluate(s: State, ctx: Context, expr: &LocExpr) -> Result<Val> {
	use Expr::*;
	let LocExpr(expr, loc) = expr;
	// let bp = with_state(|s| s.0.stop_at.borrow().clone());
	Ok(match &**expr {
		Literal(LiteralType::This) => {
			Val::Obj(ctx.this().clone().ok_or(CantUseSelfOutsideOfObject)?)
		}
		Literal(LiteralType::Super) => {
			Val::Obj(ctx.super_obj().clone().ok_or(NoSuperFound)?.with_this(
				ctx.this()
					.clone()
					.expect("if super exists - then this should to"),
			))
		}
		Literal(LiteralType::Dollar) => {
			Val::Obj(ctx.dollar().clone().ok_or(NoTopLevelObjectFound)?)
		}
		Literal(LiteralType::True) => Val::Bool(true),
		Literal(LiteralType::False) => Val::Bool(false),
		Literal(LiteralType::Null) => Val::Null,
		Parened(e) => evaluate(s, ctx, e)?,
		Str(v) => Val::Str(v.clone()),
		Num(v) => Val::new_checked_num(*v)?,
		BinaryOp(v1, o, v2) => evaluate_binary_op_special(s, ctx, v1, *o, v2)?,
		UnaryOp(o, v) => evaluate_unary_op(*o, &evaluate(s, ctx, v)?)?,
		Var(name) => s.push(
			CallLocation::new(loc),
			|| format!("variable <{}> access", name),
			|| ctx.binding(name.clone())?.evaluate(s.clone()),
		)?,
		Index(value, index) => {
			match (
				evaluate(s.clone(), ctx.clone(), value)?,
				evaluate(s.clone(), ctx, index)?,
			) {
				(Val::Obj(v), Val::Str(key)) => s.push(
					CallLocation::new(loc),
					|| format!("field <{}> access", key),
					|| match v.get(s.clone(), key.clone()) {
						Ok(Some(v)) => Ok(v),
						#[cfg(not(feature = "friendly-errors"))]
						Ok(None) => throw!(NoSuchField(key.clone(), vec![])),
						#[cfg(feature = "friendly-errors")]
						Ok(None) => {
							let mut heap = Vec::new();
							for field in v.fields_ex(
								true,
								#[cfg(feature = "exp-preserve-order")]
								false,
							) {
								let conf = strsim::jaro_winkler(&field as &str, &key as &str);
								if conf < 0.8 {
									continue;
								}
								heap.push((conf, field));
							}
							heap.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));

							throw!(NoSuchField(
								key.clone(),
								heap.into_iter().map(|(_, v)| v).collect()
							))
						}
						Err(e) if matches!(e.error(), MagicThisFileUsed) => {
							Ok(Val::Str(loc.0.full_path().into()))
						}
						Err(e) => Err(e),
					},
				)?,
				(Val::Obj(_), n) => throw!(ValueIndexMustBeTypeGot(
					ValType::Obj,
					ValType::Str,
					n.value_type(),
				)),

				(Val::Arr(v), Val::Num(n)) => {
					if n.fract() > f64::EPSILON {
						throw!(FractionalIndex)
					}
					v.get(s, n as usize)?
						.ok_or_else(|| ArrayBoundsError(n as usize, v.len()))?
				}
				(Val::Arr(_), Val::Str(n)) => throw!(AttemptedIndexAnArrayWithString(n)),
				(Val::Arr(_), n) => throw!(ValueIndexMustBeTypeGot(
					ValType::Arr,
					ValType::Num,
					n.value_type(),
				)),

				(Val::Str(s), Val::Num(n)) => Val::Str({
					let v: IStr = s
						.chars()
						.skip(n as usize)
						.take(1)
						.collect::<String>()
						.into();
					if v.is_empty() {
						let size = s.chars().count();
						throw!(StringBoundsError(n as usize, size))
					}
					v
				}),
				(Val::Str(_), n) => throw!(ValueIndexMustBeTypeGot(
					ValType::Str,
					ValType::Num,
					n.value_type(),
				)),

				(v, _) => throw!(CantIndexInto(v.value_type())),
			}
		}
		LocalExpr(bindings, returned) => {
			let mut new_bindings: GcHashMap<IStr, Thunk<Val>> =
				GcHashMap::with_capacity(bindings.len());
			let fctx = Context::new_future();
			for b in bindings {
				evaluate_dest(b, fctx.clone(), &mut new_bindings)?;
			}
			let ctx = ctx.extend(new_bindings, None, None, None).into_future(fctx);
			evaluate(s, ctx, &returned.clone())?
		}
		Arr(items) => {
			let mut out = Vec::with_capacity(items.len());
			for item in items {
				// TODO: Implement ArrValue::Lazy with same context for every element?
				#[derive(Trace)]
				struct ArrayElement {
					ctx: Context,
					item: LocExpr,
				}
				impl ThunkValue for ArrayElement {
					type Output = Val;
					fn get(self: Box<Self>, s: State) -> Result<Val> {
						evaluate(s, self.ctx, &self.item)
					}
				}
				out.push(Thunk::new(tb!(ArrayElement {
					ctx: ctx.clone(),
					item: item.clone(),
				})));
			}
			Val::Arr(out.into())
		}
		ArrComp(expr, comp_specs) => {
			let mut out = Vec::new();
			evaluate_comp(s.clone(), ctx, comp_specs, &mut |ctx| {
				out.push(evaluate(s.clone(), ctx, expr)?);
				Ok(())
			})?;
			Val::Arr(ArrValue::Eager(Cc::new(out)))
		}
		Obj(body) => Val::Obj(evaluate_object(s, ctx, body)?),
		ObjExtend(a, b) => evaluate_add_op(
			s.clone(),
			&evaluate(s.clone(), ctx.clone(), a)?,
			&Val::Obj(evaluate_object(s, ctx, b)?),
		)?,
		Apply(value, args, tailstrict) => {
			evaluate_apply(s, ctx, value, args, CallLocation::new(loc), *tailstrict)?
		}
		Function(params, body) => {
			evaluate_method(ctx, "anonymous".into(), params.clone(), body.clone())
		}
		Intrinsic(name) => Val::Func(FuncVal::StaticBuiltin(
			BUILTINS
				.with(|b| b.get(name).copied())
				.ok_or_else(|| IntrinsicNotFound(name.clone()))?,
		)),
		IntrinsicThisFile => return Err(MagicThisFileUsed.into()),
		IntrinsicId => Val::Func(FuncVal::identity()),
		AssertExpr(assert, returned) => {
			evaluate_assert(s.clone(), ctx.clone(), assert)?;
			evaluate(s, ctx, returned)?
		}
		ErrorStmt(e) => s.push(
			CallLocation::new(loc),
			|| "error statement".to_owned(),
			|| {
				throw!(RuntimeError(
					evaluate(s.clone(), ctx, e)?.to_string(s.clone())?,
				))
			},
		)?,
		IfElse {
			cond,
			cond_then,
			cond_else,
		} => {
			if s.push(
				CallLocation::new(loc),
				|| "if condition".to_owned(),
				|| bool::from_untyped(evaluate(s.clone(), ctx.clone(), &cond.0)?, s.clone()),
			)? {
				evaluate(s, ctx, cond_then)?
			} else {
				match cond_else {
					Some(v) => evaluate(s, ctx, v)?,
					None => Val::Null,
				}
			}
		}
		Slice(value, desc) => {
			fn parse_idx<T: Typed>(
				loc: CallLocation,
				s: State,
				ctx: &Context,
				expr: &Option<LocExpr>,
				desc: &'static str,
			) -> Result<Option<T>> {
				if let Some(value) = expr {
					Ok(Some(s.push(
						loc,
						|| format!("slice {}", desc),
						|| T::from_untyped(evaluate(s.clone(), ctx.clone(), value)?, s.clone()),
					)?))
				} else {
					Ok(None)
				}
			}

			let indexable = evaluate(s.clone(), ctx.clone(), value)?;
			let loc = CallLocation::new(loc);

			let start = parse_idx(loc, s.clone(), &ctx, &desc.start, "start")?;
			let end = parse_idx(loc, s.clone(), &ctx, &desc.end, "end")?;
			let step = parse_idx(loc, s, &ctx, &desc.step, "step")?;

			std_slice(indexable.into_indexable()?, start, end, step)?
		}
		i @ (Import(path) | ImportStr(path) | ImportBin(path)) => {
			let tmp = loc.clone().0;
			let import_location = tmp
				.path()
				.map(|p| {
					let mut p = p.to_owned();
					p.pop();
					p
				})
				.unwrap_or_default();
			let resolved_path = s.resolve_file(&import_location, path as &str)?;
			match i {
				Import(_) => s.push(
					CallLocation::new(loc),
					|| format!("import {:?}", path.clone()),
					|| s.import(resolved_path.clone()),
				)?,
				ImportStr(_) => Val::Str(s.import_str(resolved_path)?),
				ImportBin(_) => Val::Arr(ArrValue::Bytes(s.import_bin(resolved_path)?)),
				_ => unreachable!(),
			}
		}
	})
}
