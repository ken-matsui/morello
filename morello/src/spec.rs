use super::common::{DimSize, Shape};
use crate::action_seq::ActionSeq;
use crate::common::Dtype;
use crate::datadeps::SpecKey;
use crate::grid::canon::CanonicalBimap;
use crate::grid::general::{BiMap, SurMap};
use crate::grid::linear::BimapInt;
use crate::layout::row_major;
use crate::memorylimits::{MemoryLimits, MemoryLimitsBimap};
use crate::scheduling::{Action, TileOut};
use crate::target::MemoryLevel;
use crate::target::Target;
use crate::tensorspec::{self, TensorSpec, TensorSpecAux};
use crate::tiling::Tiling;
use crate::utils::{
    bit_length_inverse, bit_length_u32, is_power_of_two_u32, join_into_string,
    prev_power_of_two_u32,
};

use itertools::Either;
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use std::collections::HashMap;
use std::fmt;
use std::fmt::Display;
use std::iter::Iterator;
use std::iter::{self, once};
use std::marker::PhantomData;
use std::mem;
use std::num::NonZeroU32;
use std::panic;
use std::{assert_eq, debug_assert_eq};

/// Whether `tile_out` actions should tile in all dimensions per Spec.
const MULTI_DIM_TILING: bool = false;

/// An empirically chosen initial capacity for the [LogicalSpec::move_actions] results buffer.
const MOVE_RESULTS_CAPACITY: usize = 16;

#[cfg(test)]
const ARBITRARY_SPEC_MAX_SIZE: DimSize = nonzero::nonzero!(8u32);

#[derive(Clone, PartialEq, Eq, Hash, Debug, Deserialize, Serialize)]
#[serde(bound = "")]
pub struct Spec<Tgt: Target>(pub LogicalSpec<Tgt>, pub MemoryLimits);

// The following should probably just be Spec::Primitive and Spec::Compose variants once
// there are good conversions to/from image/filter shapes for Conv.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(bound = "")]
pub enum LogicalSpec<Tgt: Target> {
    Primitive(PrimitiveBasics, Vec<TensorSpecAux<Tgt>>, bool),
    Compose {
        // Components contain Spec shapes, which can be partially inferred, so
        // the following stores a little bit of redundant information.
        components: Vec<PrimitiveBasics>,
        operand_auxes: Vec<TensorSpecAux<Tgt>>,
        serial_only: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PrimitiveBasics {
    pub typ: PrimitiveSpecType,
    pub spec_shape: Shape,
    pub dtypes: Vec<Dtype>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub enum PrimitiveSpecType {
    Zero,
    Move,
    Matmul { accum: bool },
    Conv { accum: bool },
}

/// Tilings and dimension bindings for a particular output tiling.
///
/// Each dimension of an input tensor/tiling may have a binding to an output
/// tensor dimension. This means that loops should zip those dimensions of each
/// tensor to ensure data dependencies are correct. As an example, a matrix
/// multiplication will give the bindings `vec![Some(0), None]` and
/// `vec![None, Some(1)]` for each of its inputs, indicating that the first
/// dimension of the first input (the m dimension) is bound to the m dimension
/// of the output, and so on for the n dimension.
#[derive(Debug)]
pub struct TilingInference(pub Vec<(Tiling, Vec<Option<u8>>)>);

/// A [BiMap] which extends [LogicalSpecSurMap] with memory limits dimensions.
///
/// Memory limits are represented identically in the codomain. They are not scaled logarithmically
/// or inverted to be in data dependency order.
pub struct SpecSurMap<Tgt: Target, F, A, Aa> {
    pub logical_spec_surmap: LogicalSpecSurMap<Tgt, F, A, Aa>,
    pub memory_limits_bimap: MemoryLimitsBimap<Tgt>,
}

#[derive(Clone)]
pub struct LogicalSpecSurMap<Tgt, F, A, Aa> {
    pub primitive_basics_bimap: PrimitiveBasicsBimap,
    pub aux_surmap_fn: F,
    marker: std::marker::PhantomData<(Tgt, A, Aa)>,
}

#[derive(Clone)]
pub struct PrimitiveBasicsBimap {
    pub binary_scale_shapes: bool,
}

pub struct ShapeBimap(pub bool);

#[derive(thiserror::Error, Debug)]
pub enum CanonicalizeError {
    #[error("Failed to canonicalize the TensorSpecAux: {0}")]
    TensorSpecAuxCanonicalizeError(tensorspec::CanonicalizeError),
}

impl<Tgt: Target> Spec<Tgt> {
    pub fn canonicalize(&mut self) -> Result<(), CanonicalizeError> {
        let parameters = self.0.parameters();
        let levels = parameters.iter().map(|p| p.level()).collect::<Vec<_>>();
        self.1.zero_levels_slower_than_all::<Tgt>(&levels);
        self.0.canonicalize()
    }

    pub fn is_canonical(&self) -> bool {
        let parameters = self.0.parameters();
        let levels = parameters.iter().map(|p| p.level()).collect::<Vec<_>>();
        !self.1.any_nonzero_levels_slower_than::<Tgt>(&levels) && self.0.is_canonical()
    }

    /// Returns the FLOPs required to implement this Spec, if appropriate.
    pub fn flops(&self) -> Option<u64> {
        match self {
            Spec(LogicalSpec::Primitive(basics, _, _), _) => match basics.typ {
                PrimitiveSpecType::Matmul { .. } => {
                    let [m, k, n] = basics.spec_shape[..] else {
                        unreachable!();
                    };
                    Some(2 * u64::from(m.get()) * u64::from(k.get()) * u64::from(n.get()))
                }
                PrimitiveSpecType::Conv { .. } => {
                    // TODO: Implement for floating-pt. Convs.
                    None
                }
                PrimitiveSpecType::Move | PrimitiveSpecType::Zero => None,
            },
            Spec(LogicalSpec::Compose { .. }, _) => None,
        }
    }
}

impl<Tgt: Target> Display for Spec<Tgt> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.0, self.1)
    }
}

#[cfg(test)]
impl<Tgt: Target> proptest::arbitrary::Arbitrary for Spec<Tgt> {
    type Parameters = (Option<DimSize>, Option<u64>);
    type Strategy = proptest::strategy::BoxedStrategy<Spec<Tgt>>;

    fn arbitrary_with(args: Self::Parameters) -> Self::Strategy {
        use crate::memorylimits::arb_memorylimits;
        use proptest::prelude::*;

        // Optionally lower the max memory limits.
        let MemoryLimits::Standard(mut max_memory) = Tgt::max_mem();
        if let Some(lower_max) = args.1 {
            max_memory = max_memory.map(|v| v.min(lower_max));
        }

        (
            any_with::<LogicalSpec<Tgt>>(args.0),
            arb_memorylimits::<Tgt>(&max_memory),
        )
            .prop_map(|(logical_spec, mem_limits)| Spec(logical_spec, mem_limits))
            .boxed()
    }
}

#[cfg(test)]
pub fn arb_canonical_spec<Tgt: Target>(
    max_size: Option<DimSize>,
    max_memory: Option<u64>,
) -> impl proptest::strategy::Strategy<Value = Spec<Tgt>> {
    use proptest::prelude::*;

    any_with::<Spec<Tgt>>((max_size, max_memory)).prop_filter_map(
        "Must be possible to canonicalize Spec",
        |mut s| {
            if s.canonicalize().is_err() {
                return None;
            }
            Some(s)
        },
    )
}

impl PrimitiveBasics {
    pub fn replace_io(&mut self, new_operands: &[(&[DimSize], Dtype)]) {
        self.dtypes = new_operands.iter().map(|o| o.1).collect();

        match self.typ {
            PrimitiveSpecType::Matmul { accum: _ } => {
                debug_assert_eq!(new_operands.len(), 3);
                debug_assert_eq!(new_operands[0].0[0], new_operands[2].0[0]);
                debug_assert_eq!(new_operands[1].0[1], new_operands[2].0[1]);
                debug_assert_eq!(new_operands[0].0[1], new_operands[1].0[0]);
                self.spec_shape = vec![
                    new_operands[0].0[0],
                    new_operands[0].0[1],
                    new_operands[1].0[1],
                ];
            }
            PrimitiveSpecType::Conv { accum: _ } => {
                let [b, c, h, w] = new_operands[0].0[..] else {
                    panic!();
                };
                let [f, alt_c, fh, fw] = new_operands[1].0[..] else {
                    panic!()
                };
                assert_eq!(c, alt_c);
                self.spec_shape = vec![b, f, c, h, w, fh, fw];
                // TODO: Assert output shape is expected.
            }
            PrimitiveSpecType::Move => {
                let [src, dest] = new_operands else {
                    panic!("Move must have 2 operands");
                };
                assert_eq!(src.0, dest.0);
                self.spec_shape = src.0.into();
            }
            PrimitiveSpecType::Zero => {
                assert_eq!(new_operands.len(), 1);
                self.spec_shape = new_operands[0].0.into();
            }
        }
    }

    pub fn aux_from_operand_auxes<'a, Tgt, I>(&self, operand_auxes: I) -> Vec<TensorSpecAux<Tgt>>
    where
        Tgt: Target,
        I: IntoIterator<Item = &'a TensorSpecAux<Tgt>> + 'a,
    {
        operand_auxes.into_iter().cloned().collect()
    }

    pub fn parameter_shapes(&self) -> Vec<Shape> {
        match self.typ {
            PrimitiveSpecType::Matmul { .. } => {
                let [m, k, n] = self.spec_shape[..] else {
                    panic!("Matmul spec_shape must have length 3")
                };
                vec![vec![m, k], vec![k, n], vec![m, n]]
            }
            PrimitiveSpecType::Conv { .. } => {
                let [b, f, c, h, w, fh, fw] = self.spec_shape[..] else {
                    panic!("Conv must have rank 7")
                };
                debug_assert!(
                    h >= fh && w >= fw,
                    "Conv spatial dims. {}x{} were larger than filter {}x{}",
                    h,
                    w,
                    fh,
                    fw
                );
                let img = vec![b, c, h, w];
                let filt = vec![f, c, fh, fw];
                let out = conv_infer_output_shape(&img, &filt);
                vec![img, filt, out]
            }
            PrimitiveSpecType::Move => {
                vec![self.spec_shape.clone(), self.spec_shape.clone()]
            }
            PrimitiveSpecType::Zero => vec![self.spec_shape.clone()],
        }
    }

    pub fn parameter_dtypes(&self) -> Vec<Dtype> {
        self.dtypes.clone()
    }

