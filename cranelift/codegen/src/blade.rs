//! A pass over Cranelift IR which implements the Blade algorithm

use crate::cursor::{Cursor, EncCursor};
use crate::entity::SecondaryMap;
use crate::flowgraph::ControlFlowGraph;
use crate::ir::{condcodes::IntCC, dfg::Bounds, Function, Inst, InstBuilder, InstructionData, Opcode, Value, ValueDef};
use crate::isa::TargetIsa;
use crate::settings;
use rs_graph::linkedlistgraph::{Edge, LinkedListGraph, Node};
use rs_graph::maxflow::pushrelabel::PushRelabel;
use rs_graph::maxflow::MaxFlow;
use rs_graph::traits::Directed;
use rs_graph::Buildable;
use rs_graph::Builder;
use std::collections::{HashMap, HashSet};
use alloc::vec::Vec;
use alloc::boxed::Box;

/// If this is `true`, then Blade will use a fake bound information for any
/// address which does not have associated bounds information.
pub(crate) const ALLOW_FAKE_SLH_BOUNDS: bool = true;

/// Nonsense array length (in bytes) to use as the fake bound information (see
/// `ALLOW_FAKE_SLH_BOUNDS` above)
pub(crate) const FAKE_SLH_ARRAY_LENGTH_BYTES: u32 = 2345;

const DEBUG_PRINT_FUNCTION_BEFORE_AND_AFTER: bool = false;

pub fn do_blade(func: &mut Function, isa: &dyn TargetIsa, cfg: &ControlFlowGraph, blade_setting: settings::Blade) {
    if blade_setting == settings::Blade::None {
        return;
    }

    if DEBUG_PRINT_FUNCTION_BEFORE_AND_AFTER {
        println!("Function before blade:\n{}", func.display(isa));
    }

    let blade_graph = build_blade_graph_for_func(func, cfg, true);

    let cut_edges = blade_graph.min_cut();

    // insert the fences / SLHs
    let mut slh_ctx = SLHContext::new();
    for cut_edge in cut_edges {
        let edge_src = blade_graph.graph.src(cut_edge);
        let edge_snk = blade_graph.graph.snk(cut_edge);
        match blade_setting {
            settings::Blade::Lfence | settings::Blade::LfencePerBlock => {
                if edge_src == blade_graph.source_node {
                    // source -> n : fence after n
                    insert_fence_after(
                        func,
                        blade_graph
                            .node_to_bladenode_map
                            .get(&edge_snk)
                            .unwrap()
                            .clone(),
                        blade_setting,
                    );
                } else if edge_snk == blade_graph.sink_node {
                    // n -> sink : fence before (def of) n
                    insert_fence_before(
                        func,
                        blade_graph
                            .node_to_bladenode_map
                            .get(&edge_src)
                            .unwrap()
                            .clone(),
                        blade_setting,
                    );
                } else {
                    // n -> m : fence before m
                    insert_fence_before(
                        func,
                        blade_graph
                            .node_to_bladenode_map
                            .get(&edge_snk)
                            .unwrap()
                            .clone(),
                        blade_setting,
                    );
                }
            }
            settings::Blade::Slh => {
                if edge_src == blade_graph.source_node {
                    // source -> n : apply SLH to the instruction that produces n
                    slh_ctx.do_slh_on(func, isa, blade_graph.node_to_bladenode_map[&edge_snk].clone());
                } else if edge_snk == blade_graph.sink_node {
                    // n -> sink : for SLH we can't cut at n (which is a sink instruction), we have
                    // to trace back through the graph and cut at all sources which lead to n
                    for node in blade_graph.ancestors_of(edge_src) {
                        slh_ctx.do_slh_on(func, isa, blade_graph.node_to_bladenode_map[&node].clone());
                    }
                } else {
                    // n -> m : likewise, apply SLH to all sources which lead to n
                    for node in blade_graph.ancestors_of(edge_src) {
                        slh_ctx.do_slh_on(func, isa, blade_graph.node_to_bladenode_map[&node].clone());
                    }
                }
            }
            settings::Blade::None => panic!("Shouldn't reach here with Blade setting None"),
        }
    }

    if DEBUG_PRINT_FUNCTION_BEFORE_AND_AFTER {
        println!("Function after blade:\n{}", func.display(isa));
    }
}

