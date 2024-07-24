use super::*;
use crate::LISTABLE_SPLIT;
use mir::pat::{DecTree, Range, TreeTail};
use ssa::{Block, Value};
use std::collections::VecDeque;

impl<'a> FuncLower<'a> {
    pub fn to_pat_lower<'f, 'v>(
        &'f mut self,
        branches: &'v Map<key::DecisionTreeTail, mir::Expr>,
    ) -> PatLower<'f, 'v, 'a> {
        PatLower {
            continuation_block: None,
            continuation_value: None,

            expressions: branches.keys().map(|_| None).collect(),

            branches,

            f: self,

            constructors: vec![],

            map: vec![],

            can_skip_continuation: true,
        }
    }
}

pub struct PatLower<'f, 'v, 'a> {
    f: &'f mut FuncLower<'a>,

    // All branches put their yielded value as a parameter to this block and jump to it
    //
    // This desugars `f (match ...)`
    continuation_block: Option<ssa::Block>,
    continuation_value: Option<ssa::Value>,

    branches: &'v Map<key::DecisionTreeTail, mir::Expr>,
    expressions: Map<key::DecisionTreeTail, Option<Value>>,

    constructors: Vec<VecDeque<Value>>,
    map: Vec<ssa::Value>,

    can_skip_continuation: bool,
}

impl<'f, 'v, 'a> PatLower<'f, 'v, 'a> {
    fn ssa(&mut self) -> &mut ssa::Blocks {
        self.f.ssa()
    }

    fn block(&self) -> Block {
        self.f.lir.functions[self.f.current.mfkey].blocks.block()
    }

    pub fn run(&mut self, on: ssa::Value, tree: &mir::DecTree) -> Value {
        self.tree(on, tree);

        if self.can_skip_continuation {
            assert_eq!(self.continuation_block, None);
            self.continuation_value.unwrap()
        } else {
            assert_eq!(self.continuation_value, None);
            let block = self.continuation_block.unwrap();
            self.ssa().switch_to_block(block);
            ssa::Value::BlockParam(ssa::BlockParam(0))
        }
    }

    fn make_reset(&self) -> ResetPoint {
        ResetPoint {
            constructors: self.constructors.clone(),
            map: self.map.clone(),
        }
    }
    fn reset(&mut self, block: Block, point: ResetPoint) {
        self.ssa().switch_to_block(block);
        self.map = point.map;
        self.constructors = point.constructors;
    }

    fn tree(&mut self, on: ssa::Value, tree: &mir::DecTree) {
        let on = self.f.ensure_no_scope_escape(on);
        self.map.push(on);

        match tree {
            DecTree::Record { next, .. } => self.record(on, next),
            DecTree::Tuple { next, .. } => self.tuple(on, next),
            DecTree::List { next, ty } => self.list(on, ty, next),
            DecTree::Ints { bitsize, signed, next } => self.ints(on, *signed, *bitsize, next),
            DecTree::Bools(next) => self.bools(on, next),
            DecTree::Sum { sum, params, next } => self.sum(on, *sum, params, next),
            DecTree::Wildcard { next, .. } | DecTree::Opaque { next, .. } => self.next(next),
            DecTree::End(tail) => self.tail(tail),
        }
    }

    fn tail(&mut self, tail: &TreeTail<key::DecisionTreeTail>) {
        match tail {
            TreeTail::Poison => {}
            TreeTail::Unreached(_) => {}
            TreeTail::Reached(table, _excess, tail) => {
                for &(bind, i) in &table.binds {
                    let v = self.map[i];
                    trace!("binding {bind} -> {v}");
                    self.f.current.bindmap.insert(bind, v);
                }

                match self.expressions[*tail] {
                    Some(_) => {}
                    None => {
                        let expr = &self.branches[*tail];
                        let v = self.f.expr_to_value(expr);
                        self.expressions[*tail] = Some(v);

                        let ty = self.f.type_of_value(v);

                        if self.can_skip_continuation {
                            self.continuation_value = Some(v);
                        } else {
                            let con = self.get_continuation(ty);
                            self.ssa().jump_continuation(con, vec![v]);
                        }
                    }
                }
            }
        }
    }