    pub fn input_tilings_for_tile_out(&self, smaller_output: &Tiling) -> TilingInference {
        match (self, smaller_output.is_simple()) {
            (
                PrimitiveBasics {
                    typ: PrimitiveSpecType::Matmul { .. },
                    spec_shape,
                    ..
                },
                true,
            ) => TilingInference(vec![
                (
                    Tiling::new_sliding(
                        vec![smaller_output.shape()[0], spec_shape[1]],
                        vec![smaller_output.step_sizes()[0], spec_shape[1]],
                    ),
                    vec![Some(0), None],
                ),
                (
                    Tiling::new_sliding(
                        vec![spec_shape[1], smaller_output.shape()[1]],
                        vec![spec_shape[1], smaller_output.step_sizes()[1]],
                    ),
                    vec![None, Some(1)],
                ),
            ]),
            (
                PrimitiveBasics {
                    typ: PrimitiveSpecType::Conv { .. },
                    spec_shape,
                    ..
                },
                _,
            ) => {
                let [_, _, channels, _, _, fh, fw] = spec_shape[..] else {
                    unreachable!()
                };

                // Compute the new input image Tiling.
                let new_image_shape: Shape = [smaller_output.shape()[0], channels]
                    .into_iter()
                    .chain(
                        smaller_output.shape()[2..]
                            .iter()
                            .zip([fh, fw])
                            .map(|(&o, f)| o.get() + f.get() - 1)
                            .map(|d| DimSize::new(d).unwrap()),
                    )
                    .collect();
                let mut new_image_steps: Shape = smaller_output.step_sizes().into();
                new_image_steps[1] = channels;

                // Compute the new filters Tiling.
                let new_filters_shape: Shape = [smaller_output.shape()[1], channels]
                    .into_iter()
                    .chain([fh, fw])
                    .collect();
                let mut new_filters_steps: Shape = new_filters_shape.clone();
                new_filters_steps[0] = smaller_output.step_sizes()[1];

                // Construct the bindings Vecs.
                let image_bindings = vec![Some(0), None, None, None];
                let filter_bindings = vec![None, Some(1), None, None];

                TilingInference(vec![
                    (
                        Tiling::new_sliding(new_image_shape, new_image_steps),
                        image_bindings,
                    ),
                    (
                        Tiling::new_sliding(new_filters_shape, new_filters_steps),
                        filter_bindings,
                    ),
                ])
            }
            (
                PrimitiveBasics {
                    typ: PrimitiveSpecType::Move,
                    ..
                },
                true,
            ) => TilingInference(vec![(
                smaller_output.clone(),
                (0..smaller_output.shape().len())
                    .map(|d| Some(d.try_into().unwrap()))
                    .collect(),
            )]),
            (
                PrimitiveBasics {
                    typ: PrimitiveSpecType::Zero,
                    ..
                },
                true,
            ) => TilingInference(vec![]),
            _ => unimplemented!(
                "Output tiling not implemented for {:?} and {:?}",
                self,
                smaller_output
            ),
        }
    }

    pub fn parameter_dim_axes(&self) -> Vec<Vec<u8>> {
        match self.typ {
            PrimitiveSpecType::Matmul { .. } => {
                vec![vec![0, 2], vec![2, 1], vec![0, 1]]
            }
            PrimitiveSpecType::Conv { .. } => {
                // Only correct for 2 spatial dimensions.
                // TODO: Extend this to arbitrary number of spatial dimensions.
                let (b, f, c, h, w, fh, fw) = (0, 1, 2, 3, 4, 5, 6);
                let img = vec![b, c, h, w];
                let filt = vec![f, c, fh, fw];
                let out = vec![b, f, h, w];
                vec![img, filt, out]
            }
            PrimitiveSpecType::Move { .. } | PrimitiveSpecType::Zero { .. } => self
                .parameter_shapes()
                .iter()
                .map(|o| (0..u8::try_from(o.len()).unwrap()).collect())
                .collect(),
        }
    }
}

impl Display for PrimitiveBasics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shape_str = join_into_string(&self.spec_shape, "×");
        write!(f, "{}({}, ", self.typ, shape_str)?;
        if self.dtypes.len() == 1 {
            write!(f, "{})", self.dtypes[0])
        } else {
            write!(f, "[{}])", self.dtypes.iter().join(", "))
        }
    }
}

#[cfg(test)]
impl proptest::arbitrary::Arbitrary for PrimitiveBasics {
    type Parameters = Option<DimSize>;
    type Strategy = proptest::strategy::BoxedStrategy<PrimitiveBasics>;

    fn arbitrary_with(args: Self::Parameters) -> Self::Strategy {
        use proptest::prelude::*;

        let max_size = args.unwrap_or(ARBITRARY_SPEC_MAX_SIZE).get();

        any::<PrimitiveSpecType>()
            .prop_flat_map(|typ| {
                let cnt = typ.operand_count();
                (Just(typ), proptest::collection::vec(any::<Dtype>(), cnt))
            })
            .prop_flat_map(move |(typ, dtypes)| {
                let shape_strategy = match typ {
                    PrimitiveSpecType::Matmul { accum: _ } => {
                        proptest::collection::vec(1..=max_size, 3).boxed()
                    }
                    PrimitiveSpecType::Conv { accum: _ } => (1..=max_size, 1..=max_size)
                        .prop_flat_map(move |(h, w)| {
                            (
                                1..max_size,
                                1..8u32,
                                1..4u32,
                                Just(h),
                                Just(w),
                                1..=h,
                                1..=w,
                            )
                        })
                        .prop_map(|(b, f, c, h, w, fh, fw)| vec![b, f, c, h, w, fh, fw])
                        .boxed(),
                    PrimitiveSpecType::Move | PrimitiveSpecType::Zero => (1..=4usize)
                        .prop_flat_map(move |tensor_rank| {
                            proptest::collection::vec(1..=max_size, tensor_rank)
                        })
                        .boxed(),
                };
                (Just(typ), Just(dtypes), shape_strategy)
            })
            .prop_map(move |(typ, dtypes, spec_shape)| PrimitiveBasics {
                typ,
                spec_shape: spec_shape
                    .into_iter()
                    .map(|x| DimSize::new(x).unwrap())
                    .collect(),
                dtypes,
            })
            .boxed()
    }
}

impl PrimitiveSpecType {
    pub fn operand_count(&self) -> usize {
        self.input_count() + 1
    }

    pub fn input_count(&self) -> usize {
        match self {
            PrimitiveSpecType::Matmul { .. } => 2,
            PrimitiveSpecType::Conv { .. } => 2,
            PrimitiveSpecType::Move => 1,
            PrimitiveSpecType::Zero => 0,
        }
    }

    pub fn output_idx(&self) -> usize {
        match self {
            PrimitiveSpecType::Matmul { .. } | PrimitiveSpecType::Conv { .. } => 2,
            PrimitiveSpecType::Move { .. } => 1,
            PrimitiveSpecType::Zero { .. } => 0,
        }
    }

    pub fn output_is_read(&self) -> bool {
        match self {
            PrimitiveSpecType::Matmul { accum } | PrimitiveSpecType::Conv { accum } => *accum,
            _ => false,
        }
    }

    pub fn infer_output_shape(&self, inputs: &[&[DimSize]]) -> Shape {
        // TODO: Can this be rewritten as output inference + `from_io` call?
        debug_assert_eq!(inputs.len(), self.input_count());
        match self {
            PrimitiveSpecType::Matmul { .. } => {
                let ([m, _k], [_, n]) = (inputs[0], inputs[1]) else {
                    panic!("Matmul inputs must have 2 dimensions each");
                };
                vec![*m, *n]
            }
            PrimitiveSpecType::Conv { .. } => {
                let ([b, _, h, w], [f, _, fh, fw]) = (inputs[0], inputs[1]) else {
                    panic!("Conv inputs must have 4 dimensions each");
                };
                debug_assert!(h.get() >= fh.get() && w.get() >= fw.get());
                vec![
                    *b,
                    *f,
                    DimSize::new(1 + h.get() - fh.get()).unwrap(),
                    DimSize::new(1 + w.get() - fw.get()).unwrap(),
                ]
            }
            PrimitiveSpecType::Move | PrimitiveSpecType::Zero => {
                // The shape and dtype match for moves and zero.
                inputs[0].to_vec()
            }
        }
    }
}

impl Display for PrimitiveSpecType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrimitiveSpecType::Matmul { accum, .. } if *accum => write!(f, "MatmulAccum"),
            PrimitiveSpecType::Matmul { .. } => write!(f, "Matmul"),
            PrimitiveSpecType::Conv { accum, .. } if *accum => write!(f, "ConvAccum"),
            PrimitiveSpecType::Conv { .. } => write!(f, "Conv"),
            PrimitiveSpecType::Move { .. } => write!(f, "Move"),
            PrimitiveSpecType::Zero { .. } => write!(f, "Zero"),
        }
    }
}

impl<Tgt: Target> LogicalSpec<Tgt> {
    pub fn serial_only(&self) -> bool {
        match self {
            LogicalSpec::Primitive(_, _, serial_only) => *serial_only,
            LogicalSpec::Compose { serial_only, .. } => *serial_only,
        }
    }

    pub fn set_serial_only(&mut self, serial_only: bool) {
        match self {
            LogicalSpec::Primitive(_, _, ref mut s) => *s = serial_only,
            LogicalSpec::Compose {
                serial_only: ref mut s,
                ..
            } => *s = serial_only,
        }
    }

    pub fn operand_count(&self) -> usize {
        match self {
            LogicalSpec::Compose { components, .. } => {
                let (innermost_component, outer_components) = components.split_last().unwrap();
                let mut cnt = innermost_component.typ.operand_count();
                cnt += outer_components
                    .iter()
                    .map(|p| p.typ.operand_count() - 2)
                    .sum::<usize>();
                cnt
            }
            LogicalSpec::Primitive(basics, _, _) => basics.typ.operand_count(),
        }
    }