fn insert_fence_before(func: &mut Function, bnode: BladeNode, blade_setting: settings::Blade) {
    match bnode {
        BladeNode::ValueDef(val) => match func.dfg.value_def(val) {
            ValueDef::Result(inst, _) => {
                match blade_setting {
                    settings::Blade::Lfence => {
                        // cut at this value by putting lfence before `inst`
                        func.pre_lfence[inst] = true;
                    }
                    settings::Blade::LfencePerBlock => {
                        // just put one fence at the beginning of the block.
                        // this stops speculation due to branch mispredictions.
                        insert_fence_at_beginning_of_block(func, inst);
                    }
                    _ => panic!(
                        "This function didn't expect to be called with blade setting {:?}",
                        blade_setting
                    ),
                }
            }
            ValueDef::Param(block, _) => {
                // cut at this value by putting lfence at beginning of
                // the `block`, that is, before the first instruction
                let first_inst = func
                    .layout
                    .first_inst(block)
                    .expect("block has no instructions");
                func.pre_lfence[first_inst] = true;
            }
        },
        BladeNode::Sink(inst) => {
            match blade_setting {
                settings::Blade::Lfence => {
                    // cut at this instruction by putting lfence before it
                    func.pre_lfence[inst] = true;
                }
                settings::Blade::LfencePerBlock => {
                    // just put one fence at the beginning of the block.
                    // this stops speculation due to branch mispredictions.
                    insert_fence_at_beginning_of_block(func, inst);
                }
                _ => panic!(
                    "This function didn't expect to be called with blade setting {:?}",
                    blade_setting
                ),
            }
        }
    }
}

fn insert_fence_after(func: &mut Function, bnode: BladeNode, blade_setting: settings::Blade) {
    match bnode {
        BladeNode::ValueDef(val) => match func.dfg.value_def(val) {
            ValueDef::Result(inst, _) => {
                match blade_setting {
                    settings::Blade::Lfence => {
                        // cut at this value by putting lfence after `inst`
                        func.post_lfence[inst] = true;
                    }
                    settings::Blade::LfencePerBlock => {
                        // just put one fence at the beginning of the block.
                        // this stops speculation due to branch mispredictions.
                        insert_fence_at_beginning_of_block(func, inst);
                    }
                    _ => panic!(
                        "This function didn't expect to be called with blade setting {:?}",
                        blade_setting
                    ),
                }
            }
            ValueDef::Param(block, _) => {
                // cut at this value by putting lfence at beginning of
                // the `block`, that is, before the first instruction
                let first_inst = func
                    .layout
                    .first_inst(block)
                    .expect("block has no instructions");
                func.pre_lfence[first_inst] = true;
            }
        },
        BladeNode::Sink(_) => panic!("Fencing after a sink instruction"),
    }
}

// Inserts a fence at the beginning of the _basic block_ containing the given
// instruction. "Basic block" is not to be confused with the _EBB_ or "extended
// basic block" (which is what Cranelift considers a "block").
// For our purposes in this function, all branch, call, and ret instructions
// terminate blocks. In contrast, in Cranelift, only unconditional branch and
// ret instructions terminate EBBs, while conditional branches and call
// instructions do not terminate EBBs.
fn insert_fence_at_beginning_of_block(func: &mut Function, inst: Inst) {
    let ebb = func
        .layout
        .inst_block(inst)
        .expect("Instruction is not in layout");
    let first_inst = func
        .layout
        .first_inst(ebb)
        .expect("EBB has no instructions");
    let mut cur_inst = inst;
    loop {
        if cur_inst == first_inst {
            // got to beginning of EBB: insert at beginning of EBB
            func.pre_lfence[first_inst] = true;
            break;
        }
        cur_inst = func
            .layout
            .prev_inst(cur_inst)
            .expect("Ran off the beginning of the EBB");
        let opcode = func.dfg[cur_inst].opcode();
        if opcode.is_call() || opcode.is_branch() || opcode.is_indirect_branch() {
            // found the previous call or branch instruction:
            // insert after that call or branch instruction
            func.post_lfence[cur_inst] = true;
            break;
        }
    }
}

struct SLHContext {
    /// tracks which `BladeNode`s have already had SLH applied to them
    bladenodes_done: HashSet<BladeNode>,
}

impl SLHContext {
    /// A blank SLHContext
    fn new() -> Self {
        Self {
            bladenodes_done: HashSet::new(),
        }
    }

