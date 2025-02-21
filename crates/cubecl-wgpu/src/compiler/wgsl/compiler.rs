use std::{borrow::Cow, sync::Arc};

use super::{shader::ComputeShader, ConstantArray, Item, SharedMemory};
use super::{LocalArray, Subgroup};
use crate::{
    compiler::{base::WgpuCompiler, wgsl},
    WgpuServer,
};
use cubecl_core::{
    ir::{self as cube, HybridAllocator},
    prelude::CompiledKernel,
    server::ComputeServer,
    Feature,
};
use cubecl_runtime::{DeviceProperties, ExecutionMode};
use wgpu::{ComputePipeline, DeviceDescriptor, ShaderModuleDescriptor};

/// Wgsl Compiler.
#[derive(Clone, Default)]
pub struct WgslCompiler {
    num_inputs: usize,
    num_outputs: usize,
    local_invocation_index: bool,
    local_invocation_id: bool,
    global_invocation_id: bool,
    workgroup_id: bool,
    subgroup_size: bool,
    rank: bool,
    id: bool,
    stride: bool,
    shape: bool,
    num_workgroups: bool,
    workgroup_id_no_axis: bool,
    workgroup_size_no_axis: bool,
    num_workgroup_no_axis: bool,
    shared_memories: Vec<SharedMemory>,
    const_arrays: Vec<ConstantArray>,
    local_arrays: Vec<LocalArray>,
}

impl core::fmt::Debug for WgslCompiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WgslCompiler")
    }
}

impl cubecl_core::Compiler for WgslCompiler {
    type Representation = ComputeShader;

    fn compile(shader: cube::KernelDefinition, _mode: ExecutionMode) -> Self::Representation {
        let mut compiler = Self::default();
        compiler.compile_shader(shader)
    }

    fn elem_size(elem: cube::Elem) -> usize {
        Self::compile_elem(elem).size()
    }

    fn max_shared_memory_size() -> usize {
        32768
    }

    fn local_allocator() -> impl cube::LocalAllocator {
        HybridAllocator::default()
    }
}

impl WgpuCompiler for WgslCompiler {
    fn create_pipeline(
        server: &mut WgpuServer<Self>,
        kernel: CompiledKernel<Self>,
        mode: ExecutionMode,
    ) -> Arc<ComputePipeline> {
        let source = &kernel.source;
        let module = match mode {
            ExecutionMode::Checked => server.device.create_shader_module(ShaderModuleDescriptor {
                label: None,
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(source)),
            }),
            ExecutionMode::Unchecked => unsafe {
                server
                    .device
                    .create_shader_module_unchecked(ShaderModuleDescriptor {
                        label: None,
                        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(source)),
                    })
            },
        };

        Arc::new(
            server
                .device
                .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: None,
                    layout: None,
                    module: &module,
                    entry_point: "main",
                    compilation_options: wgpu::PipelineCompilationOptions {
                        zero_initialize_workgroup_memory: false,
                        ..Default::default()
                    },
                    cache: None,
                }),
        )
    }

    fn compile(
        _server: &mut WgpuServer<Self>,
        kernel: <WgpuServer<Self> as ComputeServer>::Kernel,
        mode: ExecutionMode,
    ) -> CompiledKernel<Self> {
        kernel.compile(mode)
    }

    async fn request_device(adapter: &wgpu::Adapter) -> (wgpu::Device, wgpu::Queue) {
        let limits = adapter.limits();
        adapter
            .request_device(
                &DeviceDescriptor {
                    label: None,
                    required_features: adapter.features(),
                    required_limits: limits,
                    // The default is MemoryHints::Performance, which tries to do some bigger
                    // block allocations. However, we already batch allocations, so we
                    // can use MemoryHints::MemoryUsage to lower memory usage.
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                },
                None,
            )
            .await
            .map_err(|err| {
                format!(
                    "Unable to request the device with the adapter {:?}, err {:?}",
                    adapter.get_info(),
                    err
                )
            })
            .unwrap()
    }

    fn register_features(
        _adapter: &wgpu::Adapter,
        _device: &wgpu::Device,
        props: &mut DeviceProperties<Feature>,
    ) {
        register_types(props);
    }
}

fn register_types(props: &mut DeviceProperties<Feature>) {
    use cubecl_core::ir::{Elem, FloatKind, IntKind};

    let supported_types = [
        Elem::UInt,
        Elem::Int(IntKind::I32),
        Elem::AtomicInt(IntKind::I32),
        Elem::AtomicUInt,
        Elem::Float(FloatKind::F32),
        Elem::Bool,
    ];

    for ty in supported_types {
        props.register_feature(Feature::Type(ty));
    }
}

