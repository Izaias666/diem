// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! The read/write set analysis is a compositional analysis that starts from the leaves of the
//! call graph and analyzes each procedure once. The result is a summary of the abstract paths
//! read/written by each procedure and the value(s) returned by the procedure.
//!
//! When the analysis encounters a call, it fetches the summary for the callee and applies it to the
//! current state. This logic (implemented in `apply_summary`) is by far the most complex part of the
//! analysis.

use crate::{
    access_path::{AbsAddr, AccessPath, Addr, FootprintDomain, Offset, Root},
    access_path_trie::AccessPathTrie,
    compositional_analysis::{CompositionalAnalysis, SummaryCache},
    dataflow_analysis::{AbstractDomain, DataflowAnalysis, JoinResult, TransferFunctions},
    function_target::{FunctionData, FunctionTarget},
    function_target_pipeline::{FunctionTargetProcessor, FunctionTargetsHolder, FunctionVariant},
    stackless_bytecode::{Bytecode, Constant, Operation},
};
use move_model::{
    ast::TempIndex,
    model::{FunctionEnv, GlobalEnv, ModuleId, StructId},
    ty::Type,
};
use std::{cmp::Ordering, fmt, fmt::Formatter};
use vm::file_format::CodeOffset;

// =================================================================================================
// Data Model

/// An access to local or global state
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Access {
    /// Not read or written; only accessed via a field borrow &, Vector::borrow, or borrow_global
    /// E.g., in *&x.f.g = 7, f is Borrow, g is Write
    Borrow,
    /// Read via RHS * or exists
    Read,
    /// Written via LHS *, move_to, or move_from
    Write,
    /// Could be read, written, or borrowed
    ReadWriteBorrow,
}

/// A record of the glocals and locals accessed by the current procedure + the address values stored
/// by locals or globals
#[derive(Debug, Clone, Eq, PartialOrd, PartialEq)]
struct ReadWriteSetState {
    /// memory accessed so far
    accesses: AccessPathTrie<Access>,
    /// mapping from locals to formal or global roots
    locals: AccessPathTrie<AbsAddr>,
}

// =================================================================================================
// Abstract Domain Operations

impl ReadWriteSetState {
    /// Aplly `callee_summary` to the caller state in `self`. There are three steps.
    /// 1. Substitute footprint values in the callee summary with their values in the caller state (including both actuals and values read from globals)
    /// 2. Bind return values in the callee summary to the return variables in the caller state
    /// 3. Join caller accesses and callee accesses
    pub fn apply_summary(
        &mut self, // caller state
        callee_summary_: &Self,
        actuals: &[TempIndex],
        type_actuals: &[Type],
        returns: &[TempIndex],
    ) {
        // TODO: refactor this to work without copies
        let callee_summary = callee_summary_.clone();
        let actual_values: Vec<AbsAddr> = actuals
            .iter()
            .map(|i| self.locals.get_local(*i).cloned().unwrap_or_default())
            .collect();
        // (1) bind all footprint values and types in callee locals to their caller values
        let mut new_callee_locals = callee_summary.locals.substitute_footprint(
            &actual_values,
            type_actuals,
            &self.locals,
            AbsAddr::substitute_footprint,
        );
        // (2) bind all footprint values and types in callee accesses to their caller values
        let mut new_callee_accesses = callee_summary.accesses.substitute_footprint_skip_data(
            &actual_values,
            type_actuals,
            &self.locals,
        );
        // (3) bind footprint paths in callee accesses with their caller values
        for (i, actual_v) in actual_values.iter().enumerate() {
            let formal_i = Root::Local(i);
            if let Some(node) = new_callee_accesses.0.remove(&formal_i) {
                let formal_ap = AccessPath::new(formal_i, vec![]);
                for v in formal_ap.prepend_addrs(actual_v).iter() {
                    match v {
                        Addr::Footprint(ap) => {
                            self.accesses.join_access_path(ap.clone(), node.clone())
                        }
                        Addr::Constant(c) => {
                            for (offset, child) in node.children().iter() {
                                match offset {
                                    Offset::Global(g) => {
                                        // create new root out of c/g, add c/g/child to summary
                                        self.accesses.join_access_path(
                                            AccessPath::new_global_constant(c.clone(), g.clone()),
                                            child.clone(),
                                        )
                                    }
                                    o => panic!("Bad offset type {:?} for address base", o),
                                }
                            }
                        }
                    }
                }
            }
        }
        // (4) bind return values in caller locals
        for (i, ret) in returns.iter().enumerate() {
            let retvar_i = Root::Return(i);
            if let Some(node) = new_callee_locals.0.remove(&retvar_i) {
                self.locals.bind_local_node(*ret, node)
            }
        }
        // (5) join caller and callee accesses
        // TODO: can we do a strong update here in some cases?
        self.accesses.join(&new_callee_accesses);
    }

