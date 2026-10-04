//! Control-flow graph utilities: predecessors, reverse post-order,
//! dominators (Cooper–Harvey–Kennedy), dominance frontiers, natural loops and
//! unreachable-block removal.

use super::*;

pub struct Cfg {
    pub preds: Vec<Vec<BlockId>>,
    pub succs: Vec<Vec<BlockId>>,
}

pub fn build(f: &Func) -> Cfg {
    let n = f.blocks.len();
    let mut preds = vec![Vec::new(); n];
    let mut succs = vec![Vec::new(); n];
    for (i, b) in f.blocks.iter().enumerate() {
        let mut ss = b.term.successors();
        // A switch/condbr may name the same target twice; keep edges unique.
        ss.sort();
        ss.dedup();
        for s in &ss {
            preds[s.idx()].push(BlockId(i as u32));
        }
        succs[i] = ss;
    }
    Cfg { preds, succs }
}

/// Blocks reachable from the entry, in reverse post-order.
pub fn rpo(f: &Func, cfg: &Cfg) -> Vec<BlockId> {
    let n = f.blocks.len();
    let mut seen = vec![false; n];
    let mut post: Vec<BlockId> = Vec::with_capacity(n);
    // iterative DFS
    let mut stack: Vec<(BlockId, usize)> = vec![(BlockId(0), 0)];
    seen[0] = true;
    while let Some((b, i)) = stack.pop() {
        if i < cfg.succs[b.idx()].len() {
            stack.push((b, i + 1));
            let s = cfg.succs[b.idx()][i];
            if !seen[s.idx()] {
                seen[s.idx()] = true;
                stack.push((s, 0));
            }
        } else {
            post.push(b);
        }
    }
    post.reverse();
    post
}

pub fn reachable(f: &Func, cfg: &Cfg) -> Vec<bool> {
    let mut r = vec![false; f.blocks.len()];
    for b in rpo(f, cfg) {
        r[b.idx()] = true;
    }
    r
}

pub struct DomTree {
    /// Immediate dominator; `None` for the entry and unreachable blocks.
    pub idom: Vec<Option<BlockId>>,
    pub children: Vec<Vec<BlockId>>,
    /// Position in reverse post-order (`usize::MAX` if unreachable).
    pub rpo_index: Vec<usize>,
    pub order: Vec<BlockId>,
}