    pub fn parameters(&self) -> Vec<TensorSpec<Tgt>> {
        match self {
            LogicalSpec::Primitive(basics, auxes, _) => match basics.typ {
                PrimitiveSpecType::Matmul { .. } | PrimitiveSpecType::Conv { .. } => basics
                    .parameter_shapes()
                    .into_iter()
                    .zip(&basics.dtypes)
                    .zip(auxes)
                    .map(|((s, dt), a)| TensorSpec::new_noncanon_with_aux(s, *dt, a.clone()))
                    .collect(),
                PrimitiveSpecType::Move | PrimitiveSpecType::Zero => auxes
                    .iter()
                    .zip(&basics.dtypes)
                    .map(|(a, dtype)| {
                        TensorSpec::new_noncanon_with_aux(
                            basics.spec_shape.clone(),
                            *dtype,
                            a.clone(),
                        )
                    })
                    .collect(),
            },
            LogicalSpec::Compose {
                components,
                operand_auxes,
                serial_only: _,
            } => {
                let mut result_basics = Vec::with_capacity(self.operand_count());
                let mut last_seen_output = None;
                for (i, c) in components.iter().rev().enumerate() {
                    let mut operand_basics: Vec<(Shape, Dtype)> = c
                        .parameter_shapes()
                        .into_iter()
                        .zip(c.dtypes.iter().copied())
                        .collect::<Vec<_>>();
                    last_seen_output = operand_basics.pop();
                    debug_assert!(last_seen_output.is_some());
                    operand_basics.reverse();
                    if i != 0 {
                        operand_basics.pop();
                    }
                    result_basics.append(&mut operand_basics);
                }
                result_basics.reverse();
                result_basics.push(last_seen_output.unwrap());
                debug_assert_eq!(result_basics.len(), operand_auxes.len());
                result_basics
                    .into_iter()
                    .zip(operand_auxes)
                    .map(|((s, d), a)| TensorSpec::new_noncanon_with_aux(s, d, a.clone()))
                    .collect()
            }
        }
    }

    pub fn parameter_shapes(&self) -> Vec<Shape> {
        match self {
            LogicalSpec::Primitive(basics, _, _) => basics.parameter_shapes(),
            LogicalSpec::Compose { .. } => todo!(),
        }
    }

    pub fn inputs(&self) -> Vec<TensorSpec<Tgt>> {
        let mut operands = self.parameters();
        operands.remove(self.output_idx());
        operands
    }

    pub fn output(&self) -> TensorSpec<Tgt> {
        self.parameters()[self.output_idx()].clone()
    }

    pub fn output_idx(&self) -> usize {
        match &self {
            LogicalSpec::Primitive(PrimitiveBasics { typ, .. }, _, _) => typ.output_idx(),
            LogicalSpec::Compose { .. } => self.operand_count() - 1,
        }
    }

    pub fn canonicalize(&mut self) -> Result<(), CanonicalizeError> {
        match self {
            LogicalSpec::Primitive(basics, primitive_aux, _) => match &basics.typ {
                PrimitiveSpecType::Matmul { accum: _ } | PrimitiveSpecType::Conv { accum: _ } => {
                    for (shp, aux) in basics.parameter_shapes().iter().zip(primitive_aux) {
                        aux.canonicalize(shp)
                            .map_err(CanonicalizeError::TensorSpecAuxCanonicalizeError)?;
                    }
                }
                PrimitiveSpecType::Move => {
                    for aux in primitive_aux.iter_mut() {
                        aux.canonicalize(&basics.spec_shape)
                            .map_err(CanonicalizeError::TensorSpecAuxCanonicalizeError)?;
                    }

                    // It source and destination are fully contiguous and the dtypes and layouts
                    // match, then we can canonicalize to a row-major bitwise move. This is a
                    // workaround for not being able to split interleaved layouts with a tile, but
                    // can be generalized to be a useful symmetry-breaking predicate later on.
                    // TODO: Do just that: generalize this caonicalizaton rule.
                    if basics.dtypes.iter().all_equal()
                        && primitive_aux.iter().map(|a| &a.layout).all_equal()
                        && primitive_aux
                            .iter()
                            .all(|aux| aux.contig == aux.layout.contiguous_full())
                    {
                        let rm = row_major(basics.spec_shape.len().try_into().unwrap());
                        let new_contig = rm.contiguous_full();
                        for aux in primitive_aux.iter_mut() {
                            aux.layout = rm.clone();
                            aux.contig = new_contig;
                        }
                    }
                }
                PrimitiveSpecType::Zero => {
                    primitive_aux[0]
                        .canonicalize(&basics.spec_shape)
                        .map_err(CanonicalizeError::TensorSpecAuxCanonicalizeError)?;
                }
            },
            LogicalSpec::Compose { .. } => todo!(),
        }
        Ok(())
    }

    pub fn is_canonical(&self) -> bool {
        match self {
            LogicalSpec::Primitive(basics, primitive_aux, _) => match &basics.typ {
                PrimitiveSpecType::Matmul { accum: _ } | PrimitiveSpecType::Conv { accum: _ } => {
                    for (shp, aux) in basics.parameter_shapes().iter().zip(primitive_aux) {
                        if !aux.is_canonical(shp) {
                            return false;
                        }
                    }
                }
                PrimitiveSpecType::Move => {
                    for aux in primitive_aux {
                        if !aux.is_canonical(&basics.spec_shape) {
                            return false;
                        }
                    }

                    if basics.dtypes.iter().all_equal()
                        && primitive_aux.iter().map(|a| &a.layout).all_equal()
                        && primitive_aux
                            .iter()
                            .all(|aux| aux.contig == aux.layout.contiguous_full())
                        && primitive_aux.iter().any(|aux| {
                            !aux.layout.is_row_major() || aux.contig != aux.layout.contiguous_full()
                        })
                    {
                        return false;
                    }
                }
                PrimitiveSpecType::Zero => {
                    if !primitive_aux[0].is_canonical(&basics.spec_shape) {
                        return false;
                    }
                }
            },
            LogicalSpec::Compose { .. } => todo!(),
        }
        true
    }

    pub fn actions(&self, tiling_depth: Option<NonZeroU32>) -> impl ActionSeq<Tgt> + '_ {
        let iter = self.tile_out_actions(tiling_depth);
        let iter = iter.chain(self.move_actions());
        let iter = iter.chain(Tgt::actions(self));

