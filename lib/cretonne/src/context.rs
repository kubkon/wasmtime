//! Cretonne compilation context and main entry point.
//!
//! When compiling many small functions, it is important to avoid repeatedly allocating and
//! deallocating the data structures needed for compilation. The `Context` struct is used to hold
//! on to memory allocations between function compilations.
//!
//! The context does not hold a `TargetIsa` instance which has to be provided as an argument
//! instead. This is because an ISA instance is immutable and can be used by multiple compilation
//! contexts concurrently. Typically, you would have one context per compilation thread and only a
//! single ISA instance.

use binemit::{CodeOffset, relax_branches, MemoryCodeSink, RelocSink};
use dominator_tree::DominatorTree;
use flowgraph::ControlFlowGraph;
use ir::Function;
use loop_analysis::LoopAnalysis;
use isa::TargetIsa;
use legalize_function;
use regalloc;
use result::{CtonError, CtonResult};
use settings::{FlagsOrIsa, OptLevel};
use unreachable_code::eliminate_unreachable_code;
use verifier;
use simple_gvn::do_simple_gvn;
use licm::do_licm;
use preopt::do_preopt;
use timing;

/// Persistent data structures and compilation pipeline.
pub struct Context {
    /// The function we're compiling.
    pub func: Function,

    /// The control flow graph of `func`.
    pub cfg: ControlFlowGraph,

    /// Dominator tree for `func`.
    pub domtree: DominatorTree,

    /// Register allocation context.
    pub regalloc: regalloc::Context,

    /// Loop analysis of `func`.
    pub loop_analysis: LoopAnalysis,
}

impl Context {
    /// Allocate a new compilation context.
    ///
    /// The returned instance should be reused for compiling multiple functions in order to avoid
    /// needless allocator thrashing.
    pub fn new() -> Self {
        Context::for_function(Function::new())
    }

    /// Allocate a new compilation context with an existing Function.
    ///
    /// The returned instance should be reused for compiling multiple functions in order to avoid
    /// needless allocator thrashing.
    pub fn for_function(func: Function) -> Self {
        Self {
            func: func,
            cfg: ControlFlowGraph::new(),
            domtree: DominatorTree::new(),
            regalloc: regalloc::Context::new(),
            loop_analysis: LoopAnalysis::new(),
        }
    }

    /// Clear all data structures in this context.
    pub fn clear(&mut self) {
        self.func.clear();
        self.cfg.clear();
        self.domtree.clear();
        self.regalloc.clear();
        self.loop_analysis.clear();
    }

    /// Compile the function.
    ///
    /// Run the function through all the passes necessary to generate code for the target ISA
    /// represented by `isa`. This does not include the final step of emitting machine code into a
    /// code sink.
    ///
    /// Returns the size of the function's code.
    pub fn compile(&mut self, isa: &TargetIsa) -> Result<CodeOffset, CtonError> {
        let _tt = timing::compile();
        self.verify_if(isa)?;

        self.compute_cfg();
        self.preopt(isa)?;
        self.legalize(isa)?;
        if isa.flags().opt_level() == OptLevel::Best {
            self.compute_domtree();
            self.compute_loop_analysis();
            self.licm(isa)?;
            self.simple_gvn(isa)?;
        }
        self.compute_domtree();
        self.eliminate_unreachable_code(isa)?;
        self.regalloc(isa)?;
        self.prologue_epilogue(isa)?;
        self.relax_branches(isa)
    }

    /// Emit machine code directly into raw memory.
    ///
    /// Write all of the function's machine code to the memory at `mem`. The size of the machine
    /// code is returned by `compile` above.
    ///
    /// The machine code is not relocated. Instead, any relocations are emitted into `relocs`.
    pub fn emit_to_memory(&self, mem: *mut u8, relocs: &mut RelocSink, isa: &TargetIsa) {
        let _tt = timing::binemit();
        isa.emit_function(&self.func, &mut MemoryCodeSink::new(mem, relocs));
    }

    /// Run the verifier on the function.
    ///
    /// Also check that the dominator tree and control flow graph are consistent with the function.
    pub fn verify<'a, FOI: Into<FlagsOrIsa<'a>>>(&self, fisa: FOI) -> verifier::Result {
        verifier::verify_context(&self.func, &self.cfg, &self.domtree, fisa)
    }

