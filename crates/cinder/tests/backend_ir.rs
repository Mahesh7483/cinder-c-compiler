//! Backend tests on hand-built IR: register pressure, spilling, values live
//! across calls, phi copy cycles, fixed-register instructions. These cover
//! allocator paths that `-O0` code (everything in memory) never reaches.
#![cfg(target_os = "linux")]

use cinder::backend::{compile_module, BackendOptions};
use cinder::intern::Symbol;
use cinder::ir::*;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

fn sym(name: &str, linkage: Linkage, body: SymBody) -> Sym {
    Sym { name: Symbol::new(name), linkage, body }
}

struct Fb {
    f: Func,
    cur: BlockId,
}

impl Fb {
    fn new(name: &str, sym_id: SymId, rets: Vec<Type>) -> Fb {
        let mut f = Func::new(Symbol::new(name), sym_id, Linkage::External);
        f.rets = rets;
        let entry = f.new_block("entry");
        Fb { f, cur: entry }
    }

    fn param(&mut self, t: Type) -> Operand {
        Operand::Value(self.f.add_param(ParamKind::Value(t), None, None))
    }

    fn block(&mut self, name: &str) -> BlockId {
        self.f.new_block(name)
    }

    fn at(&mut self, b: BlockId) {
        self.cur = b;
    }

    fn v(&mut self, kind: InstKind, ty: Type) -> Operand {
        self.f.push(self.cur, kind, Some(ty), None, 0).unwrap()
    }

    fn bin(&mut self, op: BinOp, ty: Type, a: Operand, b: Operand) -> Operand {
        self.v(InstKind::Bin { op, ty, lhs: a, rhs: b }, ty)
    }

    fn load(&mut self, ty: Type, p: Operand) -> Operand {
        self.v(InstKind::Load { ty, ptr: p, volatile: false }, ty)
    }

    fn ptr_at(&mut self, base: Operand, off: i64) -> Operand {
        self.v(InstKind::PtrAdd { base, offset: Operand::Int(off, Type::I64) }, Type::Ptr)
    }

    fn term(&mut self, t: Term) {
        self.f.blocks[self.cur.idx()].term = t;
    }
}