impl WgslCompiler {
    fn compile_shader(&mut self, mut value: cube::KernelDefinition) -> wgsl::ComputeShader {
        self.num_inputs = value.inputs.len();
        self.num_outputs = value.outputs.len();

        let instructions = self.compile_scope(&mut value.body);
        let extensions = register_extensions(&instructions);
        let body = wgsl::Body {
            instructions,
            rank: true,
            id: self.id,
            stride: self.stride,
            shape: self.shape,
        };

        wgsl::ComputeShader {
            inputs: value
                .inputs
                .into_iter()
                .map(Self::compile_binding)
                .collect(),
            outputs: value
                .outputs
                .into_iter()
                .map(Self::compile_binding)
                .collect(),
            named: value
                .named
                .into_iter()
                .map(|(name, binding)| (name, Self::compile_binding(binding)))
                .collect(),
            shared_memories: self.shared_memories.clone(),
            constant_arrays: self.const_arrays.clone(),
            local_arrays: self.local_arrays.clone(),
            workgroup_size: value.cube_dim,
            global_invocation_id: self.global_invocation_id || self.id,
            local_invocation_index: self.local_invocation_index,
            local_invocation_id: self.local_invocation_id,
            num_workgroups: self.id
                || self.num_workgroups
                || self.num_workgroup_no_axis
                || self.workgroup_id_no_axis,
            workgroup_id: self.workgroup_id || self.workgroup_id_no_axis,
            subgroup_size: self.subgroup_size,
            body,
            extensions,
            num_workgroups_no_axis: self.num_workgroup_no_axis,
            workgroup_id_no_axis: self.workgroup_id_no_axis,
            workgroup_size_no_axis: self.workgroup_size_no_axis,
        }
    }

    fn compile_item(item: cube::Item) -> Item {
        let elem = Self::compile_elem(item.elem);
        match item.vectorization.map(|it| it.get()).unwrap_or(1) {
            1 => wgsl::Item::Scalar(elem),
            2 => wgsl::Item::Vec2(elem),
            3 => wgsl::Item::Vec3(elem),
            4 => wgsl::Item::Vec4(elem),
            _ => panic!("Unsupported vectorizations scheme {:?}", item.vectorization),
        }
    }

    fn compile_elem(value: cube::Elem) -> wgsl::Elem {
        match value {
            cube::Elem::Float(f) => match f {
                cube::FloatKind::F16 => panic!("f16 is not yet supported"),
                cube::FloatKind::BF16 => panic!("bf16 is not a valid WgpuElement"),
                cube::FloatKind::F32 => wgsl::Elem::F32,
                cube::FloatKind::F64 => panic!("f64 is not a valid WgpuElement"),
            },
            cube::Elem::Int(i) => match i {
                cube::IntKind::I32 => wgsl::Elem::I32,
                cube::IntKind::I64 => panic!("i64 is not a valid WgpuElement"),
            },
            cube::Elem::UInt => wgsl::Elem::U32,
            cube::Elem::Bool => wgsl::Elem::Bool,
            cube::Elem::AtomicInt(i) => match i {
                cube::IntKind::I32 => wgsl::Elem::AtomicI32,
                cube::IntKind::I64 => panic!("atomic<i64> is not a valid WgpuElement"),
            },
            cube::Elem::AtomicUInt => wgsl::Elem::AtomicU32,
        }
    }

