use crate::codegen::c_utils::VecType;
use crate::common::DimSize;
use crate::cost::MainCost;
use crate::grid::canon::CanonicalBimap;
use crate::grid::general::BiMap;
use crate::imp::kernels::KernelType;
use crate::layout::{col_major, nhwc, row_major, Layout};
use crate::memorylimits::{MemVec, MemoryLimits};
use crate::scheduling::Action;
use crate::spec::{dim_range, LogicalSpec, PrimitiveBasics, PrimitiveSpecType};
use crate::target::{MemoryLevel, Target, TargetId, LEVEL_COUNT};
use crate::tensorspec::TensorSpec;

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt::{Debug, Display};
use std::iter;

pub(super) trait CpuTarget:
    Clone + Copy + std::hash::Hash + Eq + Default + Debug + 'static
{
    fn target_id() -> TargetId;
    fn vec_types() -> &'static [VecType; 4];
}

#[allow(clippy::upper_case_acronyms)]
#[derive(
    Eq, PartialEq, Debug, Copy, Clone, Hash, Deserialize, Serialize, enum_iterator::Sequence,
)]
pub enum CpuMemoryLevel {
    RF,
    VRF,
    L1,
    GL,
}

#[derive(Debug, Clone, Copy)]
pub struct CpuMemoryLevelBimap;

impl<T: CpuTarget> Target for T {
    type Level = CpuMemoryLevel;

    fn line_size() -> u32 {
        32
    }

    fn max_mem() -> MemoryLimits {
        MemoryLimits::Standard(MemVec::new([64, 1024, 32_768, 1_073_741_824]))
    }

    fn processors() -> u8 {
        32
    }

    fn default_level() -> Self::Level {
        CpuMemoryLevel::GL
    }

    fn levels() -> [Self::Level; LEVEL_COUNT] {
        [
            CpuMemoryLevel::RF,
            CpuMemoryLevel::VRF,
            CpuMemoryLevel::L1,
            CpuMemoryLevel::GL,
        ]
    }

    fn possible_destination_levels(slower: Self::Level) -> Vec<Self::Level> {
        match slower {
            CpuMemoryLevel::RF | CpuMemoryLevel::VRF => vec![slower],
            CpuMemoryLevel::L1 => vec![slower, CpuMemoryLevel::RF, CpuMemoryLevel::VRF],
            CpuMemoryLevel::GL => vec![slower, CpuMemoryLevel::L1],
        }
    }

    fn all_layouts_for_shape(shape: &[DimSize]) -> Vec<Layout> {
        let mut results = Self::move_destination_layouts(shape);
        if shape.len() == 2 {
            let cm = col_major(2);
            if !results.contains(&cm) {
                results.push(cm);
            }
        }
        results
    }

    /// Return layouts that are valid destinations for a move of the given shape.
    ///
    /// All returned layouts are applicable to the given shape.
    /// ([Layout::applies_to_shape] returns `true`.)
    fn move_destination_layouts(shape: &[DimSize]) -> Vec<Layout> {
        let rank = u8::try_from(shape.len()).unwrap();
        let non_one_dims = shape.iter().filter(|&d| *d > 1).count();
        let it = iter::once(row_major(rank));
        match rank {
            2 if non_one_dims == 2 => {
                extend_move_actions_with_packed(shape, it.chain(iter::once(col_major(2)))).collect()
            }
            4 => {
                extend_move_actions_with_packed(shape, it.chain(iter::once(nhwc(shape)))).collect()
            }
            _ => extend_move_actions_with_packed(shape, it).collect(),
        }
    }

    fn actions(spec: &LogicalSpec<Self>) -> Box<dyn Iterator<Item = Action<Self>>> {
        match spec {
            LogicalSpec::Primitive(PrimitiveBasics { typ, .. }, _, _) => match typ {
                PrimitiveSpecType::Matmul { accum } => {
                    if *accum {
                        let mut microkernels = vec![];
                        if mult_applies_to_operands(&spec.parameters()) {
                            microkernels.push(Action::Place(KernelType::Mult));
                        }
                        if broadcastvecmult_applies_to_operands(&spec.parameters()) {
                            microkernels.push(Action::Place(KernelType::BroadcastVecMult));
                        }
                        Box::new(microkernels.into_iter())
                    } else {
                        Box::new(iter::empty())
                    }
                }
                PrimitiveSpecType::Conv { .. } => Box::new(iter::empty()),
                PrimitiveSpecType::Move { .. } => {
                    let mut microkernels = vec![];
                    if valueassign_applies_to_operands(&spec.parameters()) {
                        microkernels.push(Action::Place(KernelType::ValueAssign));
                    }
                    if vectorassign_applies_to_operands(&spec.parameters()) {
                        microkernels.push(Action::Place(KernelType::VectorAssign));
                    }
                    Box::new(microkernels.into_iter())
                }
                PrimitiveSpecType::Zero { .. } => {
                    let mut microkernels = vec![];
                    if memsetzero_applies_to_operands(&spec.parameters()) {
                        microkernels.push(Action::Place(KernelType::MemsetZero));
                    }
                    if vectorzero_applies_to_operands(&spec.parameters()) {
                        microkernels.push(Action::Place(KernelType::VectorZero));
                    }
                    Box::new(microkernels.into_iter())
                }
            },
            LogicalSpec::Compose { .. } => Box::new(iter::empty()),
        }
    }

