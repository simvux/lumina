use super::*;
use crate::{CLOSURE_CAPTURES, TRAIT_OBJECT_DATA_FIELD, VTABLE_FIELD};
use ssa::Value;
use std::cmp::Ordering;

impl<'a> FuncLower<'a> {
    pub fn expr_to_flow(&mut self, expr: &mir::Expr) {
        trace!("lowering expression {expr}");

        match expr {
            // TODO: we also want to edge-case tail calls here
            _ => {
                let value = self.expr_to_value(expr);
                self.ssa().return_(value);
            }
        }
    }

    // Used to stop us from creating unecesarry blocks if the contents are very simple
    pub fn expr_to_value_no_side_effects(&mut self, expr: &mir::Expr) -> Option<Value> {
        let simple = match expr {
            mir::Expr::Yield(local) => self.yield_to_value(*local),
            mir::Expr::YieldFunc(_, _) => todo!(),
            mir::Expr::YieldLambda(_, _) => todo!(),
            mir::Expr::UInt(bitsize, n) => Value::UInt(*n, *bitsize),
            mir::Expr::Int(bitsize, n) => Value::Int(*n, *bitsize),
            mir::Expr::Bool(b) => Value::UInt(*b as u8 as u128, Bitsize(8)),
            mir::Expr::Float(n) => Value::Float(*n),
            mir::Expr::ReadOnly(_) => todo!(),
            _ => return None,
        };

        Some(simple)
    }

    fn yield_to_value(&mut self, local: mir::Local) -> Value {
        match local {
            mir::Local::Param(pid) if self.current.has_captures => {
                assert_eq!(self.ssa().block(), ssa::Block::entry());
                Value::BlockParam(ssa::BlockParam(pid.0 + 1))
            }
            mir::Local::Param(pid) => {
                assert_eq!(self.ssa().block(), ssa::Block::entry());
                Value::BlockParam(ssa::BlockParam(pid.0))
            }
            mir::Local::Binding(bind) => self.current.bindmap[&bind],
        }
    }