    pub(crate) fn compile_variable(&mut self, value: cube::Variable) -> wgsl::Variable {
        match value {
            cube::Variable::GlobalInputArray { id, item } => {
                wgsl::Variable::GlobalInputArray(id, Self::compile_item(item))
            }
            cube::Variable::GlobalScalar { id, elem } => {
                wgsl::Variable::GlobalScalar(id, Self::compile_elem(elem), elem)
            }
            cube::Variable::Local { id, item, depth }
            | cube::Variable::Versioned {
                id, item, depth, ..
            } => wgsl::Variable::Local {
                id,
                item: Self::compile_item(item),
                depth,
            },
            cube::Variable::LocalBinding { id, item, .. } => wgsl::Variable::LocalBinding {
                id,
                item: Self::compile_item(item),
            },
            cube::Variable::Slice { id, item, depth } => wgsl::Variable::Slice {
                id,
                item: Self::compile_item(item),
                depth,
            },
            cube::Variable::GlobalOutputArray { id, item } => {
                wgsl::Variable::GlobalOutputArray(id, Self::compile_item(item))
            }
            cube::Variable::ConstantScalar(value) => {
                wgsl::Variable::ConstantScalar(value, Self::compile_elem(value.elem()))
            }
            cube::Variable::SharedMemory { id, item, length } => {
                let item = Self::compile_item(item);
                if !self.shared_memories.iter().any(|s| s.index == id) {
                    self.shared_memories
                        .push(SharedMemory::new(id, item, length));
                }
                wgsl::Variable::SharedMemory(id, item, length)
            }
            cube::Variable::ConstantArray { id, item, length } => {
                let item = Self::compile_item(item);
                wgsl::Variable::ConstantArray(id, item, length)
            }
            cube::Variable::LocalArray {
                id,
                item,
                depth,
                length,
            } => {
                let item = Self::compile_item(item);
                if !self.local_arrays.iter().any(|s| s.index == id) {
                    self.local_arrays
                        .push(LocalArray::new(id, item, depth, length));
                }
                wgsl::Variable::LocalArray(id, item, depth, length)
            }
            cube::Variable::AbsolutePos => {
                self.id = true;
                wgsl::Variable::Id
            }
            cube::Variable::Rank => {
                self.rank = true;
                wgsl::Variable::Rank
            }
            cube::Variable::UnitPos => {
                self.local_invocation_index = true;
                wgsl::Variable::LocalInvocationIndex
            }
            cube::Variable::UnitPosX => {
                self.local_invocation_id = true;
                wgsl::Variable::LocalInvocationIdX
            }
            cube::Variable::UnitPosY => {
                self.local_invocation_id = true;
                wgsl::Variable::LocalInvocationIdY
            }
            cube::Variable::UnitPosZ => {
                self.local_invocation_id = true;
                wgsl::Variable::LocalInvocationIdZ
            }
            cube::Variable::CubePosX => {
                self.workgroup_id = true;
                wgsl::Variable::WorkgroupIdX
            }
            cube::Variable::CubePosY => {
                self.workgroup_id = true;
                wgsl::Variable::WorkgroupIdY
            }
            cube::Variable::CubePosZ => {
                self.workgroup_id = true;
                wgsl::Variable::WorkgroupIdZ
            }
            cube::Variable::AbsolutePosX => {
                self.global_invocation_id = true;
                wgsl::Variable::GlobalInvocationIdX
            }
            cube::Variable::AbsolutePosY => {
                self.global_invocation_id = true;
                wgsl::Variable::GlobalInvocationIdY
            }
            cube::Variable::AbsolutePosZ => {
                self.global_invocation_id = true;
                wgsl::Variable::GlobalInvocationIdZ
            }
            cube::Variable::CubeDimX => wgsl::Variable::WorkgroupSizeX,
            cube::Variable::CubeDimY => wgsl::Variable::WorkgroupSizeY,
            cube::Variable::CubeDimZ => wgsl::Variable::WorkgroupSizeZ,
            cube::Variable::CubeCountX => {
                self.num_workgroups = true;
                wgsl::Variable::NumWorkgroupsX
            }
            cube::Variable::CubeCountY => {
                self.num_workgroups = true;
                wgsl::Variable::NumWorkgroupsY
            }
            cube::Variable::CubeCountZ => {
                self.num_workgroups = true;
                wgsl::Variable::NumWorkgroupsZ
            }
            cube::Variable::CubePos => {
                self.workgroup_id_no_axis = true;
                wgsl::Variable::WorkgroupId
            }
            cube::Variable::CubeDim => {
                self.workgroup_size_no_axis = true;
                wgsl::Variable::WorkgroupSize
            }
            cube::Variable::CubeCount => {
                self.num_workgroup_no_axis = true;
                wgsl::Variable::NumWorkgroups
            }
            cube::Variable::SubcubeDim => {
                self.subgroup_size = true;
                wgsl::Variable::SubgroupSize
            }
            cube::Variable::Matrix { .. } => {
                panic!("Cooperative matrix-multiply and accumulate not supported.")
            }
        }
    }

