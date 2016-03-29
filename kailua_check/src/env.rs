use std::fmt;
use std::mem;
use std::ops;
use std::cell::Cell;
use std::collections::HashMap;
use vec_map::VecMap;

use kailua_syntax::Name;
use diag::CheckResult;
use ty::{Ty, T, TVar, Mark, Lattice, TypeContext};

#[derive(Clone)]
pub struct TyInfo {
    pub ty: T<'static>,
}

impl TyInfo {
    pub fn from<'a>(ty: T<'a>) -> TyInfo {
        TyInfo { ty: ty.into_send() }
    }
}

impl fmt::Debug for TyInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.ty)
    }
}

pub struct Frame {
    pub vararg: Option<TyInfo>,
    pub returns: Ty,
}

pub struct Scope {
    names: HashMap<Name, TyInfo>,
    frame: Option<Frame>,
}

impl Scope {
    pub fn new() -> Scope {
        Scope { names: HashMap::new(), frame: None }
    }

    pub fn new_function(frame: Frame) -> Scope {
        Scope { names: HashMap::new(), frame: Some(frame) }
    }

    pub fn get<'a>(&'a self, name: &Name) -> Option<&'a TyInfo> {
        self.names.get(name)
    }

    pub fn get_mut<'a>(&'a mut self, name: &Name) -> Option<&'a mut TyInfo> {
        self.names.get_mut(name)
    }

    pub fn get_frame<'a>(&'a self) -> Option<&'a Frame> {
        self.frame.as_ref()
    }

    pub fn get_frame_mut<'a>(&'a mut self) -> Option<&'a mut Frame> {
        self.frame.as_mut()
    }

    pub fn put(&mut self, name: Name, info: TyInfo) {
        self.names.insert(name, info);
    }
}

trait Partition {
    fn create(parent: usize, rank: usize) -> Self;
    fn read(&self) -> (usize /*parent*/, usize /*rank*/);
    fn write_parent(&self, parent: usize);
    fn increment_rank(&mut self);
}

struct Partitions<T> {
    map: VecMap<T>,
}

impl<T: Partition> Partitions<T> {
    fn new() -> Partitions<T> {
        Partitions { map: VecMap::new() }
    }

    fn find(&self, i: usize) -> usize {
        if let Some(u) = self.map.get(&i) {
            let (mut parent, _) = u.read();
            if parent != i { // path compression
                while let Some(v) = self.map.get(&parent) {
                    let (newparent, _) = v.read();
                    if newparent == parent { break; }
                    parent = newparent;
                }
                u.write_parent(parent);
            }
            parent
        } else {
            i
        }
    }

    fn union(&mut self, lhs: usize, rhs: usize) -> usize {
        use std::cmp::Ordering;

        let lhs = self.find(lhs);
        let rhs = self.find(rhs);
        if lhs == rhs { return rhs; }

        let (_, lrank) = self.map.entry(lhs).or_insert_with(|| Partition::create(lhs, 0)).read();
        let (_, rrank) = self.map.entry(rhs).or_insert_with(|| Partition::create(rhs, 0)).read();
        match lrank.cmp(&rrank) {
            Ordering::Less => {
                self.map.get_mut(&lhs).unwrap().write_parent(rhs);
                rhs
            }
            Ordering::Greater => {
                self.map.get_mut(&rhs).unwrap().write_parent(lhs);
                lhs
            }
            Ordering::Equal => {
                self.map.get_mut(&rhs).unwrap().write_parent(lhs);
                self.map.get_mut(&lhs).unwrap().increment_rank();
                lhs
            }
        }
    }
}

impl<T> ops::Deref for Partitions<T> {
    type Target = VecMap<T>;
    fn deref(&self) -> &VecMap<T> { &self.map }
}

impl<T> ops::DerefMut for Partitions<T> {
    fn deref_mut(&mut self) -> &mut VecMap<T> { &mut self.map }
}

struct Bound {
    parent: Cell<u32>,
    rank: u8,
    bound: Option<Ty>,
}

// a set of constraints that can be organized as a tree
struct Constraints {
    op: &'static str,
    bounds: Partitions<Box<Bound>>,
}

// is this bound trivial so that one can always overwrite?
fn is_bound_trivial(t: &Option<Ty>) -> bool {
    // TODO special casing ? is not enough, should resolve b.bound's inner ?s as well
    if let Some(ref t) = *t {
        match **t { T::None | T::Dynamic => true, _ => false }
    } else {
        true
    }
}

