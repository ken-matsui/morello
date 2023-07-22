use itertools::Itertools;
use std::collections::HashMap;
use std::fmt::{self, Debug, Write};
use std::rc::Rc;

use super::c_utils::CBuffer;
use super::namegen::NameGenerator;
use super::CodeGen;
use crate::codegen::c_utils::c_type;
use crate::codegen::header::HeaderEmitter;
use crate::color::do_color;
use crate::common::{DimSize, Dtype};
use crate::expr::{AffineExpr, Term};
use crate::highlight;
use crate::imp::blocks::Block;
use crate::imp::kernels::{Kernel, KernelType};
use crate::imp::loops::Loop;
use crate::imp::moves::{MoveLet, TensorOrCacheView};
use crate::imp::{Impl, ImplNode};
use crate::layout::BufferExprTerm;
use crate::target::{Target, X86MemoryLevel, X86Target};
use crate::views::{Param, Tensor, View};

const STACK_CUTOFF: u32 = 256;

#[derive(Default)]
struct X86CodeGenerator<'a> {
    name_env: HashMap<Rc<Tensor<X86Target>>, CBuffer>,
    namer: NameGenerator,
    loop_iter_names: HashMap<BufferExprTerm, String>,
    param_bindings: HashMap<Param<X86Target>, &'a dyn View<Tgt = X86Target>>,
    headers: HeaderEmitter,
}

impl<Aux: Clone + Debug> CodeGen<X86Target> for ImplNode<X86Target, Aux> {
    fn emit_kernel<W: Write>(&self, out: &mut W) -> fmt::Result {
        let top_arg_tensors = self
            .parameters()
            .map(|parameter| Rc::new(Tensor::new(parameter.clone())))
            .collect::<Vec<_>>();
        let mut generator = X86CodeGenerator::default();
        generator.headers.emit_x86 = true;
        generator.emit_kernel(self, &top_arg_tensors, out)?;
        Ok(())
    }
}

impl<'a> X86CodeGenerator<'a> {
    fn emit_kernel<W: Write, Aux: Clone + Debug>(
        &mut self,
        imp: &'a ImplNode<X86Target, Aux>,
        top_arg_tensors: &'a [Rc<Tensor<X86Target>>],
        out: &mut W,
    ) -> fmt::Result {
        debug_assert_eq!(top_arg_tensors.len(), usize::from(imp.parameter_count()));

        let mut main_body_str = String::new();
        writeln!(main_body_str, "__attribute__((noinline))\nvoid kernel(")?;
        for ((operand_idx, operand), tensor) in imp.parameters().enumerate().zip(top_arg_tensors) {
            let spec = tensor.spec();
            let new_c_buffer = self.make_buffer(
                spec.dim_sizes(),
                spec.vector_shape().map(|v| &v[..]),
                spec.dtype(),
                spec.level(),
            );
            writeln!(
                main_body_str,
                "  {} *restrict {}{}",
                c_type(operand.dtype),
                new_c_buffer.name().unwrap(),
                if operand_idx + 1 < imp.parameter_count().into() {
                    ", "
                } else {
                    ") {"
                }
            )?;
            self.name_env.insert(Rc::clone(tensor), new_c_buffer);
        }

        // Put the tensor->c_buffer binding into `self.name_env`. (And fill
        // tensors_as_trait_obj_ptrs.)
        let tensors_as_trait_obj_ptrs = top_arg_tensors
            .iter()
            .map(|tensor| tensor.as_ref() as &dyn View<Tgt = X86Target>)
            .collect::<Vec<_>>();

        imp.bind(&tensors_as_trait_obj_ptrs, &mut self.param_bindings);
        self.emit(&mut main_body_str, &imp)?;

        writeln!(main_body_str, "}}")?;

        self.headers.emit(out)?;
        if do_color() {
            highlight::c(&main_body_str);
        } else {
            out.write_str(&main_body_str)?;
        }
        Ok(())
    }

    fn make_buffer(
        &mut self,
        shape: &[DimSize],
        _vector_shape: Option<&[DimSize]>,
        dtype: Dtype,
        level: X86MemoryLevel,
    ) -> CBuffer {
        let name = self.namer.fresh_name();
        let size = shape.iter().product::<DimSize>();
        match level {
            X86MemoryLevel::VRF => todo!("Add vector reg. support"),
            X86MemoryLevel::RF => {
                if size > 1 {
                    CBuffer::StackArray { name, size, dtype }
                } else {
                    CBuffer::ValueVar { name, dtype }
                }
            }
            X86MemoryLevel::L1 | X86MemoryLevel::GL => {
                if size * u32::from(dtype.size()) < STACK_CUTOFF {
                    CBuffer::HeapArray { name, size, dtype }
                } else {
                    CBuffer::StackArray { name, size, dtype }
                }
            }
        }
    }

