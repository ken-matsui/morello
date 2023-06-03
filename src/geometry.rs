use itertools::{iproduct, Itertools};
use smallvec::{smallvec, SmallVec, ToSmallVec};

use crate::common::{Contig, DimSize, Dtype, Shape};
use crate::layout::Layout;
use crate::spec::{conv_infer_output_shape, gen_vector_shapes, Spec, SpecAux};
use crate::target::{MemoryLevel, Target, X86Target};
use crate::tensorspec::TensorSpec;

use crate::X86MemoryLevel;

use std::hash::Hash;
use std::iter;

pub trait ToFromDependencyLatticeCoordinate {
    type Key: Eq + Hash;
    type InnerKey: Eq + Hash;

    fn to_grid(&self) -> Option<(Self::Key, Vec<u32>, Self::InnerKey)>;
    fn from_grid(key: &Self::Key, pt: &[u32], inner_key: &Self::InnerKey) -> Self;
    // TODO: Return an iterator instead.
    fn inner_keys_for_grid_pt(key: &Self::Key, pt: &[u32]) -> Vec<Self::InnerKey>;
}

// TODO: Simplify code by making this the foundation of our Spec enum.
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum SpecKey {
    Matmul { dtype: Dtype },
    Conv { dtype: Dtype },
    Move { is_load: bool, dtype: Dtype },
    Zero { dtype: Dtype },
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum SpecInnerKey {
    Matmul {
        contiguous_abstractions: SmallVec<[Contig; 3]>,
        alignments: SmallVec<[bool; 3]>,
        layouts: SmallVec<[Layout; 3]>,
        vector_shapes: SmallVec<[Option<Shape>; 3]>,
    },
    Conv {
        contiguous_abstractions: SmallVec<[Contig; 3]>,
        alignments: SmallVec<[bool; 3]>,
        layouts: SmallVec<[Layout; 3]>,
        vector_shapes: SmallVec<[Option<Shape>; 3]>,
    },
    Move {
        source_contiguous_abs: Contig,
        source_aligned: bool,
        source_layout: Layout,
        source_vector_shape: Option<Shape>,
        destination_level: X86MemoryLevel,
        destination_layout: Layout,
        destination_vector_shape: Option<Shape>,
    },
    Zero {
        contiguous_abs: Contig,
        aligned: bool,
        layout: Layout,
        vector_shape: Option<Shape>,
    },
}

impl ToFromDependencyLatticeCoordinate for Spec<X86Target> {
    type Key = SpecKey;
    type InnerKey = SpecInnerKey;

    fn to_grid(&self) -> Option<(SpecKey, Vec<u32>, SpecInnerKey)> {
        match self {
            Spec::Matmul {
                accum,
                m,
                k,
                n,
                dtype,
                aux,
                serial_only,
            } => Some((
                SpecKey::Matmul { dtype: *dtype },
                [
                    if *accum { 0 } else { 1 },
                    to_log2_dim_space(*m)?,
                    to_log2_dim_space(*k)?,
                    to_log2_dim_space(*n)?,
                    if *serial_only { 0 } else { 1 },
                ]
                .into_iter()
                .chain(aux.iter().map(|a| level_to_int(&a.level).into()))
                .collect(),
                SpecInnerKey::Matmul {
                    contiguous_abstractions: aux.iter().map(|a| a.contig).collect(),
                    alignments: aux.iter().map(|a| a.aligned).collect(),
                    layouts: aux.iter().map(|a| a.layout.clone()).collect(),
                    vector_shapes: aux.iter().map(|a| a.vector_shape.clone()).collect(),
                },
            )),
            Spec::Conv {
                accum,
                image_shape,
                filters_shape,
                dtype,
                aux,
                serial_only,
            } => {
                let mut shape_vec = Vec::with_capacity(image_shape.len() + filters_shape.len() - 1);
                println!(
                    "Image shape: {:?}\tFilters shape: {:?}",
                    image_shape, filters_shape
                );
                shape_vec.extend(
                    image_shape
                        .iter()
                        .zip(filters_shape.iter())
                        .map(|(&i, &f)| i - f),
                );
                for &d in filters_shape {
                    shape_vec.push(d - 1);
                }
                // TODO: The image and filters have the same channel count, so there's a
                // redundant dimension in the below.
                debug_assert_eq!(shape_vec.len(), 8);
                Some((
                    SpecKey::Conv { dtype: *dtype },
                    [if *accum { 0 } else { 1 }]
                        .into_iter()
                        .chain(shape_vec.into_iter())
                        .chain([if *serial_only { 0 } else { 1 }].into_iter())
                        .chain(aux.iter().map(|a| level_to_int(&a.level).into()))
                        .collect(),
                    SpecInnerKey::Conv {
                        contiguous_abstractions: aux.iter().map(|a| a.contig).collect(),
                        alignments: aux.iter().map(|a| a.aligned).collect(),
                        layouts: aux.iter().map(|a| a.layout.clone()).collect(),
                        vector_shapes: aux.iter().map(|a| a.vector_shape.clone()).collect(),
                    },
                ))
            }
            Spec::Load {
                outer_tensor_spec,
                inner_level,
                inner_layout,
                inner_vector_shape,
                serial_only,
            }
            | Spec::Store {
                outer_tensor_spec,
                inner_level,
                inner_layout,
                inner_vector_shape,
                serial_only,
            } => Some((
                SpecKey::Move {
                    is_load: matches!(self, Spec::Load { .. }),
                    dtype: outer_tensor_spec.dtype(),
                },
                outer_tensor_spec
                    .dim_sizes()
                    .iter()
                    .map(|d| to_log2_dim_space(*d).unwrap())
                    .chain(iter::once(level_to_int(&outer_tensor_spec.level()).into()))
                    .chain(iter::once(if *serial_only { 0 } else { 1 }))
                    .collect(),
                SpecInnerKey::Move {
                    source_contiguous_abs: outer_tensor_spec.contiguous_abs(),
                    source_aligned: outer_tensor_spec.aligned(),
                    source_layout: outer_tensor_spec.layout(),
                    source_vector_shape: outer_tensor_spec.vector_shape().cloned(),
                    destination_level: *inner_level,
                    destination_layout: inner_layout.clone(),
                    destination_vector_shape: inner_vector_shape.clone(),
                },
            )),
            Spec::Zero {
                tensor_spec,
                serial_only,
            } => Some((
                SpecKey::Zero {
                    dtype: tensor_spec.dtype(),
                },
                tensor_spec
                    .dim_sizes()
                    .iter()
                    .map(|d| to_log2_dim_space(*d).unwrap())
                    .chain(iter::once(level_to_int(&tensor_spec.level()).into()))
                    .chain(iter::once(if *serial_only { 0 } else { 1 }))
                    .collect(),
                SpecInnerKey::Zero {
                    contiguous_abs: tensor_spec.contiguous_abs(),
                    aligned: tensor_spec.aligned(),
                    layout: tensor_spec.layout(),
                    vector_shape: tensor_spec.vector_shape().cloned(),
                },
            )),
        }
    }

    fn from_grid(key: &SpecKey, pt: &[u32], inner_key: &SpecInnerKey) -> Self {
        match (key, inner_key) {
            (
                SpecKey::Matmul { dtype },
                SpecInnerKey::Matmul {
                    contiguous_abstractions,
                    alignments,
                    layouts,
                    vector_shapes,
                },
            ) => match pt[..] {
                [accum, m, k, n, serial_only, _lvl0, _lvl1, _lvl2] => Spec::Matmul {
                    accum: accum == 0,
                    m: from_log2_dim_space(m),
                    k: from_log2_dim_space(k),
                    n: from_log2_dim_space(n),
                    dtype: *dtype,
                    aux: (0..3)
                        .map(|i| SpecAux {
                            contig: contiguous_abstractions[i],
                            aligned: alignments[i],
                            layout: layouts[i].clone(),
                            vector_shape: vector_shapes[i].clone(),
                            // TODO: Following is dangerous
                            level: int_to_level(pt[5 + i]),
                        })
                        .collect::<Vec<_>>()
                        .try_into()
                        .unwrap(),
                    serial_only: serial_only == 0,
                },
                _ => panic!("Grid point had unexpected length"),
            },
            (
                SpecKey::Conv { dtype },
                SpecInnerKey::Conv {
                    contiguous_abstractions,
                    alignments,
                    layouts,
                    vector_shapes,
                },
            ) => {
                // TODO: Can we share any of the following code with
                //  `inner_keys_for_grid_pt`?
                let accum = pt[0] == 0;
                let filters_shape = pt[5..9].iter().map(|&f| f + 1).collect::<Shape>();
                let image_shape = pt[1..5]
                    .iter()
                    .zip(filters_shape.iter())
                    .map(|(i, f)| i + f)
                    .collect::<SmallVec<_>>();

                let levels = &pt[9..12];
                let serial_only = pt[12] == 0;
                Spec::Conv {
                    accum,
                    image_shape,
                    filters_shape,
                    dtype: *dtype,
                    aux: (0..3)
                        .map(|i| SpecAux {
                            contig: contiguous_abstractions[i],
                            aligned: alignments[i],
                            layout: layouts[i].clone(),
                            level: int_to_level(levels[i]),
                            vector_shape: vector_shapes[i].clone(),
                        })
                        .collect::<Vec<_>>()
                        .try_into()
                        .unwrap(),
                    serial_only,
                }
            }
            (
                SpecKey::Move { is_load, dtype },
                SpecInnerKey::Move {
                    source_contiguous_abs,
                    source_aligned,
                    source_layout,
                    source_vector_shape,
                    destination_level,
                    destination_layout,
                    destination_vector_shape,
                },
            ) => {
                let serial_only = pt[pt.len() - 1] == 0;
                let source_level = int_to_level(pt[pt.len() - 2]);
                let dim_sizes = pt[..pt.len() - 2]
                    .iter()
                    .map(|&d| from_log2_dim_space(d))
                    .collect();
                let outer_tensor_spec = TensorSpec::new_noncanon(
                    dim_sizes,
                    *dtype,
                    *source_contiguous_abs,
                    *source_aligned,
                    source_level,
                    source_layout.clone(),
                    source_vector_shape.clone(),
                );
                // TODO: Loads and Stores should really just be combined in Spec.
                if *is_load {
                    Spec::Load {
                        outer_tensor_spec,
                        inner_level: *destination_level,
                        inner_layout: destination_layout.clone(),
                        inner_vector_shape: destination_vector_shape.clone(),
                        serial_only,
                    }
                } else {
                    Spec::Store {
                        outer_tensor_spec,
                        inner_level: *destination_level,
                        inner_layout: destination_layout.clone(),
                        inner_vector_shape: destination_vector_shape.clone(),
                        serial_only,
                    }
                }
            }
            (
                SpecKey::Zero { dtype },
                SpecInnerKey::Zero {
                    contiguous_abs,
                    aligned,
                    layout,
                    vector_shape,
                },
            ) => {
                let serial_only = pt[pt.len() - 1] == 0;
                let level = int_to_level(pt[pt.len() - 2]);
                let dim_sizes = pt[..pt.len() - 2]
                    .iter()
                    .map(|&d| from_log2_dim_space(d))
                    .collect();
                let tensor_spec = TensorSpec::new_noncanon(
                    dim_sizes,
                    *dtype,
                    *contiguous_abs,
                    *aligned,
                    level,
                    layout.clone(),
                    vector_shape.clone(),
                );
                Spec::Zero {
                    tensor_spec,
                    serial_only,
                }
            }
            _ => panic!("Mismatched key and inner key"),
        }
    }

    fn inner_keys_for_grid_pt(key: &Self::Key, pt: &[u32]) -> Vec<Self::InnerKey> {
        match key {
            SpecKey::Matmul { dtype } => {
                // TODO: Relying on indices below is fragile.
                let m = pt[1] + 1;
                let k = pt[2] + 1;
                let n = pt[3] + 1;
                let levels = pt[5..8]
                    .iter()
                    .map(|&i| int_to_level(i))
                    .collect::<Vec<_>>();

                let shapes = [smallvec![m, k], smallvec![k, n], smallvec![m, n]];

                // For each operand:
                // - alignment
                // - layout
                align_layout_contig_vector_shape_product::<X86Target>(&shapes, *dtype, &levels)
                    .map(
                        |(alignments, layouts, contigs, vector_shapes)| SpecInnerKey::Matmul {
                            contiguous_abstractions: contigs.into_iter().collect(),
                            alignments,
                            layouts,
                            vector_shapes: vector_shapes
                                .into_iter()
                                .map(|v| v.as_ref().map(|v| v.to_smallvec()))
                                .collect(),
                        },
                    )
                    .collect()
            }
            SpecKey::Conv { dtype } => {
                // TODO: Relying on indices below is fragile.
                let filters_shape = pt[5..9].iter().map(|&f| f + 1).collect::<Shape>();
                let image_shape = pt[1..5]
                    .iter()
                    .zip(filters_shape.iter())
                    .map(|(i, f)| i + f)
                    .collect::<SmallVec<_>>();
                let output_shape = conv_infer_output_shape(&image_shape, &filters_shape);
                let shapes = [image_shape, filters_shape, output_shape];

                let levels = pt[9..12]
                    .iter()
                    .map(|&i| int_to_level(i))
                    .collect::<Vec<_>>();

                align_layout_contig_vector_shape_product::<X86Target>(&shapes, *dtype, &levels)
                    .map(
                        |(alignments, layouts, contigs, vector_shapes)| SpecInnerKey::Conv {
                            contiguous_abstractions: contigs.into_iter().collect(),
                            alignments,
                            layouts,
                            vector_shapes: vector_shapes
                                .into_iter()
                                .map(|v| v.as_ref().map(|v| v.to_smallvec()))
                                .collect(),
                        },
                    )
                    .collect()
            }
            SpecKey::Move { is_load: _, dtype } => {
                let source_level = int_to_level(pt[pt.len() - 2]);
                let dim_sizes = &pt[..pt.len() - 2]
                    .iter()
                    .map(|&d| from_log2_dim_space(d))
                    .collect::<Shape>();

                let alignments = [true, false];
                let viable_layouts = X86Target::all_layouts_for_shape(dim_sizes);

                alignments
                    .into_iter()
                    .cartesian_product(viable_layouts.iter().cloned())
                    .cartesian_product(viable_layouts.iter().cloned())
                    .flat_map(
                        move |((source_aligned, source_layout), destination_layout)| {
                            let allowed_destination_levels =
                                X86Target::faster_destination_levels(source_level);
                            allowed_destination_levels
                                .into_iter()
                                .cartesian_product(source_layout.all_contiguous_abs().collect_vec())
                                .flat_map(move |(destination_level, source_contiguous_abs)| {
                                    let source_layout = source_layout.clone();
                                    let destination_layout = destination_layout.clone();
                                    [source_level, destination_level]
                                        .map(|lvl| {
                                            if lvl.vector_rf() {
                                                gen_vector_shapes(
                                                    Some(dim_sizes),
                                                    *dtype,
                                                    lvl.vector_bytes(),
                                                    None,
                                                )
                                                .map(Some)
                                                .collect::<Vec<_>>()
                                            } else {
                                                vec![None]
                                            }
                                        })
                                        .into_iter()
                                        .multi_cartesian_product()
                                        .map(
                                            move |vector_shape_pair| match &vector_shape_pair[..] {
                                                [source_vector_shape, destination_vector_shape] => {
                                                    SpecInnerKey::Move {
                                                        source_contiguous_abs,
                                                        source_aligned,
                                                        source_layout: source_layout.clone(),
                                                        source_vector_shape: source_vector_shape
                                                            .clone(),
                                                        destination_level,
                                                        destination_layout: destination_layout
                                                            .clone(),
                                                        destination_vector_shape:
                                                            destination_vector_shape.clone(),
                                                    }
                                                }
                                                _ => unreachable!(),
                                            },
                                        )
                                })
                        },
                    )
                    .collect::<Vec<_>>()
            }
            SpecKey::Zero { dtype } => {
                let level = int_to_level(pt[pt.len() - 2]);
                let dim_sizes = pt[..pt.len() - 2]
                    .iter()
                    .map(|&d| from_log2_dim_space(d))
                    .collect::<Shape>();
                align_layout_contig_vector_shape_product::<X86Target>(
                    &[dim_sizes],
                    *dtype,
                    &[level],
                )
                .map(
                    |(alignments, layouts, contigs, vector_shapes)| SpecInnerKey::Zero {
                        contiguous_abs: contigs[0],
                        aligned: alignments[0],
                        layout: layouts[0].clone(),
                        vector_shape: vector_shapes[0].clone(),
                    },
                )
                .collect()
            }
        }
    }
}

fn align_layout_contig_vector_shape_product<'s, Tgt: Target>(
    shapes: &'s [Shape],
    dtype: Dtype,
    levels: &'s [Tgt::Level],
) -> impl Iterator<
    Item = (
        SmallVec<[bool; 3]>,
        SmallVec<[Layout; 3]>,
        SmallVec<[Contig; 3]>,
        SmallVec<[Option<Shape>; 3]>,
    ),