    /// Copy the contents of `rhs_index` into `lhs_index`. Fails if `rhs_index` is not bound
    pub fn copy_local(&mut self, lhs_index: TempIndex, rhs_index: TempIndex) {
        let rhs_value = self
            .locals
            .get_local(rhs_index)
            .unwrap_or_else(|| panic!("Unbound local {:?}", rhs_index))
            .clone();
        self.locals.bind_local(lhs_index, rhs_value)
    }

    /// Return the local access paths rooted in `addr_idx`/`mid`::`sid`<`types`>
    fn get_global_paths(
        &self,
        addr_idx: TempIndex,
        mid: &ModuleId,
        sid: StructId,
        types: &[Type],
    ) -> Vec<AccessPath> {
        let mut acc = vec![];
        for v in self
            .locals
            .get_local(addr_idx)
            .unwrap_or_else(|| panic!("Untracked local {:?} of address type", addr_idx))
            .iter()
        {
            acc.push(v.clone().add_struct_offset(mid, sid, types.to_vec()))
        }
        acc
    }

    /// Remove the local access paths rooted `addr_idx`/`mid`::`sid`<`types`>
    pub fn remove_global(
        &mut self,
        addr_idx: TempIndex,
        mid: &ModuleId,
        sid: StructId,
        types: &[Type],
    ) {
        for ap in self.get_global_paths(addr_idx, mid, sid, types) {
            self.locals.update_access_path(ap, None)
        }
    }

    /// Record an access of type `access` to the path `local_idx`/`mid`::`sid`<`types`>
    fn add_global_access(
        &mut self,
        local_idx: TempIndex,
        mid: &ModuleId,
        sid: StructId,
        types: &[Type],
        access: Access,
    ) {
        for ap in self.get_global_paths(local_idx, mid, sid, types) {
            self.accesses.update_access_path_weak(ap, Some(access))
        }
    }

    /// Record an access of type `access` to the local variable `local_idx`
    fn record_access(&mut self, local_idx: TempIndex, access: Access) {
        for p in self
            .locals
            .get_local(local_idx)
            .expect("Unbound local")
            .iter()
        {
            if let Addr::Footprint(ap) = p {
                self.accesses.update_access_path(ap.clone(), Some(access))
            }
        }
    }

    /// Record an access of type `access_type` to the path `base`/`offset`
    pub fn access_offset(&mut self, base: TempIndex, offset: Offset, access_type: Access) {
        let borrowed = self.locals.get_local(base).expect("Unbound local").clone();
        let extended_aps = borrowed.add_offset(offset);
        for ap in extended_aps.footprint_paths() {
            self.accesses
                .update_access_path(ap.clone(), Some(access_type))
        }
    }

    /// Assign `ret` = `base`/`offset` and record an access of type `access_type` to `base`/`offset`
    pub fn assign_offset(
        &mut self,
        ret: TempIndex,
        base: TempIndex,
        offset: Offset,
        access_type: Access,
    ) {
        let borrowed = self.locals.get_local(base).expect("Unbound local").clone();
        let extended_aps = borrowed.add_offset(offset);
        for ap in extended_aps.footprint_paths() {
            self.locals
                .update_access_path(ap.clone(), Some(AbsAddr::footprint(ap.clone())));
            self.accesses
                .update_access_path(ap.clone(), Some(access_type))
        }
        self.locals.bind_local(ret, extended_aps)
    }

    /// Write rh
    pub fn write_ref(&mut self, lhs_ref: TempIndex, rhs: TempIndex) {
        if let Some(rhs_val) = self.locals.get_local(rhs).cloned() {
            let lhs_paths = self
                .locals
                .get_local(lhs_ref)
                .expect("Unbound local")
                .clone();
            for ap in lhs_paths.footprint_paths() {
                self.locals
                    .update_access_path(ap.clone(), Some(rhs_val.clone()))
            }
        }
    }

    /// Return a wrapper of `self` that implements `Display` using `env`
    pub fn display<'a>(&'a self, env: &'a FunctionTarget) -> ReadWriteSetStateDisplay<'a> {
        ReadWriteSetStateDisplay { state: self, env }
    }
}

// =================================================================================================
// Joins

