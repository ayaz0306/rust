// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::gather_moves::{HasMoveData, MoveData, MovePathIndex, LookupResult};
use super::dataflow::{MaybeInitializedLvals, MaybeUninitializedLvals};
use super::dataflow::{DataflowResults};
use super::{on_all_children_bits, on_all_drop_children_bits};
use super::{drop_flag_effects_for_location, on_lookup_result_bits};
use super::MoveDataParamEnv;
use rustc::ty::{self, TyCtxt};
use rustc::mir::*;
use rustc::mir::transform::{MirPass, MirSource};
use rustc::middle::const_val::ConstVal;
use rustc::util::nodemap::FxHashMap;
use rustc_data_structures::indexed_set::IdxSetBuf;
use rustc_data_structures::indexed_vec::Idx;
use rustc_mir::util::patch::MirPatch;
use rustc_mir::util::elaborate_drops::{DropFlagState, Unwind, elaborate_drop};
use rustc_mir::util::elaborate_drops::{DropElaborator, DropStyle, DropFlagMode};
use syntax::ast;
use syntax_pos::Span;

use std::fmt;
use std::u32;

pub struct ElaborateDrops;

impl MirPass for ElaborateDrops {
    fn run_pass<'a, 'tcx>(&self,
                          tcx: TyCtxt<'a, 'tcx, 'tcx>,
                          src: MirSource,
                          mir: &mut Mir<'tcx>)
    {
        debug!("elaborate_drops({:?} @ {:?})", src, mir.span);
        match src {
            MirSource::Fn(..) => {},
            _ => return
        }
        let id = src.item_id();
        let param_env = tcx.param_env(tcx.hir.local_def_id(id));
        let move_data = MoveData::gather_moves(mir, tcx, param_env);
        let elaborate_patch = {
            let mir = &*mir;
            let env = MoveDataParamEnv {
                move_data: move_data,
                param_env: param_env
            };
            let dead_unwinds = find_dead_unwinds(tcx, mir, id, &env);
            let flow_inits =
                super::do_dataflow(tcx, mir, id, &[], &dead_unwinds,
                                   MaybeInitializedLvals::new(tcx, mir, &env),
                                   |bd, p| &bd.move_data().move_paths[p]);
            let flow_uninits =
                super::do_dataflow(tcx, mir, id, &[], &dead_unwinds,
                                   MaybeUninitializedLvals::new(tcx, mir, &env),
                                   |bd, p| &bd.move_data().move_paths[p]);

            ElaborateDropsCtxt {
                tcx: tcx,
                mir: mir,
                env: &env,
                flow_inits: flow_inits,
                flow_uninits: flow_uninits,
                drop_flags: FxHashMap(),
                patch: MirPatch::new(mir),
            }.elaborate()
        };
        elaborate_patch.apply(mir);
    }
}

/// Return the set of basic blocks whose unwind edges are known
/// to not be reachable, because they are `drop` terminators
/// that can't drop anything.
fn find_dead_unwinds<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    mir: &Mir<'tcx>,
    id: ast::NodeId,
    env: &MoveDataParamEnv<'tcx>)
    -> IdxSetBuf<BasicBlock>
{
    debug!("find_dead_unwinds({:?})", mir.span);
    // We only need to do this pass once, because unwind edges can only
    // reach cleanup blocks, which can't have unwind edges themselves.
    let mut dead_unwinds = IdxSetBuf::new_empty(mir.basic_blocks().len());
    let flow_inits =
        super::do_dataflow(tcx, mir, id, &[], &dead_unwinds,
                           MaybeInitializedLvals::new(tcx, mir, &env),
                           |bd, p| &bd.move_data().move_paths[p]);
    for (bb, bb_data) in mir.basic_blocks().iter_enumerated() {
        match bb_data.terminator().kind {
            TerminatorKind::Drop { ref location, unwind: Some(_), .. } |
            TerminatorKind::DropAndReplace { ref location, unwind: Some(_), .. } => {
                let mut init_data = InitializationData {
                    live: flow_inits.sets().on_entry_set_for(bb.index()).to_owned(),
                    dead: IdxSetBuf::new_empty(env.move_data.move_paths.len()),
                };
                debug!("find_dead_unwinds @ {:?}: {:?}; init_data={:?}",
                       bb, bb_data, init_data.live);
                for stmt in 0..bb_data.statements.len() {
                    let loc = Location { block: bb, statement_index: stmt };
                    init_data.apply_location(tcx, mir, env, loc);
                }

                let path = match env.move_data.rev_lookup.find(location) {
                    LookupResult::Exact(e) => e,
                    LookupResult::Parent(..) => {
                        debug!("find_dead_unwinds: has parent; skipping");
                        continue
                    }
                };

                debug!("find_dead_unwinds @ {:?}: path({:?})={:?}", bb, location, path);

                let mut maybe_live = false;
                on_all_drop_children_bits(tcx, mir, &env, path, |child| {
                    let (child_maybe_live, _) = init_data.state(child);
                    maybe_live |= child_maybe_live;
                });

                debug!("find_dead_unwinds @ {:?}: maybe_live={}", bb, maybe_live);
                if !maybe_live {
                    dead_unwinds.add(&bb);
                }
            }
            _ => {}
        }
    }

    dead_unwinds
}

struct InitializationData {
    live: IdxSetBuf<MovePathIndex>,
    dead: IdxSetBuf<MovePathIndex>
}

impl InitializationData {
    fn apply_location<'a,'tcx>(&mut self,
                               tcx: TyCtxt<'a, 'tcx, 'tcx>,
                               mir: &Mir<'tcx>,
                               env: &MoveDataParamEnv<'tcx>,
                               loc: Location)
    {
        drop_flag_effects_for_location(tcx, mir, env, loc, |path, df| {
            debug!("at location {:?}: setting {:?} to {:?}",
                   loc, path, df);
            match df {
                DropFlagState::Present => {
                    self.live.add(&path);
                    self.dead.remove(&path);
                }
                DropFlagState::Absent => {
                    self.dead.add(&path);
                    self.live.remove(&path);
                }
            }
        });
    }

