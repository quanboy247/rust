use rustc_data_structures::fx::FxHashSet;
use rustc_index::bit_set::BitSet;
use rustc_index::IndexVec;
use rustc_middle::mir::visit::*;
use rustc_middle::mir::*;
use rustc_middle::ty::TyCtxt;
use rustc_mir_dataflow::impls::MaybeStorageDead;
use rustc_mir_dataflow::storage::always_storage_live_locals;
use rustc_mir_dataflow::Analysis;

use crate::ssa::{SsaLocals, StorageLiveLocals};
use crate::MirPass;

/// Propagate references using SSA analysis.
///
/// MIR building may produce a lot of borrow-dereference patterns.
///
/// This pass aims to transform the following pattern:
///   _1 = &raw? mut? PLACE;
///   _3 = *_1;
///   _4 = &raw? mut? *_1;
///
/// Into
///   _1 = &raw? mut? PLACE;
///   _3 = PLACE;
///   _4 = &raw? mut? PLACE;
///
/// where `PLACE` is a direct or an indirect place expression.
///
/// There are 3 properties that need to be upheld for this transformation to be legal:
/// - place stability: `PLACE` must refer to the same memory wherever it appears;
/// - pointer liveness: we must not introduce dereferences of dangling pointers;
/// - `&mut` borrow uniqueness.
///
/// # Stability
///
/// If `PLACE` is an indirect projection, if its of the form `(*LOCAL).PROJECTIONS` where:
/// - `LOCAL` is SSA;
/// - all projections in `PROJECTIONS` have a stable offset (no dereference and no indexing).
///
/// If `PLACE` is a direct projection of a local, we consider it as constant if:
/// - the local is always live, or it has a single `StorageLive`;
/// - all projections have a stable offset.
///
/// # Liveness
///
/// When performing a substitution, we must take care not to introduce uses of dangling locals.
/// To ensure this, we walk the body with the `MaybeStorageDead` dataflow analysis:
/// - if we want to replace `*x` by reborrow `*y` and `y` may be dead, we allow replacement and
///   mark storage statements on `y` for removal;
/// - if we want to replace `*x` by non-reborrow `y` and `y` must be live, we allow replacement;
/// - if we want to replace `*x` by non-reborrow `y` and `y` may be dead, we do not replace.
///
/// # Uniqueness
///
/// For `&mut` borrows, we also need to preserve the uniqueness property:
/// we must avoid creating a state where we interleave uses of `*_1` and `_2`.
/// To do it, we only perform full substitution of mutable borrows:
/// we replace either all or none of the occurrences of `*_1`.
///
/// Some care has to be taken when `_1` is copied in other locals.
///   _1 = &raw? mut? _2;
///   _3 = *_1;
///   _4 = _1
///   _5 = *_4
/// In such cases, fully substituting `_1` means fully substituting all of the copies.
///
/// For immutable borrows, we do not need to preserve such uniqueness property,
/// so we perform all the possible substitutions without removing the `_1 = &_2` statement.
pub struct ReferencePropagation;

impl<'tcx> MirPass<'tcx> for ReferencePropagation {
    fn is_enabled(&self, sess: &rustc_session::Session) -> bool {
        sess.mir_opt_level() >= 4
    }

    #[instrument(level = "trace", skip(self, tcx, body))]
    fn run_pass(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        debug!(def_id = ?body.source.def_id());
        propagate_ssa(tcx, body);
    }
}