    /// Do SLH on `bnode`, but only if we haven't already done SLH on `bnode`
    fn do_slh_on(&mut self, func: &mut Function, isa: &dyn TargetIsa, bnode: BladeNode) {
        if self.bladenodes_done.insert(bnode.clone()) {
            _do_slh_on(func, isa, bnode);
        }
    }
}

fn _do_slh_on(func: &mut Function, isa: &dyn TargetIsa, bnode: BladeNode) {
    match bnode {
        BladeNode::Sink(_) => panic!("Can't do SLH to protect a sink, have to protect a source"),
        BladeNode::ValueDef(value) => {
            // The value that needs protecting is `value`, so we need to apply SLH to the load which produced `value`
            match func.dfg.value_def(value) {
                ValueDef::Param(_, _) => unimplemented!("SLH on a block parameter"),
                ValueDef::Result(inst, _) => {
                    assert!(func.dfg[inst].opcode().can_load(), "SLH on a non-load instruction: {:?}", func.dfg[inst]);
                    let mut cur = EncCursor::new(func, isa).at_inst(inst);
                    // Find the arguments to `inst` which are marked as pointers / have bounds
                    // (as pairs (argnum, argvalue))
                    let mut pointer_args = cur.func.dfg.inst_args(inst).iter().copied().enumerate().filter(|&(_, arg)| cur.func.dfg.bounds[arg].is_some());
                    let (pointer_arg_num, pointer_arg, bounds) = match pointer_args.next() {
                        Some((num, arg)) => match pointer_args.next() {
                            Some(_) => panic!("SLH: multiple pointer args found to instruction {:?}", func.dfg[inst]),
                            None => {
                                // all good, there is exactly one pointer arg
                                let bounds = cur.func.dfg.bounds[arg].clone().expect("we already checked that there's bounds here");
                                (num, arg, bounds)
                            }
                        }
                        None => {
                            if ALLOW_FAKE_SLH_BOUNDS {
                                let pointer_arg_num = 0; // we pick the first arg, arbitrarily
                                let pointer_arg = cur.func.dfg.inst_args(inst)[pointer_arg_num];
                                let lower = pointer_arg;
                                let upper = cur.ins().iadd_imm(pointer_arg, (FAKE_SLH_ARRAY_LENGTH_BYTES as u64) as i64);
                                let bounds = Bounds {
                                    lower,
                                    upper,
                                    directly_annotated: false,
                                };
                                (pointer_arg_num, pointer_arg, bounds)
                            } else {
                                panic!("SLH: no pointer arg found for instruction {:?}", func.dfg[inst])
                            }
                        }
                    };
                    let masked_pointer = {
                        let pointer_ty = cur.func.dfg.value_type(pointer_arg);
                        let zero = cur.ins().iconst(pointer_ty, 0);
                        let all_ones = cur.ins().iconst(pointer_ty, -1);
                        let flags = cur.ins().ifcmp(pointer_arg, bounds.lower);
                        let mask = cur.ins().selectif(pointer_ty, IntCC::UnsignedGreaterThanOrEqual, flags, all_ones, zero);
                        let op_size_bytes = {
                            let bytes = cur.func.dfg.value_type(value).bytes() as u64;
                            cur.ins().iconst(pointer_ty, bytes as i64)
                        };
                        let adjusted_upper_bound = cur.ins().isub(bounds.upper, op_size_bytes);
                        let flags = cur.ins().ifcmp(pointer_arg, adjusted_upper_bound);
                        let mask = cur.ins().selectif(pointer_ty, IntCC::UnsignedLessThanOrEqual, flags, mask, zero);
                        cur.ins().band(pointer_arg, mask)
                    };
                    // now update the original load instruction to use the masked pointer instead
                    cur.func.dfg.inst_args_mut(inst)[pointer_arg_num] = masked_pointer;
                }
            }
        }
    }
}

struct DefUseGraph {
    /// Maps a value to its uses
    map: SecondaryMap<Value, Vec<ValueUse>>,
}