    fn compile_scope(&mut self, value: &mut cube::Scope) -> Vec<wgsl::Instruction> {
        let mut instructions = Vec::new();

        let const_arrays = value
            .const_arrays
            .drain(..)
            .map(|(var, values)| ConstantArray {
                index: var.index().unwrap(),
                item: Self::compile_item(var.item()),
                size: values.len() as u32,
                values: values
                    .into_iter()
                    .map(|val| self.compile_variable(val))
                    .collect(),
            })
            .collect::<Vec<_>>();
        self.const_arrays.extend(const_arrays);

        let processing = value.process();

        for var in processing.variables {
            // We don't declare slices.
            if let cube::Variable::Slice { .. } = var {
                continue;
            }

            instructions.push(wgsl::Instruction::DeclareVariable {
                var: self.compile_variable(var),
            });
        }

        processing
            .operations
            .into_iter()
            .for_each(|op| self.compile_operation(&mut instructions, op));

        instructions
    }

    fn compile_operation(
        &mut self,
        instructions: &mut Vec<wgsl::Instruction>,
        operation: cube::Operation,
    ) {
        match operation {
            cube::Operation::Operator(op) => instructions.push(self.compile_instruction(op)),
            cube::Operation::Metadata(op) => instructions.push(self.compile_metadata(op)),
            cube::Operation::Branch(val) => self.compile_branch(instructions, val),
            cube::Operation::Synchronization(val) => {
                self.compile_synchronization(instructions, val)
            }
            cube::Operation::Subcube(op) => self.compile_subgroup(instructions, op),
            cube::Operation::CoopMma(_) => {
                panic!("Cooperative matrix-multiply and accumulate isn't supported on wgpu.")
            }
        }
    }