fn propagate_ssa<'tcx>(tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
    let ssa = SsaLocals::new(body);

    let mut replacer = compute_replacement(tcx, body, &ssa);
    debug!(?replacer.targets, ?replacer.allowed_replacements, ?replacer.storage_to_remove);

    replacer.visit_body_preserves_cfg(body);

    if replacer.any_replacement {
        crate::simplify::remove_unused_definitions(body);
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Value<'tcx> {
    /// Not a pointer, or we can't know.
    Unknown,
    /// We know the value to be a pointer to this place.
    /// The boolean indicates whether the reference is mutable, subject the uniqueness rule.
    Pointer(Place<'tcx>, bool),
}

/// For each local, save the place corresponding to `*local`.
#[instrument(level = "trace", skip(tcx, body))]
fn compute_replacement<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    ssa: &SsaLocals,
) -> Replacer<'tcx> {
    let always_live_locals = always_storage_live_locals(body);

    // Compute which locals have a single `StorageLive` statement ever.
    let storage_live = StorageLiveLocals::new(body, &always_live_locals);

    // Compute `MaybeStorageDead` dataflow to check that we only replace when the pointee is
    // definitely live.
    let mut maybe_dead = MaybeStorageDead::new(always_live_locals)
        .into_engine(tcx, body)
        .iterate_to_fixpoint()
        .into_results_cursor(body);

    // Map for each local to the pointee.
    let mut targets = IndexVec::from_elem(Value::Unknown, &body.local_decls);
    // Set of locals for which we will remove their storage statement. This is useful for
    // reborrowed references.
    let mut storage_to_remove = BitSet::new_empty(body.local_decls.len());

    let fully_replacable_locals = fully_replacable_locals(ssa);

    // Returns true iff we can use `place` as a pointee.
    //
    // Note that we only need to verify that there is a single `StorageLive` statement, and we do
    // not need to verify that it dominates all uses of that local.
    //
    // Consider the three statements:
    //   SL : StorageLive(a)
    //   DEF: b = &raw? mut? a
    //   USE: stuff that uses *b
    //
    // First, we recall that DEF is checked to dominate USE. Now imagine for the sake of
    // contradiction there is a DEF -> SL -> USE path. Consider two cases:
    //
    // - DEF dominates SL. We always have UB the first time control flow reaches DEF,
    //   because the storage of `a` is dead. Since DEF dominates USE, that means we cannot
    //   reach USE and so our optimization is ok.
    //
    // - DEF does not dominate SL. Then there is a `START_BLOCK -> SL` path not including DEF.
    //   But we can extend this path to USE, meaning there is also a `START_BLOCK -> USE` path not
    //   including DEF. This violates the DEF dominates USE condition, and so is impossible.
    let is_constant_place = |place: Place<'_>| {
        // We only allow `Deref` as the first projection, to avoid surprises.
        if place.projection.first() == Some(&PlaceElem::Deref) {
            // `place == (*some_local).xxx`, it is constant only if `some_local` is constant.
            // We approximate constness using SSAness.
            ssa.is_ssa(place.local) && place.projection[1..].iter().all(PlaceElem::is_stable_offset)
        } else {
            storage_live.has_single_storage(place.local)
                && place.projection[..].iter().all(PlaceElem::is_stable_offset)
        }
    };

    let mut can_perform_opt = |target: Place<'tcx>, loc: Location| {
        if target.projection.first() == Some(&PlaceElem::Deref) {
            // We are creating a reborrow. As `place.local` is a reference, removing the storage
            // statements should not make it much harder for LLVM to optimize.
            storage_to_remove.insert(target.local);
            true
        } else {
            // This is a proper dereference. We can only allow it if `target` is live.
            maybe_dead.seek_after_primary_effect(loc);
            let maybe_dead = maybe_dead.contains(target.local);
            !maybe_dead
        }
    };

    for (local, rvalue, location) in ssa.assignments(body) {
        debug!(?local);

        // Only visit if we have something to do.
        let Value::Unknown = targets[local] else { bug!() };

        let ty = body.local_decls[local].ty;

        // If this is not a reference or pointer, do nothing.
        if !ty.is_any_ptr() {
            debug!("not a reference or pointer");
            continue;
        }

        // If this a mutable reference that we cannot fully replace, mark it as unknown.
        if ty.is_mutable_ptr() && !fully_replacable_locals.contains(local) {
            debug!("not fully replaceable");
            continue;
        }

        debug!(?rvalue);
        match rvalue {
            // This is a copy, just use the value we have in store for the previous one.
            // As we are visiting in `assignment_order`, ie. reverse postorder, `rhs` should
            // have been visited before.
            Rvalue::Use(Operand::Copy(place) | Operand::Move(place))
            | Rvalue::CopyForDeref(place) => {
                if let Some(rhs) = place.as_local() {
                    let target = targets[rhs];
                    if matches!(target, Value::Pointer(..)) {
                        targets[local] = target;
                    } else if ssa.is_ssa(rhs) {
                        let refmut = body.local_decls[rhs].ty.is_mutable_ptr();
                        targets[local] = Value::Pointer(tcx.mk_place_deref(rhs.into()), refmut);
                    }
                }
            }
            Rvalue::Ref(_, _, place) | Rvalue::AddressOf(_, place) => {
                let mut place = *place;
                // Try to see through `place` in order to collapse reborrow chains.
                if place.projection.first() == Some(&PlaceElem::Deref)
                    && let Value::Pointer(target, refmut) = targets[place.local]
                    // Only see through immutable reference and pointers, as we do not know yet if
                    // mutable references are fully replaced.
                    && !refmut
                    // Only collapse chain if the pointee is definitely live.
                    && can_perform_opt(target, location)
                {
                    place = target.project_deeper(&place.projection[1..], tcx);
                }
                assert_ne!(place.local, local);
                if is_constant_place(place) {
                    targets[local] = Value::Pointer(place, ty.is_mutable_ptr());
                }
            }
            // We do not know what to do, so keep as not-a-pointer.
            _ => {}
        }
    }

    debug!(?targets);

    let mut finder = ReplacementFinder {
        targets: &mut targets,
        can_perform_opt,
        allowed_replacements: FxHashSet::default(),
    };
    let reachable_blocks = traversal::reachable_as_bitset(body);
    for (bb, bbdata) in body.basic_blocks.iter_enumerated() {
        // Only visit reachable blocks as we rely on dataflow.
        if reachable_blocks.contains(bb) {
            finder.visit_basic_block_data(bb, bbdata);
        }
    }

    let allowed_replacements = finder.allowed_replacements;
    return Replacer {
        tcx,
        targets,
        storage_to_remove,
        allowed_replacements,
        any_replacement: false,
    };

    struct ReplacementFinder<'a, 'tcx, F> {
        targets: &'a mut IndexVec<Local, Value<'tcx>>,
        can_perform_opt: F,
        allowed_replacements: FxHashSet<(Local, Location)>,
    }

    impl<'tcx, F> Visitor<'tcx> for ReplacementFinder<'_, 'tcx, F>
    where
        F: FnMut(Place<'tcx>, Location) -> bool,
    {
        fn visit_place(&mut self, place: &Place<'tcx>, ctxt: PlaceContext, loc: Location) {
            if matches!(ctxt, PlaceContext::NonUse(_)) {
                // There is no need to check liveness for non-uses.
                return;
            }

            if let Value::Pointer(target, refmut) = self.targets[place.local]
                && place.projection.first() == Some(&PlaceElem::Deref)
            {
                let perform_opt = (self.can_perform_opt)(target, loc);
                if perform_opt {
                    self.allowed_replacements.insert((target.local, loc));
                } else if refmut {
                    // This mutable reference is not fully replacable, so drop it.
                    self.targets[place.local] = Value::Unknown;
                }
            }
        }
    }
}