    fn state(&self, path: MovePathIndex) -> (bool, bool) {
        (self.live.contains(&path), self.dead.contains(&path))
    }
}

struct Elaborator<'a, 'b: 'a, 'tcx: 'b> {
    init_data: &'a InitializationData,
    ctxt: &'a mut ElaborateDropsCtxt<'b, 'tcx>,
}

impl<'a, 'b, 'tcx> fmt::Debug for Elaborator<'a, 'b, 'tcx> {
    fn fmt(&self, _f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        Ok(())
    }
}

impl<'a, 'b, 'tcx> DropElaborator<'a, 'tcx> for Elaborator<'a, 'b, 'tcx> {
    type Path = MovePathIndex;

    fn patch(&mut self) -> &mut MirPatch<'tcx> {
        &mut self.ctxt.patch
    }

    fn mir(&self) -> &'a Mir<'tcx> {
        self.ctxt.mir
    }

    fn tcx(&self) -> ty::TyCtxt<'a, 'tcx, 'tcx> {
        self.ctxt.tcx
    }

    fn param_env(&self) -> ty::ParamEnv<'tcx> {
        self.ctxt.param_env()
    }

    fn drop_style(&self, path: Self::Path, mode: DropFlagMode) -> DropStyle {
        let ((maybe_live, maybe_dead), multipart) = match mode {
            DropFlagMode::Shallow => (self.init_data.state(path), false),
            DropFlagMode::Deep => {
                let mut some_live = false;
                let mut some_dead = false;
                let mut children_count = 0;
                on_all_drop_children_bits(
                    self.tcx(), self.mir(), self.ctxt.env, path, |child| {
                        let (live, dead) = self.init_data.state(child);
                        debug!("elaborate_drop: state({:?}) = {:?}",
                               child, (live, dead));
                        some_live |= live;
                        some_dead |= dead;
                        children_count += 1;
                    });
                ((some_live, some_dead), children_count != 1)
            }
        };
        match (maybe_live, maybe_dead, multipart) {
            (false, _, _) => DropStyle::Dead,
            (true, false, _) => DropStyle::Static,
            (true, true, false) => DropStyle::Conditional,
            (true, true, true) => DropStyle::Open,
        }
    }

    fn clear_drop_flag(&mut self, loc: Location, path: Self::Path, mode: DropFlagMode) {
        match mode {
            DropFlagMode::Shallow => {
                self.ctxt.set_drop_flag(loc, path, DropFlagState::Absent);
            }
            DropFlagMode::Deep => {
                on_all_children_bits(
                    self.tcx(), self.mir(), self.ctxt.move_data(), path,
                    |child| self.ctxt.set_drop_flag(loc, child, DropFlagState::Absent)
                 );
            }
        }
    }

    fn field_subpath(&self, path: Self::Path, field: Field) -> Option<Self::Path> {
        super::move_path_children_matching(self.ctxt.move_data(), path, |p| {
            match p {
                &Projection {
                    elem: ProjectionElem::Field(idx, _), ..
                } => idx == field,
                _ => false
            }
        })
    }

    fn deref_subpath(&self, path: Self::Path) -> Option<Self::Path> {
        super::move_path_children_matching(self.ctxt.move_data(), path, |p| {
            match p {
                &Projection { elem: ProjectionElem::Deref, .. } => true,
                _ => false
            }
        })
    }

    fn downcast_subpath(&self, path: Self::Path, variant: usize) -> Option<Self::Path> {
        super::move_path_children_matching(self.ctxt.move_data(), path, |p| {
            match p {
                &Projection {
                    elem: ProjectionElem::Downcast(_, idx), ..
                } => idx == variant,
                _ => false
            }
        })
    }

    fn get_drop_flag(&mut self, path: Self::Path) -> Option<Operand<'tcx>> {
        self.ctxt.drop_flag(path).map(Operand::Consume)
    }
}