impl AbstractDomain for ReadWriteSetState {
    fn join(&mut self, other: &Self) -> JoinResult {
        match (
            self.accesses.join(&other.accesses),
            self.locals.join(&other.locals),
        ) {
            (JoinResult::Unchanged, JoinResult::Unchanged) => JoinResult::Unchanged,
            _ => JoinResult::Changed,
        }
    }
}

impl FootprintDomain for Access {
    fn make_footprint(_ap: AccessPath) -> Option<Self> {
        None
    }
}

impl PartialOrd for Access {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self == other {
            return Some(Ordering::Equal);
        }
        match (self, other) {
            (Access::Borrow, _) => Some(Ordering::Less),
            (_, Access::Borrow) => Some(Ordering::Greater),
            (Access::ReadWriteBorrow, _) => Some(Ordering::Greater),
            (_, Access::ReadWriteBorrow) => Some(Ordering::Less),
            _ => None,
        }
    }
}

impl AbstractDomain for Access {
    fn join(&mut self, other: &Self) -> JoinResult {
        if self == other {
            return JoinResult::Unchanged;
        }
        let res = match (*self, *other) {
            (Access::Borrow, x) | (x, Access::Borrow) => x,
            _ => Access::ReadWriteBorrow,
        };
        *self = res;
        JoinResult::Changed
    }
}

// =================================================================================================
// Transfer functions

struct ReadWriteSetAnalysis<'a> {
    cache: SummaryCache<'a>,
}

impl<'a> TransferFunctions for ReadWriteSetAnalysis<'a> {
    type State = ReadWriteSetState;
    const BACKWARD: bool = false;

    fn execute(&self, state: &mut Self::State, instr: &Bytecode, _offset: CodeOffset) {
        use Bytecode::*;
        use Operation::*;

        match instr {
            Call(_, rets, oper, args, _abort_action) => match oper {
                BorrowField(_mid, _sid, _types, fld) => {
                    if state.locals.local_exists(args[0]) {
                        state.assign_offset(rets[0], args[0], Offset::field(*fld), Access::Borrow);
                    }
                }
                ReadRef => {
                    if state.locals.local_exists(args[0]) {
                        state.record_access(args[0], Access::Read);
                        // rets[0] = args[0]
                        state.copy_local(rets[0], args[0])
                    }
                }
                WriteRef => {
                    state.record_access(args[0], Access::Write);
                    // *args[0] = args1
                    state.write_ref(args[0], args[1])
                }
                FreezeRef | BorrowLoc => {
                    if state.locals.local_exists(args[0]) {
                        state.copy_local(rets[0], args[0])
                    }
                }
                BorrowGlobal(mid, sid, types) => {
                    state.add_global_access(args[0], mid, *sid, types, Access::Borrow);
                    // borrow_global<T>(a). bind ret to a/T
                    let addrs = state
                        .locals
                        .get_local(args[0])
                        .expect("Unbound address local")
                        .clone();
                    let offset = Offset::global(mid, *sid, types.clone());
                    let mut extended_aps: AbsAddr = AbsAddr::default();
                    for p in addrs.iter() {
                        match p {
                            Addr::Footprint(ap) => {
                                let mut extended_ap = ap.clone();
                                extended_ap.add_offset(offset.clone());
                                extended_aps.insert(Addr::Footprint(extended_ap.clone()));
                                state.locals.update_access_path(extended_ap.clone(), None);
                                state
                                    .accesses
                                    .update_access_path(extended_ap, Some(Access::Borrow))
                            }
                            Addr::Constant(c) => {
                                let extended_ap = AccessPath::new_address_constant(
                                    c.clone(),
                                    mid,
                                    *sid,
                                    types.clone(),
                                );
                                extended_aps.insert(Addr::footprint(extended_ap));
                            }
                        }
                    }
                    state.locals.bind_local(rets[0], extended_aps)
                }
                MoveFrom(mid, sid, types) => {
                    state.add_global_access(args[0], mid, *sid, types, Access::Write);
                    state.remove_global(args[0], mid, *sid, types)
                }
                MoveTo(mid, sid, types) => {
                    state.add_global_access(args[1], mid, *sid, types, Access::Write);
                }
                Exists(mid, sid, types) => {
                    state.add_global_access(args[0], mid, *sid, types, Access::Read)
                }
                Function(mid, fid, types) => {
                    let fun_id = mid.qualified(*fid);
                    let global_env = self.cache.global_env();
                    let callee_fun_env = global_env.get_function(fun_id);
                    if let Some(callee_summary) = self
                        .cache
                        .get::<ReadWriteSetState>(fun_id, &FunctionVariant::Baseline)
                    {
                        state.apply_summary(callee_summary, args, types, rets);
                    } else {
                        // native fun. use handwritten model
                        call_native_function(
                            state,
                            callee_fun_env.module_env.get_identifier().as_str(),
                            callee_fun_env.get_identifier().as_str(),
                            args,
                            rets,
                        )
                    }
                }
                Destroy => state.locals.remove_local(args[0]),
                Eq | Neq => {
                    // These operations read reference types passed to them. Add Access::Read's for both operands
                    if state.locals.local_exists(args[0]) {
                        state.record_access(args[0], Access::Read)
                    }
                    if state.locals.local_exists(args[1]) {
                        state.record_access(args[1], Access::Read)
                    }
                }
                Pack(_mid, _sid, _types) => {
                    // rets[0] = Pack<mid::sid<types>>(args)
                }
                Unpack(..) => {
                    // rets = Unpack<mid::sid<types>>(args[0])
                    // pack and unpack touch non-reference values; nothing to do
                }
                CastU8 | CastU64 | CastU128 | Not | Add | Sub | Mul | Div | Mod | BitOr
                | BitAnd | Xor | Shl | Shr | Lt | Gt | Le | Ge | Or | And => {
                    // These operations touch non-reference values; nothing to do
                }
                oper => panic!("unsupported oper {:?}", oper),
            },
            Load(_attr_id, lhs, constant) => {
                if let Constant::Address(a) = constant {
                    state.locals.bind_local(*lhs, AbsAddr::constant(a.clone()))
                }
            }
            Assign(_attr_id, lhs, rhs, _assign_kind) => {
                if let Some(rhs_data) = state.locals.get_local(*rhs).cloned() {
                    state.locals.bind_local(*lhs, rhs_data)
                } else {
                    state.locals.remove_local(*lhs)
                }
            }
            Ret(_attr_id, rets) => {
                let ret_vals: Vec<Option<AbsAddr>> = rets
                    .iter()
                    .map(|ret| state.locals.get_local(*ret).cloned())
                    .collect();
                for (ret_index, ret_val_opt) in ret_vals.iter().enumerate() {
                    if let Some(ret_val) = ret_val_opt {
                        state.locals.bind_return(ret_index, ret_val.clone())
                    }
                }
            }
            Abort(..) => {}
            SaveMem(..) | Prop(..) | SaveSpecVar(..) | Branch(..) | Jump(..) | Label(..)
            | SpecBlock(..) | Nop(..) => (),
        }
    }
}