    fn emit<Aux: Clone + Debug, W: Write>(
        &mut self,
        w: &mut W,
        imp: &ImplNode<X86Target, Aux>,
    ) -> fmt::Result {
        match imp {
            ImplNode::Loop(l) => {
                let axes_to_emit = axis_order_and_steps(l).collect::<Vec<_>>();

                // Map non-degen. axis names to fresh loop iterator names.
                let iter_var_names = axes_to_emit
                    .iter()
                    .map(|(axis, _)| (*axis, self.namer.fresh_name()))
                    .collect::<HashMap<_, _>>();

                // Associate each of the tile indices in each LoopTile with the correct
                // name and store that association in the `self.loop_iter_names`.
                for loop_tile in &l.tiles {
                    let tile = &loop_tile.tile;
                    for tt in tile.tile_dim_terms() {
                        let BufferExprTerm::TileIdx(dim, _) = &tt else {
                            unreachable!();
                        };
                        let subscript = loop_tile.subscripts[usize::from(*dim)];
                        if let Some(axis_loop_iter_name) = iter_var_names.get(&subscript) {
                            if self
                                .loop_iter_names
                                .insert(tt.clone(), axis_loop_iter_name.clone())
                                .is_some()
                            {
                                panic!("Symbol {:?} already assigned a loop iterator", tt);
                            }
                        }
                    }
                }

                if l.parallel {
                    writeln!(
                        w,
                        "#pragma omp parallel for collapse({}) schedule(static)",
                        axes_to_emit.len()
                    )?;
                }

                for (var_name, (_, steps)) in iter_var_names.values().zip(&axes_to_emit) {
                    writeln!(
                        w,
                        "for (int {} = 0; {} < {}; {}++) {{",
                        var_name, var_name, steps, var_name
                    )?;
                }

                // TODO: Indent before recursing!
                self.emit(w, &l.body)?;

                for _ in 0..axes_to_emit.len() {
                    writeln!(w, "}}")?;
                }
                Ok(())
            }
            ImplNode::MoveLet(
                move_let @ MoveLet {
                    parameter_idx,
                    source_spec,
                    introduced,
                    has_prologue: _,
                    has_epilogue: _,
                    children,
                    prefetch,
                    aux: _,
                },
            ) => {
                let introduced_spec = introduced.spec();
                match introduced_spec.level() {
                    X86MemoryLevel::VRF => todo!("Implement MoveLet for VRF"),
                    X86MemoryLevel::RF | X86MemoryLevel::L1 | X86MemoryLevel::GL => {
                        match introduced {
                            TensorOrCacheView::Tensor(tensor) => {
                                // Emit variable declaration(s) and store association between the
                                // CBuffer and Tensor.
                                let dest_buffer = self.make_buffer(
                                    introduced_spec.dim_sizes(),
                                    None,
                                    introduced_spec.dtype(),
                                    introduced_spec.level(),
                                );
                                dest_buffer.emit(w, false)?;

                                if self
                                    .name_env
                                    .insert(Rc::clone(tensor), dest_buffer)
                                    .is_some()
                                {
                                    panic!("Duplicate name for buffer");
                                }
                            }
                            TensorOrCacheView::CacheView(_) => (),
                        };
                        if let Some(prologue) = move_let.prologue() {
                            self.emit(w, prologue)?;
                        }
                        self.emit(w, move_let.main_stage())?;
                        if let Some(epilogue) = move_let.epilogue() {
                            self.emit(w, epilogue)?;
                        }
                        Ok(())
                    }
                }
            }
            ImplNode::Block(Block {
                stages,
                bindings: _,
                parameters: _,
                aux: _,
            }) => {
                for stage in stages {
                    self.emit(w, stage)?;
                }
                Ok(())
            }
            ImplNode::Pipeline(_) => todo!("Emit code for Pipeline"),
            ImplNode::ProblemApp(p) => {
                writeln!(w, "assert(false);  /* {:?} */", p)
            }
            ImplNode::Kernel(Kernel {
                kernel_type,
                arguments,
                aux: _,
            }) => {
                match kernel_type {
                    KernelType::Mult => {
                        let exprs = self.param_args_to_c_indices(arguments);
                        writeln!(
                            w,
                            "{} += {} * {};  /* Mult */",
                            exprs[2], exprs[0], exprs[1]
                        )
                    }
                    KernelType::BroadcastVecMult => todo!(),
                    KernelType::ValueAssign => {
                        let exprs = self.param_args_to_c_indices(arguments);
                        writeln!(w, "{} = {};", exprs[1], exprs[0])
                    }
                    KernelType::VectorAssign => todo!(),
                    KernelType::MemsetZero => {
                        // TODO: Merge this duplicate `exprs` block. It's used also in the ValueAssign.
                        debug_assert_eq!(arguments.len(), 1);
                        let backing_tensor =
                            arguments[0].backing_tensor(&self.param_bindings).unwrap();
                        let buffer = self.name_env.get(backing_tensor).unwrap();
                        let mut buffer_indexing_expr =
                            arguments[0].make_buffer_indexing_expr(&self.param_bindings);
                        zero_points(&mut buffer_indexing_expr);
                        let arg_expr = self.c_index_ptr(buffer, &buffer_indexing_expr, None);
                        writeln!(
                            w,
                            "memset((void *)({}), 0, {});",
                            arg_expr,
                            arguments[0].1.bytes_used()
                        )
                    }
                    KernelType::VectorZero => todo!(),
                    KernelType::CacheAccess => Ok(()),
                }
            }
        }
    }