impl Partition for Box<Bound> {
    fn create(parent: usize, rank: usize) -> Box<Bound> {
        Box::new(Bound { parent: Cell::new(parent as u32), rank: rank as u8, bound: None })
    }

    fn read(&self) -> (usize /*parent*/, usize /*rank*/) {
        (self.parent.get() as usize, self.rank as usize)
    }

    fn write_parent(&self, parent: usize) {
        self.parent.set(parent as u32);
    }

    fn increment_rank(&mut self) {
        self.rank += 1;
    }
}

impl Constraints {
    fn new(op: &'static str) -> Constraints {
        Constraints { op: op, bounds: Partitions::new() }
    }

    fn is(&self, lhs: TVar, rhs: TVar) -> bool {
        lhs == rhs || self.bounds.find(lhs.0 as usize) == self.bounds.find(rhs.0 as usize)
    }

    fn get_bound<'a>(&'a self, lhs: TVar) -> Option<&'a Bound> {
        let lhs = self.bounds.find(lhs.0 as usize);
        self.bounds.get(&lhs).map(|b| &**b)
    }

    fn add_bound(&mut self, lhs: TVar, rhs: &T) -> CheckResult<()> {
        let lhs_ = self.bounds.find(lhs.0 as usize);
        let b = self.bounds.entry(lhs_).or_insert_with(|| Partition::create(lhs_, 0));
        if is_bound_trivial(&b.bound) {
            b.bound = Some(Box::new(rhs.clone().into_send()));
        } else if b.bound.as_ref().map(|t| &**t) != Some(rhs) {
            // TODO check if this restriction has a real world implication
            return Err(format!("variable {:?} cannot have multiple bounds \
                                (original {} {:?}, later {} {:?})",
                               lhs, self.op, b.bound, self.op, *rhs));
        }
        Ok(())
    }

    fn add_relation(&mut self, lhs: TVar, rhs: TVar) -> CheckResult<()> {
        if lhs == rhs { return Ok(()); }

        let lhs_ = self.bounds.find(lhs.0 as usize);
        let rhs_ = self.bounds.find(rhs.0 as usize);
        if lhs_ == rhs_ { return Ok(()); }

        fn take_bound(bounds: &mut VecMap<Box<Bound>>, i: usize) -> Option<Ty> {
            if let Some(b) = bounds.get_mut(&i) {
                mem::replace(&mut b.bound, None)
            } else {
                None
            }
        }

        // take the bounds from each representative variable.
        let lhsbound = take_bound(&mut self.bounds, lhs_);
        let rhsbound = take_bound(&mut self.bounds, rhs_);

        let bound = match (is_bound_trivial(&lhsbound), is_bound_trivial(&rhsbound)) {
            (false, _) => rhsbound,
            (true, false) => lhsbound,
            (true, true) =>
                if lhsbound == rhsbound {
                    lhsbound
                } else {
                    return Err(format!("variables {:?}/{:?} cannot have multiple bounds \
                                        (left {} {:?}, right {} {:?})",
                                       lhs, rhs, self.op, lhsbound, self.op, rhsbound));
                },
        };

        // update the shared bound to the merged representative
        let new = self.bounds.union(lhs_, rhs_);
        if !is_bound_trivial(&bound) {
            // the merged entry should have non-zero rank, so unwrap() is fine
            self.bounds.get_mut(&new).unwrap().bound = bound;
        }

        Ok(())
    }
}

struct MarkDeps {
    // this mark implies the following mark
    follows: Option<Mark>,
    // the preceding mark implies this mark
    precedes: Option<Mark>,
    // this mark requires all those types to be equal
    // the first type is considered the base type and should be associated to this mark forever
    eq_types: Option<(T<'static>, Vec<T<'static>>)>,
}

impl MarkDeps {
    fn new() -> MarkDeps {
        MarkDeps { follows: None, precedes: None, eq_types: None }
    }

    fn assert_true(self, ctx: &mut TypeContext) -> CheckResult<()> {
        if let Some((ref base, ref others)) = self.eq_types {
            for other in others { try!(base.assert_eq(other, ctx)); }
        }
        if let Some(follows) = self.follows {
            try!(ctx.assert_mark_true(follows));
        }
        Ok(())
    }