impl DefUseGraph {
    /// Create a `DefUseGraph` for the given `Function`.
    ///
    /// `cfg`: the `ControlFlowGraph` for the `Function`.
    pub fn for_function(func: &Function, cfg: &ControlFlowGraph) -> Self {
        let mut map: SecondaryMap<Value, Vec<ValueUse>> =
            SecondaryMap::with_capacity(func.dfg.num_values());

        for block in func.layout.blocks() {
            // Iterate over every instruction. Mark that instruction as a use of
            // each of its parameters.
            for inst in func.layout.block_insts(block) {
                for arg in func.dfg.inst_args(inst) {
                    map[*arg].push(ValueUse::Inst(inst));
                }
            }
            // Also, mark each block parameter as a use of the corresponding argument
            // in all branch instructions which can feed this block
            for incoming_bb in cfg.pred_iter(block) {
                let incoming_branch = &func.dfg[incoming_bb.inst];
                let branch_args = match incoming_branch {
                    InstructionData::Branch { .. }
                    | InstructionData::BranchFloat { .. }
                    | InstructionData::BranchIcmp { .. }
                    | InstructionData::BranchInt { .. }
                    | InstructionData::Call { .. }
                    | InstructionData::CallIndirect { .. }
                    | InstructionData::IndirectJump { .. }
                    | InstructionData::Jump { .. } => func.dfg.inst_variable_args(incoming_bb.inst),
                    _ => panic!(
                        "incoming_branch is an unexpected type: {:?}",
                        incoming_branch
                    ),
                };
                let block_params = func.dfg.block_params(block);
                assert_eq!(branch_args.len(), block_params.len());
                for (param, arg) in block_params.iter().zip(branch_args.iter()) {
                    map[*arg].push(ValueUse::Value(*param));
                }
            }
        }

        Self { map }
    }

    /// Iterate over all the uses of the given `Value`
    pub fn uses_of_val(&self, val: Value) -> impl Iterator<Item = &ValueUse> {
        self.map[val].iter()
    }

    /// Iterate over all the uses of the result of the given `Inst` in the given `Function`
    // (function is currently unused)
    pub fn _uses_of_inst<'a>(
        &'a self,
        inst: Inst,
        func: &'a Function,
    ) -> impl Iterator<Item = &'a ValueUse> {
        func.dfg
            .inst_results(inst)
            .iter()
            .map(move |&val| self.uses_of_val(val))
            .flatten()
    }
}

/// Describes a way in which a given `Value` is used
#[derive(Clone, Debug)]
enum ValueUse {
    /// This `Instruction` uses the `Value`
    Inst(Inst),
    /// The `Value` may be forwarded to this `Value`
    Value(Value),
}

struct BladeGraph {
    /// the actual graph
    graph: LinkedListGraph<usize>,
    /// the (single) source node. there are edges from this to sources
    source_node: Node<usize>,
    /// the (single) sink node. there are edges from sinks to this
    sink_node: Node<usize>,
    /// maps graph nodes to the `BladeNode`s which they correspond to
    node_to_bladenode_map: HashMap<Node<usize>, BladeNode>,
    /// maps `BladeNode`s to the graph nodes which they correspond to
    _bladenode_to_node_map: HashMap<BladeNode, Node<usize>>,
}

#[derive(PartialEq, Eq, Clone, Debug, Hash)]
enum BladeNode {
    /// A `BladeNode` representing the definition of a value
    ValueDef(Value),
    /// A `BladeNode` representing an instruction that serves as a sink
    Sink(Inst),
}

impl BladeGraph {
    /// Return the cut-edges in the mincut of the graph
    fn min_cut(&self) -> Vec<Edge<usize>> {
        // TODO: our options are `Dinic`, `EdmondsKarp`, or `PushRelabel`.
        // I'm not sure what the tradeoffs are.
        // SC: from my limited wikipedia'ing, pushrelabel is supposedly the best
        let mut maxflow = PushRelabel::<LinkedListGraph<usize>, usize>::new(&self.graph);
        maxflow.solve(self.source_node, self.sink_node, |_| 1); // all edges have weight 1

        // turns out `mincut` returns the set of nodes reachable from the source node after
        //   the graph is cut; we have to recreate the cut based on this set
        let reachable_from_source = maxflow.mincut();
        // XXX there's probably a more efficient algorithm
        reachable_from_source
            .iter()
            .map(move |node| self.graph.outedges(*node))
            .flatten()
            .filter(|(_, dst)| !reachable_from_source.contains(dst))
            .map(|(edge, _)| edge)
            .collect()
    }

