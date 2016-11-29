use std::fmt;
use std::ops;
use std::mem;
use std::borrow::Cow;
use std::collections::BTreeMap;

use kailua_syntax::{K, SlotKind, Str};
use diag::CheckResult;
use super::{F, Slot, SlotWithNil};
use super::{TypeContext, NoTypeContext, TypeResolver, Lattice, TySeq, Display};
use super::{Numbers, Strings, Key, Tables, Function, Functions, Unioned, TVar, Builtin, Class};
use super::{error_not_sub, error_not_eq};
use super::flags::*;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Dyn {
    User, // user-generated dynamic type: WHATEVER
    Oops, // checker-generated dynamic type: <error type>
}

impl Dyn {
    pub fn or(lhs: Option<Dyn>, rhs: Option<Dyn>) -> Option<Dyn> {
        match (lhs, rhs) {
            (Some(Dyn::Oops), _) | (_, Some(Dyn::Oops)) => Some(Dyn::Oops),
            (Some(Dyn::User), _) | (_, Some(Dyn::User)) => Some(Dyn::User),
            (None, None) => None,
        }
    }
}

impl ops::BitOr<Dyn> for Dyn {
    type Output = Dyn;
    fn bitor(self, rhs: Dyn) -> Dyn {
        match (self, rhs) {
            (Dyn::Oops, _) | (_, Dyn::Oops) => Dyn::Oops,
            (Dyn::User, Dyn::User) => Dyn::User,
        }
    }
}

impl Lattice<Dyn> for Dyn {
    type Output = Dyn;
    fn union(&self, other: &Dyn, _ctx: &mut TypeContext) -> Dyn { *self | *other }
    fn assert_sub(&self, _other: &Dyn, _ctx: &mut TypeContext) -> CheckResult<()> { Ok(()) }
    fn assert_eq(&self, _other: &Dyn, _ctx: &mut TypeContext) -> CheckResult<()> { Ok(()) }
}