        match &self {
            LogicalSpec::Primitive(
                PrimitiveBasics {
                    typ,
                    spec_shape: _,
                    dtypes: _,
                },
                _primitive_aux,
                _serial_only,
            ) => match typ {
                PrimitiveSpecType::Matmul { accum } if !*accum => {
                    iter.chain(once(Action::ToAccum)).collect::<Vec<_>>()
                }
                PrimitiveSpecType::Matmul { accum } if *accum => iter
                    .chain(self.split_actions(tiling_depth))
                    .collect::<Vec<_>>(),
                PrimitiveSpecType::Conv { accum } => {
                    if *accum {
                        if self.can_spatial_split() {
                            iter.chain(once(Action::SpatialSplit)).collect::<Vec<_>>()
                        } else {
                            iter.collect::<Vec<_>>()
                        }
                    } else {
                        iter.chain(once(Action::ToAccum)).collect::<Vec<_>>()
                    }
                }
                _ => iter.collect::<Vec<_>>(),
            },
            LogicalSpec::Compose {
                components: _,
                operand_auxes: _,
                serial_only: _,
            } => {
                // TODO: Add head reduce split actions as well.
                iter.chain(self.peel_actions()).collect::<Vec<_>>()
            }
        }
    }

    fn can_spatial_split(&self) -> bool {
        let LogicalSpec::Primitive(PrimitiveBasics { typ, .. }, primitive_aux, _) = self else {
            panic!("can_spatial_split called on non-Primitive spec");
        };
        let PrimitiveSpecType::Conv { accum } = typ else {
            panic!("can_spatial_split called on non-Conv spec");
        };
        if !*accum {
            panic!("can_spatial_split called on non-accum Conv spec");
        };

        let parameters = self.parameters();
        let image_shape = parameters[0].shape();
        let filters_shape = parameters[1].shape();

        if image_shape[2..] != filters_shape[2..] {
            return false;
        }
        for a in primitive_aux {
            if let Some(vector_size) = a.vector_size {
                if vector_size.get() != 1 {
                    return false;
                }
            }
        }
        true
    }

    // TODO: Avoid boxed trait object return type
    fn tile_out_actions(
        &self,
        depth: Option<NonZeroU32>,
    ) -> Box<dyn Iterator<Item = Action<Tgt>> + '_> {
        let serial_only = self.serial_only();
        let output_shape = self.parameter_shapes().swap_remove(self.output_idx());
        let multi_dim = MULTI_DIM_TILING || !serial_only;
        if multi_dim {
            // TODO: Simplfy following, knowing multi_dim is true.
            Box::new(
                gen_tile_sizes::<Tgt>(&output_shape, true, multi_dim, depth).flat_map(
                    move |tile_shape| {
                        let left = once(Action::TileOut(TileOut::MultiLoop {
                            output_shape: tile_shape.clone(),
                            parallel: false,
                        }));
                        let mut right = None;
                        if !serial_only {
                            right = Some(Action::TileOut(TileOut::MultiLoop {
                                output_shape: tile_shape,
                                parallel: true,
                            }));
                        }
                        left.into_iter().chain(right)
                    },
                ),
            )
        } else {
            // Yield all output tilings up to the *maximum* dimension size so that the actions have
            // relatively stable order between Specs.
            let output_tensor_rank = output_shape.len();
            let max_dim_size =
                DimSize::try_from(output_shape.iter().map(|d| d.get()).max().unwrap()).unwrap();
            Box::new(dim_range(max_dim_size, true, depth).flat_map(move |size| {
                (0..output_tensor_rank).flat_map(move |dim| {
                    let dim = u8::try_from(dim).unwrap();
                    let left = once(Action::TileOut(TileOut::SingleLoop {
                        dim,
                        size,
                        parallel: false,
                    }));
                    let mut right = None;
                    if !serial_only {
                        right = Some(Action::TileOut(TileOut::SingleLoop {
                            dim,
                            size,
                            parallel: true,
                        }));
                    }
                    left.into_iter().chain(right)
                })
            }))
        }
    }

    fn split_actions(
        &self,
        tiling_depth: Option<NonZeroU32>,
    ) -> impl Iterator<Item = Action<Tgt>> + '_ {
        let LogicalSpec::Primitive(
            PrimitiveBasics {
                typ, spec_shape, ..
            },
            ..,
        ) = self
        else {
            panic!("split_actions called on non-primitive Spec");
        };
        let PrimitiveSpecType::Matmul { accum } = typ else {
            panic!("split_actions called on non-Matmul");
        };
        if !accum {
            panic!("split_actions called on non-accumulating Matmul");
        }
        let [m, orig_k, n] = spec_shape[..] else {
            unreachable!();
        };

        let operands = self.parameters();
        dim_range(orig_k, false, tiling_depth)
            .filter(move |&new_k| {
                // TODO: Shouldn't this be rejected during application instead?
                operands[0].is_valid_tile_shape(&[m, new_k], false)
                    && operands[1].is_valid_tile_shape(&[new_k, n], false)
            })
            .map(|k| Action::Split { k })
    }

    fn peel_actions(&self) -> impl Iterator<Item = Action<Tgt>> + '_ {
        let LogicalSpec::Compose {
            components,
            operand_auxes: _,
            serial_only: _,
        } = self
        else {
            panic!("peel_actions called on non-Compose Spec");
        };

        let mut results = vec![];

        let o = components[1].parameter_shapes();
        let comp_out_idx = components[1].typ.output_idx();
        let intermediate_shape = &o[comp_out_idx];
        let intermediate_dtype = components[1].dtypes[comp_out_idx];

        for level in Tgt::levels() {
            let vector_bytes = level.vector_bytes();

            for layout in Tgt::move_destination_layouts(intermediate_shape, intermediate_dtype) {
                // TODO: Need to implement `can_move_to`-style logic here.

                if !vector_bytes.is_empty() {
                    for vector_size in gen_vector_sizes(intermediate_dtype, vector_bytes) {
                        results.push(Action::Peel {
                            layout: layout.clone(),
                            level,
                            vector_size: Some(vector_size),
                        });
                    }
                } else {
                    results.push(Action::Peel {
                        layout: layout.clone(),
                        level,
                        vector_size: None,
                    });
                }
            }
        }

        results.into_iter()
    }

    fn move_actions(&self) -> impl Iterator<Item = Action<Tgt>> + '_ {
        // TODO: Don't accumulate. Return an iterator.
        let mut results = Vec::with_capacity(MOVE_RESULTS_CAPACITY);

        for (i, operand) in self.parameters().iter().enumerate() {
            // Yield actions for movement with register file destination, which
            // includes relayouts in registers and movements from level 1 to RF.
            let i = u8::try_from(i).unwrap();
            let operand_dtype = operand.dtype();
            for layout in Tgt::move_destination_layouts(operand.shape(), operand_dtype) {
                // TODO: Prevent moving into packed layouts where strip size equals the whole dim.
                for level in Tgt::possible_destination_levels(operand.level()) {
                    if !operand.can_move_to(&layout, &level) {
                        continue;
                    }

                    for &destination_dtype in
                        iter::once(&operand_dtype).chain(operand_dtype.higher_precision_types())
                    {
                        results.extend(
                            gen_vector_sizes_opt(operand_dtype, level.vector_bytes()).map(
                                |vector_size| {
                                    // This may return Moves with identical source and destination
                                    // TensorSpecs (i.e., within-level copies). These will be filtered in
                                    // [apply_with_aux].
                                    Action::Move {
                                        source_idx: i,
                                        destination_dtype,
                                        destination_level: level,
                                        destination_layout: layout.clone(),
                                        destination_vector_size: vector_size,
                                    }
                                },
                            ),
                        )
                    }
                }
            }
        }

        results.into_iter()
    }

    pub fn input_tilings_for_tile_out(&self, smaller_output: &Tiling) -> TilingInference {
        match self {
            LogicalSpec::Primitive(basics, _, _) => {
                basics.input_tilings_for_tile_out(smaller_output)
            }
            LogicalSpec::Compose { .. } => {
                todo!("Resolve axes.");
                // let mut accumulated_input_tilings = Vec::with_capacity(self.operand_count() - 1);
                // let mut last_output_tiling = smaller_output.clone();
                // for (i, subspec) in components.iter().enumerate().rev() {
                //     let mut subspec_input_tilings =
                //         subspec.input_tilings_for_tile_out(&last_output_tiling);
                //     debug_assert!(
                //         !subspec_input_tilings.is_empty(),
                //         "Compose contains {:?}, which has no inputs",
                //         subspec
                //     );
                //     if i == 0 {
                //         accumulated_input_tilings.extend(subspec_input_tilings);
                //     } else {
                //         accumulated_input_tilings.extend(subspec_input_tilings.drain(1..));
                //         last_output_tiling = subspec_input_tilings.remove(0);
                //     }
                // }
                // accumulated_input_tilings
            }
        }
    }

    // TODO: Can we replace this entirely with Spec shapes?
    pub fn operands_dim_axes(&self) -> Vec<Vec<u8>> {
        match self {
            LogicalSpec::Primitive(basics, _, _) => basics.parameter_dim_axes(),
            LogicalSpec::Compose { components, .. } => {
                let mut max_seen = 0;
                let mut accum: Vec<Vec<u8>> = Vec::new();
                let mut last_out_subs: Option<Vec<u8>> = None;

                for compose_subspec in components.iter().rev() {
                    let mut kls_axes = Self::increment_dims_axes(
                        &compose_subspec.parameter_dim_axes(),
                        &mut max_seen,
                    );
                    if accum.is_empty() {
                        // Drop the output only
                        accum.extend_from_slice(&kls_axes[..kls_axes.len() - 1]);
                        last_out_subs = Some(kls_axes.last().unwrap().clone());
                    } else {
                        assert!(last_out_subs.is_some());
                        assert_eq!(last_out_subs.as_ref().unwrap().len(), kls_axes[0].len());
                        let substitution_dict = kls_axes
                            .first()
                            .unwrap()
                            .iter()
                            .copied()
                            .zip(last_out_subs.unwrap())
                            .collect::<HashMap<_, _>>();
                        kls_axes = Self::sub_axis(&kls_axes, &substitution_dict);
                        last_out_subs = Some(kls_axes.last().unwrap().clone());
                        let mut new_accum = Vec::with_capacity(accum.len() + kls_axes.len());
                        new_accum.extend_from_slice(&kls_axes[1..kls_axes.len() - 1]);
                        new_accum.extend(accum.drain(..accum.len()));
                        mem::swap(&mut accum, &mut new_accum);
                    }
                    max_seen = kls_axes.into_iter().flatten().max().unwrap();
                }

                // Add the Compose' output
                assert!(last_out_subs.is_some());
                accum.push(last_out_subs.unwrap());
                accum
            }
        }
    }

    fn increment_dims_axes(subs: &[Vec<u8>], inc: &mut u8) -> Vec<Vec<u8>> {
        let mut result = Vec::new();
        for dims in subs {
            let mut subresult = Vec::with_capacity(dims.len());
            for &d in dims {
                *inc = (*inc).max(d);
                subresult.push(d + *inc);
            }
            result.push(subresult);
        }
        *inc += 1;
        result
    }

    fn sub_axis(source: &[Vec<u8>], substitutions: &HashMap<u8, u8>) -> Vec<Vec<u8>> {
        let mut result = Vec::new();
        for dims in source {
            let mut subresult = Vec::with_capacity(dims.len());
            for &d in dims {
                subresult.push(*substitutions.get(&d).unwrap_or(&d));
            }
            result.push(subresult);
        }
        result
    }

    // TODO: Need IO? Would inputs alone be sufficient? Caller can check inferred output.
    // TODO: Should move new_operands in.
    pub fn replace_io(&mut self, new_operands: &[TensorSpec<Tgt>]) {
        assert_eq!(new_operands.len(), self.operand_count());
        match self {
            LogicalSpec::Compose {
                components,
                operand_auxes,
                serial_only: _,
            } => {
                let new_inputs = &new_operands[..new_operands.len() - 1];
                let mut remaining_inputs = new_inputs
                    .iter()
                    .map(|t| (t.shape(), t.dtype()))
                    .collect::<Vec<_>>();
                let mut component_inputs: Vec<(Shape, Dtype)> = vec![];
                for component in components.iter_mut().rev() {
                    // Any missing inputs? Gather them here.
                    let needed = component.typ.input_count() - component_inputs.len();
                    component_inputs.extend(
                        remaining_inputs
                            .drain(remaining_inputs.len() - needed..)
                            .map(|(shape, dtype)| (Shape::from(shape), dtype)),
                    );

                    let new_output_shape = {
                        let inp_shapes = component_inputs
                            .iter()
                            .map(|t| t.0.as_slice())
                            .collect::<Vec<_>>();
                        component.typ.infer_output_shape(&inp_shapes)
                    };
                    let mut new_operands = component_inputs.clone();
                    new_operands.push((
                        new_output_shape,
                        component.dtypes[component.typ.output_idx()],
                    ));
                    component.replace_io(
                        new_operands
                            .iter()
                            .map(|(s, d)| (&s[..], *d))
                            .collect::<Vec<_>>()
                            .as_slice(),
                    );

                    // Next loop iteration should have have the output as its own argument.
                    component_inputs.clear();
                    component_inputs.push(new_operands.pop().unwrap());
                }

                // At termination, component_inputs should contain exactly the
                // provided replacement output. If it differs, then the replacement
                // output has an invalid shape.
                debug_assert_eq!(component_inputs.len(), 1);
                debug_assert_eq!(
                    new_operands.last().unwrap().shape(),
                    &component_inputs[0].0[..]
                );
                debug_assert_eq!(component_inputs[0].1, new_operands.last().unwrap().dtype());

                *operand_auxes = new_operands.iter().map(|t| t.aux.clone()).collect();
            }
            LogicalSpec::Primitive(basics, primitive_aux, _) => {
                basics.replace_io(
                    &new_operands
                        .iter()
                        .map(|o| (o.shape(), o.dtype))
                        .collect::<Vec<_>>(),
                );

                debug_assert_eq!(primitive_aux.len(), new_operands.len());
                for i in 0..primitive_aux.len() {
                    primitive_aux[i] = new_operands[i].aux.clone();
                }
            }
        }
        debug_assert!(
            self.parameters()
                .iter()
                .zip(new_operands)
                .all(|(a, b)| { a == b }),
            "Parameter mismatch after replace_io; Spec is {} after replacing with [{}]",
            self,
            new_operands.iter().map(|o| o.to_string()).join(", "),
        );
    }

    pub fn output_is_read(&self) -> bool {
        match self {
            LogicalSpec::Primitive(PrimitiveBasics { typ, .. }, _, _) => typ.output_is_read(),
            LogicalSpec::Compose { components, .. } => components[0].typ.output_is_read(),
        }
    }

    pub fn clone_as_accum(&self) -> Self {
        let mut cloned = self.clone();
        match &mut cloned {
            LogicalSpec::Primitive(basics, _, _) => match &mut basics.typ {
                PrimitiveSpecType::Matmul { accum } | PrimitiveSpecType::Conv { accum } => {
                    *accum = true;
                }
                _ => panic!("Cannot clone_as_accum for {:?}", self),
            },
            LogicalSpec::Compose { .. } => todo!("Compose can accumulate if head can."),
        }
        cloned
    }
}