    fn assert_false(self, ctx: &mut TypeContext) -> CheckResult<()> {
        if let Some(precedes) = self.precedes {
            try!(ctx.assert_mark_false(precedes));
        }
        Ok(())
    }

    fn merge(self, other: MarkDeps, ctx: &mut TypeContext) -> CheckResult<MarkDeps> {
        // while technically possible, the base type should be equal for the simplicity.
        let eq_types = match (self.eq_types, other.eq_types) {
            (None, None) => None,
            (None, Some(r)) => Some(r),
            (Some(l), None) => Some(l),
            (Some((lb, mut lt)), Some((rb, rt))) => {
                try!(lb.assert_eq(&rb, ctx));
                lt.extend(rt.into_iter());
                Some((lb, lt))
            }
        };

        let merge_marks = |l: Option<Mark>, r: Option<Mark>| match (l, r) {
            (None, None) => None,
            (None, Some(r)) => Some(r),
            (Some(l), None) => Some(l),
            (Some(_), Some(_)) => panic!("non-linear deps detected"),
        };

        let follows = merge_marks(self.follows, other.follows);
        let precedes = merge_marks(self.precedes, other.precedes);
        Ok(MarkDeps { follows: follows, precedes: precedes, eq_types: eq_types })
    }
}

enum MarkValue {
    Invalid,
    True,
    False,
    Unknown(Option<Box<MarkDeps>>),
}

impl MarkValue {
    fn assert_true(self, mark: Mark, ctx: &mut TypeContext) -> CheckResult<()> {
        match self {
            MarkValue::Invalid => panic!("self-recursive mark resolution"),
            MarkValue::True => Ok(()),
            MarkValue::False => Err(format!("mark {:?} cannot be true", mark)),
            MarkValue::Unknown(None) => Ok(()),
            MarkValue::Unknown(Some(deps)) => deps.assert_true(ctx),
        }
    }

    fn assert_false(self, mark: Mark, ctx: &mut TypeContext) -> CheckResult<()> {
        match self {
            MarkValue::Invalid => panic!("self-recursive mark resolution"),
            MarkValue::True => Err(format!("mark {:?} cannot be false", mark)),
            MarkValue::False => Ok(()),
            MarkValue::Unknown(None) => Ok(()),
            MarkValue::Unknown(Some(deps)) => deps.assert_false(ctx),
        }
    }
}

struct MarkInfo {
    parent: Cell<u32>,
    rank: u8,
    value: MarkValue,
}

impl Partition for Box<MarkInfo> {
    fn create(parent: usize, rank: usize) -> Box<MarkInfo> {
        Box::new(MarkInfo { parent: Cell::new(parent as u32), rank: rank as u8,
                            value: MarkValue::Unknown(None) })
    }

    fn read(&self) -> (usize /*parent*/, usize /*rank*/) {
        (self.parent.get() as usize, self.rank as usize)
    }

    fn write_parent(&self, parent: usize) {
        self.parent.set(parent as u32);
    }

    fn increment_rank(&mut self) {
        self.rank += 1;
    }
}

pub struct Context {
    global_scope: Scope,
    next_tvar: Cell<TVar>,
    tvar_sub: Constraints, // upper bound
    tvar_sup: Constraints, // lower bound
    tvar_eq: Constraints, // tight bound
    next_mark: Cell<Mark>,
    mark_infos: Partitions<Box<MarkInfo>>,
}

impl Context {
    pub fn new() -> Context {
        Context {
            global_scope: Scope::new(),
            next_tvar: Cell::new(TVar(0)),
            tvar_sub: Constraints::new("<:"),
            tvar_sup: Constraints::new(":>"),
            tvar_eq: Constraints::new("="),
            next_mark: Cell::new(Mark(0)),
            mark_infos: Partitions::new(),
        }
    }

    pub fn global_scope(&self) -> &Scope {
        &self.global_scope
    }

    pub fn global_scope_mut(&mut self) -> &mut Scope {
        &mut self.global_scope
    }
}

impl TypeContext for Context {
    fn last_tvar(&self) -> Option<TVar> {
        let tvar = self.next_tvar.get();
        if tvar == TVar(0) { None } else { Some(TVar(tvar.0 - 1)) }
    }

    fn gen_tvar(&mut self) -> TVar {
        let tvar = self.next_tvar.get();
        self.next_tvar.set(TVar(tvar.0 + 1));
        tvar
    }