/// Execute `rets` = call `module_name`::`function_name`(`args`) in `state`
fn call_native_function(
    state: &mut ReadWriteSetState,
    module_name: &str,
    fun_name: &str,
    args: &[TempIndex],
    rets: &[TempIndex],
) {
    // native fun. use handwritten model
    match (module_name, fun_name) {
        ("BCS", "to_bytes") => {
            if state.locals.local_exists(args[0]) {
                state.record_access(args[0], Access::Read)
            }
        }
        ("Signer", "borrow_address") => {
            if state.locals.local_exists(args[0]) {
                // treat as identity function
                state.record_access(args[0], Access::Borrow);
                state.copy_local(rets[0], args[0])
            }
        }
        ("Vector", "borrow_mut") | ("Vector", "borrow") => {
            if state.locals.local_exists(args[0]) {
                // this will look at vector length. record as read of an index
                state.access_offset(args[0], Offset::VectorIndex, Access::Read);
                state.assign_offset(rets[0], args[0], Offset::VectorIndex, Access::Borrow)
            }
        }
        ("Vector", "length") | ("Vector", "is_empty") => {
            if state.locals.local_exists(args[0]) {
                state.record_access(args[0], Access::Read)
            }
        }
        ("Vector", "pop_back") => {
            if state.locals.local_exists(args[0]) {
                // this will look at vector length. record as read of an index
                state.access_offset(args[0], Offset::VectorIndex, Access::Read);
                state.access_offset(args[0], Offset::VectorIndex, Access::Write);
                state.assign_offset(rets[0], args[0], Offset::VectorIndex, Access::Read)
            }
        }
        ("Vector", "push_back") | ("Vector", "append") | ("Vector", "swap") => {
            if state.locals.local_exists(args[0]) {
                // this will look at vector length. record as read of an index
                state.access_offset(args[0], Offset::VectorIndex, Access::Read);
                // writes an index (or several indexes)
                state.access_offset(args[0], Offset::VectorIndex, Access::Write);
            }
        }
        ("Vector", "contains") => {
            if state.locals.local_exists(args[0]) {
                state.record_access(args[0], Access::Read); // reads the length + contents
            }
        }
        ("DiemAccount", "create_signer") => {
            if state.locals.local_exists(args[0]) {
                state.record_access(args[0], Access::Read); // reads the input address
                                                            // treat as assignment
                state.copy_local(rets[0], args[0])
            }
        }
        ("Vector", "empty") | ("Vector", "destroy_empty") => (),
        ("Event", "write_to_event_store") => (),
        ("Hash", "sha3_256") | ("Hash", "sha2_256") => (),
        ("Signature", "ed25519_validate_pubkey") | ("Signature", "ed25519_verify") => (),
        (m, f) => {
            unimplemented!("Unsupported native function {:?}::{:?}", m, f)
        }
    }
}