pub fn dominators(f: &Func, cfg: &Cfg) -> DomTree {
    let n = f.blocks.len();
    let order = rpo(f, cfg);
    let mut rpo_index = vec![usize::MAX; n];
    for (i, b) in order.iter().enumerate() {
        rpo_index[b.idx()] = i;
    }
    let mut idom: Vec<Option<BlockId>> = vec![None; n];
    idom[0] = Some(BlockId(0));
    let intersect = |idom: &Vec<Option<BlockId>>, mut a: BlockId, mut b: BlockId| -> BlockId {
        while a != b {
            while rpo_index[a.idx()] > rpo_index[b.idx()] {
                a = idom[a.idx()].unwrap();
            }
            while rpo_index[b.idx()] > rpo_index[a.idx()] {
                b = idom[b.idx()].unwrap();
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &b in order.iter().skip(1) {
            let mut new_idom: Option<BlockId> = None;
            for &p in &cfg.preds[b.idx()] {
                if rpo_index[p.idx()] == usize::MAX || idom[p.idx()].is_none() {
                    continue;
                }
                new_idom = Some(match new_idom {
                    None => p,
                    Some(cur) => intersect(&idom, p, cur),
                });
            }
            if new_idom != idom[b.idx()] {
                idom[b.idx()] = new_idom;
                changed = true;
            }
        }
    }
    idom[0] = None;
    let mut children = vec![Vec::new(); n];
    for (i, d) in idom.iter().enumerate() {
        if let Some(d) = d {
            children[d.idx()].push(BlockId(i as u32));
        }
    }
    DomTree { idom, children, rpo_index, order }
}

impl DomTree {
    /// Does `a` dominate `b`? (Every block dominates itself.)
    pub fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        if self.rpo_index[a.idx()] == usize::MAX || self.rpo_index[b.idx()] == usize::MAX {
            return false;
        }
        let mut cur = b;
        loop {
            if cur == a {
                return true;
            }
            match self.idom[cur.idx()] {
                Some(d) => cur = d,
                None => return false,
            }
        }
    }

    pub fn is_reachable(&self, b: BlockId) -> bool {
        self.rpo_index[b.idx()] != usize::MAX
    }

    /// Dominance frontiers (Cytron et al.).
    pub fn frontiers(&self, cfg: &Cfg) -> Vec<Vec<BlockId>> {
        let n = self.idom.len();
        let mut df: Vec<Vec<BlockId>> = vec![Vec::new(); n];
        for b in 0..n {
            if !self.is_reachable(BlockId(b as u32)) || cfg.preds[b].len() < 2 {
                continue;
            }
            for &p in &cfg.preds[b] {
                if !self.is_reachable(p) {
                    continue;
                }
                let mut runner = p;
                while Some(runner) != self.idom[b] {
                    if !df[runner.idx()].contains(&BlockId(b as u32)) {
                        df[runner.idx()].push(BlockId(b as u32));
                    }
                    match self.idom[runner.idx()] {
                        Some(d) => runner = d,
                        None => break,
                    }
                }
            }
        }
        df
    }
}

/// A natural loop: a header and the set of blocks that can reach a back edge
/// without leaving through the header.
#[derive(Clone, Debug)]
pub struct Loop {
    pub header: BlockId,
    pub blocks: Vec<BlockId>,
    pub latches: Vec<BlockId>,
}

/// Natural loops, with loops sharing a header merged. Innermost loops first.
pub fn find_loops(f: &Func, cfg: &Cfg, dom: &DomTree) -> Vec<Loop> {
    let mut by_header: Vec<(BlockId, Vec<BlockId>)> = Vec::new();
    for b in 0..f.blocks.len() {
        let b = BlockId(b as u32);
        if !dom.is_reachable(b) {
            continue;
        }
        for &s in &cfg.succs[b.idx()] {
            if dom.dominates(s, b) {
                match by_header.iter_mut().find(|(h, _)| *h == s) {
                    Some((_, latches)) => latches.push(b),
                    None => by_header.push((s, vec![b])),
                }
            }
        }
    }
    let mut loops: Vec<Loop> = Vec::new();
    for (header, latches) in by_header {
        let mut in_loop = vec![false; f.blocks.len()];
        in_loop[header.idx()] = true;
        let mut work: Vec<BlockId> = Vec::new();
        for &l in &latches {
            if !in_loop[l.idx()] {
                in_loop[l.idx()] = true;
                work.push(l);
            }
        }
        while let Some(b) = work.pop() {
            for &p in &cfg.preds[b.idx()] {
                if dom.is_reachable(p) && !in_loop[p.idx()] {
                    in_loop[p.idx()] = true;
                    work.push(p);
                }
            }
        }
        let blocks: Vec<BlockId> = (0..f.blocks.len()).filter(|&i| in_loop[i]).map(|i| BlockId(i as u32)).collect();
        loops.push(Loop { header, blocks, latches });
    }
    loops.sort_by_key(|l| l.blocks.len());
    loops
}

/// Delete blocks that cannot be reached from the entry and renumber the rest.
/// Phi inputs from removed predecessors are dropped. Returns true if anything changed.
pub fn remove_unreachable(f: &mut Func) -> bool {
    let cfg = build(f);
    let reach = reachable(f, &cfg);
    if reach.iter().all(|&r| r) {
        return false;
    }
    // Kill instructions of removed blocks.
    for (i, b) in f.blocks.iter().enumerate() {
        if !reach[i] {
            for &id in &b.insts {
                f.dead_insts[id.idx()] = true;
            }
        }
    }
    // New numbering.
    let mut map: Vec<Option<BlockId>> = vec![None; f.blocks.len()];
    let mut next = 0u32;
    for (i, &r) in reach.iter().enumerate() {
        if r {
            map[i] = Some(BlockId(next));
            next += 1;
        }
    }
    // Rewrite phis and terminators of surviving blocks.
    let old_blocks = std::mem::take(&mut f.blocks);
    let mut new_blocks: Vec<Block> = Vec::with_capacity(next as usize);
    for (i, mut b) in old_blocks.into_iter().enumerate() {
        if !reach[i] {
            continue;
        }
        for &id in &b.insts {
            if let InstKind::Phi { incoming, .. } = &mut f.insts[id.idx()].kind {
                incoming.retain(|(p, _)| map[p.idx()].is_some());
                for (p, _) in incoming.iter_mut() {
                    *p = map[p.idx()].unwrap();
                }
            }
        }
        remap_term(&mut b.term, &map);
        new_blocks.push(b);
    }
    f.blocks = new_blocks;
    true
}

fn remap_term(t: &mut Term, map: &[Option<BlockId>]) {
    let m = |b: &mut BlockId| *b = map[b.idx()].expect("edge to removed block");
    match t {
        Term::Br(b) => m(b),
        Term::CondBr { then_bb, else_bb, .. } => {
            m(then_bb);
            m(else_bb);
        }
        Term::Switch { cases, default, .. } => {
            for (_, b) in cases.iter_mut() {
                m(b);
            }
            m(default);
        }
        _ => {}
    }
}

/// Replace every use of `from` with `to` across the function (instructions and terminators).
pub fn replace_uses(f: &mut Func, from: ValueId, to: Operand) {
    for (i, inst) in f.insts.iter_mut().enumerate() {
        if f.dead_insts[i] {
            continue;
        }
        inst.kind.for_each_operand_mut(|o| {
            if *o == Operand::Value(from) {
                *o = to;
            }
        });
    }
    for b in &mut f.blocks {
        match &mut b.term {
            Term::CondBr { cond, .. } => {
                if *cond == Operand::Value(from) {
                    *cond = to;
                }
            }
            Term::Switch { val, .. } => {
                if *val == Operand::Value(from) {
                    *val = to;
                }
            }
            Term::Ret(vs) => {
                for v in vs {
                    if *v == Operand::Value(from) {
                        *v = to;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Number of uses of each value.
pub fn use_counts(f: &Func) -> Vec<u32> {
    let mut c = vec![0u32; f.values.len()];
    for b in &f.blocks {
        for &id in &b.insts {
            f.insts[id.idx()].kind.for_each_operand(|o| {
                if let Operand::Value(v) = o {
                    c[v.idx()] += 1;
                }
            });
        }
        for o in b.term.operands() {
            if let Operand::Value(v) = o {
                c[v.idx()] += 1;
            }
        }
    }
    c
}