    fn assert_tvar_sub(&mut self, lhs: TVar, rhs: &T) -> CheckResult<()> {
        println!("adding a constraint {:?} <: {:?}", lhs, *rhs);
        try!(self.tvar_sub.add_bound(lhs, rhs));
        if let Some(eb) = self.tvar_eq.get_bound(lhs).and_then(|b| b.bound.clone()) {
            try!((*eb).assert_sub(rhs, self));
        }
        if let Some(lb) = self.tvar_sup.get_bound(lhs).and_then(|b| b.bound.clone()) {
            try!((*lb).assert_sub(rhs, self));
        }
        Ok(())
    }

    fn assert_tvar_sup(&mut self, lhs: TVar, rhs: &T) -> CheckResult<()> {
        println!("adding a constraint {:?} :> {:?}", lhs, *rhs);
        try!(self.tvar_sup.add_bound(lhs, rhs));
        if let Some(eb) = self.tvar_eq.get_bound(lhs).and_then(|b| b.bound.clone()) {
            try!(rhs.assert_sub(&eb, self));
        }
        if let Some(ub) = self.tvar_sub.get_bound(lhs).and_then(|b| b.bound.clone()) {
            try!(rhs.assert_sub(&ub, self));
        }
        Ok(())
    }

    fn assert_tvar_eq(&mut self, lhs: TVar, rhs: &T) -> CheckResult<()> {
        println!("adding a constraint {:?} = {:?}", lhs, *rhs);
        try!(self.tvar_eq.add_bound(lhs, rhs));
        if let Some(ub) = self.tvar_sub.get_bound(lhs).and_then(|b| b.bound.clone()) {
            try!(rhs.assert_sub(&ub, self));
        }
        if let Some(lb) = self.tvar_sup.get_bound(lhs).and_then(|b| b.bound.clone()) {
            try!((*lb).assert_sub(rhs, self));
        }
        Ok(())
    }

    fn assert_tvar_sub_tvar(&mut self, lhs: TVar, rhs: TVar) -> CheckResult<()> {
        println!("adding a constraint {:?} <: {:?}", lhs, rhs);
        if !self.tvar_eq.is(lhs, rhs) {
            try!(self.tvar_sub.add_relation(lhs, rhs));
            try!(self.tvar_sup.add_relation(rhs, lhs));
        }
        Ok(())
    }

    fn assert_tvar_eq_tvar(&mut self, lhs: TVar, rhs: TVar) -> CheckResult<()> {
        println!("adding a constraint {:?} = {:?}", lhs, rhs);
        // do not update tvar_sub & tvar_sup, 
        self.tvar_eq.add_relation(lhs, rhs)
    }

    fn gen_mark(&mut self) -> Mark {
        let mark = self.next_mark.get();
        self.next_mark.set(Mark(mark.0 + 1));
        mark
    }

    fn assert_mark_true(&mut self, mark: Mark) -> CheckResult<()> {
        println!("asserting {:?} is true", mark);
        let mark_ = self.mark_infos.find(mark.0 as usize);
        let value = {
            // take the value out of the mapping. even if the mark is somehow recursively consulted
            // (which is normally an error), it should assume that it's true by now.
            let info = self.mark_infos.entry(mark_).or_insert_with(|| Partition::create(mark_, 0));
            mem::replace(&mut info.value, MarkValue::True)
        };
        value.assert_true(mark, self)
    }

    fn assert_mark_false(&mut self, mark: Mark) -> CheckResult<()> {
        println!("asserting {:?} is false", mark);
        let mark_ = self.mark_infos.find(mark.0 as usize);
        let value = {
            // same as above, but it's false instead
            let info = self.mark_infos.entry(mark_).or_insert_with(|| Partition::create(mark_, 0));
            mem::replace(&mut info.value, MarkValue::False)
        };
        value.assert_false(mark, self)
    }