// a value type excluding nil (which is specially treated).
#[derive(Clone)]
pub enum T<'a> {
    Dynamic(Dyn),                       // dynamic type
    All,                                // any (top)
    None,                               // (bottom)
    Nil,                                // nil (XXX to be removed)
    Boolean,                            // boolean
    True,                               // true
    False,                              // false
    Integer,                            // integer
    Number,                             // number
    String,                             // string
    Thread,                             // thread
    UserData,                           // userdata
    Int(i32),                           // an integer literal
    Str(Cow<'a, Str>),                  // a string literal
    Tables(Cow<'a, Tables>),            // table, ...
    Functions(Cow<'a, Functions>),      // function, ...
    Class(Class),                       // nominal type
    TVar(TVar),                         // type variable
    Builtin(Builtin, Box<T<'a>>),       // types with an attribute (can be nested)
    Union(Cow<'a, Unioned>),            // union types A | B | ...
}

impl<'a> T<'a> {
    pub fn dummy() -> T<'a> { T::Dynamic(Dyn::Oops) }

    pub fn table()           -> T<'a> { T::Tables(Cow::Owned(Tables::All)) }
    pub fn empty_table()     -> T<'a> { T::Tables(Cow::Owned(Tables::Empty)) }
    pub fn function()        -> T<'a> { T::Functions(Cow::Owned(Functions::All)) }
    pub fn func(f: Function) -> T<'a> { T::Functions(Cow::Owned(Functions::Simple(f))) }

    pub fn ints<I: IntoIterator<Item=i32>>(i: I) -> T<'a> {
        let mut u = Unioned::empty();
        u.numbers = Some(Numbers::Some(i.into_iter().collect()));
        T::Union(Cow::Owned(u))
    }
    pub fn strs<I: IntoIterator<Item=Str>>(i: I) -> T<'a> {
        let mut u = Unioned::empty();
        u.strings = Some(Strings::Some(i.into_iter().collect()));
        T::Union(Cow::Owned(u))
    }
    pub fn tuple<'b, I: IntoIterator<Item=Slot>>(i: I) -> T<'a> {
        let i = i.into_iter().enumerate();
        let fields = i.map(|(i,v)| ((i as i32 + 1).into(), v));
        T::Tables(Cow::Owned(Tables::Fields(fields.collect())))
    }
    pub fn record<'b, I: IntoIterator<Item=(Str,Slot)>>(i: I) -> T<'a> {
        let i = i.into_iter();
        let fields = i.map(|(k,v)| (k.into(), v));
        T::Tables(Cow::Owned(Tables::Fields(fields.collect())))
    }
    pub fn array(v: Slot) -> T<'a> {
        T::Tables(Cow::Owned(Tables::Array(SlotWithNil::from_slot(v))))
    }
    pub fn map(k: T, v: Slot) -> T<'a> {
        T::Tables(Cow::Owned(Tables::Map(Ty::new(k.into_send()), SlotWithNil::from_slot(v))))
    }

    pub fn flags(&self) -> Flags {
        match *self {
            T::Dynamic(Dyn::User) => T_ALL | T_WHATEVER,
            T::Dynamic(Dyn::Oops) => T_ALL | T_DYNAMIC,

            T::All      => T_ALL,
            T::None     => T_NONE,
            T::Nil      => T_NIL,
            T::Boolean  => T_BOOLEAN,
            T::True     => T_TRUE,
            T::False    => T_FALSE,
            T::Thread   => T_THREAD,
            T::UserData => T_USERDATA,

            T::Number   => T_NUMBER,
            T::Integer  => T_INTEGER,
            T::Int(_)   => T_INTEGER,
            T::String   => T_STRING,
            T::Str(_)   => T_STRING,

            T::Tables(..) => T_TABLE,
            T::Functions(..) => T_FUNCTION,
            T::Class(..) => T_TABLE,

            T::TVar(..) => T_NONE,
            T::Builtin(_, ref t) => t.flags(),
            T::Union(ref u) => u.flags(),
        }
    }

    pub fn to_ref<'b: 'a>(&'b self) -> T<'b> {
        match *self {
            T::Dynamic(dyn) => T::Dynamic(dyn),

            T::All      => T::All,
            T::None     => T::None,
            T::Nil      => T::Nil,
            T::Boolean  => T::Boolean,
            T::True     => T::True,
            T::False    => T::False,
            T::Thread   => T::Thread,
            T::UserData => T::UserData,

            T::Number     => T::Number,
            T::Integer    => T::Integer,
            T::Int(v)     => T::Int(v),
            T::String     => T::String,
            T::Str(ref s) => T::Str(Cow::Borrowed(&**s)),

            T::Tables(ref tab) => T::Tables(Cow::Borrowed(&**tab)),
            T::Functions(ref func) => T::Functions(Cow::Borrowed(&**func)),
            T::Class(c) => T::Class(c),
            T::TVar(v) => T::TVar(v),
            T::Builtin(b, ref t) => T::Builtin(b, Box::new(t.to_ref())),
            T::Union(ref u) => T::Union(Cow::Borrowed(&**u)),
        }
    }

    pub fn to_ref_without_simple<'b: 'a>(&'b self, filter: UnionedSimple) -> T<'b> {
        match *self {
            T::Nil      if filter.contains(U_NIL)      => T::None,
            T::Boolean  if filter.contains(U_BOOLEAN)  => T::None,
            T::Boolean  if filter.contains(U_TRUE)     => T::False,
            T::Boolean  if filter.contains(U_FALSE)    => T::True,
            T::True     if filter.contains(U_TRUE)     => T::None,
            T::False    if filter.contains(U_FALSE)    => T::None,
            T::Thread   if filter.contains(U_THREAD)   => T::None,
            T::UserData if filter.contains(U_USERDATA) => T::None,
            T::Union(ref u) if u.simple.intersects(filter) => {
                let mut u = u.clone().into_owned();
                u.simple.remove(filter);
                u.simplify()
            },
            _ => self.to_ref(),
        }
    }

    pub fn without_simple(self, filter: UnionedSimple) -> T<'a> {
        match self {
            T::Nil      if filter.contains(U_NIL)      => T::None,
            T::Boolean  if filter.contains(U_BOOLEAN)  => T::None,
            T::Boolean  if filter.contains(U_TRUE)     => T::False,
            T::Boolean  if filter.contains(U_FALSE)    => T::True,
            T::True     if filter.contains(U_TRUE)     => T::None,
            T::False    if filter.contains(U_FALSE)    => T::None,
            T::Thread   if filter.contains(U_THREAD)   => T::None,
            T::UserData if filter.contains(U_USERDATA) => T::None,
            T::Union(u) => {
                if u.simple.intersects(filter) {
                    let mut u = u.into_owned();
                    u.simple.remove(filter);
                    u.simplify()
                } else {
                    T::Union(u)
                }
            },
            t => t,
        }
    }

    // used for value slots in array and mapping types

    pub fn to_ref_without_nil<'b: 'a>(&'b self) -> T<'b> { self.to_ref_without_simple(U_NIL) }
    pub fn without_nil(self) -> T<'a> { self.without_simple(U_NIL) }

    // used for simplifying logical conditions

    pub fn to_ref_truthy<'b: 'a>(&'b self) -> T<'b> { self.to_ref_without_simple(U_NIL | U_FALSE) }
    pub fn truthy(self) -> T<'a> { self.without_simple(U_NIL | U_FALSE) }

    pub fn falsey(&self) -> T<'static> {
        match *self {
            T::Nil => T::Nil,
            T::Boolean | T::False => T::False,
            T::Union(ref u) => {
                let has_nil = u.simple & U_NIL != U_NONE;
                let has_false = u.simple & U_FALSE != U_NONE;
                match (has_nil, has_false) {
                    (true, true) => T::Nil | T::False,
                    (true, false) => T::Nil,
                    (false, true) => T::False,
                    (false, false) => T::None,
                }
            },
            T::Builtin(b, ref t) => T::Builtin(b, Box::new(t.falsey())),
            _ => T::None,
        }
    }

    pub fn is_dynamic(&self)  -> bool { self.flags().is_dynamic() }
    pub fn is_integral(&self) -> bool { self.flags().is_integral() }
    pub fn is_numeric(&self)  -> bool { self.flags().is_numeric() }
    pub fn is_stringy(&self)  -> bool { self.flags().is_stringy() }
    pub fn is_tabular(&self)  -> bool { self.flags().is_tabular() }
    pub fn is_callable(&self) -> bool { self.flags().is_callable() }
    pub fn is_truthy(&self)   -> bool { self.flags().is_truthy() }
    pub fn is_falsy(&self)    -> bool { self.flags().is_falsy() }

    // XXX for now
    pub fn is_referential(&self) -> bool { self.flags().is_tabular() }

    pub fn get_dynamic(&self) -> Option<Dyn> {
        match *self {
            T::Dynamic(dyn) => Some(dyn),
            T::Builtin(_, ref t) => t.get_dynamic(),
            _ => None,
        }
    }

    pub fn get_tables(&self) -> Option<&Tables> {
        match *self {
            T::Tables(ref tab) => Some(tab),
            T::Builtin(_, ref t) => t.get_tables(),
            T::Union(ref u) => u.tables.as_ref(),
            _ => None,
        }
    }

    pub fn get_functions(&self) -> Option<&Functions> {
        match *self {
            T::Functions(ref func) => Some(func),
            T::Builtin(_, ref t) => t.get_functions(),
            T::Union(ref u) => u.functions.as_ref(),
            _ => None,
        }
    }

    pub fn get_tvar(&self) -> Option<TVar> {
        match *self {
            T::TVar(tv) => Some(tv),
            T::Builtin(_, ref t) => t.get_tvar(),
            T::Union(ref u) => u.tvar,
            _ => None,
        }
    }

    pub fn split_tvar(&self) -> (Option<TVar>, Option<T<'a>>) {
        match *self {
            T::TVar(tv) => (Some(tv), None),
            T::Union(ref u) => {
                if let Some(tv) = u.tvar {
                    let mut u = u.clone().into_owned();
                    u.tvar = None;
                    (Some(tv), Some(u.simplify()))
                } else {
                    (None, Some(T::Union(u.clone())))
                }
            },
            _ => (None, Some(self.clone())),
        }
    }

    pub fn builtin(&self) -> Option<Builtin> {
        match *self { T::Builtin(b, _) => Some(b), _ => None }
    }

    pub fn as_base(&self) -> &T<'a> {
        match self { &T::Builtin(_, ref t) => &*t, t => t }
    }

    pub fn into_base(self) -> T<'a> {
        match self { T::Builtin(_, t) => *t, t => t }
    }

    pub fn as_string(&self) -> Option<&Str> {
        // unlike flags, type variable should not be present
        match *self {
            T::Str(ref s) => Some(s.as_ref()),
            T::Builtin(_, ref t) => t.as_string(),
            T::Union(ref u) if u.flags() == T_STRING && u.tvar.is_none() => {
                match *u.strings.as_ref().unwrap() {
                    Strings::One(ref s) => Some(s),
                    Strings::Some(ref set) if set.len() == 1 => Some(set.iter().next().unwrap()),
                    _ => None,
                }
            },
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i32> {
        // unlike flags, type variable should not be present
        match *self {
            T::Int(v) => Some(v),
            T::Builtin(_, ref t) => t.as_integer(),
            T::Union(ref u) if u.flags() == T_INTEGER && u.tvar.is_none() => {
                match *u.numbers.as_ref().unwrap() {
                    Numbers::One(v) => Some(v),
                    Numbers::Some(ref set) if set.len() == 1 => Some(*set.iter().next().unwrap()),
                    _ => None,
                }
            },
            _ => None,
        }
    }

    pub fn into_send(self) -> T<'static> {
        match self {
            T::Dynamic(dyn) => T::Dynamic(dyn),

            T::All        => T::All,
            T::None       => T::None,
            T::Nil        => T::Nil,
            T::Boolean    => T::Boolean,
            T::True       => T::True,
            T::False      => T::False,
            T::Thread     => T::Thread,
            T::UserData   => T::UserData,

            T::Number     => T::Number,
            T::Integer    => T::Integer,
            T::Int(v)     => T::Int(v),
            T::String     => T::String,
            T::Str(s)     => T::Str(Cow::Owned(s.into_owned())),

            T::Tables(tab)     => T::Tables(Cow::Owned(tab.into_owned())),
            T::Functions(func) => T::Functions(Cow::Owned(func.into_owned())),
            T::Class(c)        => T::Class(c),
            T::TVar(tv)        => T::TVar(tv),

            T::Builtin(b, t) => T::Builtin(b, Box::new(t.into_send())),
            T::Union(u) => T::Union(Cow::Owned(u.into_owned())),
        }
    }

    pub fn filter_by_flags(self, flags: Flags, ctx: &mut TypeContext) -> CheckResult<T<'a>> {
        fn flags_to_ubound(flags: Flags) -> T<'static> {
            assert!(!flags.intersects(T_DYNAMIC));

            let mut t = T::None;
            if flags.contains(T_NIL)        { t = t | T::Nil; }
            if flags.contains(T_TRUE)       { t = t | T::True; }
            if flags.contains(T_FALSE)      { t = t | T::False; }
            if flags.contains(T_NONINTEGER) { t = t | T::Number; }
            if flags.contains(T_INTEGER)    { t = t | T::Integer; }
            if flags.contains(T_STRING)     { t = t | T::String; }
            if flags.contains(T_TABLE)      { t = t | T::table(); }
            if flags.contains(T_FUNCTION)   { t = t | T::function(); }
            if flags.contains(T_THREAD)     { t = t | T::Thread; }
            if flags.contains(T_USERDATA)   { t = t | T::UserData; }
            t
        }

        fn narrow_numbers<'a>(num: Cow<'a, Numbers>, flags: Flags) -> Option<Cow<'a, Numbers>> {
            let is_all = match num.as_ref() { &Numbers::All => true, _ => false };
            match (flags & T_NUMBER, is_all) {
                (T_NONINTEGER, false) => None,
                (T_INTEGER, true) => Some(Cow::Owned(Numbers::Int)),
                (T_NONE, _) => None,
                (_, _) => Some(num),
            }
        }

        fn narrow_tvar(tvar: TVar, flags: Flags, ctx: &mut TypeContext) -> CheckResult<TVar> {
            let ubound = flags_to_ubound(flags);

            // make a type variable i such that i <: ubound and i <: tvar
            let i = ctx.gen_tvar();
            ctx.assert_tvar_sub_tvar(i, tvar)?;
            ctx.assert_tvar_sub(i, &Ty::new(ubound))?;

            Ok(i)
        }

        let flags_or_none = |bit, t| if flags.contains(bit) { t } else { T::None };

        match self {
            T::Dynamic(dyn) => Ok(T::Dynamic(dyn)),
            T::None => Ok(T::None),
            T::All => Ok(flags_to_ubound(flags)),
            T::Boolean => match flags & T_BOOLEAN {
                T_BOOLEAN => Ok(T::Boolean),
                T_TRUE => Ok(T::True),
                T_FALSE => Ok(T::False),
                _ => Ok(T::None),
            },
            T::Number => match flags & T_NUMBER {
                T_NUMBER | T_NONINTEGER => Ok(T::Number),
                T_INTEGER => Ok(T::Integer),
                _ => Ok(T::None),
            },
            T::Integer         => Ok(flags_or_none(T_INTEGER,  T::Integer)),
            T::Int(v)          => Ok(flags_or_none(T_INTEGER,  T::Int(v))),
            T::Nil             => Ok(flags_or_none(T_NIL,      T::Nil)),
            T::True            => Ok(flags_or_none(T_TRUE,     T::True)),
            T::False           => Ok(flags_or_none(T_FALSE,    T::False)),
            T::Thread          => Ok(flags_or_none(T_THREAD,   T::Thread)),
            T::UserData        => Ok(flags_or_none(T_USERDATA, T::UserData)),
            T::String          => Ok(flags_or_none(T_STRING,   T::String)),
            T::Str(s)          => Ok(flags_or_none(T_STRING,   T::Str(s))),
            T::Tables(tab)     => Ok(flags_or_none(T_TABLE,    T::Tables(tab))),
            T::Functions(func) => Ok(flags_or_none(T_FUNCTION, T::Functions(func))),
            T::Class(c)        => Ok(flags_or_none(T_TABLE,    T::Class(c))),

            T::TVar(tv) => {
                Ok(T::TVar(narrow_tvar(tv, flags, ctx)?))
            },
            T::Builtin(b, t) => {
                Ok(T::Builtin(b, Box::new((*t).filter_by_flags(flags, ctx)?)))
            },
            T::Union(mut u) => {
                // if the union contains a type variable, that type variable should be
                // also narrowed (and consequently `u` has to be copied).
                if let Some(tv) = u.tvar {
                    let tv = narrow_tvar(tv, flags, ctx)?;
                    u.to_mut().tvar = Some(tv);
                }

                // compile a list of flags to remove, and only alter if there is any removal
                let removed = !flags & u.flags();
                if removed.is_empty() { return Ok(T::Union(u)); }

                let mut u = u.into_owned();
                let removed_simple = UnionedSimple::from_bits_truncate(removed.bits());
                if !removed_simple.is_empty() { u.simple &= !removed_simple; }
                if removed.intersects(T_NUMBER) {
                    let num = Cow::Owned(u.numbers.unwrap());
                    u.numbers = narrow_numbers(num, flags).map(|num| num.into_owned());
                }
                if removed.contains(T_STRING)   { u.strings   = None; }
                if removed.contains(T_TABLE)    { u.tables    = None; }
                if removed.contains(T_FUNCTION) { u.functions = None; }
                Ok(u.simplify())
            },
        }
    }
}

