extern crate kailua_syntax;

pub use ty::{Ty, T, Builtin};
pub use env::{TyInfo, Env, CheckResult};
pub use check::{Options, Checker};

mod ty;
mod env;
mod check;

#[test]
fn test_check() {
    fn check(s: &str) -> CheckResult<()> {
        use std::collections::HashMap;

        let parsed = kailua_syntax::parse_chunk(s.as_bytes());
        let chunk = try!(parsed.map_err(|s| format!("parse error: {}", s)));

        struct Opts;
        impl Options for Opts {}
        let mut globals = HashMap::new();
        let mut opts = Opts;
        let mut checker = Checker::new(&mut globals, &mut opts);
        checker.visit(&chunk)
    }

    macro_rules! assert_ok { ($e:expr) => (assert_eq!(check($e), Ok(()))) }
    macro_rules! assert_err { ($e:expr) => (assert!(check($e).is_err())) }

    assert_err!("local p
                 p()");
    assert_ok!("local function p() end
                p()");
    assert_err!("local c
                 if c then local p end
                 p()");
    //assert_err!("local c, p
    //             if c then p = 4 end
    //             p()");
    assert_err!("p()");
    assert_ok!("--# assume p: ?
                p()");
    assert_ok!("--# assume s: ?
                local p = s:find('hello')");
    assert_ok!("--# assume p: number
                local x = p + 3");
    assert_err!("--# assume p: number
                 local x = p + 'foo'");
    assert_err!("--# assume p: unknown_type");
    assert_ok!("local p = 3 + 4");
    assert_err!("local p = 3 + 'foo'");
    assert_err!("local p = true + 7");
    assert_ok!("local p = ({})[3]");
    assert_ok!("local p = ({}):hello()");
    assert_err!("local p = (function() end)[3]");
    //assert_ok!("local f
    //            f = 'hello?'");
    assert_ok!("local f = function() end
                f = function() return 54 end");
    assert_err!("local f = function() end
                 f = {54, 49}");
    assert_ok!("local f = function() end
                --# assume f: table
                local p = f.index");
}