struct ElaborateDropsCtxt<'a, 'tcx: 'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    mir: &'a Mir<'tcx>,
    env: &'a MoveDataParamEnv<'tcx>,
    flow_inits: DataflowResults<MaybeInitializedLvals<'a, 'tcx>>,
    flow_uninits:  DataflowResults<MaybeUninitializedLvals<'a, 'tcx>>,
    drop_flags: FxHashMap<MovePathIndex, Local>,
    patch: MirPatch<'tcx>,
}

impl<'b, 'tcx> ElaborateDropsCtxt<'b, 'tcx> {
    fn move_data(&self) -> &'b MoveData<'tcx> { &self.env.move_data }

    fn param_env(&self) -> ty::ParamEnv<'tcx> {
        self.env.param_env
    }

    fn initialization_data_at(&self, loc: Location) -> InitializationData {
        let mut data = InitializationData {
            live: self.flow_inits.sets().on_entry_set_for(loc.block.index())
                .to_owned(),
            dead: self.flow_uninits.sets().on_entry_set_for(loc.block.index())
                .to_owned(),
        };
        for stmt in 0..loc.statement_index {
            data.apply_location(self.tcx, self.mir, self.env,
                                Location { block: loc.block, statement_index: stmt });
        }
        data
    }

    fn create_drop_flag(&mut self, index: MovePathIndex, span: Span) {
        let tcx = self.tcx;
        let patch = &mut self.patch;
        debug!("create_drop_flag({:?})", self.mir.span);
        self.drop_flags.entry(index).or_insert_with(|| {
            patch.new_temp(tcx.types.bool, span)
        });
    }

    fn drop_flag(&mut self, index: MovePathIndex) -> Option<Lvalue<'tcx>> {
        self.drop_flags.get(&index).map(|t| Lvalue::Local(*t))
    }

    /// create a patch that elaborates all drops in the input
    /// MIR.
    fn elaborate(mut self) -> MirPatch<'tcx>
    {
        self.collect_drop_flags();

        self.elaborate_drops();

        self.drop_flags_on_init();
        self.drop_flags_for_fn_rets();
        self.drop_flags_for_args();
        self.drop_flags_for_locs();

        self.patch
    }

    fn collect_drop_flags(&mut self)
    {
        for (bb, data) in self.mir.basic_blocks().iter_enumerated() {
            let terminator = data.terminator();
            let location = match terminator.kind {
                TerminatorKind::Drop { ref location, .. } |
                TerminatorKind::DropAndReplace { ref location, .. } => location,
                _ => continue
            };

            let init_data = self.initialization_data_at(Location {
                block: bb,
                statement_index: data.statements.len()
            });

            let path = self.move_data().rev_lookup.find(location);
            debug!("collect_drop_flags: {:?}, lv {:?} ({:?})",
                   bb, location, path);

            let path = match path {
                LookupResult::Exact(e) => e,
                LookupResult::Parent(None) => continue,
                LookupResult::Parent(Some(parent)) => {
                    let (_maybe_live, maybe_dead) = init_data.state(parent);
                    if maybe_dead {
                        span_bug!(terminator.source_info.span,
                                  "drop of untracked, uninitialized value {:?}, lv {:?} ({:?})",
                                  bb, location, path);
                    }
                    continue
                }
            };

            on_all_drop_children_bits(self.tcx, self.mir, self.env, path, |child| {
                let (maybe_live, maybe_dead) = init_data.state(child);
                debug!("collect_drop_flags: collecting {:?} from {:?}@{:?} - {:?}",
                       child, location, path, (maybe_live, maybe_dead));
                if maybe_live && maybe_dead {
                    self.create_drop_flag(child, terminator.source_info.span)
                }
            });
        }
    }

    fn elaborate_drops(&mut self)
    {
        for (bb, data) in self.mir.basic_blocks().iter_enumerated() {
            let loc = Location { block: bb, statement_index: data.statements.len() };
            let terminator = data.terminator();

            let resume_block = self.patch.resume_block();
            match terminator.kind {
                TerminatorKind::Drop { ref location, target, unwind } => {
                    let init_data = self.initialization_data_at(loc);
                    match self.move_data().rev_lookup.find(location) {
                        LookupResult::Exact(path) => {
                            elaborate_drop(
                                &mut Elaborator {
                                    init_data: &init_data,
                                    ctxt: self
                                },
                                terminator.source_info,
                                location,
                                path,
                                target,
                                if data.is_cleanup {
                                    Unwind::InCleanup
                                } else {
                                    Unwind::To(Option::unwrap_or(unwind, resume_block))
                                },
                                bb)
                        }
                        LookupResult::Parent(..) => {
                            span_bug!(terminator.source_info.span,
                                      "drop of untracked value {:?}", bb);
                        }
                    }
                }
                TerminatorKind::DropAndReplace { ref location, ref value,
                                                 target, unwind } =>
                {
                    assert!(!data.is_cleanup);

                    self.elaborate_replace(
                        loc,
                        location, value,
                        target, unwind
                    );
                }
                _ => continue
            }
        }
    }

    /// Elaborate a MIR `replace` terminator. This instruction
    /// is not directly handled by translation, and therefore
    /// must be desugared.
    ///
    /// The desugaring drops the location if needed, and then writes
    /// the value (including setting the drop flag) over it in *both* arms.
    ///
    /// The `replace` terminator can also be called on lvalues that
    /// are not tracked by elaboration (for example,
    /// `replace x[i] <- tmp0`). The borrow checker requires that
    /// these locations are initialized before the assignment,
    /// so we just generate an unconditional drop.
    fn elaborate_replace(
        &mut self,
        loc: Location,
        location: &Lvalue<'tcx>,
        value: &Operand<'tcx>,
        target: BasicBlock,
        unwind: Option<BasicBlock>)
    {
        let bb = loc.block;
        let data = &self.mir[bb];
        let terminator = data.terminator();
        assert!(!data.is_cleanup, "DropAndReplace in unwind path not supported");

        let assign = Statement {
            kind: StatementKind::Assign(location.clone(), Rvalue::Use(value.clone())),
            source_info: terminator.source_info
        };

        let unwind = unwind.unwrap_or(self.patch.resume_block());
        let unwind = self.patch.new_block(BasicBlockData {
            statements: vec![assign.clone()],
            terminator: Some(Terminator {
                kind: TerminatorKind::Goto { target: unwind },
                ..*terminator
            }),
            is_cleanup: true
        });

        let target = self.patch.new_block(BasicBlockData {
            statements: vec![assign],
            terminator: Some(Terminator {
                kind: TerminatorKind::Goto { target: target },
                ..*terminator
            }),
            is_cleanup: false,
        });

        match self.move_data().rev_lookup.find(location) {
            LookupResult::Exact(path) => {
                debug!("elaborate_drop_and_replace({:?}) - tracked {:?}", terminator, path);
                let init_data = self.initialization_data_at(loc);

                elaborate_drop(
                    &mut Elaborator {
                        init_data: &init_data,
                        ctxt: self
                    },
                    terminator.source_info,
                    location,
                    path,
                    target,
                    Unwind::To(unwind),
                    bb);
                on_all_children_bits(self.tcx, self.mir, self.move_data(), path, |child| {
                    self.set_drop_flag(Location { block: target, statement_index: 0 },
                                       child, DropFlagState::Present);
                    self.set_drop_flag(Location { block: unwind, statement_index: 0 },
                                       child, DropFlagState::Present);
                });
            }
            LookupResult::Parent(parent) => {
                // drop and replace behind a pointer/array/whatever. The location
                // must be initialized.
                debug!("elaborate_drop_and_replace({:?}) - untracked {:?}", terminator, parent);
                self.patch.patch_terminator(bb, TerminatorKind::Drop {
                    location: location.clone(),
                    target: target,
                    unwind: Some(unwind)
                });
            }
        }
    }

    fn constant_bool(&self, span: Span, val: bool) -> Rvalue<'tcx> {
        Rvalue::Use(Operand::Constant(Box::new(Constant {
            span: span,
            ty: self.tcx.types.bool,
            literal: Literal::Value { value: ConstVal::Bool(val) }
        })))
    }

    fn set_drop_flag(&mut self, loc: Location, path: MovePathIndex, val: DropFlagState) {
        if let Some(&flag) = self.drop_flags.get(&path) {
            let span = self.patch.source_info_for_location(self.mir, loc).span;
            let val = self.constant_bool(span, val.value());
            self.patch.add_assign(loc, Lvalue::Local(flag), val);
        }
    }

    fn drop_flags_on_init(&mut self) {
        let loc = Location { block: START_BLOCK, statement_index: 0 };
        let span = self.patch.source_info_for_location(self.mir, loc).span;
        let false_ = self.constant_bool(span, false);
        for flag in self.drop_flags.values() {
            self.patch.add_assign(loc, Lvalue::Local(*flag), false_.clone());
        }
    }

    fn drop_flags_for_fn_rets(&mut self) {
        for (bb, data) in self.mir.basic_blocks().iter_enumerated() {
            if let TerminatorKind::Call {
                destination: Some((ref lv, tgt)), cleanup: Some(_), ..
            } = data.terminator().kind {
                assert!(!self.patch.is_patched(bb));

                let loc = Location { block: tgt, statement_index: 0 };
                let path = self.move_data().rev_lookup.find(lv);
                on_lookup_result_bits(
                    self.tcx, self.mir, self.move_data(), path,
                    |child| self.set_drop_flag(loc, child, DropFlagState::Present)
                );
            }
        }
    }

    fn drop_flags_for_args(&mut self) {
        let loc = Location { block: START_BLOCK, statement_index: 0 };
        super::drop_flag_effects_for_function_entry(
            self.tcx, self.mir, self.env, |path, ds| {
                self.set_drop_flag(loc, path, ds);
            }
        )
    }

    fn drop_flags_for_locs(&mut self) {
        // We intentionally iterate only over the *old* basic blocks.
        //
        // Basic blocks created by drop elaboration update their
        // drop flags by themselves, to avoid the drop flags being
        // clobbered before they are read.

        for (bb, data) in self.mir.basic_blocks().iter_enumerated() {
            debug!("drop_flags_for_locs({:?})", data);
            for i in 0..(data.statements.len()+1) {
                debug!("drop_flag_for_locs: stmt {}", i);
                let mut allow_initializations = true;
                if i == data.statements.len() {
                    match data.terminator().kind {
                        TerminatorKind::Drop { .. } => {
                            // drop elaboration should handle that by itself
                            continue
                        }
                        TerminatorKind::DropAndReplace { .. } => {
                            // this contains the move of the source and
                            // the initialization of the destination. We
                            // only want the former - the latter is handled
                            // by the elaboration code and must be done
                            // *after* the destination is dropped.
                            assert!(self.patch.is_patched(bb));
                            allow_initializations = false;
                        }
                        _ => {
                            assert!(!self.patch.is_patched(bb));
                        }
                    }
                }
                let loc = Location { block: bb, statement_index: i };
                super::drop_flag_effects_for_location(
                    self.tcx, self.mir, self.env, loc, |path, ds| {
                        if ds == DropFlagState::Absent || allow_initializations {
                            self.set_drop_flag(loc, path, ds)
                        }
                    }
                )
            }

            // There may be a critical edge after this call,
            // so mark the return as initialized *before* the
            // call.
            if let TerminatorKind::Call {
                destination: Some((ref lv, _)), cleanup: None, ..
            } = data.terminator().kind {
                assert!(!self.patch.is_patched(bb));

                let loc = Location { block: bb, statement_index: data.statements.len() };
                let path = self.move_data().rev_lookup.find(lv);
                on_lookup_result_bits(
                    self.tcx, self.mir, self.move_data(), path,
                    |child| self.set_drop_flag(loc, child, DropFlagState::Present)
                );
            }
        }
    }
}