impl<'a> Lattice<Unioned> for T<'a> {
    type Output = Unioned;

    fn union(&self, other: &Unioned, ctx: &mut TypeContext) -> Unioned {
        Unioned::from(self).union(other, ctx)
    }

    // assumes that the Unioned itself has been simplified.
    fn assert_sub(&self, other: &Unioned, ctx: &mut TypeContext) -> CheckResult<()> {
        // try to match each component
        match *self {
            T::Dynamic(_) | T::None => return Ok(()),

            T::Nil      => if other.simple.contains(U_NIL)      { return Ok(()); },
            T::Boolean  => if other.simple.contains(U_BOOLEAN)  { return Ok(()); },
            T::True     => if other.simple.contains(U_TRUE)     { return Ok(()); },
            T::False    => if other.simple.contains(U_FALSE)    { return Ok(()); },
            T::Thread   => if other.simple.contains(U_THREAD)   { return Ok(()); },
            T::UserData => if other.simple.contains(U_USERDATA) { return Ok(()); },

            T::Number => match other.numbers {
                Some(Numbers::All) => { return Ok(()); }
                _ => {}
            },
            T::Integer => match other.numbers {
                Some(Numbers::All) | Some(Numbers::Int) => { return Ok(()); }
                _ => {}
            },
            T::Int(lhs) => match other.numbers {
                Some(Numbers::All) | Some(Numbers::Int) => { return Ok(()); }
                Some(Numbers::Some(ref rhs)) if rhs.contains(&lhs) => { return Ok(()); }
                Some(Numbers::One(rhs)) if lhs == rhs => { return Ok(()); }
                _ => {}
            },

            T::String => match other.strings {
                Some(Strings::All) => { return Ok(()); }
                _ => {}
            },
            T::Str(ref lhs) => match other.strings {
                Some(Strings::All) => { return Ok(()); }
                Some(Strings::Some(ref rhs)) if rhs.contains(lhs) => { return Ok(()); }
                Some(Strings::One(ref rhs)) if **lhs == *rhs => { return Ok(()); }
                _ => {}
            },

            T::Tables(ref lhs) =>
                if let Some(ref num) = other.tables { return lhs.assert_sub(num, ctx); },
            T::Functions(ref lhs) =>
                if let Some(ref num) = other.functions { return lhs.assert_sub(num, ctx); },
            T::Class(c) => if other.classes.contains(&c) { return Ok(()); },

            T::TVar(lhs) => {
                if other.tvar.is_some() { // XXX cannot determine the type var relation
                    return error_not_sub(self, other);
                } else {
                    return ctx.assert_tvar_sub(lhs, &Ty::new(T::Union(Cow::Owned(other.clone()))));
                }
            },

            T::Union(ref lhs) => return lhs.assert_sub(other, ctx),

            _ => {}
        }

        // the union sans type variable is not a subtype of self.
        // XXX we can try asserting an additional constraint to the union's type variable if any,
        // but for now we bail out
        error_not_sub(self, other)
    }

    // assumes that the Unioned itself has been simplified.
    fn assert_eq(&self, other: &Unioned, ctx: &mut TypeContext) -> CheckResult<()> {
        match *self {
            T::Dynamic(_) => return Ok(()),
            T::Union(ref lhs) => return lhs.assert_eq(other, ctx),
            _ => {}
        }

        error_not_eq(self, other)
    }
}

impl<'a, 'b> Lattice<T<'b>> for T<'a> {
    type Output = T<'static>;

