use std::collections::HashMap;

use kailua_syntax::{Name, Str, Var, Params, E, Exp, UnOp, BinOp, FuncScope, SelfParam, S, Stmt, Block};
use ty::{Builtin, Ty, T};
use env::{Error, CheckResult, TyInfo, Env};

pub trait Options {
    fn require_block(&mut self, path: &[u8]) -> CheckResult<Block> {
        Err("not implemented".into())
    }
}

pub struct Checker<'a> {
    env: Env<'a>,
    opts: &'a mut Options,
}

fn is_possibly_numeric(ty: &T) -> bool {
    // strings can be also used in place of numbers in Lua but omitted here
    match *ty { T::Number | T::Dynamic => true, _ => false }
}

fn is_possibly_stringy(ty: &T) -> bool {
    match *ty { T::Number | T::String | T::Dynamic => true, _ => false }
}

impl<'a> Checker<'a> {
    pub fn new(globals: &'a mut HashMap<Name, TyInfo>, opts: &'a mut Options) -> Checker<'a> {
        Checker { env: Env::new(globals), opts: opts }
    }

    fn scoped<F>(&mut self, f: F) -> CheckResult<()>
            where F: FnOnce(&mut Checker) -> CheckResult<()> {
        let mut sub = Checker { env: self.env.make_subenv(), opts: self.opts };
        f(&mut sub)
    }

    pub fn check_un_op(&mut self, op: UnOp, info: &TyInfo) -> CheckResult<TyInfo> {
        match (op, &*info.ty) {
            (UnOp::Neg, &T::Number)  => Ok(TyInfo::new(T::Number)),
            (UnOp::Not, _)           => Ok(TyInfo::new(T::Boolean)),
            (UnOp::Len, &T::String)  => Ok(TyInfo::new(T::Number)),
            (UnOp::Len, &T::Table)   => Ok(TyInfo::new(T::Number)),
            (_,         &T::Dynamic) => Ok(TyInfo::new(T::Dynamic)),

            (op, ty) => Err(format!("tried to apply {} operator to {:?}", op.symbol(), ty)),
        }
    }

    pub fn check_bin_op(&mut self, lhs: &TyInfo, op: BinOp, rhs: &TyInfo) -> CheckResult<TyInfo> {
        let lty = &*lhs.ty;
        let rty = &*rhs.ty;

        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Pow | BinOp::Mod => {
                if is_possibly_numeric(lty) && is_possibly_numeric(rty) {
                    return Ok(TyInfo::new(T::Number));
                }
            }

            BinOp::Cat => {
                if is_possibly_stringy(lty) && is_possibly_stringy(rty) {
                    return Ok(TyInfo::new(T::String));
                }
            }

            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                match (lty, rty) {
                    (&T::Number, &T::Number) |
                    (&T::Number, &T::Dynamic) |
                    (&T::String, &T::String) |
                    (&T::String, &T::Dynamic) |
                    (&T::Dynamic, &T::Number) |
                    (&T::Dynamic, &T::String) |
                    (&T::Dynamic, &T::Dynamic) => {
                        return Ok(TyInfo::new(T::Boolean));
                    }
                    _ => {}
                }
            }

            BinOp::Eq | BinOp::Ne => {
                return Ok(TyInfo::new(T::Boolean));
            }

            BinOp::And | BinOp::Or => {
                // TODO we need union here...
                match (lty, rty) {
                    (&T::Nil,      _)            => return Ok(TyInfo::new(rty.clone())),
                    (&T::Boolean,  &T::Boolean)  => return Ok(TyInfo::new(T::Boolean)),
                    (&T::Number,   &T::Number)   => return Ok(TyInfo::new(T::Number)),
                    (&T::String,   &T::String)   => return Ok(TyInfo::new(T::String)),
                    (&T::Table,    &T::Table)    => return Ok(TyInfo::new(T::Table)),
                    (&T::Function, &T::Function) => return Ok(TyInfo::new(T::Function)),
                    (_,            _)            => return Ok(TyInfo::new(T::Dynamic)),
                }
            }
        }

        Err(format!("tried to apply {} operator to {:?} and {:?}", op.symbol(), lty, rty))
    }

    pub fn visit(&mut self, chunk: &[Stmt]) -> CheckResult<()> {
        self.visit_block(chunk)
    }

    fn visit_block(&mut self, block: &[Stmt]) -> CheckResult<()> {
        self.scoped(|scope| {
            for stmt in block {
                try!(scope.visit_stmt(stmt));
            }
            Ok(())
        })
    }

    fn visit_stmt(&mut self, stmt: &S) -> CheckResult<()> {
        match *stmt {
            S::Void(ref exp) => {
                try!(self.visit_exp(exp));
            }

            S::Assign(ref vars, ref exps) => {
                for var in vars {
                    try!(self.visit_var(var));
                }
                for (i, exp) in exps.iter().enumerate() {
                    let info = try!(self.visit_exp(exp));
                    if i < vars.len() {
                        if let &Var::Name(ref name) = &vars[i] {
                            // XXX last exp should unpack
                            try!(self.env.assign_to_var(name, info));
                        }
                    }
                }
                if vars.len() > exps.len() {
                    for var in &vars[exps.len()..] {
                        if let &Var::Name(ref name) = var {
                            let info = TyInfo::new(T::Dynamic);
                            // XXX last exp should unpack
                            try!(self.env.assign_to_var(name, info));
                        }
                    }
                }
            }

            S::Do(ref block) => {
                try!(self.visit_block(block));
            }

            S::While(ref cond, ref block) => {
                try!(self.visit_exp(cond));
                try!(self.visit_block(block));
            }

            S::Repeat(ref block, ref cond) => {
                try!(self.visit_block(block));
                try!(self.visit_exp(cond));
            }

            S::If(ref conds, ref lastblock) => {
                for &(ref cond, ref block) in conds {
                    try!(self.visit_exp(cond));
                    try!(self.visit_block(block));
                }
                if let &Some(ref block) = lastblock {
                    try!(self.visit_block(block));
                }
            }

            S::For(ref name, ref start, ref end, ref step, ref block) => {
                try!(self.visit_exp(start));
                try!(self.visit_exp(end));
                if let &Some(ref step) = step {
                    try!(self.visit_exp(step));
                }
                try!(self.scoped(|scope| {
                    scope.env.add_local_var(name, TyInfo::new(T::Number));
                    scope.visit_block(block)
                }));
            }

            S::ForIn(ref names, ref exps, ref block) => {
                for exp in exps {
                    try!(self.visit_exp(exp));
                }
                try!(self.scoped(|scope| {
                    for name in names {
                        scope.env.add_local_var(name, TyInfo::new(T::Dynamic));
                    }
                    scope.visit_block(block)
                }));
            }

            S::FuncDecl(scope, ref name, ref params, ref block) => {
                let info = TyInfo::new(T::Dynamic);
                match scope {
                    FuncScope::Local => self.env.add_local_var(name, info),
                    FuncScope::Global => try!(self.env.assign_to_var(name, info)),
                }
                // `name` itself is available to the inner scope
                try!(self.visit_func_body(None, params, block));
            }

            S::MethodDecl(ref names, selfparam, ref params, ref block) => {
                // TODO verify names
                let selfinfo = match selfparam {
                    SelfParam::Yes => Some(TyInfo::new(T::Dynamic)),
                    SelfParam::No => None,
                };
                try!(self.visit_func_body(selfinfo, params, block));
            }

            S::Local(ref names, ref exps) => {
                for (i, exp) in exps.iter().enumerate() {
                    let info = try!(self.visit_exp(exp));
                    if i < names.len() {
                        // XXX last exp should unpack
                        self.env.add_local_var(&names[i], info);
                    }
                }
                if names.len() > exps.len() {
                    for name in &names[exps.len()..] {
                        let info = TyInfo::new(T::Nil);
                        // XXX last exp should unpack
                        self.env.add_local_var(name, info);
                    }
                }
            }

            S::Return(ref exps) => {
                // XXX should unify with the current function
                for exp in exps {
                    try!(self.visit_exp(exp));
                }
            }

            S::Break => {}

            S::KailuaAssume(ref name, ref kind, ref builtin) => {
                let builtin = if let Some(ref builtin) = *builtin {
                    match &***builtin {
                        b"require" => Some(Builtin::Require),
                        _ => {
                            println!("unrecognized builtin name {:?} for {:?} ignored",
                                     *builtin, *name);
                            None
                        }
                    }
                } else {
                    None
                };
                let info = TyInfo { ty: Box::new(T::from(kind)), builtin: builtin };
                try!(self.env.assume_var(name, info));
            }
        }
        Ok(())
    }

    fn visit_func_body(&mut self, selfinfo: Option<TyInfo>, params: &Params,
                       block: &[Stmt]) -> CheckResult<()> {
        self.scoped(|scope| {
            let selfinfo = selfinfo;
            if let Some(selfinfo) = selfinfo {
                scope.env.add_local_var(&Name::from(&b"self"[..]), selfinfo);
            }
            for param in &params.0 {
                scope.env.add_local_var(param, TyInfo::new(T::Dynamic));
            }
            let vararg = Name::from(&b"..."[..]);
            if params.1 {
                scope.env.add_local_var(&vararg, TyInfo::new(T::Dynamic));
            } else {
                // function a(...)
                //   return function b() return ... end -- this is an error
                // end
                if scope.env.get_local_var(&vararg).is_some() {
                    scope.env.remove_local_var(&vararg);
                }
            }
            scope.visit_block(block)
        })
    }

    fn visit_var(&mut self, var: &Var) -> CheckResult<Option<TyInfo>> {
        match *var {
            Var::Name(ref name) => {
                // may refer to the global variable yet to be defined!
                if let Some(info) = self.env.get_var(name) {
                    Ok(Some(info.to_owned()))
                } else {
                    Ok(None)
                }
            },
            Var::Index(ref e, ref key) => {
                try!(self.visit_exp(e));
                try!(self.visit_exp(key));
                Ok(Some(TyInfo::new(T::Dynamic))) // XXX
            },
        }
    }

    fn visit_exp(&mut self, exp: &E) -> CheckResult<TyInfo> {
        match *exp {
            E::Nil => Ok(TyInfo::new(T::Nil)),
            E::False => Ok(TyInfo::new(T::Boolean)),
            E::True => Ok(TyInfo::new(T::Boolean)),
            E::Num(_) => Ok(TyInfo::new(T::Number)),
            E::Str(_) => Ok(TyInfo::new(T::String)),

            E::Varargs => {
                if let Some(info) = self.env.get_var(&Name::from(&b"..."[..])) {
                    Ok(info.to_owned())
                } else {
                    Err("vararg not declared in the innermost func".into())
                }
            },
            E::Var(ref name) => {
                if let Some(info) = self.env.get_var(name) {
                    Ok(info.to_owned())
                } else {
                    Err(format!("global or local variable {:?} not defined", *name))
                }
            },

            E::Func(ref params, ref block) => {
                try!(self.visit_func_body(None, params, block));
                Ok(TyInfo::new(T::Function))
            },
            E::Table(ref fields) => {
                for &(ref key, ref value) in fields {
                    if let Some(ref key) = *key {
                        try!(self.visit_exp(key));
                    }
                    try!(self.visit_exp(value));
                }
                Ok(TyInfo::new(T::Table))
            },

            E::FuncCall(ref func, ref args) => {
                let funcinfo = try!(self.visit_exp(func));
                match &*funcinfo.ty {
                    &T::Function | &T::Dynamic => {}
                    functy => return Err(format!("tried to call a non-function type {:?}", functy))
                }

                for arg in args {
                    try!(self.visit_exp(arg));
                }

                match funcinfo.builtin {
                    // require("foo")
                    Some(Builtin::Require) if args.len() >= 1 => {
                        if let E::Str(ref path) = *args[0] {
                            let block = match self.opts.require_block(path) {
                                Ok(block) => block,
                                Err(e) => return Err(format!("failed to require {:?}: {}",
                                                             *path, e)),
                            };
                            let mut sub = Checker { env: self.env.make_module(), opts: self.opts };
                            try!(sub.visit_block(&block));
                        }
                        Ok(TyInfo::new(T::Dynamic))
                    },

                    _ => Ok(TyInfo::new(T::Dynamic)),
                }
            },

            E::MethodCall(ref e, ref _method, ref args) => {
                let info = try!(self.visit_exp(e));
                match &*info.ty {
                    // "default" types that metatables are set or can be set
                    // XXX shouldn't this be customizable?
                    &T::Table | &T::String | &T::Dynamic => {}
                    ty => return Err(format!("tried to index a non-table type {:?}", ty))
                }

                for arg in args {
                    try!(self.visit_exp(arg));
                }
                Ok(TyInfo::new(T::Dynamic))
            },

            E::Index(ref e, ref key) => {
                let info = try!(self.visit_exp(e));
                match &*info.ty {
                    // "default" types that metatables are set or can be set
                    // XXX shouldn't this be customizable?
                    &T::Table | &T::String | &T::Dynamic => {}
                    ty => return Err(format!("tried to index a non-table type {:?}", ty))
                }

                try!(self.visit_exp(key));
                Ok(TyInfo::new(T::Dynamic))
            },

            E::Un(op, ref e) => {
                let info = try!(self.visit_exp(e));
                self.check_un_op(op, &info)
            },

            E::Bin(ref l, op, ref r) => {
                let lhs = try!(self.visit_exp(l));
                let rhs = try!(self.visit_exp(r));
                self.check_bin_op(&lhs, op, &rhs)
            },
        }
    }
}

