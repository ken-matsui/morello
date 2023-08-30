use blocks::Block;
use enum_dispatch::enum_dispatch;
use kernels::Kernel;
use std::collections::HashMap;
use std::fmt::Debug;

use crate::tensorspec::TensorSpec;
use crate::views::{Param, View};
use crate::{
    cost::MainCost,
    imp::{loops::Loop, moves::MoveLet, pipeline::Pipeline, subspecs::SpecApp},
    memorylimits::{MemVec, MemoryAllocation},
    nameenv::NameEnv,
    spec::Spec,
    target::Target,
};

pub mod blocks;
pub mod kernels;
pub mod loops;
pub mod moves;
pub mod pipeline;
pub mod subspecs;

#[enum_dispatch]
pub trait Impl<Tgt: Target, Aux: Clone> {
    fn parameters(&self) -> Box<dyn Iterator<Item = &TensorSpec<Tgt>> + '_>;

    fn parameter_count(&self) -> u8 {
        self.parameters().count().try_into().unwrap()
    }

    fn children(&self) -> &[ImplNode<Tgt, Aux>];

    fn memory_allocated(&self) -> MemoryAllocation;

    fn compute_main_cost(&self, child_costs: &[MainCost]) -> MainCost;

    #[must_use]
    fn replace_children(&self, new_children: impl Iterator<Item = ImplNode<Tgt, Aux>>) -> Self;

    fn bind<'i, 'j: 'i>(
        &'j self,
        args: &[&'j dyn View<Tgt = Tgt>],
        env: &'i mut HashMap<Param<Tgt>, &'j dyn View<Tgt = Tgt>>,
    );

    fn pprint_line<'a>(
        &'a self,
        names: &mut NameEnv<'a, dyn View<Tgt = Tgt>>,
        param_bindings: &HashMap<Param<Tgt>, &dyn View<Tgt = Tgt>>,
    ) -> Option<String>;

    fn aux(&self) -> &Aux;
}

pub trait ImplExt<Tgt: Target, Aux: Clone>: Impl<Tgt, Aux> {
    fn peak_memory_from_child_peaks(&self, child_peaks: &[MemVec]) -> MemVec;
}

/// A non-Spec node in an Impl program tree.
///
/// These usually result from applying an [Action](crate::scheduling::Action).
///
/// Unlike [Action](crate::scheduling::Action)s, parameters may be bound to "concrete" [Tensor]s and
/// other [View]s and stored in [Rc]s (rather than an explicit environment structure).
#[derive(Debug, Clone)]
#[enum_dispatch(Impl<Tgt, Aux>)]
pub enum ImplNode<Tgt: Target, Aux: Clone> {
    Loop(Loop<Tgt, Aux>),
    MoveLet(MoveLet<Tgt, Aux>),
    Block(Block<Tgt, Aux>),
    Pipeline(Pipeline<Tgt, Aux>),
    Kernel(Kernel<Tgt, Aux>),
    SpecApp(SpecApp<Tgt, Spec<Tgt>, Aux>),
}

impl<Tgt: Target, Aux: Clone, T: Impl<Tgt, Aux>> ImplExt<Tgt, Aux> for T {
    fn peak_memory_from_child_peaks(&self, child_peaks: &[MemVec]) -> MemVec {
        let mut peak = MemVec::zero::<Tgt>();
        match self.memory_allocated() {
            MemoryAllocation::Simple(own) => {
                for child_peak in child_peaks {
                    for i in 0..peak.len() {
                        peak[i] = peak[i].max(own[i] + child_peak[i]);
                    }
                }
            }
            MemoryAllocation::Inner(child_adds) => {
                debug_assert_eq!(child_peaks.len(), child_adds.len());
                for (child_peak, own_child_alloc) in child_peaks.iter().zip(&child_adds) {
                    for i in 0..peak.len() {
                        peak[i] = peak[i].max(child_peak[i] + own_child_alloc[i]);
                    }
                }
            }
            MemoryAllocation::Pipeline {
                intermediate_consumption,
            } => {
                debug_assert_eq!(child_peaks.len() + 1, intermediate_consumption.len());
                let z = MemVec::zero::<Tgt>();
                let mut preceding_consumption = &z;
                let mut following_consumption = &intermediate_consumption[0];
                for (child_idx, child_peak) in child_peaks.iter().enumerate() {
                    for i in 0..peak.len() {
                        peak[i] = peak[i].max(
                            preceding_consumption[i] + child_peak[i] + following_consumption[i],
                        );
                    }
                    preceding_consumption = following_consumption;
                    following_consumption =
                        intermediate_consumption.get(child_idx + 1).unwrap_or(&z);
                }
            }
        }
        peak
    }
}

/// Calls the given function on all leaves of an Impl.
///
/// The given may return `false` to short-circuit, which will be propogated to the caller of this
/// function.
pub fn visit_leaves<Tgt, Aux: Clone, F>(imp: &ImplNode<Tgt, Aux>, f: &mut F) -> bool
where
    Tgt: Target,
    F: FnMut(&ImplNode<Tgt, Aux>) -> bool,
{
    let children = imp.children();
    if children.is_empty() {
        f(imp)
    } else {
        let c = imp.children();
        for child in c {
            let should_complete = visit_leaves(child, f);
            if !should_complete {
                return false;
            }
        }
        true
    }
}