    fn target_id() -> TargetId {
        <Self as CpuTarget>::target_id()
    }

    fn vec_types() -> &'static [VecType; 4] {
        <Self as CpuTarget>::vec_types()
    }
}

impl MemoryLevel for CpuMemoryLevel {
    fn is_addressed(&self) -> bool {
        match &self {
            CpuMemoryLevel::RF => true,
            CpuMemoryLevel::VRF => true,
            CpuMemoryLevel::L1 => false,
            CpuMemoryLevel::GL => true,
        }
    }

    fn cache_hit_cost(&self) -> MainCost {
        match &self {
            CpuMemoryLevel::RF => 0,
            CpuMemoryLevel::VRF => 0,
            CpuMemoryLevel::L1 => 10,
            CpuMemoryLevel::GL => 100,
        }
    }

    fn vector_bytes(&self) -> &'static [u32] {
        match &self {
            // CpuMemoryLevel::VRF => &[16, 32],
            CpuMemoryLevel::VRF => &[16],
            _ => &[],
        }
    }
}

impl PartialOrd for CpuMemoryLevel {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self == other {
            return Some(Ordering::Equal);
        }

        match (self, other) {
            (CpuMemoryLevel::RF, CpuMemoryLevel::VRF) => None,
            (CpuMemoryLevel::VRF, CpuMemoryLevel::RF) => None,
            (CpuMemoryLevel::RF, _) => Some(Ordering::Less),
            (CpuMemoryLevel::VRF, _) => Some(Ordering::Less),
            (_, CpuMemoryLevel::RF) => Some(Ordering::Greater),
            (_, CpuMemoryLevel::VRF) => Some(Ordering::Greater),
            (CpuMemoryLevel::L1, CpuMemoryLevel::GL) => Some(Ordering::Less),
            (CpuMemoryLevel::GL, CpuMemoryLevel::L1) => Some(Ordering::Greater),
            (CpuMemoryLevel::L1, CpuMemoryLevel::L1) => unreachable!(),
            (CpuMemoryLevel::GL, CpuMemoryLevel::GL) => unreachable!(),
        }
    }
}

impl Display for CpuMemoryLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match &self {
                CpuMemoryLevel::RF => "RF",
                CpuMemoryLevel::VRF => "VRF",
                CpuMemoryLevel::L1 => "L1",
                CpuMemoryLevel::GL => "GL",
            }
        )
    }
}

impl BiMap for CpuMemoryLevelBimap {
    type Domain = CpuMemoryLevel;
    type Codomain = u8;

    fn apply(&self, level: &CpuMemoryLevel) -> u8 {
        match level {
            CpuMemoryLevel::RF => 0,
            CpuMemoryLevel::VRF => 1,
            CpuMemoryLevel::L1 => 2,
            CpuMemoryLevel::GL => 3,
        }
    }

    fn apply_inverse(&self, i: &u8) -> CpuMemoryLevel {
        match *i {
            0 => CpuMemoryLevel::RF,
            1 => CpuMemoryLevel::VRF,
            2 => CpuMemoryLevel::L1,
            3 => CpuMemoryLevel::GL,
            _ => panic!("Invalid index: {}", i),
        }
    }
}

impl CanonicalBimap for CpuMemoryLevel {
    type Bimap = CpuMemoryLevelBimap;

    fn bimap() -> Self::Bimap {
        CpuMemoryLevelBimap
    }
}

pub fn valueassign_applies_to_operands<Tgt: Target<Level = CpuMemoryLevel>>(
    operands: &[TensorSpec<Tgt>],
) -> bool {
    debug_assert_eq!(operands.len(), 2);

    if operands.iter().flat_map(|o| o.shape()).any(|&d| d != 1) {
        return false;
    }

    for o in &operands[1..] {
        if (o.dtype(), o.layout()) != (operands[0].dtype(), operands[0].layout()) {
            return false;
        }
    }

    operands.iter().any(|o| o.level() == CpuMemoryLevel::RF)
        && operands
            .iter()
            .all(|o| o.level() == CpuMemoryLevel::RF || o.level() == CpuMemoryLevel::L1)
}

