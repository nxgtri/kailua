#[macro_use] extern crate custom_derive;
#[macro_use] extern crate newtype_derive;

pub use lex::Error;
pub use ast::{Name, Str, Var, TypeSpec, Sig, Ex, Exp, UnOp, BinOp, FuncScope, SelfParam};
pub use ast::{St, Stmt, Block, M, K, Kind};

mod lex;
mod ast;
mod parser;

pub fn parse_chunk(s: &[u8]) -> Result<Block, Error> {
    let lexer = lex::Lexer::new(s.iter().cloned());
    parser::Parser::new(lexer).into_chunk()
}

#[test]
fn test_parse() {
    fn test(s: &str) -> String {
        match parse_chunk(s.as_bytes()) {
            Ok(ast) => format!("{:?}", ast),
            Err(_) => String::from("parse error"),
        }
    }

    assert_eq!(test(""), "[]");
    assert_eq!(test("\n"), "[]");
    assert_eq!(test("\n\n"), "[]");
    assert_eq!(test("do break; end; break"), "[Do([Break]), Break]");
    assert_eq!(test("function r(p) --[[...]] end"), "[FuncDecl(Global, `r`, [`p`], [])]");
    assert_eq!(test("local function r(p,...)\n\nend"), "[FuncDecl(Local, `r`, [`p`, ...], [])]");
    assert_eq!(test("local a, b"), "[Local([`a`, `b`], [])]");
    assert_eq!(test("local a --: integer\n, b --: var ?"),
               "[Local([`a`: _ Integer, `b`: Var Dynamic], [])]");
    assert_eq!(test("local a, --: const table\nb"),
               "[Local([`a`: Const Table, `b`], [])]");
    assert_eq!(test("local function r(p --: integer\n)\n\nend"),
               "[FuncDecl(Local, `r`, [`p`: _ Integer], [])]");
    assert_eq!(test("local function r(p, q) --: integer --> string\n\nend"),
               "[FuncDecl(Local, `r`, [`p`, `q`: _ Integer] -> String, [])]");
    assert_eq!(test("f()"), "[Void(`f`())]");
    assert_eq!(test("f(3)"), "[Void(`f`(3))]");
    assert_eq!(test("f(3+4)"), "[Void(`f`((3 + 4)))]");
    assert_eq!(test("f(3+4-5)"), "[Void(`f`(((3 + 4) - 5)))]");
    assert_eq!(test("f(3+4*5)"), "[Void(`f`((3 + (4 * 5))))]");
    assert_eq!(test("f((3+4)*5)"), "[Void(`f`(((3 + 4) * 5)))]");
    assert_eq!(test("f(2^3^4)"), "[Void(`f`((2 ^ (3 ^ 4))))]");
    assert_eq!(test("f'oo'"), "[Void(`f`(\"oo\"))]");
    assert_eq!(test("f{a=1,[3.1]=4e5;[=[[[]]]=],}"),
               "[Void(`f`(Table([(Some(\"a\"), 1), \
                                 (Some(3.1), 400000), \
                                 (None, \"[[]]\")])))]");
    assert_eq!(test("f{a=a, a}"), "[Void(`f`(Table([(Some(\"a\"), `a`), (None, `a`)])))]");
    assert_eq!(test("f{a=a; a;}"), "[Void(`f`(Table([(Some(\"a\"), `a`), (None, `a`)])))]");
    assert_eq!(test("--[a]\ndo end--]]"), "[Do([])]");
    assert_eq!(test("--[[a]\ndo end--]]"), "[]");
    assert_eq!(test("--#\ndo end"), "[Do([])]");
    assert_eq!(test("--#\n"), "[]");
    assert_eq!(test("--#"), "[]");
    assert_eq!(test("--#\n--#"), "[]");
    assert_eq!(test("--#\n\n--#"), "[]");
    assert_eq!(test("--# --foo\ndo end"), "[Do([])]");
    assert_eq!(test("--# --[[foo]]\ndo end"), "[Do([])]");
    assert_eq!(test("--# --[[foo\n--# --foo]]\ndo end"), "parse error");
    assert_eq!(test("--# assume a: string"), "[KailuaAssume(`a`, _, String, None)]");
    assert_eq!(test("--# assume a: string\n--#"), "[KailuaAssume(`a`, _, String, None)]");
    assert_eq!(test("--# assume a:
                     --#   string"), "[KailuaAssume(`a`, _, String, None)]");
    assert_eq!(test("--# assume a:
                     --# assume b: string"), "parse error");
    assert_eq!(test("--# assume a: {x=string}
                     --# assume b: {y=string}"),
               "[KailuaAssume(`a`, _, Record([\"x\": _ String]), None), \
                 KailuaAssume(`b`, _, Record([\"y\": _ String]), None)]");
    assert_eq!(test("local x --: {b=var string, a=integer, c=const {d=const {}}}"),
               "[Local([`x`: _ Record([\"b\": Var String, \"a\": _ Integer, \
                                       \"c\": Const Record([\"d\": Const Record([])])])], [])]");
    assert_eq!(test("local x --: function()"), "[Local([`x`: _ Func([() -> ()])], [])]");
    assert_eq!(test("local x --: function()->()"), "[Local([`x`: _ Func([() -> ()])], [])]");
    assert_eq!(test("local x --: function () & (integer,...)->string?"),
               "[Local([`x`: _ Func([() -> (), (Integer, ...) -> Union([String, Nil])])], [])]");
    assert_eq!(test("local x --: function (...) | string?"),
               "[Local([`x`: _ Union([Func([(...) -> ()]), Union([String, Nil])])], [])]");
    assert_eq!(test("local x --: (integer, string)"), "parse error");
    assert_eq!(test("local x --: (integer)"), "[Local([`x`: _ Integer], [])]");
    assert_eq!(test("local x --: (integer)?"), "[Local([`x`: _ Union([Integer, Nil])], [])]");
    assert_eq!(test("local x --:
                             --: (integer)?"), "[Local([`x`: _ Union([Integer, Nil])], [])]");
    assert_eq!(test("local x --: (
                             --:   integer
                             --: )?"), "[Local([`x`: _ Union([Integer, Nil])], [])]");
    assert_eq!(test("local x --: (
                             --:   integer"), "parse error");
    assert_eq!(test("local x --: {a = const function (), b = var string,
                             --:  c = const function (string) -> integer &
                             --:                     (string, integer) -> number}?"),
               "[Local([`x`: _ Union([Record([\"a\": Const Func([() -> ()]), \
                                              \"b\": Var String, \
                                              \"c\": Const Func([(String) -> Integer, \
                                                                 (String, Integer) -> Number])\
                                             ]), Nil])], [])]");
    assert_eq!(test("--v ()
                     function foo() end"),
               "[FuncDecl(Global, `foo`, [], [])]");
    assert_eq!(test("--v (a: integer)
                     function foo() end"), "parse error");
    assert_eq!(test("--v (a: integer)
                     function foo(b) end"), "parse error");
    assert_eq!(test("--v ()
                     function foo(b) end"), "parse error");
    assert_eq!(test("--v (a: integer, b: integer)
                     function foo(b, a) end"), "parse error");
    assert_eq!(test("--v (a: integer)
                     local function foo(a) end"),
               "[FuncDecl(Local, `foo`, [`a`: _ Integer], [])]");
    assert_eq!(test("(--v (a: const integer,
                      --v  ...)
                      --v -> string
                      function(a, ...) end)()"),
               "[Void(Func([`a`: Const Integer, ...] -> String, [])())]");
    assert_eq!(test("--v ()
                     function foo() --> string
                     end"), "parse error");
    assert_eq!(test("--v ()
                     function foo(a) --: integer --> string
                     end"), "parse error");
    assert_eq!(test("--v ()
                     function foo(a, --: integer
                                  b) --> string
                     end"), "parse error");
    assert_eq!(test("--v ()"), "parse error");
    assert_eq!(test("--v ()
                     local v = 42"), "parse error");
    assert_eq!(test("--v ()
                     --# assume x: integer"), "parse error");
    assert_eq!(test("--v ()
                     for i = 1, 3 do end"), "parse error");
}