    fn next(&mut self, tree: &mir::DecTree) {
        match self.constructors.last_mut() {
            Some(params) => match params.pop_front() {
                Some(v) => self.tree(v, tree),
                None => {
                    self.constructors.pop();
                    self.next(tree)
                }
            },
            None => match &tree {
                DecTree::End(tail) => self.tail(tail),
                other => unreachable!("misaligned constructor ordering:\n{other}"),
            },
        }
    }

    fn ints(
        &mut self,
        on: ssa::Value,
        signed: bool,
        bitsize: Bitsize,
        next: &mir::Branching<Range>,
    ) {
        let resetpoint = self.make_reset();

        for (range, next) in &next.branches {
            if range.end == range.con.max {
                return self.next(next);
            }

            let [on_true, on_false] = [self.ssa().new_block(), self.ssa().new_block()];

            let to_value = |n| {
                if signed {
                    Value::Int(n, bitsize)
                } else {
                    Value::UInt(n as u128, bitsize)
                }
            };

            let check = if range.end == range.start {
                // TODO: jump-table optimisation for adjecent single-numbers
                self.ssa().eq([on, to_value(range.end)], bitsize)
            } else {
                let mut check = self.ssa().lti([on, to_value(range.end)], bitsize);
                if range.con.min != range.start {
                    let high_enough = self.ssa().gti([on, to_value(range.start)], bitsize);
                    let ty = self.f.type_of_value(on);
                    check = self.ssa().bit_and([check.value(), high_enough.value()], ty);
                }
                check
            }
            .value();

            self.ssa()
                .select(check, [(on_true, vec![]), (on_false, vec![])]);

            self.ssa().switch_to_block(on_true);
            self.next(&next);

            self.reset(on_false, resetpoint.clone());
        }
    }

    fn tuple(&mut self, on: Value, next: &mir::DecTree) {
        let mk = self.f.type_of_value(on).as_key();

        let constructor = self
            .f
            .lir
            .types
            .fields(mk)
            .map(|field| {
                let ty = self.f.lir.types.types.type_of_field(mk, field);
                self.ssa().field(on, mk, field, ty).into()
            })
            .collect();

        self.constructors.push(constructor);

        self.next(next)
    }

    fn bools(&mut self, on: Value, v: &mir::Branching<bool>) {
        self.can_skip_continuation = false;

        let [fst, snd] = v.branches.as_slice() else {
            panic!("incorrect bool count");
        };

        let [truthy, falsey] = [
            fst.0.then_some(fst).unwrap_or(snd),
            fst.0.then_some(snd).unwrap_or(fst),
        ];

        let resetpoint = self.make_reset();

        let [on_true, on_false] = [self.ssa().new_block(), self.ssa().new_block()];

        self.ssa()
            .select(on, [(on_true, vec![]), (on_false, vec![])]);

        self.ssa().switch_to_block(on_true);
        self.next(&truthy.1);

        self.reset(on_false, resetpoint);
        self.next(&falsey.1);
    }

    fn list(&mut self, on: Value, ty: &Type, vars: &SumBranches) {
        self.can_skip_continuation = false;

        let oblock = self.block();
        let on = self.f.ensure_no_scope_escape(on);

        let mut morph = to_morphization!(self.f.lir, self.f.mir, &mut self.f.current.tmap);
        let listmt = morph.apply(&ty);
        let list = morph.apply_weak(&ty);
        let (_, inner) = match &ty {
            Type::Defined(kind, params) | Type::List(kind, params) => {
                let inner = params[0].clone();
                assert_eq!(params.len(), 1);
                (kind, inner)
            }
            _ => unreachable!(),
        };

        let innermt = morph.apply(&inner);
        let inner = morph.apply_weak(&inner);

        let (ikey, tmap) = self.f.find_implementation(
            self.f.info.listable,
            &[inner.clone()],
            list.clone(),
            listmt.clone(),
        );

        let split = FuncOrigin::Method(ikey, LISTABLE_SPLIT);
        let (split, ret) = self.f.call_to_mfunc(split, tmap);

        let maybe = self.ssa().call(split, vec![on], ret).value();
        let maybe_mk = self.f.type_of_value(maybe).as_key();

        let tag_ty = MonoType::UInt(mono::TAG_SIZE);
        let tag = self
            .ssa()
            .field(maybe, maybe_mk, key::RecordField(0), tag_ty);

        let data_ty = MonoType::SumDataCast {
            largest: self.f.lir.types.types.size_of_defined(maybe_mk) - mono::TAG_SIZE.0 as u32,
        };
        let data = self
            .ssa()
            .field(maybe, maybe_mk, key::RecordField(1), data_ty)
            .into();

        let is_just = self
            .ssa()
            .eq([tag.value(), Value::maybe_just()], mono::TAG_SIZE);

        let [con_block, nil_block] = [mir::pat::LIST_CONS, mir::pat::LIST_NIL].map(|constr| {
            let vblock = self.ssa().new_block();
            self.ssa().switch_to_block(vblock);

            let resetpoint = self.make_reset();

            let mut vparams = VecDeque::new();

            // Add parameters matching the MIR pattern of `Cons x xs`
            if constr == mir::pat::LIST_CONS {
                let mut offset = BitOffset(0);

                let x = self.ssa().sum_field(data, offset, innermt.clone());
                offset.0 += self.f.lir.types.types.size_of(&innermt) as u32;

                let xs = self.ssa().sum_field(data, offset, listmt.clone());

                vparams.push_back(x.value());
                vparams.push_back(xs.value());
            }

            self.constructors.push(vparams);

            let next = vars
                .branches
                .iter()
                .find_map(|(con, n)| (*con == constr).then_some(n))
                .unwrap();

            self.next(next);
            self.reset(oblock, resetpoint);

            vblock
        });

        self.ssa()
            .select(is_just.value(), [(con_block, vec![]), (nil_block, vec![])]);
    }