> + 's {
    assert_eq!(shapes.len(), levels.len());
    let align_prod = iter::repeat([true, false])
        .take(shapes.len())
        .multi_cartesian_product();
    let layout_prod = shapes
        .iter()
        .map(|s| X86Target::all_layouts_for_shape(s))
        .multi_cartesian_product();
    align_prod
        .cartesian_product(layout_prod)
        .flat_map(move |(alignments, layouts)| {
            // - contig.
            let contigs = layouts
                .iter()
                // TODO: Make iterator cloneable instead of collecting into Vec.
                .map(|l| l.all_contiguous_abs().collect::<Vec<_>>())
                .multi_cartesian_product();
            // - vector shape
            let vector_shapes = levels
                .iter()
                // TODO: Make iterator cloneable instead of collecting into Vec.
                .enumerate()
                .map(|(idx, lvl)| {
                    //  TODO: Avoid this collection.
                    if lvl.vector_rf() {
                        gen_vector_shapes(Some(&shapes[idx]), dtype, lvl.vector_bytes(), None)
                            .map(Some)
                            .collect::<SmallVec<[_; 3]>>()
                    } else {
                        smallvec![None]
                    }
                })
                .multi_cartesian_product();
            iproduct!(
                iter::once(alignments),
                iter::once(layouts),
                contigs,
                vector_shapes
            )
        })
        .map(|(alignments, layouts, contigs, vector_shapes)| {
            // TODO: Collect into SmallVecs immediately instead of converting.
            (
                SmallVec::<[_; 3]>::from(alignments),
                SmallVec::<[_; 3]>::from(layouts),
                SmallVec::<[_; 3]>::from(contigs),
                SmallVec::<[_; 3]>::from(vector_shapes),
            )
        })
}