    fn param_args_to_c_indices(&self, arguments: &[Param<X86Target>]) -> Vec<String> {
        arguments
            .iter()
            .map(|arg| {
                let backing_tensor = arg.backing_tensor(&self.param_bindings).unwrap();
                let buffer = self.name_env.get(backing_tensor).unwrap();
                let mut buffer_indexing_expr = arg.make_buffer_indexing_expr(&self.param_bindings);
                zero_points(&mut buffer_indexing_expr);
                self.c_index(buffer, &buffer_indexing_expr, None)
            })
            .collect()
    }

    fn expr_to_c(&self, e: &AffineExpr<BufferExprTerm>) -> String {
        let mut buf =
            e.0.iter()
                .map(|Term(coef, sym)| {
                    // TODO: Remove expensive format!
                    let sym_str = self.loop_iter_names.get(sym).expect(&format!(
                        "BufferExprTerm {:?} should have had a name in the environment. Found this in {:?}",
                        sym, e.0
                    ));
                    match &coef {
                        0 => panic!("AffineExpr contained zero term"),
                        1 => sym_str.clone(),
                        _ => format!("{} * {}", coef, sym_str),
                    }
                })
                .join(" + ");
        if e.1 != 0 {
            if buf.is_empty() {
                buf = e.1.to_string();
            } else {
                buf += &format!(" + {}", e.1);
            }
        }
        if buf.is_empty() {
            buf = String::from("0");
        }
        buf
    }

    /// Returns a C expression referring to the value at a given expression.
    ///
    /// Additionally, `reinterpret` may be provided to introduce a type cast.
    /// This is useful for interpreting a (partial) buffer as a vector type.
    fn c_index(
        &self,
        buffer: &CBuffer,
        expr: &AffineExpr<BufferExprTerm>,
        reinterpret: Option<String>,
    ) -> String {
        match buffer {
            CBuffer::Ptr { name, .. } => match reinterpret {
                Some(_) => unimplemented!(),
                None => format!("{}[{}]", name, self.expr_to_c(expr)),
            },
            CBuffer::UnsizedHeapArray { name, .. } => match reinterpret {
                Some(_) => unimplemented!(),
                None => format!("{}[{}]", name, self.expr_to_c(expr)),
            },
            CBuffer::HeapArray { name, .. } => match reinterpret {
                Some(_) => unimplemented!(),
                None => format!("{}[{}]", name, self.expr_to_c(expr)), // assuming expr.c_expr() is available in scope
            },
            CBuffer::StackArray { name, .. } => match reinterpret {
                Some(_) => unimplemented!(),
                None => format!("{}[{}]", name, self.expr_to_c(expr)),
            },
            CBuffer::ValueVar { name, .. } => match reinterpret {
                Some(_) => unimplemented!(),
                None => name.clone(),
            },
        }
    }