    fn union(&self, other: &T<'b>, ctx: &mut TypeContext) -> T<'static> {
        match (self, other) {
            // built-in types are destructured first unless they point to the same builtin
            (&T::Builtin(lb, ref lhs), &T::Builtin(rb, ref rhs)) if lb == rb =>
                T::Builtin(lb, Box::new(lhs.union(rhs, ctx))),
            (&T::Builtin(_, ref lhs), &T::Builtin(_, ref rhs)) => lhs.union(rhs, ctx),
            (&T::Builtin(_, ref lhs), rhs) => (**lhs).union(rhs, ctx),
            (lhs, &T::Builtin(_, ref rhs)) => lhs.union(&**rhs, ctx),

            // dynamic eclipses everything else
            (&T::Dynamic(dyn1), &T::Dynamic(dyn2)) => T::Dynamic(dyn1.union(&dyn2, ctx)),
            (&T::Dynamic(dyn), _) => T::Dynamic(dyn),
            (_, &T::Dynamic(dyn)) => T::Dynamic(dyn),

            // top eclipses everything else except for dynamic and oops
            (&T::All, _) => T::All,
            (_, &T::All) => T::All,

            (&T::None, ty) => ty.clone().into_send(),
            (ty, &T::None) => ty.clone().into_send(),

            (&T::Nil,      &T::Nil)      => T::Nil,
            (&T::Boolean,  &T::Boolean)  => T::Boolean,
            (&T::Boolean,  &T::True)     => T::Boolean,
            (&T::Boolean,  &T::False)    => T::Boolean,
            (&T::True,     &T::Boolean)  => T::Boolean,
            (&T::False,    &T::Boolean)  => T::Boolean,
            (&T::True,     &T::True)     => T::True,
            (&T::True,     &T::False)    => T::Boolean,
            (&T::False,    &T::True)     => T::Boolean,
            (&T::False,    &T::False)    => T::False,
            (&T::Thread,   &T::Thread)   => T::Thread,
            (&T::UserData, &T::UserData) => T::UserData,

            (&T::Number,     &T::Number)     => T::Number,
            (&T::Integer,    &T::Number)     => T::Number,
            (&T::Int(_),     &T::Number)     => T::Number,
            (&T::Number,     &T::Integer)    => T::Number,
            (&T::Number,     &T::Int(_))     => T::Number,
            (&T::Integer,    &T::Integer)    => T::Integer,
            (&T::Int(_),     &T::Integer)    => T::Integer,
            (&T::Integer,    &T::Int(_))     => T::Integer,
            (&T::String,     &T::String)     => T::String,
            (&T::Str(_),     &T::String)     => T::String,
            (&T::String,     &T::Str(_))     => T::String,

            (&T::Int(a), &T::Int(b)) if a == b => T::Int(a),
            (&T::Str(ref a), &T::Str(ref b)) if *a == *b => T::Str(Cow::Owned((**a).to_owned())),

            (&T::Tables(ref a), &T::Tables(ref b)) =>
                T::Tables(Cow::Owned(a.union(b, ctx))),
            (&T::Functions(ref a), &T::Functions(ref b)) =>
                T::Functions(Cow::Owned(a.union(b, ctx))),
            (&T::Class(a), &T::Class(b)) if a == b =>
                T::Class(a),
            (&T::TVar(ref a), &T::TVar(ref b)) =>
                T::TVar(a.union(b, ctx)),

            (a, b) => Unioned::from(&a).union(&Unioned::from(&b), ctx).simplify(),
        }
    }

    fn assert_sub(&self, other: &T<'b>, ctx: &mut TypeContext) -> CheckResult<()> {
        debug!("asserting a constraint {:?} <: {:?}", *self, *other);

        let ok = match (self, other) {
            // built-in types are destructured first
            // some built-in requires the subtyping, so if any operand has such built-in
            // and the built-in doesn't match bail out
            (&T::Builtin(lb, ref lhs), &T::Builtin(rb, ref rhs)) => {
                if (lb.needs_subtype() || rb.needs_subtype()) && lb != rb {
                    false
                } else {
                    return lhs.assert_sub(rhs, ctx);
                }
            },
            (&T::Builtin(_lb, ref lhs), rhs) => {
                // every built-in types are subtypes of the original type
                return (**lhs).assert_sub(rhs, ctx);
            },
            (lhs, &T::Builtin(rb, ref rhs)) => {
                if rb.needs_subtype() {
                    false
                } else {
                    return lhs.assert_sub(&**rhs, ctx);
                }
            },

            (&T::Dynamic(dyn1), &T::Dynamic(dyn2)) => return dyn1.assert_sub(&dyn2, ctx),
            (&T::Dynamic(_), _) => true,
            (_, &T::Dynamic(_)) => true,

            (_, &T::All) => true,

            (&T::None, _) => true,
            (_, &T::None) => false,

            (&T::Nil,      &T::Nil)      => true,
            (&T::Boolean,  &T::Boolean)  => true,
            (&T::True,     &T::Boolean)  => true,
            (&T::True,     &T::True)     => true,
            (&T::False,    &T::Boolean)  => true,
            (&T::False,    &T::False)    => true,
            (&T::Thread,   &T::Thread)   => true,
            (&T::UserData, &T::UserData) => true,

            (&T::Number,     &T::Number)     => true,
            (&T::Integer,    &T::Number)     => true,
            (&T::Int(_),     &T::Number)     => true,
            (&T::Integer,    &T::Integer)    => true,
            (&T::Int(_),     &T::Integer)    => true,
            (&T::Int(a),     &T::Int(b))     => a == b,
            (&T::String,     &T::String)     => true,
            (&T::Str(_),     &T::String)     => true,
            (&T::Str(ref a), &T::Str(ref b)) => *a == *b,

            (&T::Tables(ref a),    &T::Tables(ref b))    => return a.assert_sub(b, ctx),
            (&T::Functions(ref a), &T::Functions(ref b)) => return a.assert_sub(b, ctx),

            // prototypes are NOT compatible to each other!
            (&T::Class(Class::Instance(a)), &T::Class(Class::Instance(b))) => {
                ctx.is_subclass_of(a, b)
            },

            (&T::Union(ref a), &T::Union(ref b)) => return a.assert_sub(b, ctx),
            (&T::Union(ref a), &T::TVar(b)) if a.tvar.is_none() => {
                // do NOT try to split `T|U <: x` into `T <: x AND U <: x` if possible
                return ctx.assert_tvar_sup(b, &Ty::new(self.clone().into_send()));
            },
            (&T::Union(ref a), b) => {
                // a1 \/ a2 <: b === a1 <: b AND a2 <: b
                return a.visit(|i| i.assert_sub(b, ctx));
            },

            (a, &T::Union(ref b)) => return a.assert_sub(&**b, ctx),

            (&T::TVar(a), &T::TVar(b)) => return a.assert_sub(&b, ctx),
            (a, &T::TVar(b)) => return ctx.assert_tvar_sup(b, &Ty::new(a.clone().into_send())),
            (&T::TVar(a), b) => return ctx.assert_tvar_sub(a, &Ty::new(b.clone().into_send())),

            (_, _) => false,
        };

        if ok { Ok(()) } else { error_not_sub(self, other) }
    }

    fn assert_eq(&self, other: &T<'b>, ctx: &mut TypeContext) -> CheckResult<()> {
        debug!("asserting a constraint {:?} = {:?}", *self, *other);

        let ok = match (self, other) {
            // built-in types are destructured first
            // some built-in requires the subtyping, so if any operand has such built-in
            // and the built-in doesn't match bail out
            (&T::Builtin(lb, ref lhs), &T::Builtin(rb, ref rhs)) => {
                if (lb.needs_subtype() || rb.needs_subtype()) && lb != rb {
                    false
                } else {
                    return lhs.assert_eq(rhs, ctx);
                }
            },
            (&T::Builtin(lb, ref lhs), rhs) => {
                if lb.needs_subtype() {
                    false
                } else {
                    return (**lhs).assert_eq(rhs, ctx);
                }
            },
            (lhs, &T::Builtin(rb, ref rhs)) => {
                if rb.needs_subtype() {
                    false
                } else {
                    return lhs.assert_eq(&**rhs, ctx);
                }
            },

            (&T::Dynamic(dyn1), &T::Dynamic(dyn2)) => return dyn1.assert_eq(&dyn2, ctx),
            (&T::Dynamic(_), _) => true,
            (_, &T::Dynamic(_)) => true,

            (&T::All, _) => true,
            (_, &T::All) => true,

            (&T::None, _) => true,
            (_, &T::None) => false,

            (&T::Nil,      &T::Nil)      => true,
            (&T::Boolean,  &T::Boolean)  => true,
            (&T::True,     &T::True)     => true,
            (&T::False,    &T::False)    => true,
            (&T::Thread,   &T::Thread)   => true,
            (&T::UserData, &T::UserData) => true,

            (&T::Number,     &T::Number)     => true,
            (&T::Integer,    &T::Integer)    => true,
            (&T::Int(a),     &T::Int(b))     => a == b,
            (&T::String,     &T::String)     => true,
            (&T::Str(ref a), &T::Str(ref b)) => *a == *b,

            (&T::Tables(ref a),    &T::Tables(ref b))    => return a.assert_eq(b, ctx),
            (&T::Functions(ref a), &T::Functions(ref b)) => return a.assert_eq(b, ctx),
            (&T::Class(a),         &T::Class(b))         => a == b,

            (&T::TVar(a), &T::TVar(b)) => return a.assert_eq(&b, ctx),
            (a, &T::TVar(b)) => return ctx.assert_tvar_eq(b, &Ty::new(a.clone().into_send())),
            (&T::TVar(a), b) => return ctx.assert_tvar_eq(a, &Ty::new(b.clone().into_send())),

            (a, &T::Union(ref b)) => return a.assert_eq(&**b, ctx),
            (&T::Union(ref _a), _b) => false, // XXX for now

            (_, _) => false,
        };

        if ok { Ok(()) } else { error_not_eq(self, other) }
    }
}

impl<'a, 'b> ops::BitOr<T<'b>> for T<'a> {
    type Output = T<'static>;
    fn bitor(self, rhs: T<'b>) -> T<'static> {
        self.union(&rhs, &mut NoTypeContext)
    }
}

// not intended to be complete equality, but enough for testing
impl<'a, 'b> PartialEq<T<'b>> for T<'a> {
    fn eq(&self, other: &T<'b>) -> bool {
        match (self, other) {
            (&T::Dynamic(dyn1), &T::Dynamic(dyn2)) => dyn1 == dyn2,

            (&T::All,      &T::All)      => true,
            (&T::None,     &T::None)     => true,
            (&T::Nil,      &T::Nil)      => true,
            (&T::Boolean,  &T::Boolean)  => true,
            (&T::True,     &T::True)     => true,
            (&T::False,    &T::False)    => true,
            (&T::Thread,   &T::Thread)   => true,
            (&T::UserData, &T::UserData) => true,

            (&T::Number,     &T::Number)     => true,
            (&T::Integer,    &T::Integer)    => true,
            (&T::Int(a),     &T::Int(b))     => a == b,
            (&T::String,     &T::String)     => true,
            (&T::Str(ref a), &T::Str(ref b)) => *a == *b,

            (&T::Tables(ref a),    &T::Tables(ref b))    => *a == *b,
            (&T::Functions(ref a), &T::Functions(ref b)) => *a == *b,
            (&T::Class(a),         &T::Class(b))         => a == b,
            (&T::TVar(a),          &T::TVar(b))          => a == b,
            (&T::Builtin(ba, _),   &T::Builtin(bb, _))   => ba == bb, // XXX lifetime issues?
            (&T::Union(ref a),     &T::Union(ref b))     => a == b,

            (_, _) => false,
        }
    }
}

impl<'a> Display for T<'a> {
    fn fmt_displayed(&self, f: &mut fmt::Formatter, ctx: &TypeContext) -> fmt::Result {
        match *self {
            T::Dynamic(Dyn::User) => write!(f, "WHATEVER"),
            T::Dynamic(Dyn::Oops) => write!(f, "<error type>"),

            T::All      => write!(f, "any"),
            T::None     => write!(f, "<impossible type>"),
            T::Nil      => write!(f, "nil"),
            T::Boolean  => write!(f, "boolean"),
            T::True     => write!(f, "true"),
            T::False    => write!(f, "false"),
            T::Thread   => write!(f, "thread"),
            T::UserData => write!(f, "userdata"),

            T::Number     => write!(f, "number"),
            T::Integer    => write!(f, "integer"),
            T::Int(v)     => write!(f, "{}", v),
            T::String     => write!(f, "string"),
            T::Str(ref s) => write!(f, "{:?}", s),

            T::TVar(tv) => {
                if let Some(t) = ctx.get_tvar_exact_type(tv) {
                    fmt::Display::fmt(&t.display(ctx), f)
                } else {
                    write!(f, "<unknown type>")
                }
            },

            T::Tables(ref tab)     => fmt::Display::fmt(&tab.display(ctx), f),
            T::Functions(ref func) => fmt::Display::fmt(&func.display(ctx), f),
            T::Class(c)            => ctx.fmt_class(c, f),
            T::Builtin(b, ref t)   => write!(f, "[{}] {}", b.name(), t.display(ctx)),
            T::Union(ref u)        => fmt::Display::fmt(&u.display(ctx), f),
        }
    }
}

impl<'a> fmt::Debug for T<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            T::Dynamic(Dyn::User) => write!(f, "WHATEVER"),
            T::Dynamic(Dyn::Oops) => write!(f, "<error>"),