fn level_to_int(lvl: &X86MemoryLevel) -> u8 {
    match &lvl {
        X86MemoryLevel::GL => 3,
        X86MemoryLevel::L1 => 2,
        X86MemoryLevel::VRF => 1,
        X86MemoryLevel::RF => 0,
    }
}

fn int_to_level(i: u32) -> X86MemoryLevel {
    match i {
        0 => X86MemoryLevel::RF,
        1 => X86MemoryLevel::VRF,
        2 => X86MemoryLevel::L1,
        3 => X86MemoryLevel::GL,
        _ => panic!("Invalid level"),
    }
}

fn iter_vector_shape_args<M: MemoryLevel>(
    level: M,
    outer_shape: &[usize],
    dtype: Dtype,
) -> Box<dyn Iterator<Item = Option<Shape>>> {
    if level.vector_bytes() == 0 {
        Box::new(iter::once(None))
    } else {
        Box::new(
            gen_vector_shapes(
                None,
                dtype,
                level.vector_bytes(),
                Some(outer_shape.len().try_into().unwrap()),
            )
            .map(Some),
        )
    }
}

fn to_log2_dim_space(dim: DimSize) -> Option<u32> {
    assert!(dim > 0);
    Some(dim - 1)
    // let r = bit_length_u32(dim) - 1;
    // if from_log2_dim_space(r) == dim {
    //     Some(r)
    // } else {
    //     None
    // }
}

fn from_log2_dim_space(log2_dim: u32) -> DimSize {
    // 1 << log2_dim
    log2_dim + 1
}