    fn assert_mark_eq(&mut self, lhs: Mark, rhs: Mark) -> CheckResult<()> {
        println!("asserting {:?} and {:?} are same", lhs, rhs);

        if lhs == rhs { return Ok(()); }

        let lhs_ = self.mark_infos.find(lhs.0 as usize);
        let rhs_ = self.mark_infos.find(rhs.0 as usize);
        if lhs_ == rhs_ { return Ok(()); }

        { // early error checks
            let lvalue = self.mark_infos.get(&lhs_).map(|info| &info.value);
            let rvalue = self.mark_infos.get(&rhs_).map(|info| &info.value);
            match (lvalue, rvalue) {
                (Some(&MarkValue::Invalid), _) | (_, Some(&MarkValue::Invalid)) =>
                    panic!("self-recursive mark resolution"),
                (Some(&MarkValue::True), Some(&MarkValue::True)) => return Ok(()),
                (Some(&MarkValue::True), Some(&MarkValue::False)) =>
                    return Err(format!("{:?} (known to be true) and {:?} (known to be false) \
                                        cannot never be same", lhs, rhs)),
                (Some(&MarkValue::False), Some(&MarkValue::True)) =>
                    return Err(format!("{:?} (known to be false) and {:?} (known to be true) \
                                        cannot never be same", lhs, rhs)),
                (Some(&MarkValue::False), Some(&MarkValue::False)) => return Ok(()),
                (_, _) => {}
            }
        }

        fn take_value(mark_infos: &mut VecMap<Box<MarkInfo>>, i: usize) -> MarkValue {
            if let Some(info) = mark_infos.get_mut(&i) {
                mem::replace(&mut info.value, MarkValue::Invalid)
            } else {
                MarkValue::Unknown(None)
            }
        }

        let lvalue = take_value(&mut self.mark_infos, lhs_);
        let rvalue = take_value(&mut self.mark_infos, rhs_);

        let new = self.mark_infos.union(lhs_, rhs_);

        let newvalue = match (lvalue, rvalue) {
            (MarkValue::Invalid, _) | (_, MarkValue::Invalid) |
            (MarkValue::True, MarkValue::True) |
            (MarkValue::True, MarkValue::False) |
            (MarkValue::False, MarkValue::True) |
            (MarkValue::False, MarkValue::False) => unreachable!(),

            (MarkValue::True, MarkValue::Unknown(deps)) |
            (MarkValue::Unknown(deps), MarkValue::True) => {
                if let Some(deps) = deps { try!(deps.assert_true(self)); }
                MarkValue::True
            }

            (MarkValue::False, MarkValue::Unknown(deps)) |
            (MarkValue::Unknown(deps), MarkValue::False) => {
                if let Some(deps) = deps { try!(deps.assert_false(self)); }
                MarkValue::False
            }

            (MarkValue::Unknown(None), MarkValue::Unknown(None)) =>
                MarkValue::Unknown(None),
            (MarkValue::Unknown(None), MarkValue::Unknown(Some(deps))) |
            (MarkValue::Unknown(Some(deps)), MarkValue::Unknown(None)) =>
                MarkValue::Unknown(Some(deps)),

            // the only case that we need the true merger of dependencies
            (MarkValue::Unknown(Some(mut ldeps)), MarkValue::Unknown(Some(mut rdeps))) => {
                // implication may refer to each other; they should be eliminated first
                if ldeps.follows  == Some(Mark(rhs_ as u32)) { ldeps.follows  = None; }
                if ldeps.precedes == Some(Mark(rhs_ as u32)) { ldeps.precedes = None; }
                if rdeps.follows  == Some(Mark(lhs_ as u32)) { rdeps.follows  = None; }
                if rdeps.precedes == Some(Mark(lhs_ as u32)) { rdeps.precedes = None; }

                let deps = try!(ldeps.merge(*rdeps, self));

                // update dependencies for *other* marks depending on this mark
                if let Some(m) = deps.follows {
                    let info = self.mark_infos.get_mut(&(m.0 as usize)).unwrap();
                    if let MarkValue::Unknown(Some(ref mut deps)) = info.value {
                        assert!(deps.precedes == Some(Mark(lhs_ as u32)) ||
                                deps.precedes == Some(Mark(rhs_ as u32)));
                        deps.precedes = Some(Mark(new as u32));
                    } else {
                        panic!("desynchronized dependency implication \
                                from {:?} or {:?} to {:?}", lhs, rhs, m);
                    }
                }
                if let Some(m) = deps.precedes {
                    let info = self.mark_infos.get_mut(&(m.0 as usize)).unwrap();
                    if let MarkValue::Unknown(Some(ref mut deps)) = info.value {
                        assert!(deps.follows == Some(Mark(lhs_ as u32)) ||
                                deps.follows == Some(Mark(rhs_ as u32)));
                        deps.follows = Some(Mark(new as u32));
                    } else {
                        panic!("desynchronized dependency implication \
                                from {:?} to {:?} or {:?}", m, lhs, rhs);
                    }
                }

                MarkValue::Unknown(Some(Box::new(deps)))
            }
        };

        self.mark_infos.get_mut(&new).unwrap().value = newvalue;
        Ok(())
    }