    pub fn expr_to_value(&mut self, expr: &mir::Expr) -> Value {
        trace!("lowering {expr}");

        match expr {
            mir::Expr::CallFunc(func, inst, params) => self.call_nfunc(*func, inst, params),
            mir::Expr::CallLambda(lambda, inst, params) => {
                let mut params = self.params_to_values(params);
                let (mfunc, captures, _, returns) = self.morphise_lambda(*lambda, inst);

                // Add the captures as the first parameter
                params.insert(0, captures);

                self.ssa().call(mfunc, params, returns).value()
            }
            mir::Expr::PartialLambda(lambda, inst, partials) => {
                let (mfunc, captures, _, _) = self.morphise_lambda(*lambda, inst);

                let mut partials = self.params_to_values(partials);
                partials.insert(0, captures);
                self.partially_applicate_func(mfunc, partials)
            }
            mir::Expr::PartialLocal(local, partials) => {
                let cap = self.yield_to_value(*local);
                let mut partials = self.params_to_values(partials);
                partials.insert(0, cap);
                todo!("what should we do here?");
                // self.partially_applicate_func(mfunc, partials)
            }
            mir::Expr::PartialFunc(func, inst, partials) => match self.resolve_nfunc(*func, inst) {
                ResolvedNFunc::Static(mfunc, _) => {
                    let partials = self.params_to_values(partials);
                    self.partially_applicate_func(mfunc, partials)
                }
                ResolvedNFunc::Extern(_, _) => todo!(),
                ResolvedNFunc::Sum { tag, payload_size, ty } => todo!(),
                ResolvedNFunc::Val(_, _) => todo!(),
            },
            mir::Expr::YieldLambda(lambda, inst) => {
                let (mfunc, captures, capture_tuple_ty, _ret) = self.morphise_lambda(*lambda, inst);

                let mut methods = Map::new();
                methods.push(mfunc);

                let vtable =
                    self.trait_impl_vtable(self.info.closure, capture_tuple_ty.into(), methods);

                self.construct_dyn_object(&vtable, captures)
            }
            mir::Expr::CallLocal(local, params) => {
                let params = self.params_to_values(params);
                let to_call = self.yield_to_value(*local);
                let ty = self.type_of_value(to_call);
                match ty {
                    MonoType::FnPointer(_, ret) => {
                        self.ssa().call(to_call, params, (*ret).clone()).into()
                    }
                    MonoType::Monomorphised(mk) => self.call_closure(mk, to_call, params),
                    _ => panic!("attempted to call {ty:#?} as a function"),
                }
            }
            mir::Expr::ValToRef(val) => match &**val {
                mir::Expr::CallFunc(M { value: ast::NFunc::Val(val), module }, _, _) => {
                    let key = module.m(*val);
                    let ty = self.lir.vals[key].clone();
                    self.ssa().val_to_ref(key, MonoType::pointer(ty)).value()
                }
                other => panic!("non-val given to val_to_ref builtin: {other}"),
            },
            mir::Expr::Yield(local) => self.yield_to_value(*local),
            mir::Expr::YieldFunc(nfunc, inst) => {
                let mfunc = self.callable_to_mfunc(*nfunc, inst);
                Value::FuncPtr(mfunc)
            }
            mir::Expr::Access(object, record, types, field) => {
                let value = self.expr_to_value(object);

                let mut morph = to_morphization!(self, &mut self.current.tmap);

                let mk = morph.record(*record, types);

                let ty = self.lir.types.types.type_of_field(mk, *field);
                let v = self.ssa().field(value, mk, *field, ty);

                Value::V(v)
            }
            mir::Expr::Record(record, types, fields) => {
                let mut mono = to_morphization!(self, &mut self.current.tmap);

                let ty = MonoType::Monomorphised(mono.record(*record, types));

                let values = fields
                    .iter()
                    .map(|(_, expr)| self.expr_to_value(expr))
                    .collect::<Vec<Value>>();

                let sorted = (0..fields.len() as u32)
                    .map(key::RecordField)
                    .map(|field| values[fields.iter().position(|(f, _)| *f == field).unwrap()])
                    .collect();

                self.ssa().construct(sorted, ty).into()
            }
            mir::Expr::UInt(bitsize, n) => Value::UInt(*n, *bitsize),
            mir::Expr::Int(bitsize, n) => Value::Int(*n, *bitsize),
            mir::Expr::Bool(b) => Value::UInt(*b as u8 as u128, Bitsize(8)),
            mir::Expr::Float(n) => Value::Float(*n),
            mir::Expr::ReadOnly(ro) => Value::ReadOnly(*ro),
            mir::Expr::Tuple(elems) => {
                let params = self.params_to_values(elems);
                self.elems_to_tuple(params, None)
            }
            mir::Expr::IntCast(expr, from, to) => {
                let inner = self.expr_to_value(&expr);

                let ty =
                    to.0.then_some(MonoType::Int(to.1))
                        .unwrap_or(MonoType::UInt(to.1));

                match from.1.cmp(&to.1) {
                    Ordering::Equal => inner,
                    Ordering::Less => self.ssa().extend(inner, from.0, ty).into(),
                    Ordering::Greater => self.ssa().reduce(inner, ty).into(),
                }
            }
            mir::Expr::Deref(inner) => {
                let inner = self.expr_to_value(&inner);
                let ty = self.type_of_value(inner).deref();
                self.ssa().deref(inner, ty).into()
            }
            mir::Expr::Write(elems) => {
                let [ptr, value] = self.params_to_values(&**elems).try_into().unwrap();
                self.ssa().write(ptr, value).into()
            }
            mir::Expr::ObjectCast(expr, weak_impltor, trait_, trait_params) => {
                let expr = self.expr_to_value(expr);
                let impltor = self.type_of_value(expr);

                let mut morph = to_morphization!(self, &mut self.current.tmap);
                let weak_impltor = morph.apply_weak(weak_impltor);
                let weak_trait_params = morph.applys_weak::<Vec<_>>(trait_params);

                let (impl_, tmap) = self.find_implementation(
                    *trait_,
                    &weak_trait_params,
                    weak_impltor,
                    impltor.clone(),
                );

                let methods = self.mir.imethods[impl_]
                    .keys()
                    .map(|method| match self.mir.imethods[impl_][method] {
                        None => todo!("default method instantiation"),
                        Some(func) => {
                            // If the methods have generics then this isn't trait-safe therefore
                            // this would've already been stopped.
                            let mut tmap = tmap.clone();
                            let mut morph = to_morphization!(self, &mut tmap);
                            let typing = self.mir.funcs[func].as_typing();
                            let typing =
                                morph.apply_typing(FuncOrigin::Method(impl_, method), typing);
                            self.lir
                                .func(self.mir, self.iquery, self.info, tmap, typing, None)
                        }
                    })
                    .collect();

                let vtable = self.trait_impl_vtable(*trait_, impltor, methods);
                self.construct_dyn_object(&vtable, expr)
            }
            mir::Expr::Match(on, tree, branches) => {
                let on = self.expr_to_value(on);
                self.to_pat_lower(branches).run(on, tree)
            }
            mir::Expr::ReflectTypeOf(ty) => {
                let ty = to_morphization!(self, &mut self.current.tmap).apply_weak(ty);
                self.create_reflection(ty)
            }
            mir::Expr::SizeOf(ty) => {
                let ty = to_morphization!(self, &mut self.current.tmap).apply(ty);
                let size = self.lir.types.types.size_of(&ty) / 8;
                Value::Int(size as i128, Bitsize(64)) // TODO: 32-bit
            }
            mir::Expr::Cmp(cmp, params) => {
                let params = [
                    self.expr_to_value(&params[0]),
                    self.expr_to_value(&params[1]),
                ];

                let bitsize = match self.type_of_value(params[0]) {
                    MonoType::UInt(bitsize) | MonoType::Int(bitsize) => bitsize,
                    ty => panic!("not an int: {ty:?}"),
                };

                match *cmp {
                    "eq" => self.ssa().cmp(params, Ordering::Equal, bitsize),
                    "lt" => self.ssa().cmp(params, Ordering::Less, bitsize),
                    "gt" => self.ssa().cmp(params, Ordering::Greater, bitsize),
                    _ => panic!("unknown comparison operator: {cmp}"),
                }
                .value()
            }
            mir::Expr::Num(name, params) => {
                let [left, right] = [
                    self.expr_to_value(&params[0]),
                    self.expr_to_value(&params[1]),
                ];

                let ty = self.type_of_value(left);
                match *name {
                    "plus" => self.ssa().add(left, right, ty).into(),
                    "minus" => self.ssa().sub(left, right, ty).into(),
                    "mul" => self.ssa().mul(left, right, ty).into(),
                    "div" => self.ssa().div(left, right, ty).into(),
                    _ => panic!("unknown num builtin: {name}"),
                }
            }
            mir::Expr::Abort => Value::Int(1, Bitsize::default()),
            mir::Expr::Poison => todo!(),
        }
    }