pub fn vectorassign_applies_to_operands<Tgt: Target>(operands: &[TensorSpec<Tgt>]) -> bool {
    if operands.iter().any(|o| !o.is_contiguous()) {
        return false;
    }
    if operands[0].dtype() != operands[1].dtype() {
        return false;
    }
    if operands[0].shape() != operands[1].shape() {
        return false;
    }
    if operands[0].layout() != operands[1].layout() {
        return false;
    }

    let mut has_vrf = false;
    for o in operands {
        if o.level().vector_rf() {
            has_vrf = true;
            match o.vector_size() {
                Some(vector_size) => {
                    let volume = o.shape().iter().product::<DimSize>();
                    if vector_size != volume {
                        return false;
                    }
                }
                None => {
                    panic!("No vector_size on operand in level {:?}", o.level());
                }
            }
        }
    }
    has_vrf
}

pub fn cacheaccess_applies_to_operands<Tgt: Target>(_operands: &[TensorSpec<Tgt>]) -> bool {
    false

    // if operands.iter().all(|o| o.level().is_addressed()) {
    //     return false;
    // }
    // if operands.iter().any(|o| !o.is_contiguous()) {
    //     return false;
    // }
    // if operands[0].dtype() != operands[1].dtype() {
    //     return false;
    // }
    // if operands[0].shape() != operands[1].shape() {
    //     return false;
    // }
    // if operands[0].layout() != operands[1].layout() {
    //     return false;
    // }
    // true
}

pub fn memsetzero_applies_to_operands<Tgt: Target<Level = CpuMemoryLevel>>(
    operands: &[TensorSpec<Tgt>],
) -> bool {
    if !operands[0].is_contiguous() {
        return false;
    }
    if operands[0].level() != CpuMemoryLevel::RF {
        return false;
    }
    true
}

pub fn vectorzero_applies_to_operands<Tgt: Target<Level = CpuMemoryLevel>>(
    operands: &[TensorSpec<Tgt>],
) -> bool {
    if !operands[0].is_contiguous() {
        return false;
    }
    if operands[0].level() != CpuMemoryLevel::VRF {
        return false;
    }
    let volume = operands[0].shape().iter().product::<DimSize>();
    match operands[0].vector_size() {
        Some(vector_size) if vector_size != volume => {
            return false;
        }
        None => return false,
        _ => (),
    };
    true
}

pub fn broadcastvecmult_applies_to_operands<Tgt: Target<Level = CpuMemoryLevel>>(
    operands: &[TensorSpec<Tgt>],
) -> bool {
    if operands[0].level() != CpuMemoryLevel::RF {
        return false;
    }
    for i in 1..3 {
        if operands[i].level() != CpuMemoryLevel::VRF {
            return false;
        }
        let volume = operands[i].shape().iter().product::<DimSize>();
        if volume != operands[i].vector_size().unwrap() {
            return false;
        }
        if !operands[i].aligned() || !operands[i].is_contiguous() {
            return false;
        }
        if operands[0].dtype() != operands[i].dtype() {
            return false;
        }
    }
    if operands[0].shape().iter().any(|d| *d != 1) {
        return false;
    }
    if operands[1].shape().len() != 2 || operands[1].shape()[0] != 1 {
        return false;
    }
    if operands[2].shape().to_vec() != vec![1, operands[1].shape()[1]] {
        return false;
    }
    true
}

pub fn mult_applies_to_operands<Tgt: Target<Level = CpuMemoryLevel>>(
    operands: &[TensorSpec<Tgt>],
) -> bool {
    operands
        .iter()
        .all(|o| o.level() == CpuMemoryLevel::RF && o.shape().iter().all(|&d| d == 1))
}

fn extend_move_actions_with_packed(
    shape: &[DimSize],
    it: impl Iterator<Item = Layout> + 'static,
) -> impl Iterator<Item = Layout> + '_ {
    it.flat_map(move |original_layout| {
        let Layout::New(dims) = &original_layout;
        debug_assert!(dims.iter().all(|(_, s)| s.is_none()));
        let more_iter = dims[..dims.len() - 1].iter().flat_map(move |&(dim, _)| {
            let dims = dims.clone();

            let mut it_a = None;
            let mut it_b = None;
            if shape[usize::from(dim)] == 1 {
                it_a = Some(iter::empty());
            } else {
                it_b = Some(pack_sizes(shape[usize::from(dim)]).map(move |strip_size| {
                    Layout::new(
                        dims.iter()
                            .cloned()
                            .chain(iter::once((dim, Some(strip_size))))
                            .collect(),
                    )
                }));
            }
            it_a.into_iter().flatten().chain(it_b.into_iter().flatten())
        });
        // TODO: Avoid following `clone` and `collect`
        iter::once(original_layout.clone())
            .chain(more_iter)
            .collect::<Vec<_>>()
            .into_iter()
    })
}