impl<'a> DataflowAnalysis for ReadWriteSetAnalysis<'a> {}
impl<'a> CompositionalAnalysis<ReadWriteSetState> for ReadWriteSetAnalysis<'a> {
    fn to_summary(&self, mut state: Self::State, fun_target: &FunctionTarget) -> ReadWriteSetState {
        // remove locals to keep summary compact
        for i in fun_target.get_non_parameter_locals() {
            state.locals.remove_local(i)
        }
        // remove locals with no offsets
        for i in fun_target.get_parameters() {
            if let Some(node) = state.locals.get_local_node(i) {
                if node.children().is_empty() {
                    state.locals.remove_local(i)
                }
            }
        }
        // TODO: if the data associated with path P is Footprint(P), remove it

        state
    }
}
pub struct ReadWriteSetProcessor();
impl ReadWriteSetProcessor {
    pub fn new() -> Box<Self> {
        Box::new(ReadWriteSetProcessor())
    }
}

impl FunctionTargetProcessor for ReadWriteSetProcessor {
    fn process(
        &self,
        targets: &mut FunctionTargetsHolder,
        func_env: &FunctionEnv<'_>,
        data: FunctionData,
    ) -> FunctionData {
        let fun_target = FunctionTarget::new(func_env, &data);
        let mut initial_state = ReadWriteSetState::default();
        // initialize_formals
        for param_index in fun_target.get_parameters() {
            initial_state
                .locals
                .bind_local(param_index, AbsAddr::formal(param_index))
        }
        let cache = SummaryCache::new(targets, func_env.module_env.env);
        let analysis = ReadWriteSetAnalysis { cache };
        analysis.summarize(func_env, initial_state, data)
    }

    fn name(&self) -> String {
        "read_write_set_analysis".to_string()
    }
}

// =================================================================================================
// Entrypoint for clients

pub fn get_read_write_set(env: &GlobalEnv, targets: &FunctionTargetsHolder) {
    for module_env in env.get_modules() {
        let module_name = module_env.get_identifier().to_string();
        for func_env in module_env.get_functions() {
            let fun_target = targets.get_target(&func_env, &FunctionVariant::Baseline);
            let annotation = fun_target
                .get_annotations()
                .get::<ReadWriteSetState>()
                .expect(
                "Invariant violation: read/write set analysis should be run before calling this",
            );
            println!("{}::{}", module_name, func_env.get_identifier());
            println!("{}", annotation.display(&fun_target))
        }
    }
}

// =================================================================================================
// Formatting

/// Return a string representation of the summary for `target`
pub fn format_read_write_set_annotation(
    target: &FunctionTarget<'_>,
    code_offset: CodeOffset,
) -> Option<String> {
    // hack: the summary only contains the state at the exit block, but the
    // caller of this function wants to print at every `code_offset`. This
    // allows us to only print once/function
    // TODO: change printing interface to allow optional per-procedure and per-bytecode printing
    if code_offset != 0 {
        return None;
    }
    if let Some(a) = target.get_annotations().get::<ReadWriteSetState>() {
        Some(format!("{}", a.display(target)))
    } else {
        None
    }
}

struct ReadWriteSetStateDisplay<'a> {
    state: &'a ReadWriteSetState,
    env: &'a FunctionTarget<'a>,
}

impl<'a> fmt::Display for ReadWriteSetStateDisplay<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("Accesses:\n")?;
        writeln!(f, "{}", self.state.accesses.display(self.env))?;
        f.write_str("Locals:\n")?;
        self.state.locals.iter_paths(|path, v| {
            writeln!(f, "{}: {}", path.display(self.env), v.display(self.env)).unwrap();
        });
        Ok(())
    }
}

impl Default for ReadWriteSetState {
    fn default() -> Self {
        Self {
            accesses: AccessPathTrie::default(),
            locals: AccessPathTrie::default(),
        }
    }
}