    pub fn heap_alloc(&mut self, value: lir::Value, ty: MonoType) -> Value {
        let size = self.lir.types.types.size_of(&ty);
        if size == 0 {
            Value::UInt(0, Bitsize::default()) // TODO: target-dependent pointer size
        } else {
            let ptr = self.ssa().alloc(size, ty);
            self.ssa().write(ptr.value(), value);
            Value::V(ptr)
        }
    }

    fn call_nfunc(
        &mut self,
        func: M<ast::NFunc>,
        inst: &ConcreteInst,
        params: &[mir::Expr],
    ) -> Value {
        match self.resolve_nfunc(func, inst) {
            ResolvedNFunc::Extern(key, ret) => {
                let params = self.params_to_values(params);
                self.ssa().call_extern(key, params, ret).into()
            }
            ResolvedNFunc::Static(mfunc, ret) => {
                let params = self.params_to_values(params);
                self.ssa().call(mfunc, params, ret).into()
            }
            ResolvedNFunc::Sum { tag, payload_size, ty } => {
                let params = self.params_to_values(params);
                let parameters = self.elems_to_tuple(params, Some(payload_size));

                self.ssa()
                    .construct(vec![tag, parameters.into()], MonoType::Monomorphised(ty))
                    .into()
            }
            ResolvedNFunc::Val(key, ty) => {
                assert!(params.is_empty(), "giving parameters to the function returnt by a static value is not yet supported");
                let v = self.ssa().val_to_ref(key, ty.clone());
                self.ssa().deref(v.into(), ty).into()
            }
        }
    }

    fn call_closure(&mut self, objty: MonoTypeKey, obj: Value, params: Vec<Value>) -> Value {
        let objptr_type = self
            .lir
            .types
            .types
            .type_of_field(objty, TRAIT_OBJECT_DATA_FIELD);
        let vtable_ptr_type = self.lir.types.types.type_of_field(objty, VTABLE_FIELD);

        debug_assert_eq!(MonoType::u8_pointer(), objptr_type);

        let objptr = self
            .ssa()
            .field(obj, objty, TRAIT_OBJECT_DATA_FIELD, objptr_type);
        let vtableptr = self
            .ssa()
            .field(obj, objty, VTABLE_FIELD, vtable_ptr_type.clone());
        let vtable_type = vtable_ptr_type.clone().deref();
        let vtable = self.ssa().deref(vtableptr.into(), vtable_type);

        let (fnptr, ret) = {
            let call = key::RecordField(0);
            let vtable_key = vtable_ptr_type.deref().as_key();
            let ty = self.lir.types.types.type_of_field(vtable_key, call);
            let MonoType::FnPointer(_, ret) = ty.clone() else {
                panic!("first field of vtable was not an FnPointer")
            };
            let call_field = self
                .ssa()
                .field(vtable.into(), vtable_key, key::RecordField(0), ty);
            (call_field, *ret)
        };

        let param_tuple = self.elems_to_tuple(params, None);

        let call_method_params = vec![Value::V(objptr), param_tuple];

        self.ssa()
            .call(Value::from(fnptr), call_method_params, ret)
            .into()
    }