impl<Tgt: Target> Display for LogicalSpec<Tgt> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let LogicalSpec::Compose {
            components,
            operand_auxes: _,
            serial_only,
        } = self
        {
            let operands = self.parameters();
            let (output, external_inputs) = operands.split_last().unwrap();
            debug_assert_eq!(self.output_idx(), external_inputs.len());
            return write!(
                f,
                "Compose(({}), [{}, out={}],{})",
                join_into_string(components.iter().map(|c| c.typ), ", "),
                join_into_string(external_inputs, ", "),
                output,
                if *serial_only { ", serial" } else { "" }
            );
        }

        let header = match self {
            LogicalSpec::Primitive(PrimitiveBasics { typ, .. }, _, _) => format!("{}", typ),
            LogicalSpec::Compose { .. } => todo!(),
        };

        let operand_str = self
            .parameters()
            .iter()
            .map(|o| format!("{}", o))
            .collect::<Vec<_>>()
            .join(", ");
        let serial_str = if self.serial_only() { ", serial" } else { "" };

        write!(f, "{}({}{})", header, operand_str, serial_str)
    }
}

impl<Tgt, F, A, Aa, const N: usize> SurMap for SpecSurMap<Tgt, F, A, Aa>
where
    Tgt: Target,
    Tgt::Level: CanonicalBimap,
    <Tgt::Level as CanonicalBimap>::Bimap: BiMap<Domain = Tgt::Level, Codomain = u8>,
    F: Fn(&[DimSize], Dtype) -> A,
    A: SurMap<Domain = TensorSpecAux<Tgt>, Codomain = (Aa, [BimapInt; N])>,
    A::DomainIter: 'static,
    Aa: Clone,
{
    type Domain = Spec<Tgt>;
    type Codomain = <LogicalSpecSurMap<Tgt, F, A, Aa> as SurMap>::Codomain;
    type DomainIter = Box<dyn Iterator<Item = Self::Domain>>;

    fn apply(&self, t: &Self::Domain) -> Self::Codomain {
        let mut initial = SurMap::apply(&self.logical_spec_surmap, &t.0);
        initial
            .1
            .extend(BiMap::apply(&self.memory_limits_bimap, &t.1));
        initial
    }

    fn apply_inverse(&self, i: &Self::Codomain) -> Self::DomainIter {
        let (left, right) = i;
        let (inner_right, memory_right) = right.split_at(i.1.len() - Tgt::levels().len());

        let remaining_value = (
            left.clone(),
            inner_right.iter().copied().map_into().collect(),
        );
        let m = BiMap::apply_inverse(&self.memory_limits_bimap, &memory_right.into());
        Box::new(
            self.logical_spec_surmap
                .apply_inverse(&remaining_value)
                .map(move |ls| Spec(ls, m.clone())),
        )
    }
}

impl<Tgt, F, A, Aa> LogicalSpecSurMap<Tgt, F, A, Aa> {
    pub fn new(primitive_basics_bimap: PrimitiveBasicsBimap, aux_surmap_fn: F) -> Self {
        Self {
            primitive_basics_bimap,
            aux_surmap_fn,
            marker: PhantomData,
        }
    }
}

impl<Tgt, F, A, Aa, const N: usize> SurMap for LogicalSpecSurMap<Tgt, F, A, Aa>
where
    Tgt: Target,
    Tgt::Level: CanonicalBimap,
    <Tgt::Level as CanonicalBimap>::Bimap: BiMap<Domain = Tgt::Level, Codomain = u8>,
    F: Fn(&[DimSize], Dtype) -> A,
    A: SurMap<Domain = TensorSpecAux<Tgt>, Codomain = (Aa, [BimapInt; N])>,
    A::DomainIter: 'static,
    Aa: Clone,
{
    type Domain = LogicalSpec<Tgt>;
    type Codomain = ((SpecKey, Vec<Aa>), Vec<BimapInt>);
    type DomainIter = Box<dyn Iterator<Item = Self::Domain> + Send>;

    fn apply(&self, spec: &LogicalSpec<Tgt>) -> Self::Codomain {
        match spec {
            LogicalSpec::Primitive(basics, auxes, serial_only) => {
                let (key, mut pt) = BiMap::apply(&self.primitive_basics_bimap, basics);
                let aux_keys = auxes
                    .iter()
                    .zip(basics.parameter_shapes())
                    .zip(&basics.dtypes)
                    .map(|((tensor_aux, tensor_shape), dtype)| {
                        let aux_bimap = (self.aux_surmap_fn)(&tensor_shape, *dtype);
                        let (aux_key, aux_pt) = aux_bimap.apply(tensor_aux);
                        pt.extend(aux_pt);
                        aux_key
                    })
                    .collect();
                pt.push(!*serial_only as _);
                ((key, aux_keys), pt)
            }
            LogicalSpec::Compose { .. } => todo!(),
        }
    }

    fn apply_inverse(&self, i: &Self::Codomain) -> Self::DomainIter {
        let ((key, aux_keys), pt) = i;
        let dtypes = key.dtypes();
        let operand_count = aux_keys.len();

        let pt_without_serial = &pt[..pt.len() - 1];
        let (basics_pt, tensor_aux_pts) =
            pt_without_serial.split_at(pt.len() - (operand_count * N) - 1);
        let serial = pt[pt.len() - 1] == 0;

        let primitive_basics = BiMap::apply_inverse(
            &self.primitive_basics_bimap,
            &(key.clone(), basics_pt.into()),
        );
        let parameter_shapes = primitive_basics.parameter_shapes();

        Box::new(
            (0..operand_count)
                .map(move |i| {
                    let Ok(tap) = (&tensor_aux_pts[i * N..(i + 1) * N]).try_into() else {
                        panic!("Couldn't reverse the TensorSpecAux pt.");
                    };
                    let aux_surmap = (self.aux_surmap_fn)(&parameter_shapes[i], dtypes[i]);
                    // TODO: Avoid collect, which is here to avoid needing the iter to be Clone
                    aux_surmap
                        .apply_inverse(&(aux_keys[i].clone(), tap))
                        .collect::<Vec<_>>()
                })
                .multi_cartesian_product()
                .map(move |tensor_auxes| {
                    LogicalSpec::Primitive(primitive_basics.clone(), tensor_auxes, serial)
                }),
        )
    }
}

impl BiMap for PrimitiveBasicsBimap {
    type Domain = PrimitiveBasics;
    type Codomain = (SpecKey, Vec<BimapInt>);

    fn apply(&self, basics: &PrimitiveBasics) -> Self::Codomain {
        let PrimitiveBasics {
            typ,
            spec_shape,
            dtypes,
        } = basics;
        let shifted_shape = spec_shape.iter().map(|d| d.get()).map(|d| {
            if self.binary_scale_shapes {
                if !d.is_power_of_two() {
                    panic!("Given non-zero/power-of-two shape {}", d);
                }
                bit_length_u32(prev_power_of_two_u32(d - 1))
            } else {
                d - 1
            }
        });
        match *typ {
            PrimitiveSpecType::Matmul { accum } => {
                let v = once(!accum as _).chain(shifted_shape).collect();
                (
                    SpecKey::Matmul {
                        dtypes: dtypes.as_slice().try_into().unwrap(),
                    },
                    v,
                )
            }
            PrimitiveSpecType::Conv { accum } => {
                let mut v: Vec<_> = once(!accum as _).chain(shifted_shape).collect();
                // Conv's image dimensions must be larger than or equal to the corresponding filter
                // dimensions (the final two dimensions in `v`/`shifted_shape`), so we'll subtract
                // the filter sizes from the image sizes, thereby normalizing the image dims. to
                // zero.
                v[4] -= v[6];
                v[5] -= v[7];
                (
                    SpecKey::Conv {
                        dtypes: dtypes.as_slice().try_into().unwrap(),
                    },
                    v,
                )
            }
            PrimitiveSpecType::Move => (
                SpecKey::Move {
                    dtypes: dtypes.as_slice().try_into().unwrap(),
                },
                shifted_shape.collect(),
            ),
            PrimitiveSpecType::Zero => {
                (SpecKey::Zero { dtype: dtypes[0] }, shifted_shape.collect())
            }
        }
    }

    fn apply_inverse(&self, c: &Self::Codomain) -> Self::Domain {
        let (key, v) = c;
        let basics = match key {
            SpecKey::Matmul { dtypes } | SpecKey::Conv { dtypes } => {
                let accum = v[0] == 0;
                let typ = match key {
                    SpecKey::Matmul { .. } => PrimitiveSpecType::Matmul { accum },
                    SpecKey::Conv { .. } => PrimitiveSpecType::Conv { accum },
                    _ => unreachable!(),
                };

                let mut spec_shape: Vec<BimapInt> = v.iter().skip(1).copied().collect();
                // Reverse the normalization of image dimensions (see `apply`).
                if matches!(key, SpecKey::Conv { .. }) {
                    spec_shape[3] += spec_shape[5];
                    spec_shape[4] += spec_shape[6];
                }
                for d in &mut spec_shape[..] {
                    if self.binary_scale_shapes {
                        *d = u32::try_from((bit_length_inverse(*d) + 1).next_power_of_two())
                            .unwrap();
                    } else {
                        *d += 1;
                    }
                }

                PrimitiveBasics {
                    typ,
                    spec_shape: spec_shape
                        .iter()
                        .map(|&d| DimSize::new(d).unwrap())
                        .collect(),
                    dtypes: dtypes.as_slice().into(),
                }
            }
            SpecKey::Move { dtypes } => PrimitiveBasics {
                typ: PrimitiveSpecType::Move,
                spec_shape: BiMap::apply_inverse(&ShapeBimap(self.binary_scale_shapes), v),
                dtypes: dtypes.as_slice().into(),
            },
            SpecKey::Zero { dtype } => PrimitiveBasics {
                typ: PrimitiveSpecType::Zero,
                spec_shape: BiMap::apply_inverse(&ShapeBimap(self.binary_scale_shapes), v),
                dtypes: vec![*dtype],
            },
        };
        basics
    }
}

impl BiMap for ShapeBimap {
    type Domain = Vec<DimSize>;
    type Codomain = Vec<BimapInt>;