    fn c_index_vec(
        &self,
        buffer: &CBuffer,
        _expr: &AffineExpr<BufferExprTerm>,
        _reinterpret: Option<String>,
    ) -> String {
        match buffer {
            CBuffer::Ptr { .. }
            | CBuffer::UnsizedHeapArray { .. }
            | CBuffer::HeapArray { .. }
            | CBuffer::StackArray { .. }
            | CBuffer::ValueVar { .. } => unimplemented!(),
        }
    }

    fn c_index_ptr(
        &self,
        buffer: &CBuffer,
        expr: &AffineExpr<BufferExprTerm>,
        reinterpret: Option<String>,
    ) -> String {
        match buffer {
            CBuffer::Ptr { name, .. }
            | CBuffer::UnsizedHeapArray { name, .. }
            | CBuffer::HeapArray { name, .. } => match reinterpret {
                Some(_) => unimplemented!(),
                None => {
                    format!("{} + {}", name, self.expr_to_c(expr))
                }
            },
            CBuffer::StackArray { .. } => match reinterpret {
                Some(_) => unimplemented!(),
                None => format!("&{}", self.c_index(buffer, expr, None)),
            },
            CBuffer::ValueVar { .. } => {
                if reinterpret.is_some() {
                    unimplemented!();
                };
                let mut ptr_str = format!("&{}", self.c_index(buffer, expr, None));
                if ptr_str.ends_with("[0]") {
                    ptr_str = ptr_str[..ptr_str.len() - 3].to_string();
                }
                ptr_str
            }
        }
    }
}

fn axis_order_and_steps<Tgt: Target, Aux: Clone>(
    l: &Loop<Tgt, Aux>,
) -> impl Iterator<Item = (u8, u32)> + '_ {
    // TODO: Choose according to a skip-minimizing heuristic.
    let result = l
        .tiles
        .iter()
        .flat_map(|t| {
            t.subscripts
                .iter()
                .enumerate()
                .filter_map(|(dim_idx, subscript)| {
                    let s = t.tile.steps_dim(dim_idx.try_into().unwrap());
                    debug_assert_ne!(s, 0);
                    if s == 1 {
                        None
                    } else {
                        Some((*subscript, s))
                    }
                })
        })
        .unique();

    // For debug builds, assert that `r` doesn't contain duplicate subscripts.
    #[cfg(debug_assertions)]
    {
        let mut seen = std::collections::HashSet::new();
        for (axis, _steps) in result.clone() {
            assert!(seen.insert(axis));
        }
    }

    result
}

fn zero_points(expr: &mut AffineExpr<BufferExprTerm>) {
    expr.0.retain(|t| match t.1 {
        BufferExprTerm::Pt(_, _) => false,
        BufferExprTerm::TileIdx(_, _) => true,
    });
}

#[cfg(test)]
mod tests {
    use super::X86CodeGenerator;
    use crate::expr::{AffineExpr, Term};
    use crate::layout::BufferExprTerm;
    use crate::opaque_symbol::OpaqueSymbol;

    #[test]
    fn test_expr_zero_not_emitted() {
        let gen = X86CodeGenerator::default();
        assert_eq!(gen.expr_to_c(&AffineExpr(vec![], 0)), "");
    }

    #[test]
    fn test_intercept_zero_not_emitted() {
        let mut gen = X86CodeGenerator::default();
        let x = BufferExprTerm::Pt(0, OpaqueSymbol::new());
        gen.loop_iter_names.insert(x.clone(), String::from("x"));
        assert_eq!(gen.expr_to_c(&AffineExpr(vec![Term(2, x)], 0)), "2 * x")
    }

    #[test]
    fn test_lower_to_c_expr() {
        let mut gen = X86CodeGenerator::default();
        let x = BufferExprTerm::Pt(0, OpaqueSymbol::new());
        gen.loop_iter_names.insert(x.clone(), String::from("x"));
        let y = BufferExprTerm::Pt(0, OpaqueSymbol::new());
        gen.loop_iter_names.insert(y.clone(), String::from("y"));
        assert_eq!(gen.expr_to_c(&AffineExpr(vec![], 1)), "1");
        assert_eq!(gen.expr_to_c(&AffineExpr(vec![Term(1, x)], 1)), "x + 1");
        assert_eq!(gen.expr_to_c(&AffineExpr(vec![Term(2, y)], 3)), "2 * y + 3");
    }
}