    fn callable_to_mfunc(&mut self, func: M<ast::NFunc>, inst: &ConcreteInst) -> MonoFunc {
        todo!("what's the difference between this function and `resolve_nfunc`? this seems overcomplicated");
        // Think we just accidentally wrote about the same function twice -.-
        match func.value {
            ast::NFunc::Key(key) => {
                let func = FuncOrigin::Defined(func.module.m(key));
                let tmap = self.morphise_inst([GenericKind::Parent, GenericKind::Entity], inst);
                let (mfunc, _) = self.call_to_mfunc(func, tmap);
                mfunc
            }
            ast::NFunc::Method(key, method) => {
                let trait_ = func.module.m(key);

                let morph = to_morphization!(self, &mut self.current.tmap);

                let self_ = inst.self_.as_ref().unwrap();

                todo!();
                // let trtp = inst
                //     .pgenerics
                //     .values()
                //     .map(|ty| morph.apply_weak(ty))
                //     .collect::<Vec<_>>();

                // let ikey = self.find_implementation(trait_, &trtp, &weak_impltor);

                // let forigin = FuncOrigin::Method(ikey, method);
                // let tmap = self.morphise_inst([GenericKind::Parent, GenericKind::Entity], inst);

                // self.call_to_mfunc(forigin, tmap).0
            }
            ast::NFunc::SumVar(sum, var) => {
                // let params = self.params_to_values(params);

                let sum = func.map(|_| sum);

                let ptypes = inst.generics.values().cloned().collect::<Vec<_>>();

                let mut morph = to_morphization!(self, &mut self.current.tmap);
                let mk = morph.sum(sum, &ptypes);

                let tag = Value::UInt(var.0 as u128, mono::TAG_SIZE);

                let size = self.lir.types.types.size_of_defined(mk);
                let largest = size - mono::TAG_SIZE.0 as u32;
                let inline = largest <= 128;
                let ty = MonoType::SumDataCast { largest };

                todo!();

                // let parameters = self.ssa().construct(params, ty);

                // self.current
                //     .ssa
                //     .construct(vec![tag, parameters.into()], MonoType::Monomorphised(mk))
                //     .into()
            }
            ast::NFunc::Val(_) => todo!(),
        }
    }

    pub fn find_implementation(
        &mut self,
        trait_: M<key::Trait>,
        trtp: &[Type],
        weak_impltor: Type,
        impltor: MonoType,
    ) -> (M<key::Impl>, TypeMap) {
        warn!(
            "conflicting implementations is not fully implemented. Weird auto-selections may occur"
        );

        let concrete_impltor = (&weak_impltor).try_into().ok();

        info!(
            "attempting to find `impl {trait_} {} for {}` in {}",
            trtp.iter().format(" "),
            weak_impltor,
            self.current.origin.name(self.mir)
        );

        self.iquery
            .for_each_relevant(trait_, concrete_impltor, |imp| {
                let iforall = &self.mir.impls[imp];
                let (_, trait_params) = &self.mir.itraits[imp];
                let iimpltor = &self.mir.impltors[imp];

                let mut comp = lumina_typesystem::Compatibility::new(
                    &self.iquery,
                    &|_| panic!("un-monomorphised generic in LHS"),
                    &iforall,
                    &|_| unreachable!(),
                );

                let valid = trtp
                    .iter()
                    .zip(trait_params)
                    .all(|(ty, ttp)| comp.cmp(ty, ttp))
                    && comp.cmp(&weak_impltor, iimpltor);

                valid.then(|| {
                    let mut tmap = TypeMap::new();
                    tmap.self_ = Some((weak_impltor.clone(), impltor.clone()));
                    for assignment in comp.into_assignments().into_iter() {
                        let mono =
                            to_morphization!(self, &mut TypeMap::new()).apply(&assignment.ty);
                        let generic = Generic::new(assignment.key, GenericKind::Parent);
                        tmap.generics.push((generic, (assignment.ty, mono)));
                    }
                    (imp, tmap)
                })
            })
            .unwrap()
    }

    pub fn call_to_mfunc(&mut self, func: FuncOrigin, mut tmap: TypeMap) -> (MonoFunc, MonoType) {
        assert!(
            !matches!(func, FuncOrigin::Lambda(..)),
            "call_to_value does not handle captures"
        );

        let fdef = func.get_root_fdef(self.mir);

        trace!(
            "monomorphising typing of call fn {func} as {}\n  with mapping {tmap:?}",
            &fdef.typing
        );

        let typing = to_morphization!(self, &mut tmap).apply_typing(func, &fdef.typing);
        let ret = typing.returns.clone();

        info!(
            "calling function {} ({})",
            self.lir.types.fmt(&typing),
            typing.origin.name(&self.mir)
        );

        let mfunc = self
            .lir
            .func(self.mir, self.iquery, self.info, tmap, typing, None);

        (mfunc, ret)
    }
}