    fn apply(&self, shape: &Self::Domain) -> Self::Codomain {
        shape
            .iter()
            .map(|d| d.get())
            .map(|d| {
                if self.0 {
                    if !d.is_power_of_two() {
                        panic!("Given non-zero/power-of-two shape {}", d);
                    }
                    bit_length_u32(prev_power_of_two_u32(d - 1))
                } else {
                    d - 1
                }
            })
            .collect()
    }

    fn apply_inverse(&self, i: &Self::Codomain) -> Self::Domain {
        i.iter()
            .map(|&d| {
                DimSize::new(if self.0 {
                    u32::try_from((bit_length_inverse(d) + 1).next_power_of_two()).unwrap()
                } else {
                    d + 1
                })
                .unwrap()
            })
            .collect()
    }
}

#[cfg(test)]
impl<Tgt: Target> proptest::arbitrary::Arbitrary for LogicalSpec<Tgt> {
    type Parameters = Option<DimSize>;
    type Strategy = proptest::strategy::BoxedStrategy<LogicalSpec<Tgt>>;

    fn arbitrary_with(args: Self::Parameters) -> Self::Strategy {
        use crate::tensorspec::TensorSpecArbMaxShape;
        use proptest::prelude::*;

        // TODO: Generate Compose as well.
        (any_with::<PrimitiveBasics>(args), any::<bool>())
            .prop_flat_map(|(basics, serial_only)| {
                // TODO: These don't all make sense. Are they canonical for shapes?
                let auxes_strategy = basics
                    .parameter_shapes()
                    .into_iter()
                    .map(|s| any_with::<TensorSpecAux<Tgt>>(TensorSpecArbMaxShape(s)))
                    .collect::<Vec<_>>();
                (Just(basics), auxes_strategy, Just(serial_only))
            })
            .prop_map(|(basics, auxes, serial_only)| {
                LogicalSpec::Primitive(basics, auxes, serial_only)
            })
            .prop_filter("Layout must be applicable to TensorSpec shape", |s| {
                s.clone().canonicalize().is_ok()
            })
            .boxed()
    }
}

#[cfg(test)]
pub fn arb_canonical_logical_spec<Tgt: Target>(
    max_size: Option<DimSize>,
) -> impl proptest::strategy::Strategy<Value = LogicalSpec<Tgt>> {
    use proptest::prelude::*;

    any_with::<LogicalSpec<Tgt>>(max_size).prop_filter_map(
        "Must be possible to canonicalize LogicalSpec",
        |mut s| {
            if s.canonicalize().is_err() {
                return None;
            }
            Some(s)
        },
    )
}

// TODO: Modify to return an `impl Iterator` of some kind instead of a `Box`.
fn gen_tile_sizes<Tgt: Target>(
    tensor_shape: &[DimSize],
    drop_given: bool,
    multi_dim: bool,
    depth: Option<NonZeroU32>,
) -> Box<dyn Iterator<Item = Shape> + 'static> {
    if tensor_shape.is_empty() {
        return Box::new(iter::empty());
    } else if tensor_shape.len() == 1 {
        let one_dim = tensor_shape[0];
        return Box::new(dim_range(one_dim, true, depth).filter_map(move |d| {
            if drop_given && d == one_dim {
                None
            } else {
                Some(vec![d])
            }
        }));
    }

    if multi_dim {
        let tensor_shape = tensor_shape.to_vec();
        Box::new(
            gen_tile_sizes::<Tgt>(&tensor_shape[1..], false, multi_dim, depth).flat_map(
                move |rest| {
                    let tensor_shape = tensor_shape.clone();
                    dim_range(tensor_shape[0], true, depth).flat_map(move |d| {
                        let mut new_shape = vec![d];
                        new_shape.extend(rest.clone());
                        if drop_given && tensor_shape == new_shape[..] {
                            None
                        } else {
                            Some(new_shape)
                        }
                    })
                },
            ),
        )
    } else {
        let tensor_shape = tensor_shape.to_vec();
        let own_shape_iter = if !drop_given
            && tensor_shape
                .iter()
                .map(|d: &DimSize| d.get())
                .all(is_power_of_two_u32)
        {
            Either::Left(once(tensor_shape.clone()))
        } else {
            Either::Right(iter::empty())
        };
        let smaller_tiles_iter = (0..tensor_shape.len()).flat_map(move |dim| {
            let tensor_shape = tensor_shape.clone();
            dim_range(tensor_shape[dim], false, depth).map(move |d| {
                let mut new_shape = tensor_shape.clone();
                new_shape[dim] = d;
                new_shape
            })
        });
        Box::new(smaller_tiles_iter.chain(own_shape_iter))
    }
}

pub fn gen_vector_sizes(dtype: Dtype, vector_bytes: &[u32]) -> impl Iterator<Item = DimSize> + '_ {
    assert!(!vector_bytes.is_empty());
    assert!(
        vector_bytes
            .iter()
            .all(|&vb| vb % u32::from(dtype.size()) == 0),
        "vector_bytes must be a multiple of dtype size"
    );
    vector_bytes.iter().map(move |&vb| {
        let value_cnt = vb / u32::from(dtype.size());
        DimSize::new(value_cnt).unwrap()
    })
}

pub fn gen_vector_sizes_opt(
    dtype: Dtype,
    vector_bytes: &[u32],
) -> impl Iterator<Item = Option<DimSize>> + '_ {
    let mut iter_a = None;
    let mut iter_b = None;
    if vector_bytes.is_empty() {
        iter_a = Some(once(None));
    } else {
        iter_b = Some(gen_vector_sizes(dtype, vector_bytes).map(Some));
    }
    iter_a
        .into_iter()
        .flatten()
        .chain(iter_b.into_iter().flatten())
}

pub fn dim_range(
    dim_size: DimSize,
    include_end: bool,
    depth: Option<NonZeroU32>,
) -> impl Iterator<Item = DimSize> {
    let start = depth
        .map(|d| {
            assert!(dim_size.is_power_of_two());
            dim_size.trailing_zeros().saturating_sub(d.get())
        })
        .unwrap_or(0);
    let it = (start..)
        .map(|power| 2u32.pow(power))
        .take_while(move |x| *x < dim_size.get())
        .map(|x| DimSize::new(x).unwrap());

    it.chain(once(if include_end { Some(dim_size) } else { None }).flatten())
}

// TODO: Drop in favor of primary output shape inference.
pub fn conv_infer_output_shape(image_shape: &[DimSize], filters_shape: &[DimSize]) -> Shape {
    let batch_cnt = image_shape[0];
    let channels = image_shape[1];
    let filter_cnt = filters_shape[0];
    // TODO: We don't need to store this dimension twice.
    assert_eq!(
        channels, filters_shape[1],
        "Image had {} channels and filters had {}",
        channels, filters_shape[1]
    );
    vec![batch_cnt, filter_cnt]
        .into_iter()
        .chain(image_shape[2..].iter().zip(filters_shape[2..].iter()).map(
            |(&img_dim, &filt_dim)| {
                assert!(
                    img_dim >= filt_dim,
                    "Image dimension {} was smaller than filter dimension {}",
                    img_dim,
                    filt_dim
                );
                DimSize::new(img_dim.get() - filt_dim.get() + 1).unwrap()
            },
        ))
        .collect()
}

pub mod macros {
    pub mod internal {
        use crate::common::DimSize;
        pub trait IntoDimSize {
            fn into_dim_size(self) -> DimSize;
        }
        impl IntoDimSize for DimSize {
            fn into_dim_size(self) -> DimSize {
                self
            }
        }
        impl IntoDimSize for u32 {
            fn into_dim_size(self) -> DimSize {
                DimSize::new(self).unwrap()
            }
        }
    }

    #[macro_export]
    macro_rules! shape {
        ($dim:expr; $n:expr) => {{
            use $crate::spec::macros::internal::IntoDimSize;
            // Bind to a variable with an explicit type to help out type inference.
            let sv: $crate::common::Shape = vec![ ($dim).into_dim_size(); $n ];
            sv
        }};
        ($($dim:expr),*$(,)*) => {{
            use $crate::spec::macros::internal::IntoDimSize;
            // Bind to a variable with an explicit type to help out type inference.
            let sv: $crate::common::Shape = vec![ $( ($dim).into_dim_size() ),* ];
            sv
        }};
    }