            T::All      => write!(f, "any"),
            T::None     => write!(f, "<bottom>"),
            T::Nil      => write!(f, "nil"),
            T::Boolean  => write!(f, "boolean"),
            T::True     => write!(f, "true"),
            T::False    => write!(f, "false"),
            T::Thread   => write!(f, "thread"),
            T::UserData => write!(f, "userdata"),

            T::Number     => write!(f, "number"),
            T::Integer    => write!(f, "integer"),
            T::Int(v)     => write!(f, "{}", v),
            T::String     => write!(f, "string"),
            T::Str(ref s) => write!(f, "{:?}", s),

            T::Tables(ref tab)     => fmt::Debug::fmt(tab, f),
            T::Functions(ref func) => fmt::Debug::fmt(func, f),
            T::Class(ref c)        => fmt::Debug::fmt(c, f),
            T::TVar(ref tv)        => fmt::Debug::fmt(tv, f),
            T::Builtin(b, ref t)   => write!(f, "[{}] {:?}", b.name(), t),
            T::Union(ref u)        => fmt::Debug::fmt(u, f),
        }
    }
}

impl<'a> From<T<'a>> for Unioned {
    fn from(x: T<'a>) -> Unioned { Unioned::from(&x) }
}

// a "type pointer".
#[derive(Clone, PartialEq)]
pub struct Ty {
    ty: Box<T<'static>>,
}

impl Ty {
    pub fn dummy() -> Ty {
        Ty { ty: Box::new(T::dummy()) }
    }

    pub fn new(ty: T<'static>) -> Ty {
        Ty { ty: Box::new(ty) }
    }

    pub fn from_kind(kind: &K, resolv: &mut TypeResolver) -> CheckResult<Ty> {
        let slot_from_slotkind = |slotkind: &SlotKind,
                                  resolv: &mut TypeResolver| -> CheckResult<Slot> {
            let ty = Ty::from_kind(&slotkind.kind.base, resolv)?;
            let flex = F::from(slotkind.modf);
            Ok(Slot::new(flex, ty))
        };

        let ty = match *kind {
            K::Oops              => Ty::new(T::Dynamic(Dyn::Oops)), // typically from a parser error
            K::Dynamic           => Ty::new(T::Dynamic(Dyn::User)),
            K::Any               => Ty::new(T::All),
            K::Nil               => Ty::new(T::Nil),
            K::Boolean           => Ty::new(T::Boolean),
            K::BooleanLit(true)  => Ty::new(T::True),
            K::BooleanLit(false) => Ty::new(T::False),
            K::Number            => Ty::new(T::Number),
            K::Integer           => Ty::new(T::Integer),
            K::IntegerLit(v)     => Ty::new(T::Int(v)),
            K::String            => Ty::new(T::String),
            K::StringLit(ref s)  => Ty::new(T::Str(Cow::Owned(s.to_owned()))),
            K::Table             => Ty::new(T::Tables(Cow::Owned(Tables::All))),
            K::EmptyTable        => Ty::new(T::Tables(Cow::Owned(Tables::Empty))),
            K::Function          => Ty::new(T::Functions(Cow::Owned(Functions::All))),
            K::Thread            => Ty::new(T::Thread),
            K::UserData          => Ty::new(T::UserData),
            K::Named(ref name)   => resolv.ty_from_name(name)?,
            K::WithNil(ref k)    => Ty::from_kind(k, resolv)? | Ty::new(T::Nil), // XXX for now
            K::WithoutNil(ref k) => Ty::from_kind(k, resolv)?, // XXX for now
            K::Error(..)         => return Err(format!("error type not yet supported in checker")),
            // XXX should issue a proper error or should work correctly

            K::Record(ref fields) => {
                let mut newfields = BTreeMap::new();
                for &(ref name, ref slotkind) in fields {
                    let slot = slot_from_slotkind(&slotkind.base, resolv)?;
                    newfields.insert(name.base.clone().into(), slot);
                }
                Ty::new(T::Tables(Cow::Owned(Tables::Fields(newfields))))
            }

            K::Tuple(ref fields) => {
                let mut newfields = BTreeMap::new();
                for (i, slotkind) in fields.iter().enumerate() {
                    let key = Key::Int(i as i32 + 1);
                    let slot = slot_from_slotkind(slotkind, resolv)?;
                    newfields.insert(key, slot);
                }
                Ty::new(T::Tables(Cow::Owned(Tables::Fields(newfields))))
            },

            K::Array(ref v) => {
                let slot = SlotWithNil::from_slot(slot_from_slotkind(v, resolv)?);
                Ty::new(T::Tables(Cow::Owned(Tables::Array(slot))))
            },

            K::Map(ref k, ref v) => {
                let slot = SlotWithNil::from_slot(slot_from_slotkind(v, resolv)?);
                Ty::new(T::Tables(Cow::Owned(Tables::Map(Ty::from_kind(k, resolv)?, slot))))
            },

            K::Func(ref func) => {
                let func = Function {
                    args: TySeq::from_kind_seq(&func.args, resolv)?,
                    returns: TySeq::from_kind_seq(&func.returns, resolv)?,
                };
                Ty::new(T::Functions(Cow::Owned(Functions::Simple(func))))
            }

            K::Union(ref kinds) => {
                assert!(!kinds.is_empty());
                let mut ty = Ty::from_kind(&kinds[0], resolv)?;
                for kind in &kinds[1..] {
                    ty = ty | Ty::from_kind(kind, resolv)?;
                }
                ty
            }

            K::Attr(ref kind, ref attr) => {
                if let Some(builtin) = Builtin::from(attr, resolv)? {
                    Ty::from_kind(kind, resolv)?.map(|t| T::Builtin(builtin, Box::new(t)))
                } else {
                    Ty::from_kind(kind, resolv)? // `Builtin::from` has already reported the error
                }
            }
        };

        Ok(ty)
    }