    /// Given a `Node`, iterate over all of the "source nodes" which have paths
    /// to it, where "source node" is defined by `self.is_source_node`.
    ///
    /// If the given `node` is itself a "source node", we'll just return `node` itself
    fn ancestors_of<'s>(&'s self, node: Node<usize>) -> Box<dyn Iterator<Item = Node<usize>> + 's> {
        if self.is_source_node(node) {
            Box::new(std::iter::once(node))
        } else {
            Box::new(
                self.graph.inedges(node)
                    .map(move |(_, incoming)| self.ancestors_of(incoming))
                    .flatten()
            )
        }
    }

    /// Is the given `Node` a "source node" in the graph, where "source node" is
    /// here defined as any node which has an edge from the global source node to
    /// it
    fn is_source_node(&self, node: Node<usize>) -> bool {
        self.graph.inedges(node).any(|(_, incoming)| incoming == self.source_node)
    }
}

struct BladeGraphBuilder {
    /// builder for the actual graph
    graph: <LinkedListGraph<usize> as rs_graph::Buildable>::Builder,
    /// the (single) source node
    source_node: Node<usize>,
    /// the (single) sink node
    sink_node: Node<usize>,
    /// maps graph nodes to the `BladeNode`s which they correspond to
    node_to_bladenode_map: HashMap<Node<usize>, BladeNode>,
    /// maps `BladeNode`s to the graph nodes which they correspond to
    bladenode_to_node_map: HashMap<BladeNode, Node<usize>>,
}

impl BladeGraphBuilder {
    /// Creates a new `BladeGraphBuilder` with the `node_to_bladenode_map` and
    /// `bladenode_to_node_map` populated for all `Value`s in the `Function`
    fn with_nodes_for_func(func: &Function) -> Self {
        let mut gg = LinkedListGraph::<usize>::new_builder();
        let mut node_to_bladenode_map = HashMap::new();
        let mut bladenode_to_node_map = HashMap::new();
        let source_node = gg.add_node();
        let sink_node = gg.add_node();

        // add nodes for all values in the function, and populate our maps accordingly
        for val in func.dfg.values() {
            let node = gg.add_node();
            node_to_bladenode_map.insert(node, BladeNode::ValueDef(val));
            bladenode_to_node_map.insert(BladeNode::ValueDef(val), node);
        }

        Self {
            graph: gg,
            source_node,
            sink_node,
            node_to_bladenode_map,
            bladenode_to_node_map,
        }
    }

    /// Mark the given `Value` as a source.
    fn mark_as_source(&mut self, src: Value) {
        let node = self.bladenode_to_node_map[&BladeNode::ValueDef(src)];
        self.graph.add_edge(self.source_node, node);
    }

    /// Add an edge from the given `Node` to the given `Value`
    fn add_edge_from_node_to_value(&mut self, from: Node<usize>, to: Value) {
        let value_node = self.bladenode_to_node_map[&BladeNode::ValueDef(to)];
        self.graph.add_edge(from, value_node);
    }

    /// Add an edge from the given `Value` to the given `Node`
    fn add_edge_from_value_to_node(&mut self, from: Value, to: Node<usize>) {
        let value_node = self.bladenode_to_node_map[&BladeNode::ValueDef(from)];
        self.graph.add_edge(value_node, to);
    }

    /// Add a new sink node for the given `inst`
    fn add_sink_node_for_inst(&mut self, inst: Inst) -> Node<usize> {
        let inst_sink_node = self.graph.add_node();
        self.node_to_bladenode_map
            .insert(inst_sink_node, BladeNode::Sink(inst));
        self.bladenode_to_node_map
            .insert(BladeNode::Sink(inst), inst_sink_node);
        self.graph.add_edge(inst_sink_node, self.sink_node);
        inst_sink_node
    }

    /// Consumes the `BladeGraphBuilder`, generating a `BladeGraph`
    fn build(self) -> BladeGraph {
        BladeGraph {
            graph: self.graph.to_graph(),
            source_node: self.source_node,
            sink_node: self.sink_node,
            node_to_bladenode_map: self.node_to_bladenode_map,
            _bladenode_to_node_map: self.bladenode_to_node_map,
        }
    }
}