    fn assert_mark_imply(&mut self, lhs: Mark, rhs: Mark) -> CheckResult<()> {
        println!("asserting {:?} implies {:?}", lhs, rhs);

        if lhs == rhs { return Ok(()); }

        let lhs_ = self.mark_infos.find(lhs.0 as usize);
        let rhs_ = self.mark_infos.find(rhs.0 as usize);
        if lhs_ == rhs_ { return Ok(()); }

        enum Next { AddDeps, AssertRhsTrue, AssertLhsFalse }
        let next = { // early error checks
            let lvalue = self.mark_infos.get(&lhs_).map(|info| &info.value);
            let rvalue = self.mark_infos.get(&rhs_).map(|info| &info.value);
            match (lvalue, rvalue) {
                (Some(&MarkValue::Invalid), _) | (_, Some(&MarkValue::Invalid)) =>
                    panic!("self-recursive mark resolution"),
                (Some(&MarkValue::True), Some(&MarkValue::True)) => return Ok(()),
                (Some(&MarkValue::True), Some(&MarkValue::False)) =>
                    return Err(format!("{:?} (known to be true) cannot imply \
                                        {:?} (known to be false)", lhs, rhs)),
                (Some(&MarkValue::True), _) => Next::AssertRhsTrue,
                (Some(&MarkValue::False), _) => return Ok(()),
                (_, Some(&MarkValue::True)) => return Ok(()),
                (_, Some(&MarkValue::False)) => Next::AssertLhsFalse,
                (_, _) => Next::AddDeps,
            }
        };

        fn get_deps_mut(mark_infos: &mut VecMap<Box<MarkInfo>>, i: usize) -> &mut MarkDeps {
            let info = mark_infos.entry(i).or_insert_with(|| Partition::create(i, 0));
            if let MarkValue::Unknown(ref mut deps) = info.value {
                if deps.is_none() { *deps = Some(Box::new(MarkDeps::new())); }
                deps.as_mut().unwrap()
            } else {
                unreachable!()
            }
        }

        fn take_deps(mark_infos: &mut VecMap<Box<MarkInfo>>, i: usize,
                     repl: MarkValue) -> Option<Box<MarkDeps>> {
            let info = mark_infos.entry(i).or_insert_with(|| Partition::create(i, 0));
            if let MarkValue::Unknown(deps) = mem::replace(&mut info.value, repl) {
                deps
            } else {
                unreachable!()
            }
        }

        match next {
            Next::AddDeps => { // unknown implies unknown
                {
                    let deps = get_deps_mut(&mut self.mark_infos, lhs_);
                    let follows = Mark(rhs_ as u32);
                    if deps.follows == None {
                        deps.follows = Some(follows);
                    } else if deps.follows != Some(follows) {
                        panic!("non-linear deps detected");
                    }
                }

                {
                    let deps = get_deps_mut(&mut self.mark_infos, rhs_);
                    let precedes = Mark(lhs_ as u32);
                    if deps.precedes == None {
                        deps.precedes = Some(precedes);
                    } else if deps.precedes != Some(precedes) {
                        panic!("non-linear deps detected");
                    }
                }
            }

            Next::AssertRhsTrue => { // true implies unknown
                if let Some(deps) = take_deps(&mut self.mark_infos, rhs_, MarkValue::True) {
                    try!(deps.assert_true(self));
                }
            }

            Next::AssertLhsFalse => { // unknown implies false
                if let Some(deps) = take_deps(&mut self.mark_infos, lhs_, MarkValue::False) {
                    try!(deps.assert_false(self));
                }
            }
        }

        Ok(())
    }