    fn record(&mut self, on: Value, next: &mir::DecTree) {
        let mk = self.f.type_of_value(on).as_key();

        let constructor = self
            .f
            .lir
            .types
            .fields(mk)
            .map(|field| {
                let ty = self.f.lir.types.types.type_of_field(mk, field);
                self.ssa().field(on, mk, field, ty).into()
            })
            .collect();

        self.constructors.push(constructor);

        self.next(next);
    }

    fn sum(&mut self, on: Value, sum: M<key::Sum>, params: &[Type], v: &SumBranches) {
        self.can_skip_continuation &= v.branches.len() == 1;

        let oblock = self.block();
        let on = self.f.ensure_no_scope_escape(on);
        let on_mk = self.f.type_of_value(on).as_key();

        let tag_ty = MonoType::UInt(mono::TAG_SIZE);
        let copy_tag = self.ssa().field(on, on_mk, key::RecordField(0), tag_ty);

        let data = self
            .f
            .lir
            .types
            .types
            .type_of_field(on_mk, key::RecordField(1));

        let data_field = self.ssa().field(on, on_mk, key::RecordField(1), data);

        assert!(
            v.branches
                .windows(2)
                .all(|branch| branch[0].0 .0 == branch[1].0 .0 - 1),
            "sum variants in decision tree are meant to be sorted"
        );

        let jmp_table_blocks = v
            .branches
            .iter()
            .map(|(var, next)| {
                let vblock = self.ssa().new_block();
                self.ssa().switch_to_block(vblock);

                let resetpoint = self.make_reset();

                let finst = lumina_typesystem::ForeignInst::from_type_params(params);
                let raw_var_types = &self.f.mir.variant_types[sum][*var];

                let mut base_offset = BitOffset(0);
                let params = raw_var_types
                    .iter()
                    .map(|ty| {
                        let ty = finst.apply(ty);
                        let ty = to_morphization!(self.f.lir, self.f.mir, &mut self.f.current.tmap)
                            .apply(&ty);

                        let size = self.f.lir.types.types.size_of(&ty) as u32;
                        let offset = base_offset;
                        base_offset.0 += size;

                        self.ssa().sum_field(data_field.into(), offset, ty).into()
                    })
                    .collect();

                self.constructors.push(params);

                self.next(next);
                self.reset(oblock, resetpoint);

                vblock
            })
            .collect();

        self.ssa().jump_table(copy_tag.into(), jmp_table_blocks);
    }

    pub fn get_continuation(&mut self, ty: MonoType) -> Block {
        match self.continuation_block {
            Some(block) => block,
            None => {
                let block = self.ssa().new_block();
                self.ssa().add_block_param(block, ty);
                self.continuation_block = Some(block);
                block
            }
        }
    }
}

#[derive(Clone)]
struct ResetPoint {
    constructors: Vec<VecDeque<Value>>,
    map: Vec<ssa::Value>,
}

type SumBranches = mir::Branching<key::SumVariant>;