fn data_i32(name: &str, vals: &[i32]) -> Sym {
    let mut bytes = Vec::new();
    for v in vals {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    sym(
        name,
        Linkage::External,
        SymBody::Data(Some(DataDef {
            size: bytes.len() as u64,
            align: 16,
            items: vec![DataItem::Bytes(bytes)],
            readonly: false,
            zero: false,
        })),
    )
}

fn data_f64(name: &str, vals: &[f64]) -> Sym {
    let mut bytes = Vec::new();
    for v in vals {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    sym(
        name,
        Linkage::External,
        SymBody::Data(Some(DataDef {
            size: bytes.len() as u64,
            align: 16,
            items: vec![DataItem::Bytes(bytes)],
            readonly: false,
            zero: false,
        })),
    )
}

/// Assemble, link and run; returns the exit status.
fn run(m: &Module, tag: &str) -> i32 {
    if let Err(e) = cinder::ir::verify::verify_module(m) {
        panic!("{tag}: invalid IR: {e:?}\n{}", cinder::ir::print::print_module(m));
    }
    static N: AtomicU32 = AtomicU32::new(0);
    let dir =
        std::env::temp_dir().join(format!("cinder-be-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(&dir).unwrap();
    let asm = compile_module(m, &BackendOptions::default());
    let s = dir.join("t.s");
    std::fs::write(&s, &asm).unwrap();
    let exe = dir.join("t");
    let out = Command::new("cc").arg("-no-pie").arg("-o").arg(&exe).arg(&s).output().unwrap();
    assert!(out.status.success(), "{tag}: link failed:\n{}\n{}", String::from_utf8_lossy(&out.stderr), asm);
    let st = Command::new(&exe).status().unwrap();
    let code = st.code().unwrap_or(-1);
    let _ = std::fs::remove_dir_all(&dir);
    code
}

fn module(syms: Vec<Sym>, funcs: Vec<Func>) -> Module {
    Module { syms, funcs, source_file: "test.ir".into() }
}

#[test]
fn forty_simultaneously_live_ints_force_spills() {
    // g[i] = i + 1; load all 40 first, then sum them: all are live at once.
    let vals: Vec<i32> = (1..=40).collect();
    let syms = vec![data_i32("g", &vals), sym("main", Linkage::External, SymBody::Func { defined: true })];
    let mut b = Fb::new("main", SymId(1), vec![Type::I32]);
    let mut loaded = Vec::new();
    for i in 0..40 {
        let p = b.ptr_at(Operand::Global(SymId(0)), i * 4);
        loaded.push(b.load(Type::I32, p));
    }
    let mut acc = loaded[0];
    for v in &loaded[1..] {
        acc = b.bin(BinOp::Add, Type::I32, acc, *v);
    }
    // 820 % 256 = 52
    let r = b.bin(BinOp::And, Type::I32, acc, Operand::Int(255, Type::I32));
    b.term(Term::Ret(vec![r]));
    assert_eq!(run(&module(syms, vec![b.f]), "int pressure"), 52);
}

#[test]
fn thirty_live_doubles_force_xmm_spills() {
    let vals: Vec<f64> = (1..=30).map(|x| x as f64 * 0.5).collect();
    let syms = vec![data_f64("g", &vals), sym("main", Linkage::External, SymBody::Func { defined: true })];
    let mut b = Fb::new("main", SymId(1), vec![Type::I32]);
    let mut loaded = Vec::new();
    for i in 0..30 {
        let p = b.ptr_at(Operand::Global(SymId(0)), i * 8);
        loaded.push(b.load(Type::F64, p));
    }
    // multiply pairs and sum everything: 0.5 * (1 + 2 + ... + 30) = 232.5
    let mut acc = loaded[0];
    for v in &loaded[1..] {
        acc = b.bin(BinOp::FAdd, Type::F64, acc, *v);
    }
    let r = b.v(InstKind::Cast { op: CastOp::FPToSI, from: Type::F64, to: Type::I32, val: acc }, Type::I32);
    assert_eq!(
        run(
            &module(
                syms,
                vec![{
                    b.term(Term::Ret(vec![r]));
                    b.f
                }]
            ),
            "double pressure"
        ),
        232
    );
}

#[test]
fn values_survive_calls_in_callee_saved_registers_or_spills() {
    // id(x) = x; main loads 12 values, calls id 3 times in between, then sums all of them
    let vals: Vec<i32> = (1..=12).collect();
    let syms = vec![
        data_i32("g", &vals),
        sym("main", Linkage::External, SymBody::Func { defined: true }),
        sym("ident", Linkage::External, SymBody::Func { defined: true }),
    ];
    let mut id = Fb::new("ident", SymId(2), vec![Type::I32]);
    let p = id.param(Type::I32);
    id.term(Term::Ret(vec![p]));

    let mut b = Fb::new("main", SymId(1), vec![Type::I32]);
    let mut loaded = Vec::new();
    for i in 0..12 {
        let p = b.ptr_at(Operand::Global(SymId(0)), i * 4);
        loaded.push(b.load(Type::I32, p));
    }
    let mut extra = Operand::Int(0, Type::I32);
    for round in 0..3 {
        let r = b.v(
            InstKind::Call {
                callee: Callee::Direct(SymId(2)),
                args: vec![CallArg {
                    val: Operand::Int(100 * (round + 1), Type::I32),
                    kind: ArgKind::Value,
                    group: None,
                }],
                rets: vec![Type::I32],
                variadic: false,
                tail: false,
            },
            Type::I32,
        );
        extra = b.bin(BinOp::Add, Type::I32, extra, r);
    }
    let mut acc = extra; // 600
    for v in &loaded {
        acc = b.bin(BinOp::Add, Type::I32, acc, *v); // + 78
    }
    // 678 % 256 = 166
    let r = b.bin(BinOp::And, Type::I32, acc, Operand::Int(255, Type::I32));
    b.term(Term::Ret(vec![r]));
    assert_eq!(run(&module(syms, vec![b.f, id.f]), "across calls"), 166);
}

#[test]
fn phi_swap_cycle_is_copied_correctly() {
    // a = 3, b = 5; loop 3 times: (a, b) = (b, a); return a * 10 + b  -> after 3 swaps a=5, b=3 -> 53
    let syms = vec![sym("main", Linkage::External, SymBody::Func { defined: true })];
    let mut f = Fb::new("main", SymId(0), vec![Type::I32]);
    let entry = f.cur;
    let head = f.block("head");
    let body = f.block("body");
    let exit = f.block("exit");
    f.term(Term::Br(head));
    f.at(head);
    // phis are inserted by hand so that they reference each other
    let a_id = f.f.push(head, InstKind::Phi { ty: Type::I32, incoming: vec![] }, Some(Type::I32), None, 0).unwrap();
    let b_id = f.f.push(head, InstKind::Phi { ty: Type::I32, incoming: vec![] }, Some(Type::I32), None, 0).unwrap();
    let i_id = f.f.push(head, InstKind::Phi { ty: Type::I32, incoming: vec![] }, Some(Type::I32), None, 0).unwrap();
    let cond =
        f.v(InstKind::ICmp { pred: IPred::Slt, ty: Type::I32, lhs: i_id, rhs: Operand::Int(3, Type::I32) }, Type::I32);
    f.term(Term::CondBr { cond, then_bb: body, else_bb: exit });
    f.at(body);
    let inext = f.bin(BinOp::Add, Type::I32, i_id, Operand::Int(1, Type::I32));
    f.term(Term::Br(head));
    f.at(exit);
    let t = f.bin(BinOp::Mul, Type::I32, a_id, Operand::Int(10, Type::I32));
    let r = f.bin(BinOp::Add, Type::I32, t, b_id);
    f.term(Term::Ret(vec![r]));
    // fill the phis: a' = b, b' = a (a swap cycle), i' = i + 1
    let set = |f: &mut Func, v: Operand, inc: Vec<(BlockId, Operand)>| {
        let id = f.def_inst(v.value().unwrap()).unwrap();
        if let InstKind::Phi { incoming, .. } = &mut f.insts[id.idx()].kind {
            *incoming = inc;
        }
    };
    set(&mut f.f, a_id, vec![(entry, Operand::Int(3, Type::I32)), (body, b_id)]);
    set(&mut f.f, b_id, vec![(entry, Operand::Int(5, Type::I32)), (body, a_id)]);
    set(&mut f.f, i_id, vec![(entry, Operand::Int(0, Type::I32)), (body, inext)]);
    assert_eq!(run(&module(syms, vec![f.f]), "phi swap"), 53);
}

#[test]
fn division_and_shifts_with_many_live_values() {
    // fixed registers (rax/rdx/rcx) used by div and variable shifts while 14 other values are live
    let vals: Vec<i32> = (1..=14).map(|x| x * 7).collect();
    let syms = vec![data_i32("g", &vals), sym("main", Linkage::External, SymBody::Func { defined: true })];
    let mut b = Fb::new("main", SymId(1), vec![Type::I32]);
    let mut loaded = Vec::new();
    for i in 0..14 {
        let p = b.ptr_at(Operand::Global(SymId(0)), i * 4);
        loaded.push(b.load(Type::I32, p));
    }
    let q = b.bin(BinOp::SDiv, Type::I32, loaded[13], loaded[2]); // 98 / 21 = 4
    let r = b.bin(BinOp::SRem, Type::I32, loaded[12], loaded[3]); // 91 % 28 = 7
    let sh = b.bin(BinOp::Shl, Type::I32, loaded[0], q); // 7 << 4 = 112
    let sh2 = b.bin(BinOp::AShr, Type::I32, sh, r); // 112 >> 7 = 0
    let mut acc = b.bin(BinOp::Add, Type::I32, q, r); // 11
    acc = b.bin(BinOp::Add, Type::I32, acc, sh2);
    for v in &loaded {
        acc = b.bin(BinOp::Add, Type::I32, acc, *v); // + 7 * 105 = 735
    }
    // 746 % 256 = 234
    let res = b.bin(BinOp::And, Type::I32, acc, Operand::Int(255, Type::I32));
    b.term(Term::Ret(vec![res]));
    assert_eq!(run(&module(syms, vec![b.f]), "div/shift pressure"), 234);
}

#[test]
fn float_values_live_across_calls_are_spilled() {
    // all xmm registers are caller-saved: doubles live across a call must be spilled
    let syms = vec![
        data_f64("g", &[1.5, 2.5, 3.5, 4.5]),
        sym("main", Linkage::External, SymBody::Func { defined: true }),
        sym("fid", Linkage::External, SymBody::Func { defined: true }),
    ];
    let mut id = Fb::new("fid", SymId(2), vec![Type::F64]);
    let p = id.param(Type::F64);
    id.term(Term::Ret(vec![p]));
    let mut b = Fb::new("main", SymId(1), vec![Type::I32]);
    let mut vs = Vec::new();
    for i in 0..4 {
        let p = b.ptr_at(Operand::Global(SymId(0)), i * 8);
        vs.push(b.load(Type::F64, p));
    }
    let c = b.v(
        InstKind::Call {
            callee: Callee::Direct(SymId(2)),
            args: vec![CallArg { val: Operand::float(10.0, Type::F64), kind: ArgKind::Value, group: None }],
            rets: vec![Type::F64],
            variadic: false,
            tail: false,
        },
        Type::F64,
    );
    let mut acc = c;
    for v in &vs {
        acc = b.bin(BinOp::FAdd, Type::F64, acc, *v); // 10 + 12 = 22
    }
    let r = b.v(InstKind::Cast { op: CastOp::FPToSI, from: Type::F64, to: Type::I32, val: acc }, Type::I32);
    b.term(Term::Ret(vec![r]));
    assert_eq!(run(&module(syms, vec![b.f, id.f]), "float across call"), 22);
}