    fn assert_mark_require(&mut self, mark: Mark, base: &T, ty: &T) -> CheckResult<()> {
        println!("asserting {:?} requires {:?} = {:?}", mark, *base, *ty);

        let mark_ = self.mark_infos.find(mark.0 as usize);

        let mut value = {
            let info = self.mark_infos.entry(mark_).or_insert_with(|| Partition::create(mark_, 0));
            mem::replace(&mut info.value, MarkValue::Invalid)
        };

        let ret = (|value: &mut MarkValue| {
            match *value {
                MarkValue::Invalid => panic!("self-recursive mark resolution"),
                MarkValue::True => base.assert_eq(ty, self),
                MarkValue::False => Ok(()),
                MarkValue::Unknown(ref mut deps) => {
                    if deps.is_none() { *deps = Some(Box::new(MarkDeps::new())); }
                    let deps = deps.as_mut().unwrap();

                    // XXX probably we can test if `base = ty` this early with a wrapped context
                    if let Some(ref mut eq_types) = deps.eq_types {
                        try!(base.assert_eq(&mut eq_types.0, self));
                        eq_types.1.push(ty.clone().into_send());
                    } else {
                        deps.eq_types = Some((base.clone().into_send(),
                                              vec![ty.clone().into_send()]));
                    }
                    Ok(())
                }
            }
        })(&mut value);

        self.mark_infos.get_mut(&mark_).unwrap().value = value;
        ret
    }
}

pub struct Env<'ctx> {
    context: &'ctx mut Context,
    scopes: Vec<Scope>,
}

impl<'ctx> Env<'ctx> {
    pub fn new(context: &'ctx mut Context) -> Env<'ctx> {
        // we have local variables even at the global position, so we need at least one Scope
        Env { context: context, scopes: vec![Scope::new()] }
    }

    // not to be called internally; it intentionally reduces the lifetime
    pub fn context(&mut self) -> &mut Context {
        self.context
    }

    pub fn enter(&mut self, scope: Scope) {
        self.scopes.push(scope);
    }

    pub fn leave(&mut self) {
        assert!(self.scopes.len() > 1);
        self.scopes.pop();
    }

    // not to be called internally; it intentionally reduces the lifetime
    pub fn global_scope(&self) -> &Scope {
        self.context.global_scope()
    }

    // not to be called internally; it intentionally reduces the lifetime
    pub fn global_scope_mut(&mut self) -> &mut Scope {
        self.context.global_scope_mut()
    }

    pub fn current_scope(&self) -> &Scope {
        self.scopes.last().unwrap()
    }

    pub fn current_scope_mut(&mut self) -> &mut Scope {
        self.scopes.last_mut().unwrap()
    }

    pub fn get_var<'a>(&'a self, name: &Name) -> Option<&'a TyInfo> {
        for scope in self.scopes.iter().rev() {
            if let Some(info) = scope.get(name) { return Some(info); }
        }
        self.context.global_scope().get(name)
    }

    pub fn get_var_mut<'a>(&'a mut self, name: &Name) -> Option<&'a mut TyInfo> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(info) = scope.get_mut(name) { return Some(info); }
        }
        self.context.global_scope_mut().get_mut(name)
    }

    pub fn get_local_var<'a>(&'a self, name: &Name) -> Option<&'a TyInfo> {
        for scope in self.scopes.iter().rev() {
            if let Some(info) = scope.get(name) { return Some(info); }
        }
        None
    }

    pub fn get_local_var_mut<'a>(&'a mut self, name: &Name) -> Option<&'a mut TyInfo> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(info) = scope.get_mut(name) { return Some(info); }
        }
        None
    }

    pub fn get_frame<'a>(&'a self) -> Option<&'a Frame> {
        for scope in self.scopes.iter().rev() {
            if let Some(frame) = scope.get_frame() { return Some(frame); }
        }
        self.context.global_scope().get_frame()
    }

    pub fn get_frame_mut<'a>(&'a mut self) -> Option<&'a mut Frame> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(frame) = scope.get_frame_mut() { return Some(frame); }
        }
        self.context.global_scope_mut().get_frame_mut()
    }

    pub fn get_vararg<'a>(&'a self) -> Option<&'a TyInfo> {
        self.get_frame().and_then(|f| f.vararg.as_ref())
    }

    pub fn get_vararg_mut<'a>(&'a mut self) -> Option<&'a mut TyInfo> {
        self.get_frame_mut().and_then(|f| f.vararg.as_mut())
    }

    pub fn add_local_var(&mut self, name: &Name, info: TyInfo) {
        println!("adding a local variable {:?} as {:?}", *name, info);
        self.current_scope_mut().put(name.to_owned(), info);
    }

    pub fn assign_to_var(&mut self, name: &Name, info: TyInfo) -> CheckResult<()> {
        if let Some(previnfo) = self.get_local_var_mut(name) {
            if !previnfo.ty.accept(&info.ty) {
                return Err(format!("cannot assign {:?} to the variable {:?} with type {:?}",
                                   info.ty, name, previnfo.ty));
            } else {
                println!("assigning {:?} to a local variable {:?} with type {:?}",
                         info, *name, *previnfo);
                return Ok(());
            }
        }

        println!("adding a global variable {:?} as {:?}", *name, info);
        self.context.global_scope_mut().put(name.to_owned(), info);
        Ok(())
    }

    pub fn assume_var(&mut self, name: &Name, info: TyInfo) -> CheckResult<()> {
        if let Some(previnfo) = self.get_local_var_mut(name) {
            println!("(force) adding a local variable {:?} as {:?}", *name, info);
            *previnfo = info;
            return Ok(());
        }

        println!("(force) adding a global variable {:?} as {:?}", *name, info);
        self.context.global_scope_mut().put(name.to_owned(), info);
        Ok(())
    }
}