    /// Run the verifier only if the `enable_verifier` setting is true.
    pub fn verify_if<'a, FOI: Into<FlagsOrIsa<'a>>>(&self, fisa: FOI) -> CtonResult {
        let fisa = fisa.into();
        if fisa.flags.enable_verifier() {
            self.verify(fisa).map_err(Into::into)
        } else {
            Ok(())
        }
    }

    /// Run the locations verifier on the function.
    pub fn verify_locations<'a>(&self, isa: &TargetIsa) -> verifier::Result {
        verifier::verify_locations(isa, &self.func, None)
    }

    /// Run the locations verifier only if the `enable_verifier` setting is true.
    pub fn verify_locations_if<'a>(&self, isa: &TargetIsa) -> CtonResult {
        if isa.flags().enable_verifier() {
            self.verify_locations(isa).map_err(Into::into)
        } else {
            Ok(())
        }
    }

    /// Perform pre-legalization rewrites on the function.
    pub fn preopt(&mut self, isa: &TargetIsa) -> CtonResult {
        do_preopt(&mut self.func);
        self.verify_if(isa)?;
        Ok(())
    }

    /// Run the legalizer for `isa` on the function.
    pub fn legalize(&mut self, isa: &TargetIsa) -> CtonResult {
        // Legalization invalidates the domtree and loop_analysis by mutating the CFG.
        // TODO: Avoid doing this when legalization doesn't actually mutate the CFG.
        self.domtree.clear();
        self.loop_analysis.clear();
        legalize_function(&mut self.func, &mut self.cfg, isa);
        self.verify_if(isa)
    }

    /// Compute the control flow graph.
    pub fn compute_cfg(&mut self) {
        self.cfg.compute(&self.func)
    }

    /// Compute dominator tree.
    pub fn compute_domtree(&mut self) {
        self.domtree.compute(&self.func, &self.cfg)
    }

    /// Compute the loop analysis.
    pub fn compute_loop_analysis(&mut self) {
        self.loop_analysis.compute(
            &self.func,
            &self.cfg,
            &self.domtree,
        )
    }

    /// Compute the control flow graph and dominator tree.
    pub fn flowgraph(&mut self) {
        self.compute_cfg();
        self.compute_domtree()
    }

    /// Perform simple GVN on the function.
    pub fn simple_gvn<'a, FOI: Into<FlagsOrIsa<'a>>>(&mut self, fisa: FOI) -> CtonResult {
        do_simple_gvn(&mut self.func, &mut self.cfg, &mut self.domtree);
        self.verify_if(fisa)
    }

    /// Perform LICM on the function.
    pub fn licm<'a, FOI: Into<FlagsOrIsa<'a>>>(&mut self, fisa: FOI) -> CtonResult {
        do_licm(
            &mut self.func,
            &mut self.cfg,
            &mut self.domtree,
            &mut self.loop_analysis,
        );
        self.verify_if(fisa)
    }

    /// Perform unreachable code elimination.
    pub fn eliminate_unreachable_code<'a, FOI>(&mut self, fisa: FOI) -> CtonResult
    where
        FOI: Into<FlagsOrIsa<'a>>,
    {
        eliminate_unreachable_code(&mut self.func, &mut self.cfg, &self.domtree);
        self.verify_if(fisa)
    }

    /// Run the register allocator.
    pub fn regalloc(&mut self, isa: &TargetIsa) -> CtonResult {
        self.regalloc.run(
            isa,
            &mut self.func,
            &self.cfg,
            &mut self.domtree,
        )
    }

    /// Insert prologue and epilogues after computing the stack frame layout.
    pub fn prologue_epilogue(&mut self, isa: &TargetIsa) -> CtonResult {
        isa.prologue_epilogue(&mut self.func)?;
        self.verify_if(isa)?;
        self.verify_locations_if(isa)?;
        Ok(())
    }

    /// Run the branch relaxation pass and return the final code size.
    pub fn relax_branches(&mut self, isa: &TargetIsa) -> Result<CodeOffset, CtonError> {
        let code_size = relax_branches(&mut self.func, isa)?;
        self.verify_if(isa)?;
        self.verify_locations_if(isa)?;

        Ok(code_size)
    }
}