/// `store_values_are_sinks`: if `true`, then the value operand to a store
/// instruction is considered a sink. if `false`, it is not.
/// For instance in the instruction "store x to addrA", if
/// `store_values_are_sinks` is `true`, then both `x` and `addrA` are sinks,
/// but if it is `false`, then just `addrA` is a sink.
fn build_blade_graph_for_func(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    store_values_are_sinks: bool,
) -> BladeGraph {
    let mut builder = BladeGraphBuilder::with_nodes_for_func(func);

    // find sources and sinks, and add edges to/from our global source and sink nodes
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let idata = &func.dfg[inst];
            let op = idata.opcode();
            if op.can_load() {
                // loads are both sources (their loaded values) and sinks (their addresses)
                // except for fills, which don't have sinks

                // handle load as a source
                for &result in func.dfg.inst_results(inst) {
                    builder.mark_as_source(result);
                }

                // handle load as a sink, except for fills
                if !(op == Opcode::Fill || op == Opcode::FillNop) {
                    let inst_sink_node = builder.add_sink_node_for_inst(inst);
                    // for each address component variable of inst,
                    // add edge address_component_variable_node -> sink
                    // XXX X86Pop has an implicit dependency on %rsp which is not captured here
                    for &arg in func.dfg.inst_args(inst) {
                        builder.add_edge_from_value_to_node(arg, inst_sink_node);
                    }
                }

            } else if op.can_store() {
                // loads are both sources and sinks, but stores are just sinks

                let inst_sink_node = builder.add_sink_node_for_inst(inst);
                // similar to the load case above, but special treatment for the value being stored
                // XXX X86Push has an implicit dependency on %rsp which is not captured here
                if store_values_are_sinks {
                    for &arg in func.dfg.inst_args(inst) {
                        builder.add_edge_from_value_to_node(arg, inst_sink_node);
                    }
                } else {
                    // SC: as far as I can tell, all stores (that have arguments) always
                    //   have the value being stored as the first argument
                    //   and everything after is address args
                    for &arg in func.dfg.inst_args(inst).iter().skip(1) { // skip the first argument
                        builder.add_edge_from_value_to_node(arg, inst_sink_node);
                    }
                };

            } else if op.is_branch() {
                // conditional branches are sinks

                let inst_sink_node = builder.add_sink_node_for_inst(inst);

                // blade only does conditional branches but this will handle indirect jumps as well
                // `inst_fixed_args` gets the condition args for branches,
                //   and ignores the destination block params (which are also included in args)
                for &arg in func.dfg.inst_fixed_args(inst) {
                    builder.add_edge_from_value_to_node(arg, inst_sink_node);
                }

            }
            if op.is_call() {
                // call instruction: must assume that the return value(s) could be a source
                for &result in func.dfg.inst_results(inst) {
                    builder.mark_as_source(result);
                }

                // and, to avoid interprocedural analysis, we require that
                // function arguments are stable, so we mark arguments to a call
                // as sinks
                let inst_sink_node = builder.add_sink_node_for_inst(inst);
                for &arg in func.dfg.inst_args(inst) {
                    builder.add_edge_from_value_to_node(arg, inst_sink_node);
                }
            }
        }
    }

    // we no longer mark function parameters as transient, since we require that
    // they are stable on the caller side (so this is commented)
    /*
    let entry_block = func
        .layout
        .entry_block()
        .expect("Failed to find entry block");
    for &func_param in func.dfg.block_params(entry_block) {
        // parameters of the entry block == parameters of the function
        builder.mark_as_source(func_param);
    }
    */

    // now add edges for actual data dependencies
    // for instance in the following pseudocode:
    //     x = load y
    //     z = x + 2
    //     branch on z
    // we have z -> sink and source -> x, but need x -> z yet
    let def_use_graph = DefUseGraph::for_function(func, cfg);
    for val in func.dfg.values() {
        let node = builder.bladenode_to_node_map[&BladeNode::ValueDef(val)]; // must exist
        for val_use in def_use_graph.uses_of_val(val) {
            match *val_use {
                ValueUse::Inst(inst_use) => {
                    // add an edge from val to the result of inst_use
                    // TODO this assumes that all results depend on all operands;
                    // are there any instructions where this is not the case for our purposes?
                    for &result in func.dfg.inst_results(inst_use) {
                        builder.add_edge_from_node_to_value(node, result);
                    }
                }
                ValueUse::Value(val_use) => {
                    // add an edge from val to val_use
                    builder.add_edge_from_node_to_value(node, val_use);
                }
            }
        }
    }

    builder.build()
}