#[test]
fn test_context_tvar() {
    let mut ctx = Context::new();

    { // idempotency of bounds
        let v1 = ctx.gen_tvar();
        assert_eq!(ctx.assert_tvar_sub(v1, &T::integer()), Ok(()));
        assert_eq!(ctx.assert_tvar_sub(v1, &T::integer()), Ok(()));
        assert!(ctx.assert_tvar_sub(v1, &T::string()).is_err());
    }

    { // empty bounds (lb & ub = bottom)
        let v1 = ctx.gen_tvar();
        assert_eq!(ctx.assert_tvar_sub(v1, &T::integer()), Ok(()));
        assert!(ctx.assert_tvar_sup(v1, &T::string()).is_err());

        let v2 = ctx.gen_tvar();
        assert_eq!(ctx.assert_tvar_sup(v2, &T::integer()), Ok(()));
        assert!(ctx.assert_tvar_sub(v2, &T::string()).is_err());
    }

    { // empty bounds (lb & ub != bottom)
        let v1 = ctx.gen_tvar();
        assert_eq!(ctx.assert_tvar_sub(v1, &T::ints(vec![3, 4, 5])), Ok(()));
        assert!(ctx.assert_tvar_sup(v1, &T::ints(vec![1, 2, 3])).is_err());

        let v2 = ctx.gen_tvar();
        assert_eq!(ctx.assert_tvar_sup(v2, &T::ints(vec![3, 4, 5])), Ok(()));
        assert!(ctx.assert_tvar_sub(v2, &T::ints(vec![1, 2, 3])).is_err());
    }

    { // implicitly disjoint bounds
        let v1 = ctx.gen_tvar();
        let v2 = ctx.gen_tvar();
        assert_eq!(ctx.assert_tvar_sub_tvar(v1, v2), Ok(()));
        assert_eq!(ctx.assert_tvar_sub(v2, &T::string()), Ok(()));
        assert!(ctx.assert_tvar_sub(v1, &T::integer()).is_err());

        let v3 = ctx.gen_tvar();
        let v4 = ctx.gen_tvar();
        assert_eq!(ctx.assert_tvar_sub_tvar(v3, v4), Ok(()));
        assert_eq!(ctx.assert_tvar_sup(v3, &T::string()), Ok(()));
        assert!(ctx.assert_tvar_sup(v4, &T::integer()).is_err());
    }

    { // equality propagation
        let v1 = ctx.gen_tvar();
        assert_eq!(ctx.assert_tvar_eq(v1, &T::integer()), Ok(()));
        assert_eq!(ctx.assert_tvar_sub(v1, &T::number()), Ok(()));
        assert!(ctx.assert_tvar_sup(v1, &T::string()).is_err());

        let v2 = ctx.gen_tvar();
        assert_eq!(ctx.assert_tvar_sub(v2, &T::number()), Ok(()));
        assert_eq!(ctx.assert_tvar_eq(v2, &T::integer()), Ok(()));
        assert!(ctx.assert_tvar_sup(v2, &T::string()).is_err());

        let v3 = ctx.gen_tvar();
        assert_eq!(ctx.assert_tvar_sub(v3, &T::number()), Ok(()));
        assert!(ctx.assert_tvar_eq(v3, &T::string()).is_err());
    }
}