    fn compile_subgroup(
        &mut self,
        instructions: &mut Vec<wgsl::Instruction>,
        subgroup: cube::Subcube,
    ) {
        let op = match subgroup {
            cube::Subcube::Elect(op) => Subgroup::Elect {
                out: self.compile_variable(op.out),
            },
            cube::Subcube::All(op) => Subgroup::All {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Subcube::Any(op) => Subgroup::Any {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Subcube::Broadcast(op) => Subgroup::Broadcast {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Subcube::Sum(op) => Subgroup::Sum {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Subcube::Prod(op) => Subgroup::Prod {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Subcube::Min(op) => Subgroup::Min {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Subcube::Max(op) => Subgroup::Max {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
        };

        instructions.push(wgsl::Instruction::Subgroup(op));
    }

    fn compile_branch(&mut self, instructions: &mut Vec<wgsl::Instruction>, branch: cube::Branch) {
        match branch {
            cube::Branch::If(mut op) => instructions.push(wgsl::Instruction::If {
                cond: self.compile_variable(op.cond),
                instructions: self.compile_scope(&mut op.scope),
            }),
            cube::Branch::IfElse(mut op) => instructions.push(wgsl::Instruction::IfElse {
                cond: self.compile_variable(op.cond),
                instructions_if: self.compile_scope(&mut op.scope_if),
                instructions_else: self.compile_scope(&mut op.scope_else),
            }),
            cube::Branch::Select(op) => instructions.push(wgsl::Instruction::Select {
                cond: self.compile_variable(op.cond),
                then: self.compile_variable(op.then),
                or_else: self.compile_variable(op.or_else),
                out: self.compile_variable(op.out),
            }),
            cube::Branch::Switch(mut op) => instructions.push(wgsl::Instruction::Switch {
                value: self.compile_variable(op.value),
                instructions_default: self.compile_scope(&mut op.scope_default),
                cases: op
                    .cases
                    .into_iter()
                    .map(|(val, mut scope)| {
                        (self.compile_variable(val), self.compile_scope(&mut scope))
                    })
                    .collect(),
            }),
            cube::Branch::Return => instructions.push(wgsl::Instruction::Return),
            cube::Branch::Break => instructions.push(wgsl::Instruction::Break),
            cube::Branch::RangeLoop(mut range_loop) => {
                instructions.push(wgsl::Instruction::RangeLoop {
                    i: self.compile_variable(range_loop.i),
                    start: self.compile_variable(range_loop.start),
                    end: self.compile_variable(range_loop.end),
                    step: range_loop.step.map(|it| self.compile_variable(it)),
                    inclusive: range_loop.inclusive,
                    instructions: self.compile_scope(&mut range_loop.scope),
                })
            }
            cube::Branch::Loop(mut op) => instructions.push(wgsl::Instruction::Loop {
                instructions: self.compile_scope(&mut op.scope),
            }),
        };
    }

    fn compile_synchronization(
        &mut self,
        instructions: &mut Vec<wgsl::Instruction>,
        synchronization: cube::Synchronization,
    ) {
        match synchronization {
            cube::Synchronization::SyncUnits => {
                instructions.push(wgsl::Instruction::WorkgroupBarrier)
            }
            cube::Synchronization::SyncStorage => {
                instructions.push(wgsl::Instruction::StorageBarrier)
            }
        };
    }

    fn compile_metadata(&mut self, metadata: cube::Metadata) -> wgsl::Instruction {
        match metadata {
            cube::Metadata::Stride { dim, var, out } => {
                self.stride = true;
                let position = match var {
                    cube::Variable::GlobalInputArray { id, .. } => id as usize,
                    cube::Variable::GlobalOutputArray { id, .. } => self.num_inputs + id as usize,
                    _ => panic!("Only Input and Output have a stride, got: {:?}", var),
                };
                wgsl::Instruction::Stride {
                    dim: self.compile_variable(dim),
                    position,
                    out: self.compile_variable(out),
                }
            }
            cube::Metadata::Shape { dim, var, out } => {
                self.shape = true;
                let position = match var {
                    cube::Variable::GlobalInputArray { id, .. } => id as usize,
                    cube::Variable::GlobalOutputArray { id, .. } => self.num_inputs + id as usize,
                    _ => panic!("Only Input and Output have a shape, got {:?}", var),
                };
                wgsl::Instruction::Shape {
                    dim: self.compile_variable(dim),
                    position,
                    out: self.compile_variable(out),
                }
            }
            cube::Metadata::Length { var, out } => wgsl::Instruction::Length {
                out: self.compile_variable(out),
                var: self.compile_variable(var),
            },
        }
    }

    fn compile_instruction(&mut self, value: cube::Operator) -> wgsl::Instruction {
        match value {
            cube::Operator::Max(op) => wgsl::Instruction::Max {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Min(op) => wgsl::Instruction::Min {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Add(op) => wgsl::Instruction::Add {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Fma(op) => wgsl::Instruction::Fma {
                a: self.compile_variable(op.a),
                b: self.compile_variable(op.b),
                c: self.compile_variable(op.c),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Index(op) => wgsl::Instruction::Index {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::UncheckedIndex(op) => wgsl::Instruction::Index {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Modulo(op) => wgsl::Instruction::Modulo {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Sub(op) => wgsl::Instruction::Sub {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Mul(op) => wgsl::Instruction::Mul {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Div(op) => wgsl::Instruction::Div {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Abs(op) => wgsl::Instruction::Abs {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Exp(op) => wgsl::Instruction::Exp {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Log(op) => wgsl::Instruction::Log {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Log1p(op) => wgsl::Instruction::Log1p {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Cos(op) => wgsl::Instruction::Cos {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Sin(op) => wgsl::Instruction::Sin {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Tanh(op) => wgsl::Instruction::Tanh {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Powf(op) => wgsl::Instruction::Powf {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Sqrt(op) => wgsl::Instruction::Sqrt {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Round(op) => wgsl::Instruction::Round {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Floor(op) => wgsl::Instruction::Floor {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Ceil(op) => wgsl::Instruction::Ceil {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Erf(op) => wgsl::Instruction::Erf {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Recip(op) => wgsl::Instruction::Recip {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Equal(op) => wgsl::Instruction::Equal {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Lower(op) => wgsl::Instruction::Lower {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Clamp(op) => wgsl::Instruction::Clamp {
                input: self.compile_variable(op.input),
                min_value: self.compile_variable(op.min_value),
                max_value: self.compile_variable(op.max_value),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Greater(op) => wgsl::Instruction::Greater {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::LowerEqual(op) => wgsl::Instruction::LowerEqual {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::GreaterEqual(op) => wgsl::Instruction::GreaterEqual {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::NotEqual(op) => wgsl::Instruction::NotEqual {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Assign(op) => wgsl::Instruction::Assign {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::IndexAssign(op) => wgsl::Instruction::IndexAssign {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::UncheckedIndexAssign(op) => wgsl::Instruction::IndexAssign {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::And(op) => wgsl::Instruction::And {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Or(op) => wgsl::Instruction::Or {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Not(op) => wgsl::Instruction::Not {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::BitwiseOr(op) => wgsl::Instruction::BitwiseOr {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::BitwiseAnd(op) => wgsl::Instruction::BitwiseAnd {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::BitwiseXor(op) => wgsl::Instruction::BitwiseXor {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::ShiftLeft(op) => wgsl::Instruction::ShiftLeft {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::ShiftRight(op) => wgsl::Instruction::ShiftRight {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Remainder(op) => wgsl::Instruction::Remainder {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Slice(op) => wgsl::Instruction::Slice {
                input: self.compile_variable(op.input),
                start: self.compile_variable(op.start),
                end: self.compile_variable(op.end),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicLoad(op) => wgsl::Instruction::AtomicLoad {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicStore(op) => wgsl::Instruction::AtomicStore {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicSwap(op) => wgsl::Instruction::AtomicSwap {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicCompareAndSwap(op) => {
                wgsl::Instruction::AtomicCompareExchangeWeak {
                    lhs: self.compile_variable(op.input),
                    cmp: self.compile_variable(op.cmp),
                    value: self.compile_variable(op.val),
                    out: self.compile_variable(op.out),
                }
            }
            cube::Operator::Bitcast(op) => wgsl::Instruction::Bitcast {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicAdd(op) => wgsl::Instruction::AtomicAdd {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicSub(op) => wgsl::Instruction::AtomicSub {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicMax(op) => wgsl::Instruction::AtomicMax {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicMin(op) => wgsl::Instruction::AtomicMin {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicAnd(op) => wgsl::Instruction::AtomicAnd {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicOr(op) => wgsl::Instruction::AtomicOr {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::AtomicXor(op) => wgsl::Instruction::AtomicXor {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Neg(op) => wgsl::Instruction::Negate {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Magnitude(op) => wgsl::Instruction::Magnitude {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Normalize(op) => wgsl::Instruction::Normalize {
                input: self.compile_variable(op.input),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Dot(op) => wgsl::Instruction::Dot {
                lhs: self.compile_variable(op.lhs),
                rhs: self.compile_variable(op.rhs),
                out: self.compile_variable(op.out),
            },
            cube::Operator::InitLine(op) => wgsl::Instruction::VecInit {
                inputs: op
                    .inputs
                    .into_iter()
                    .map(|var| self.compile_variable(var))
                    .collect(),
                out: self.compile_variable(op.out),
            },
            cube::Operator::Copy(op) => wgsl::Instruction::Copy {
                input: self.compile_variable(op.input),
                in_index: self.compile_variable(op.in_index),
                out: self.compile_variable(op.out),
                out_index: self.compile_variable(op.out_index),
            },
            cube::Operator::CopyBulk(op) => wgsl::Instruction::CopyBulk {
                input: self.compile_variable(op.input),
                in_index: self.compile_variable(op.in_index),
                out: self.compile_variable(op.out),
                out_index: self.compile_variable(op.out_index),
                len: op.len,
            },
        }
    }

    fn compile_location(value: cube::Location) -> wgsl::Location {
        match value {
            cube::Location::Storage => wgsl::Location::Storage,
            cube::Location::Cube => wgsl::Location::Workgroup,
        }
    }

    fn compile_visibility(value: cube::Visibility) -> wgsl::Visibility {
        match value {
            cube::Visibility::Read => wgsl::Visibility::Read,
            cube::Visibility::ReadWrite => wgsl::Visibility::ReadWrite,
        }
    }

    fn compile_binding(value: cube::Binding) -> wgsl::Binding {
        wgsl::Binding {
            visibility: Self::compile_visibility(value.visibility),
            location: Self::compile_location(value.location),
            item: Self::compile_item(value.item),
            size: value.size,
        }
    }
}

fn register_extensions(instructions: &[wgsl::Instruction]) -> Vec<wgsl::Extension> {
    let mut extensions = Vec::new();

    let mut register_extension = |extension: wgsl::Extension| {
        if !extensions.contains(&extension) {
            extensions.push(extension);
        }
    };

    // Since not all instructions are native to WGSL, we need to add the custom ones.
    for instruction in instructions {
        match instruction {
            wgsl::Instruction::Powf { lhs: _, rhs, out } => {
                register_extension(wgsl::Extension::PowfPrimitive(out.item()));

                if rhs.is_always_scalar() || rhs.item().vectorization_factor() == 1 {
                    register_extension(wgsl::Extension::PowfScalar(out.item()));
                } else {
                    register_extension(wgsl::Extension::Powf(out.item()));
                }
            }
            wgsl::Instruction::Erf { input, out: _ } => {
                register_extension(wgsl::Extension::Erf(input.item()));
            }
            #[cfg(target_os = "macos")]
            wgsl::Instruction::Tanh { input, out: _ } => {
                register_extension(wgsl::Extension::SafeTanh(input.item()))
            }
            wgsl::Instruction::If {
                cond: _,
                instructions,
            } => {
                for extension in register_extensions(instructions) {
                    register_extension(extension);
                }
            }
            _ => {}
        }
    }

    extensions
}