    #[macro_export]
    macro_rules! lspec {
        ( $typ:tt( $shp:expr, $( ($($opterms:tt)*) ),+, serial ) ) => {{
            lspec!(@inner $typ($shp, $( ($($opterms)*) ),* , true))
        }};
        ( $typ:tt( $shp:expr, $( ($($opterms:tt)*) ),+ ) ) => {{
            lspec!(@inner $typ($shp, $( ($($opterms)*) ),* , false))
        }};
        ( @inner $typ:tt( $shp:expr, $( ($($opterms:tt)*) ),*, $s:literal ) ) => {{
            use $crate::spec::macros::internal::IntoDimSize;

            let auxes = [ $( lspec!(@tensorspecaux_tup $($opterms)*) ),* ];
            let dtypes = auxes.iter().map(|v| v.0.clone()).collect();
            let basics = PrimitiveBasics {
                typ: lspec!(@primitive_spec_type $typ),
                spec_shape: ($shp).into_iter().map(|x| x.into_dim_size()).collect(),
                dtypes,
            };
            LogicalSpec::Primitive(
                basics,
                auxes.into_iter().map(|v| v.1).collect(),
                $s,
            )
        }};

        ( @tensorspecaux_tup $dt:tt, $level:expr, $layout:expr, c0, ua ) => {
            lspec!(@tensorspecaux_tup_inner $dt, $level, $layout, None, false, false)
        };
        ( @tensorspecaux_tup $dt:tt, $level:expr, $layout:expr, c0 ) => {
            lspec!(@tensorspecaux_tup_inner $dt, $level, $layout, None, false, true)
        };
        ( @tensorspecaux_tup $dt:tt, $level:expr, $layout:expr, ua ) => {
            lspec!(@tensorspecaux_tup_inner $dt, $level, $layout, None, true, false)
        };
        ( @tensorspecaux_tup $dt:tt, $level:expr, $layout:expr ) => {
            lspec!(@tensorspecaux_tup_inner $dt, $level, $layout, None, true, true)
        };
        ( @tensorspecaux_tup $dt:tt, $level:expr, $layout:expr, $vs:expr, c0, ua ) => {
            lspec!(@tensorspecaux_tup_inner $dt, $level, $layout, Some($vs), false, false)
        };
        ( @tensorspecaux_tup $dt:tt, $level:expr, $layout:expr, $vs:expr, c0 ) => {
            lspec!(@tensorspecaux_tup_inner $dt, $level, $layout, Some($vs), false, true)
        };
        ( @tensorspecaux_tup $dt:tt, $level:expr, $layout:expr, $vs:expr, ua ) => {
            lspec!(@tensorspecaux_tup_inner $dt, $level, $layout, Some($vs), true, false)
        };
        ( @tensorspecaux_tup $dt:tt, $level:expr, $layout:expr, $vs:expr ) => {
            lspec!(@tensorspecaux_tup_inner $dt, $level, $layout, Some($vs), true, true)
        };

        // TODO: Accept contiguousnesses other than fully contig. or not at all.
        ( @tensorspecaux_tup_inner $dt:tt, $level:expr, $layout:expr, $vs:expr,
          $c:literal, $a:literal ) =>
        {{
            let layout: $crate::layout::Layout = $layout;
            let contig = if $c {
                layout.contiguous_full()
            } else {
                layout.contiguous_none()
            };
            (
                lspec!(@dt_convert $dt),
                TensorSpecAux {
                    contig,
                    aligned: $a,
                    level: $level,
                    layout,
                    vector_size: ($vs).map(|x: u32| {
                        $crate::common::DimSize::try_from(x).unwrap()
                    }),
                },
            )
        }};

        ( @primitive_spec_type Zero ) => {
            PrimitiveSpecType::Zero
        };
        ( @primitive_spec_type Move ) => {
            PrimitiveSpecType::Move
        };
        ( @primitive_spec_type Matmul ) => {
            PrimitiveSpecType::Matmul { accum: false }
        };
        ( @primitive_spec_type MatmulAccum ) => {
            PrimitiveSpecType::Matmul { accum: true }
        };
        ( @primitive_spec_type Conv ) => {
            PrimitiveSpecType::Conv { accum: false }
        };
        ( @primitive_spec_type ConvAccum ) => {
            PrimitiveSpecType::Conv { accum: true }
        };

        ( @dt_convert u8 ) => {
            $crate::common::Dtype::Uint8
        };
        ( @dt_convert i8 ) => {
            $crate::common::Dtype::Sint8
        };
        ( @dt_convert u16 ) => {
            $crate::common::Dtype::Uint16
        };
        ( @dt_convert i16 ) => {
            $crate::common::Dtype::Sint16
        };
        ( @dt_convert u32 ) => {
            $crate::common::Dtype::Uint32
        };
        ( @dt_convert i32 ) => {
            $crate::common::Dtype::Sint32
        };
        ( @dt_convert f32 ) => {
            $crate::common::Dtype::Float32
        };
        ( @dt_convert bf16 ) => {
            $crate::common::Dtype::Bfloat16
        };
        ( @dt_convert $val:expr ) => {
            $val
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imp::{visit_leaves, Impl, ImplExt, ImplNode};
    use crate::memorylimits::{arb_memorylimits_ext, MemVec, MemoryAllocation};
    use crate::scheduling::ApplyError;
    use crate::target::{ArmTarget, Target, X86Target};
    use crate::tensorspec::TensorSpecArbMaxShape;
    use crate::utils::{next_binary_power, sum_seqs};
    use crate::{layout::row_major, target::CpuMemoryLevel::GL};
    use crate::{lspec, shape};
    use proptest::prelude::*;

    #[test]
    fn test_lspec_1() {
        let spec: LogicalSpec<X86Target> = lspec!(MatmulAccum(
            [2, 3, 3],
            (u8, GL, row_major(2)),
            (i8, GL, row_major(2), c0),
            (u16, GL, row_major(2), ua),
            serial
        ));
        let lhs = TensorSpecAux {
            contig: row_major(2).contiguous_full(),
            aligned: true,
            level: GL,
            layout: row_major(2),
            vector_size: None,
        };
        let rhs = TensorSpecAux {
            contig: row_major(2).contiguous_none(),
            aligned: true,
            level: GL,
            layout: row_major(2),
            vector_size: None,
        };
        let out = TensorSpecAux {
            contig: row_major(2).contiguous_full(),
            aligned: false,
            level: GL,
            layout: row_major(2),
            vector_size: None,
        };
        let expected = LogicalSpec::<X86Target>::Primitive(
            PrimitiveBasics {
                typ: PrimitiveSpecType::Matmul { accum: true },
                spec_shape: shape![2, 3, 3],
                dtypes: vec![Dtype::Uint8, Dtype::Sint8, Dtype::Uint16],
            },
            vec![lhs, rhs, out],
            true,
        );
        assert_eq!(spec, expected);
    }

    #[test]
    fn test_gen_tile_sizes_empty() {
        assert_eq!(
            gen_tile_sizes::<X86Target>(&[], false, false, None).count(),
            0
        );
        assert_eq!(
            gen_tile_sizes::<X86Target>(&[], true, false, None).count(),
            0
        );
        assert_eq!(
            gen_tile_sizes::<X86Target>(&[], false, true, None).count(),
            0
        );
        assert_eq!(
            gen_tile_sizes::<X86Target>(&[], false, false, None).count(),
            0
        );
    }

    #[test]
    fn test_gen_tile_sizes_dim_1_multi_dim() {
        shared_test_gen_tile_sizes_dim_1(true);
    }

    #[test]
    fn test_gen_tile_sizes_dim_1_single_dim() {
        shared_test_gen_tile_sizes_dim_1(false);
    }

    #[test]
    fn test_gen_tile_sizes_dim_2_multi_dim() {
        assert_gen_tile_sizes(
            shape![2, 2],
            [shape![1, 1], shape![1, 2], shape![2, 1], shape![2, 2]],
            false,
            true,
        );
        assert_gen_tile_sizes(
            shape![2, 2],
            [shape![1, 1], shape![1, 2], shape![2, 1]],
            true,
            true,
        );
    }

    #[test]
    fn test_gen_tile_sizes_dim_2_multi_dim_non_powers_of_two() {
        assert_gen_tile_sizes(
            shape![2, 3],
            [
                shape![1, 1],
                shape![1, 2],
                shape![1, 3],
                shape![2, 1],
                shape![2, 2],
                shape![2, 3],
            ],
            false,
            true,
        );
        assert_gen_tile_sizes(
            shape![2, 3],
            [
                shape![1, 1],
                shape![1, 2],
                shape![1, 3],
                shape![2, 1],
                shape![2, 2],
            ],
            true,
            true,
        );
    }

    #[test]
    fn test_gen_tile_sizes_dim_2_single_dim() {
        assert_gen_tile_sizes(
            shape![2, 2],
            [shape![1, 2], shape![2, 1], shape![2, 2]],
            false,
            false,
        );
        assert_gen_tile_sizes(shape![2, 2], [shape![1, 2], shape![2, 1]], true, false);
    }

    #[test]
    fn test_gen_tile_sizes_dim_2_single_dim_non_powers_of_two() {
        for drop_given in [true, false] {
            assert_gen_tile_sizes(
                shape![2, 3],
                [shape![1, 3], shape![2, 1], shape![2, 2]],
                drop_given,
                false,
            );
        }
    }

    proptest! {
        #[test]
        fn test_no_action_panics_x86(spec in any::<Spec<X86Target>>()) {
            shared_test_no_action_panics(spec);
        }

        #[test]
        fn test_no_action_panics_arm(spec in any::<Spec<ArmTarget>>()) {
            shared_test_no_action_panics(spec);
        }

        #[test]
        fn test_actions_are_valid_through_consumed_memory_x86(
            logical_spec in arb_canonical_logical_spec::<X86Target>(None)
        ) {
            shared_test_actions_are_valid_through_consumed_memory(logical_spec)
        }

        #[test]
        fn test_actions_are_valid_through_consumed_memory_arm(
            logical_spec in arb_canonical_logical_spec::<X86Target>(None)
        ) {
            shared_test_actions_are_valid_through_consumed_memory(logical_spec)
        }

        #[test]
        fn test_canonicalize_is_noop_if_already_canonical(
            logical_spec in any::<LogicalSpec<X86Target>>()
        ) {
            let mut canonicalized_logical_spec = logical_spec.clone();
            canonicalized_logical_spec.canonicalize().unwrap();
            if logical_spec == canonicalized_logical_spec {
                prop_assert!(
                    logical_spec.is_canonical(),
                    "LogicalSpec::is_canonical was false, but canonicalizing {} was a no-op",
                    logical_spec
                );
            } else {
                prop_assert!(
                    !logical_spec.is_canonical(),
                    "LogicalSpec::is_canonical was true, but {} was canonicalized to {}",
                    logical_spec, canonicalized_logical_spec
                );
            }
        }

        #[test]
        fn test_canonicalizing_specs_canonicalizes_parameters(
            logical_spec in any::<LogicalSpec<X86Target>>()
        ) {
            let mut logical_spec = logical_spec;
            match logical_spec.canonicalize() {
                Ok(()) => {
                    for p in logical_spec.parameters() {
                        let mut recanonicalized = p.clone();
                        recanonicalized.canonicalize().unwrap();
                        assert_eq!(p, recanonicalized);
                    }
                }
                Err(_) => {
                    // If we can't canonicalize, there's nothing to test.
                }
            }
        }

        #[test]
        fn test_canonicalizing_move_tiled_to_one_canonicalizes_parameters(
            spec in
                (1usize..=4)
                    .prop_flat_map(|rank| (Just(rank), 0..rank))
                    .prop_flat_map(|(rank, nonone_idx)| {
                        (vec![1u32..=4; nonone_idx],
                         2u32..=4,
                         vec![1u32..=4; rank - nonone_idx - 1],
                        any::<Dtype>())
                    })
                    .prop_flat_map(|(left, si, right, dtype)| {
                        let shape =
                            left.into_iter().chain(iter::once(si)).chain(right).collect::<Vec<_>>();
                        let basics = PrimitiveBasics {
                            typ: PrimitiveSpecType::Move,
                            spec_shape: Shape::from(shape.into_iter().map(|x| DimSize::new(x).unwrap()).collect::<Vec<_>>()),
                            dtypes: vec![dtype, dtype],
                        };
                        let auxes_strategy = basics
                            .parameter_shapes()
                            .into_iter()
                            .map(|s| any_with::<TensorSpecAux<X86Target>>(TensorSpecArbMaxShape(s)))
                            .collect::<Vec<_>>();
                        (Just(basics), auxes_strategy, any::<bool>())
                    })
                    .prop_filter_map("Spec should be canonical", |(basics, auxes, serial_only)| {
                        let s = Spec(LogicalSpec::Primitive(basics, auxes, serial_only), X86Target::max_mem());
                        if s.is_canonical() {
                            Some(s)
                        } else {
                            None
                        }
                    })
        ) {
            let LogicalSpec::Primitive(PrimitiveBasics { spec_shape, ..}, _, _) = &spec.0 else {
                unreachable!();
            };
            let tile_out_result = Action::TileOut(TileOut::MultiLoop { output_shape: shape![1; spec_shape.len()], parallel: false })
                .apply(&spec).unwrap_or_else(|_| panic!("Couldn't tile Spec {} to single value", spec));
            let ImplNode::SpecApp(child_spec_app) = &tile_out_result.children()[0] else {
                panic!("First child was not a SpecApp; was: {:?}", tile_out_result.children()[0]);
            };
            let mut tiled_logical_spec = child_spec_app.0.0.clone();
            tiled_logical_spec.canonicalize().unwrap();
            assert!(tiled_logical_spec.parameters().iter().all(|p| {
                p.shape().iter().all(|&d| d.get() == 1)
            }));
            assert!(tiled_logical_spec.parameters().iter().all(|p| {
                let mut c = p.clone();
                c.canonicalize().unwrap();
                p == &c
            }));
        }

        #[test]
        fn test_move_actions_never_returns_within_level_copy(spec in any::<Spec<X86Target>>()) {
            for action in spec.0.actions(None) {
                if let Ok(ImplNode::MoveLet(move_let)) = action.apply(&spec) {
                    assert_ne!(&move_let.source_spec, move_let.introduced.spec(),
                        "Copying MoveLet introduced by action {:?}", action);
                }
            }
        }

        #[test]
        fn test_action_applies_everywhere_down_through_peak_memory(
            (spec, action, _, lower_limit) in arb_spec_action_and_lower_limit::<X86Target>()
        ) {
            let lower_spec = Spec(spec.0.clone(), lower_limit);
            assert!(lower_spec.0.actions(None).into_iter().contains(&action),
                "Action {:?} was not present in lower-limits Spec {:?}",
                action, lower_spec);
        }

        #[test]
        fn test_no_action_produces_same_spec_with_higher_memory_limit_x86(
            spec in any::<Spec<X86Target>>()
        ) {
            shared_test_no_action_produces_same_spec_with_higher_memory_limit(&spec)
        }

        #[test]
        fn test_no_action_produces_same_spec_with_higher_memory_limit_arm(
            spec in any::<Spec<ArmTarget>>()
        ) {
            shared_test_no_action_produces_same_spec_with_higher_memory_limit(&spec)
        }

        #[test]
        fn test_actions_produce_canonical_subspecs(
            spec in any::<Spec<X86Target>>()
        ) {
            spec.0.actions(None).into_iter().for_each(|action| {
                let Ok(applied) = action.apply(&spec) else {
                    return;
                };
                visit_leaves(&applied, &mut |leaf| {
                    if let ImplNode::SpecApp(spec_app) = leaf {
                        assert!(
                            spec_app.0.is_canonical(),
                            "Action {:?} applied to {} produced non-canonical {} (should be {})",
                            action,
                            spec,
                            spec_app.0,
                            {
                                let mut c = spec_app.0.clone();
                                c.canonicalize().unwrap();
                                c
                            }
                        );
                    }
                    true
                });
            });
        }

        #[test]
        fn test_primitivebasicsbimap_is_invertible(basics in any::<PrimitiveBasics>()) {
            // TODO: Also test binary_scale_shapes = true
            let bimap = PrimitiveBasicsBimap {
                binary_scale_shapes: false,
            };
            let projection = BiMap::apply(&bimap, &basics);
            let reversed = BiMap::apply_inverse(&bimap, &projection);
            assert_eq!(basics, reversed);
        }
    }

    fn shared_test_no_action_panics<Tgt: Target>(spec: Spec<Tgt>) {
        for action in spec.0.actions(None) {
            let _ = action.apply(&spec);
        }
    }

    fn shared_test_no_action_produces_same_spec_with_higher_memory_limit<Tgt: Target>(
        spec: &Spec<Tgt>,
    ) {
        spec.0.actions(None).into_iter().for_each(|action| {
            let Ok(applied) = action.apply(spec) else {
                return;
            };
            visit_leaves(&applied, &mut |leaf| {
                if let ImplNode::SpecApp(spec_app) = leaf {
                    assert!(
                        spec.0 != spec_app.0 .0 || spec_app.0 .1 <= spec.1,
                        "Action {:?} produced the same Spec {} with higher memory limit {}",
                        action,
                        spec,
                        spec_app.0 .1
                    );
                }
                true
            });
        });
    }

    /// Asserts that actions appear at all memory limits at and above memory consumed.
    fn shared_test_actions_are_valid_through_consumed_memory<Tgt: Target>(
        logical_spec: LogicalSpec<Tgt>,
    ) {
        // If an action consumes x bytes, then it should be valid for any Spec with the same logical
        // Spec at that memory limit and up.
        let MemoryLimits::Standard(maxes_vec) = Tgt::max_mem();
        let mut maxes = Vec::with_capacity(maxes_vec.len());
        for binary_scaled in maxes_vec.iter_binary_scaled() {
            maxes.push(u32::from(binary_scaled));
        }
        // Zero out levels which are slower than all present operands' levels.
        let parameters = logical_spec.parameters();
        for (level_idx, level) in Tgt::levels().into_iter().enumerate() {
            if parameters.iter().all(|p| p.level() < level) {
                maxes[level_idx] = 0;
            }
        }

        // The list of actions depends only on the logical Spec. Filtering by memory limit happens
        // at application. So it's safe to just collect the list of actions once, up front.
        let mut unseen_actions = logical_spec.actions(None).into_iter().collect::<Vec<_>>();

        let mut shared_spec = Spec(logical_spec, MemoryLimits::Standard(MemVec::zero::<Tgt>()));
        let mut diagonal_idx = 0;
        loop {
            let mut empty = true;
            for pt in sum_seqs(&maxes, diagonal_idx) {
                empty = false;
                shared_spec.1 = MemoryLimits::Standard(MemVec::new_from_binary_scaled(
                    pt.iter()
                        .map(|&p| u8::try_from(p).unwrap())
                        .collect::<Vec<_>>()
                        .try_into()
                        .unwrap(),
                ));
                let MemoryLimits::Standard(limits_memvec) = &shared_spec.1;
                // TODO: Assert that nothing disappears?
                for i in (0..unseen_actions.len()).rev() {
                    match unseen_actions[i].apply(&shared_spec) {
                        Ok(applied) => {
                            unseen_actions.swap_remove(i);
                            // TODO: Should we also assert that applying the same action at each level
                            //   doesn't actually accumulate additional memory?.
                            // TODO: Can we assert that the change in peak memory is exactly the
                            //   additional amount at the limit?.
                            // TODO: Assert here that the min of each level-wise limit is zero.
                            assert_eq!(&applied.peak_memory(), limits_memvec);
                        }
                        Err(ApplyError::ActionNotApplicable(_) | ApplyError::OutOfMemory) => {}
                        Err(ApplyError::SpecNotCanonical) => panic!(),
                    }
                }
            }
            if empty {
                break;
            }
            diagonal_idx += 1;
        }
    }

    fn arb_spec_action_and_lower_limit<Tgt: Target>(
    ) -> impl Strategy<Value = (Spec<Tgt>, Action<Tgt>, ImplNode<Tgt>, MemoryLimits)> {
        arb_canonical_spec::<Tgt>(None, None)
            .prop_filter_map("Spec had zero applicable actions", |spec| {
                let applied_actions = spec
                    .0
                    .actions(None)
                    .into_iter()
                    .filter_map(|a| match a.apply(&spec) {
                        Ok(applied) => Some((a, applied)),
                        Err(ApplyError::ActionNotApplicable(_) | ApplyError::OutOfMemory) => None,
                        Err(ApplyError::SpecNotCanonical) => unreachable!(),
                    })
                    .collect::<Vec<_>>();
                if applied_actions.is_empty() {
                    None
                } else {
                    Some((spec, applied_actions))
                }
            })
            .prop_flat_map(|(spec, applied_actions)| {
                (Just(spec), proptest::sample::select(applied_actions))
            })
            .prop_flat_map(|(spec, action_pair)| {
                let (action, applied) = action_pair;
                let lower_bound = match applied.memory_allocated() {
                    MemoryAllocation::Simple(allocated) => allocated,
                    MemoryAllocation::Inner(_) => todo!(),
                    MemoryAllocation::Pipeline {
                        intermediate_consumption: _,
                    } => todo!(),
                };
                let MemoryLimits::Standard(limits_memvec) = &spec.1;
                let lower_limit_strategy = arb_memorylimits_ext(
                    &MemVec::new(lower_bound.map(next_binary_power)),
                    limits_memvec,
                );
                (
                    Just(spec),
                    Just(action),
                    Just(applied),
                    lower_limit_strategy,
                )
            })
    }

    fn shared_test_gen_tile_sizes_dim_1(multi_dim: bool) {
        assert_gen_tile_sizes(shape![1], [shape![1]], false, multi_dim);
        assert_gen_tile_sizes(shape![1], [], true, multi_dim);
        assert_gen_tile_sizes(
            shape![16],
            [shape![1], shape![2], shape![4], shape![8], shape![16]],
            false,
            multi_dim,
        );
        assert_gen_tile_sizes(
            shape![16],
            [shape![1], shape![2], shape![4], shape![8]],
            true,
            multi_dim,
        );
    }

    fn assert_gen_tile_sizes(
        tensor_shape: Shape,
        expected: impl IntoIterator<Item = Shape>,
        drop_given: bool,
        multi_dim: bool,
    ) {
        let expected: Vec<Shape> = expected.into_iter().sorted().collect();
        let d = expected.first().map_or(0, |shape| shape.len());
        assert!(expected.iter().all(|shape| shape.len() == d));

        let actual: Vec<Shape> =
            gen_tile_sizes::<X86Target>(&tensor_shape, drop_given, multi_dim, None)
                .map(|s| {
                    assert_eq!(s.len(), d);
                    s
                })
                .sorted()
                .collect::<Vec<_>>();
        assert_eq!(
            actual, expected,
            "gen_tile_sizes({:?}, drop_given={}, serial={}) returned {:?}, expected {:?}",
            tensor_shape, drop_given, multi_dim, actual, expected
        );
    }
}