    pub fn map<F: FnOnce(T<'static>) -> T<'static>>(mut self, f: F) -> Ty {
        let ty = mem::replace(&mut *self.ty, T::None);
        *self.ty = f(ty);
        self
    }

    pub fn without_simple(self, filter: UnionedSimple) -> Ty {
        self.map(|t| t.without_simple(filter))
    }

    pub fn without_nil(self) -> Ty { self.map(|t| t.without_nil()) }
    pub fn truthy(self)      -> Ty { self.map(|t| t.truthy()) }
    pub fn falsey(self)      -> Ty { self.map(|t| t.falsey()) }

    pub fn split_tvar(&self) -> (Option<TVar>, Option<Ty>) {
        let (tv, ty) = self.ty.split_tvar();
        (tv, ty.map(Ty::new))
    }

    pub fn into_base(self) -> T<'static> { self.ty.into_base() }

    pub fn filter_by_flags(mut self, flags: Flags, ctx: &mut TypeContext) -> CheckResult<Ty> {
        let ty = mem::replace(&mut *self.ty, T::None);
        *self.ty = ty.filter_by_flags(flags, ctx)?;
        Ok(self)
    }

    pub fn unwrap(self) -> T<'static> {
        *self.ty
    }
}

impl<'a> From<T<'a>> for Ty {
    fn from(ty: T<'a>) -> Ty {
        Ty { ty: Box::new(ty.into_send()) }
    }
}

impl From<Box<T<'static>>> for Ty {
    fn from(ty: Box<T<'static>>) -> Ty {
        Ty { ty: ty }
    }
}

impl ops::Deref for Ty {
    type Target = T<'static>;
    fn deref(&self) -> &T<'static> { &self.ty }
}

impl ops::DerefMut for Ty {
    fn deref_mut(&mut self) -> &mut T<'static> { &mut self.ty }
}

impl ops::BitOr<Ty> for Ty {
    type Output = Ty;
    fn bitor(self, rhs: Ty) -> Ty {
        Ty::new(self.ty.union(&rhs.ty, &mut NoTypeContext))
    }
}

impl<'a> Lattice<T<'a>> for Ty {
    type Output = Ty;

    fn union(&self, other: &T<'a>, ctx: &mut TypeContext) -> Ty {
        Ty::new((*self.ty).union(other, ctx))
    }

    fn assert_sub(&self, other: &T<'a>, ctx: &mut TypeContext) -> CheckResult<()> {
        (*self.ty).assert_sub(other, ctx)
    }

    fn assert_eq(&self, other: &T<'a>, ctx: &mut TypeContext) -> CheckResult<()> {
        (*self.ty).assert_eq(other, ctx)
    }
}

impl<'a> Lattice<Ty> for T<'a> {
    type Output = Ty;

    fn union(&self, other: &Ty, ctx: &mut TypeContext) -> Ty {
        Ty::new(self.union(&*other.ty, ctx))
    }

    fn assert_sub(&self, other: &Ty, ctx: &mut TypeContext) -> CheckResult<()> {
        self.assert_sub(&*other.ty, ctx)
    }

    fn assert_eq(&self, other: &Ty, ctx: &mut TypeContext) -> CheckResult<()> {
        self.assert_eq(&*other.ty, ctx)
    }
}

impl Lattice<Ty> for Ty {
    type Output = Ty;

    fn union(&self, other: &Ty, ctx: &mut TypeContext) -> Ty {
        Ty::new((*self.ty).union(&*other.ty, ctx))
    }

    fn assert_sub(&self, other: &Ty, ctx: &mut TypeContext) -> CheckResult<()> {
        (*self.ty).assert_sub(&*other.ty, ctx)
    }

    fn assert_eq(&self, other: &Ty, ctx: &mut TypeContext) -> CheckResult<()> {
        (*self.ty).assert_eq(&*other.ty, ctx)
    }
}

impl Display for Ty {
    fn fmt_displayed(&self, f: &mut fmt::Formatter, ctx: &TypeContext) -> fmt::Result {
        self.ty.fmt_displayed(f, ctx)
    }
}

impl fmt::Debug for Ty {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.ty, f)
    }
}

#[cfg(test)]
#[allow(unused_variables, dead_code)]
mod tests {
    use kailua_diag::NoReport;
    use kailua_syntax::Str;
    use std::rc::Rc;
    use std::borrow::Cow;
    use ty::{Lattice, TypeContext, NoTypeContext, F, Slot, Mark, Builtin};
    use env::Context;
    use super::*;

    macro_rules! hash {
        ($($k:ident = $v:expr),*) => (vec![$((s(stringify!($k)), $v)),*])
    }

    fn s(x: &str) -> Str { Str::from(x.as_bytes().to_owned()) }
    fn os(x: &str) -> Cow<'static, Str> { Cow::Owned(Str::from(x.as_bytes().to_owned())) }
    fn just(t: T) -> Slot { Slot::new(F::Just, Ty::from(t)) }
    fn var(t: T) -> Slot { Slot::new(F::Var, Ty::from(t)) }
    fn cnst(t: T) -> Slot { Slot::new(F::Const, Ty::from(t)) }
    fn curr(t: T) -> Slot { Slot::new(F::Currently, Ty::from(t)) }
    fn varcnst(t: T) -> Slot { Slot::new(F::VarOrConst(Mark::any()), Ty::from(t)) }
    fn varcurr(t: T) -> Slot { Slot::new(F::VarOrCurrently(Mark::any()), Ty::from(t)) }