/// Compute the set of locals that can be fully replaced.
///
/// We consider a local to be replacable iff it's only used in a `Deref` projection `*_local` or
/// non-use position (like storage statements and debuginfo).
fn fully_replacable_locals(ssa: &SsaLocals) -> BitSet<Local> {
    let mut replacable = BitSet::new_empty(ssa.num_locals());

    // First pass: for each local, whether its uses can be fully replaced.
    for local in ssa.locals() {
        if ssa.num_direct_uses(local) == 0 {
            replacable.insert(local);
        }
    }

    // Second pass: a local can only be fully replaced if all its copies can.
    ssa.meet_copy_equivalence(&mut replacable);

    replacable
}

/// Utility to help performing subtitution of `*pattern` by `target`.
struct Replacer<'tcx> {
    tcx: TyCtxt<'tcx>,
    targets: IndexVec<Local, Value<'tcx>>,
    storage_to_remove: BitSet<Local>,
    allowed_replacements: FxHashSet<(Local, Location)>,
    any_replacement: bool,
}

impl<'tcx> MutVisitor<'tcx> for Replacer<'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx
    }

    fn visit_place(&mut self, place: &mut Place<'tcx>, ctxt: PlaceContext, loc: Location) {
        if let Value::Pointer(target, _) = self.targets[place.local]
            && place.projection.first() == Some(&PlaceElem::Deref)
        {
            let perform_opt = matches!(ctxt, PlaceContext::NonUse(_))
                || self.allowed_replacements.contains(&(target.local, loc));

            if perform_opt {
                *place = target.project_deeper(&place.projection[1..], self.tcx);
                self.any_replacement = true;
            }
        } else {
            self.super_place(place, ctxt, loc);
        }
    }

    fn visit_statement(&mut self, stmt: &mut Statement<'tcx>, loc: Location) {
        match stmt.kind {
            StatementKind::StorageLive(l) | StatementKind::StorageDead(l)
                if self.storage_to_remove.contains(l) =>
            {
                stmt.make_nop();
            }
            // Do not remove assignments as they may still be useful for debuginfo.
            _ => self.super_statement(stmt, loc),
        }
    }
}