fn pack_sizes(max_size: DimSize) -> impl Iterator<Item = DimSize> {
    let mut inner_iter = dim_range(max_size, true);
    let first_value = inner_iter.next();
    debug_assert_eq!(first_value, Some(1));
    inner_iter.filter(move |&s| max_size % s == 0)
}

#[cfg(test)]
mod tests {
    use crate::{
        common::DimSize,
        expr::{NonAffineExpr, Substitute},
        layout::BufferVar,
        layout::Layout,
        opaque_symbol::OpaqueSymbol,
        target::{Target, X86Target},
    };
    use itertools::Itertools;
    use proptest::prelude::*;
    use std::collections::HashSet;

    proptest! {
        #[test]
        fn test_all_layouts_for_shape_are_unique(
            shape in proptest::collection::vec(1..8u32, 1..=5)
        ) {
            assert_unique_layouts(&X86Target::all_layouts_for_shape(&shape));
        }

        #[test]
        fn test_move_destination_layouts_are_unique(
            shape in proptest::collection::vec(1..8u32, 1..=5)
        ) {
            assert_unique_layouts(&X86Target::move_destination_layouts(&shape));
        }

        #[test]
        fn test_all_layouts_for_shape_contains_move_destination_layouts(
            shape in proptest::collection::vec(1..8u32, 1..=5)
        ) {
            let superset: HashSet<_> = X86Target::all_layouts_for_shape(&shape)
                .into_iter()
                .collect();
            let subset: HashSet<_> = X86Target::move_destination_layouts(&shape)
                .into_iter()
                .collect();
            assert!(superset.is_superset(&subset));
        }

        #[test]
        fn test_move_destination_layouts_have_unique_index_expressions(
            shape in proptest::collection::vec(1..8u32, 1..=5)
        ) {
            let expr_id = OpaqueSymbol::new();
            let mut index_exprs: Vec<(Vec<i32>, Layout)> = Vec::new();
            for layout in X86Target::move_destination_layouts(&shape) {
                let ie = layout.buffer_indexing_expr(&expr_id, &shape);
                let evaluated_pts = eval_all_index_expr_points(&ie, &shape);
                for (prev_pts, prev_layout) in index_exprs.iter() {
                    if prev_pts == &evaluated_pts {
                        panic!(
                            "Layouts had identical indexing expressions: {:?} and {:?} (expr = {})", prev_layout, layout, ie
                        );
                    }
                }
                index_exprs.push((evaluated_pts, layout));
            }
        }

        #[test]
        fn test_packed_layout_with_strip_size_one_is_row_major(
            example in arb_test_packed_layout_with_strip_size_one_is_row_major()
        ) {
            let (shape, strip_dim) = example;
            let expr_id = OpaqueSymbol::new();

            let packed_layout = Layout::new_packed(shape.len().try_into().unwrap(), strip_dim, 1);
            let packed_ie = packed_layout.buffer_indexing_expr(&expr_id, &shape);
            let packed_pts = eval_all_index_expr_points(&packed_ie, &shape);

            let rm_layout = Layout::new_standard(
                (0..u8::try_from(shape.len()).unwrap()).collect(),
                &shape);
            let rm_ie = rm_layout.buffer_indexing_expr(&expr_id, &shape);
            let rm_pts = eval_all_index_expr_points(&rm_ie, &shape);

            assert_eq!(packed_pts, rm_pts);
        }
    }

    prop_compose! {
        fn arb_test_packed_layout_with_strip_size_one_is_row_major()
            (shape in proptest::collection::vec(1..8u32, 1..=5))
            (strip_dim in 0..shape.len(), shape in Just(shape))
            -> (Vec<DimSize>, u8)
        {
            (shape, strip_dim.try_into().unwrap())
        }
    }

    fn assert_unique_layouts(layouts: &[Layout]) {
        let layouts_set = layouts.iter().collect::<HashSet<_>>();
        assert_eq!(layouts.len(), layouts_set.len());
    }

    fn eval_all_index_expr_points(expr: &NonAffineExpr<BufferVar>, shape: &[DimSize]) -> Vec<i32> {
        let mut results = vec![];
        for pt in shape.iter().map(|&d| 0..d).multi_cartesian_product() {
            let evaluated: NonAffineExpr<&str> = expr.clone().map_vars(&mut |var| match var {
                BufferVar::TileIdx(_, _) => panic!("TileIdx in index expression"),
                BufferVar::Pt(dim, _) => NonAffineExpr::constant(pt[dim as usize] as i32),
            });
            assert!(
                evaluated.0.is_empty(),
                "Non-constant index expression: {:?}",
                evaluated
            );
            results.push(evaluated.1);
        }
        results
    }
}