    #[test]
    fn test_lattice() {
        macro_rules! check {
            ($l:expr, $r:expr; $u:expr) => ({
                let left = $l;
                let right = $r;
                let union = $u;
                let mut ctx = Context::new(Rc::new(NoReport));
                let actualunion = left.union(&right, &mut ctx);
                if actualunion != union {
                    panic!("{:?} | {:?} = expected {:?}, actual {:?}",
                           left, right, union, actualunion);
                }
                (left, right, actualunion)
            })
        }

        // dynamic & top vs. everything else
        check!(T::Dynamic(Dyn::Oops), T::Dynamic(Dyn::Oops); T::Dynamic(Dyn::Oops));
        check!(T::Dynamic(Dyn::Oops), T::Dynamic(Dyn::User); T::Dynamic(Dyn::Oops));
        check!(T::Dynamic(Dyn::User), T::Dynamic(Dyn::Oops); T::Dynamic(Dyn::Oops));
        check!(T::Dynamic(Dyn::User), T::Dynamic(Dyn::User); T::Dynamic(Dyn::User));
        check!(T::Dynamic(Dyn::User), T::Integer; T::Dynamic(Dyn::User));
        check!(T::tuple(vec![var(T::Integer), curr(T::Boolean)]), T::Dynamic(Dyn::User);
               T::Dynamic(Dyn::User));
        check!(T::All, T::Boolean; T::All);
        check!(T::Dynamic(Dyn::User), T::All; T::Dynamic(Dyn::User));
        check!(T::All, T::All; T::All);

        // integer literals
        check!(T::Integer, T::Number; T::Number);
        check!(T::Number, T::Integer; T::Number);
        check!(T::Number, T::Number; T::Number);
        check!(T::Integer, T::Integer; T::Integer);
        check!(T::Int(3), T::Int(3); T::Int(3));
        check!(T::Int(3), T::Number; T::Number);
        check!(T::Integer, T::Int(3); T::Integer);
        check!(T::Int(3), T::Int(4); T::ints(vec![3, 4]));
        check!(T::ints(vec![3, 4]), T::Int(3); T::ints(vec![3, 4]));
        check!(T::Int(5), T::ints(vec![3, 4]); T::ints(vec![3, 4, 5]));
        check!(T::ints(vec![3, 4]), T::ints(vec![5, 4, 7]); T::ints(vec![3, 4, 5, 7]));
        check!(T::ints(vec![3, 4, 5]), T::ints(vec![2, 3, 4]); T::ints(vec![2, 3, 4, 5]));

        // string literals
        check!(T::String, T::Str(os("hello")); T::String);
        check!(T::Str(os("hello")), T::String; T::String);
        check!(T::Str(os("hello")), T::Str(os("hello")); T::Str(os("hello")));
        check!(T::Str(os("hello")), T::Str(os("goodbye"));
               T::strs(vec![s("hello"), s("goodbye")]));
        check!(T::Str(os("hello")), T::strs(vec![s("goodbye")]);
               T::strs(vec![s("hello"), s("goodbye")]));
        check!(T::strs(vec![s("hello"), s("goodbye")]), T::Str(os("goodbye"));
               T::strs(vec![s("hello"), s("goodbye")]));
        check!(T::strs(vec![s("hello"), s("goodbye")]),
               T::strs(vec![s("what"), s("goodbye")]);
               T::strs(vec![s("hello"), s("goodbye"), s("what")]));
        check!(T::strs(vec![s("a"), s("b"), s("c")]),
               T::strs(vec![s("b"), s("c"), s("d")]);
               T::strs(vec![s("a"), s("b"), s("c"), s("d")]));

        // tables
        check!(T::table(), T::array(just(T::Integer)); T::table());
        check!(T::table(), T::array(var(T::Integer)); T::table());
        check!(T::table(), T::array(curr(T::Integer)); T::table());
        check!(T::array(just(T::Integer)), T::array(just(T::Integer));
               T::array(just(T::Integer)));
        check!(T::array(var(T::Integer)), T::array(var(T::Integer));
               T::array(varcnst(T::Integer)));
        check!(T::array(cnst(T::Integer)), T::array(cnst(T::Integer));
               T::array(cnst(T::Integer)));
        check!(T::array(just(T::Int(3))), T::array(just(T::Int(4)));
               T::array(just(T::ints(vec![3, 4]))));
        check!(T::array(cnst(T::Int(3))), T::array(cnst(T::Int(4)));
               T::array(cnst(T::ints(vec![3, 4]))));
        check!(T::array(var(T::Int(3))), T::array(var(T::Int(4)));
               T::array(varcnst(T::ints(vec![3, 4]))));
        check!(T::array(var(T::Int(3))), T::array(just(T::Int(4)));
               T::array(varcnst(T::ints(vec![3, 4]))));
        check!(T::tuple(vec![just(T::Integer), just(T::String)]),
               T::tuple(vec![just(T::Number), just(T::Dynamic(Dyn::User)), just(T::Boolean)]);
               T::tuple(vec![just(T::Number), just(T::Dynamic(Dyn::User)),
                             just(T::Boolean | T::Nil)]));
        check!(T::tuple(vec![just(T::Integer), just(T::String)]),
               T::tuple(vec![just(T::Number), just(T::Boolean), just(T::Dynamic(Dyn::User))]);
               T::tuple(vec![just(T::Number), just(T::String | T::Boolean),
                             just(T::Dynamic(Dyn::User))]));
        { // self-modifying unions
            let (lhs, rhs, _) = check!(
                T::tuple(vec![var(T::Integer), curr(T::String)]),
                T::tuple(vec![cnst(T::String), just(T::Number), var(T::Boolean)]);
                T::tuple(vec![cnst(T::Integer | T::String),
                              varcnst(T::String | T::Number),
                              varcnst(T::Boolean | T::Nil)]));
            assert_eq!(lhs, T::tuple(vec![var(T::Integer), var(T::String)]));
            assert_eq!(rhs, T::tuple(vec![cnst(T::String), just(T::Number), var(T::Boolean)]));

            let (lhs, rhs, _) = check!(
                T::tuple(vec![cnst(T::Integer)]),
                T::tuple(vec![cnst(T::Number), curr(T::String)]);
                T::tuple(vec![cnst(T::Number), varcnst(T::String | T::Nil)]));
            assert_eq!(lhs, T::tuple(vec![cnst(T::Integer)]));
            assert_eq!(rhs, T::tuple(vec![cnst(T::Number), var(T::String)]));

            let (lhs, _, _) = check!(
                T::tuple(vec![just(T::Integer), var(T::String), curr(T::Boolean)]),
                T::empty_table();
                T::tuple(vec![just(T::Integer | T::Nil), varcnst(T::String | T::Nil),
                              varcnst(T::Boolean | T::Nil)]));
            assert_eq!(lhs, T::tuple(vec![just(T::Integer), var(T::String), var(T::Boolean)]));
        }
        check!(T::record(hash![foo=just(T::Integer), bar=just(T::String)]),
               T::record(hash![quux=just(T::Boolean)]);
               T::record(hash![foo=just(T::Integer | T::Nil), bar=just(T::String | T::Nil),
                               quux=just(T::Boolean | T::Nil)]));
        check!(T::record(hash![foo=just(T::Int(3)), bar=just(T::String)]),
               T::record(hash![foo=just(T::Int(4))]);
               T::record(hash![foo=just(T::ints(vec![3, 4])), bar=just(T::String | T::Nil)]));
        check!(T::record(hash![foo=just(T::Integer), bar=just(T::Number),
                                    quux=just(T::array(just(T::Dynamic(Dyn::User))))]),
               T::record(hash![foo=just(T::Number), bar=just(T::String),
                                    quux=just(T::array(just(T::Boolean)))]);
               T::record(hash![foo=just(T::Number), bar=just(T::Number | T::String),
                                    quux=just(T::array(just(T::Dynamic(Dyn::User))))]));
        check!(T::record(hash![foo=just(T::Int(3)), bar=just(T::Number)]),
               T::map(T::String, just(T::Integer));
               T::map(T::String, just(T::Number)));
        check!(T::array(just(T::Integer)), T::tuple(vec![just(T::String)]);
               T::map(T::Integer, just(T::Integer | T::String)));
        check!(T::map(T::Str(os("wat")), just(T::Integer)),
               T::map(T::String, just(T::Int(42)));
               T::map(T::String, just(T::Integer)));
        check!(T::array(just(T::Number)), T::map(T::Dynamic(Dyn::User), just(T::Integer));
               T::map(T::Dynamic(Dyn::User), just(T::Number)));
        check!(T::empty_table(), T::array(just(T::Integer));
               T::array(just(T::Integer)));

        // others
        check!(T::Thread, T::Thread; T::Thread);
        check!(T::UserData, T::UserData; T::UserData);
        check!(T::All, T::UserData; T::All);
        check!(T::Thread, T::Dynamic(Dyn::User); T::Dynamic(Dyn::User));

        // general unions
        check!(T::True, T::False; T::Boolean);
        check!(T::Int(3) | T::Nil, T::Int(4) | T::Nil;
               T::ints(vec![3, 4]) | T::Nil);
        check!(T::Int(3) | T::UserData | T::Nil, T::Nil | T::Thread | T::Int(4);
               T::Thread | T::ints(vec![3, 4]) | T::UserData | T::Nil);
        check!(T::ints(vec![3, 5]) | T::Nil, T::Int(4) | T::String;
               T::String | T::ints(vec![3, 4, 5]) | T::Nil);
        check!(T::Int(3) | T::String, T::Str(os("wat")) | T::Int(4);
               T::ints(vec![3, 4]) | T::String);
        assert_eq!(T::map(T::String, just(T::Integer)),
                   T::map(T::String, just(T::Integer | T::Nil)));
    }

    #[test]
    fn test_sub() {
        assert_eq!(T::record(hash![foo=just(T::Int(3)), bar=just(T::Integer)]).assert_sub(
                       &T::map(T::Str(os("foo")) | T::Str(os("bar")), just(T::Number)),
                       &mut NoTypeContext),
                   Ok(()));

        // built-in subtyping
        let subnil = T::Builtin(Builtin::_Subtype, Box::new(T::Nil));
        let nosubnil = T::Builtin(Builtin::_NoSubtype, Box::new(T::Nil));
        let nosubtrue = T::Builtin(Builtin::_NoSubtype, Box::new(T::True));
        let nosubtrueornil = T::Builtin(Builtin::_NoSubtype, Box::new(T::True | T::Nil));
        let nosubboolornil = T::Builtin(Builtin::_NoSubtype, Box::new(T::Boolean | T::Nil));
        assert!(subnil.assert_sub(&subnil, &mut NoTypeContext).is_ok());
        assert!(subnil.assert_sub(&T::Nil, &mut NoTypeContext).is_ok());
        assert!(T::Nil.assert_sub(&subnil, &mut NoTypeContext).is_err());
        assert!(nosubnil.assert_sub(&nosubnil, &mut NoTypeContext).is_ok());
        assert!(nosubnil.assert_sub(&T::Nil, &mut NoTypeContext).is_ok());
        assert!(T::Nil.assert_sub(&nosubnil, &mut NoTypeContext).is_ok());
        assert!(nosubnil.assert_sub(&subnil, &mut NoTypeContext).is_err());
        assert!(subnil.assert_sub(&nosubnil, &mut NoTypeContext).is_err());
        assert!(nosubtrue.assert_sub(&nosubnil, &mut NoTypeContext).is_err());
        assert!(nosubnil.assert_sub(&nosubtrueornil, &mut NoTypeContext).is_ok());
        assert!(nosubtrue.assert_sub(&nosubtrueornil, &mut NoTypeContext).is_ok());
        assert!(nosubboolornil.assert_sub(&nosubtrueornil, &mut NoTypeContext).is_err());
        assert!(nosubtrueornil.assert_sub(&nosubboolornil, &mut NoTypeContext).is_ok());
        assert!(nosubboolornil.assert_sub(&subnil, &mut NoTypeContext).is_err());
        assert!(nosubboolornil.assert_sub(&nosubboolornil, &mut NoTypeContext).is_ok());

        let mut ctx = Context::new(Rc::new(NoReport));

        {
            let v1 = ctx.gen_tvar();
            // v1 <: integer
            assert_eq!(T::TVar(v1).assert_sub(&T::Integer, &mut ctx), Ok(()));
            // v1 <: integer
            assert_eq!(T::TVar(v1).assert_sub(&T::Integer, &mut ctx), Ok(()));
            // v1 <: integer AND v1 <: string (!)
            assert!(T::TVar(v1).assert_sub(&T::String, &mut ctx).is_err());
        }

        {
            let v1 = ctx.gen_tvar();
            let v2 = ctx.gen_tvar();
            // v1 <: v2
            assert_eq!(T::TVar(v1).assert_sub(&T::TVar(v2), &mut ctx), Ok(()));
            // v1 <: v2 <: string
            assert_eq!(T::TVar(v2).assert_sub(&T::String, &mut ctx), Ok(()));
            // v1 <: v2 <: string AND v1 <: integer (!)
            assert!(T::TVar(v1).assert_sub(&T::Integer, &mut ctx).is_err());
        }

        {
            let v1 = ctx.gen_tvar();
            let v2 = ctx.gen_tvar();
            let t1 = T::record(hash![a=just(T::Integer), b=just(T::TVar(v1))]);
            let t2 = T::record(hash![a=just(T::TVar(v2)), b=just(T::String), c=just(T::Boolean)]);
            // {a=just integer, b=just v1} <: {a=just v2, b=just string, c=just boolean}
            assert_eq!(t1.assert_sub(&t2, &mut ctx), Ok(()));
            // ... AND v1 <: string
            assert_eq!(T::TVar(v1).assert_sub(&T::String, &mut ctx), Ok(()));
            // ... AND v1 <: string AND v2 :> integer
            assert_eq!(T::Integer.assert_sub(&T::TVar(v2), &mut ctx), Ok(()));
            // {a=just integer, b=just v1} = {a=just v2, b=just string, c=just boolean} (!)
            assert!(t1.assert_eq(&t2, &mut ctx).is_err());
        }

        /* TODO
        {
            let v1 = ctx.gen_tvar();
            // nil|v1 <: nil|integer
            assert_eq!((T::TVar(v1) | T::Nil).assert_sub(&(T::Integer | T::Nil), &mut ctx),
                       Ok(()));
            // v1 <: nil|integer
            assert_eq!(T::TVar(v1).assert_sub(&(T::Integer | T::Nil), &mut ctx), Ok(()));
            // v1 :> nil|integer
            assert_eq!((T::Integer | T::Nil).assert_sub(&T::TVar(v1), &mut ctx), Ok(()));
        }
        */
    }

    #[test]
    fn test_eq() {
        // built-in subtyping
        let subnil = T::Builtin(Builtin::_Subtype, Box::new(T::Nil));
        let nosubnil = T::Builtin(Builtin::_NoSubtype, Box::new(T::Nil));
        let nosubtrue = T::Builtin(Builtin::_NoSubtype, Box::new(T::True));
        let nosubtrueornil = T::Builtin(Builtin::_NoSubtype, Box::new(T::True | T::Nil));
        let nosubboolornil = T::Builtin(Builtin::_NoSubtype, Box::new(T::Boolean | T::Nil));
        assert!(subnil.assert_eq(&subnil, &mut NoTypeContext).is_ok());
        assert!(subnil.assert_eq(&T::Nil, &mut NoTypeContext).is_err());
        assert!(T::Nil.assert_eq(&subnil, &mut NoTypeContext).is_err());
        assert!(nosubnil.assert_eq(&nosubnil, &mut NoTypeContext).is_ok());
        assert!(nosubnil.assert_eq(&T::Nil, &mut NoTypeContext).is_ok());
        assert!(T::Nil.assert_eq(&nosubnil, &mut NoTypeContext).is_ok());
        assert!(nosubnil.assert_eq(&subnil, &mut NoTypeContext).is_err());
        assert!(subnil.assert_eq(&nosubnil, &mut NoTypeContext).is_err());
        assert!(nosubtrue.assert_eq(&nosubnil, &mut NoTypeContext).is_err());
        assert!(nosubnil.assert_eq(&nosubtrueornil, &mut NoTypeContext).is_err());
        assert!(nosubtrue.assert_eq(&nosubtrueornil, &mut NoTypeContext).is_err());
        assert!(nosubboolornil.assert_eq(&nosubtrueornil, &mut NoTypeContext).is_err());
        assert!(nosubtrueornil.assert_eq(&nosubboolornil, &mut NoTypeContext).is_err());
        assert!(nosubboolornil.assert_eq(&subnil, &mut NoTypeContext).is_err());
        assert!(nosubboolornil.assert_eq(&nosubboolornil, &mut NoTypeContext).is_ok());
    }
}

