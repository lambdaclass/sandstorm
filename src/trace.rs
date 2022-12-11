use gpu_poly::GpuFftField;
use ministark::challenges::Challenges;
use strum_macros::EnumIter;
use gpu_poly::GpuVec;
use gpu_poly::prelude::PageAlignedAllocator;
use ministark::Matrix;
use gpu_poly::fields::p3618502788666131213697322783095070105623107215331596699973092056135872020481::Fp;
use ministark::StarkExtensionOf;
use ministark::Trace;
use ark_ff::Zero;
use ministark::constraints::AlgebraicExpression;
use ark_ff::One;
use ministark::constraints::ExecutionTraceColumn;
use strum::IntoEnumIterator;
use crate::air::CYCLE_HEIGHT;
use crate::air::MEMORY_STEP;
use crate::air::MemoryPermutation;
use crate::air::PUBLIC_MEMORY_STEP;
use crate::binary::CompiledProgram;
use crate::binary::Memory;
use crate::binary::RegisterState;
use crate::binary::RegisterStates;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

pub struct ExecutionTrace {
    pub public_memory_padding_address: usize,
    pub public_memory_padding_value: Fp,
    pub range_check_min: usize,
    pub range_check_max: usize,
    pub public_memory: Vec<(usize, Fp)>,
    pub initial_registers: RegisterState,
    pub final_registers: RegisterState,
    register_states: RegisterStates,
    program: CompiledProgram,
    mem: Memory,
    flags_column: GpuVec<Fp>,
    npc_column: GpuVec<Fp>,
    memory_column: GpuVec<Fp>,
    range_check_column: GpuVec<Fp>,
    auxiliary_column: GpuVec<Fp>,
    base_trace: Matrix<Fp>,
}

impl ExecutionTrace {
    fn new(mem: Memory, register_states: RegisterStates, program: CompiledProgram) -> Self {
        let num_program_cycles = register_states.len();
        let num_trace_cycles = register_states.len().next_power_of_two();
        let trace_len = num_trace_cycles * CYCLE_HEIGHT;
        // let half_offset = 2isize.pow(15);
        // let half_offset = 2u32.pow(15);

        for (i, v) in mem.iter().enumerate() {
            if !v.is_some() {
                println!("FOK {i}");
            }
        }

        let public_memory = program.get_public_memory();

        let mut flags_column = Vec::new_in(PageAlignedAllocator);
        flags_column.resize(trace_len, Fp::zero());

        let mut zeros_column = Vec::new_in(PageAlignedAllocator);
        zeros_column.resize(trace_len, Fp::zero());

        // set `padding_address == padding_value` to make filling the column easy
        let public_memory_padding_address = public_memory_padding_address(&mem, &register_states);
        let public_memory_padding_value = Fp::from(public_memory_padding_address as u64);
        let mut npc_column = Vec::new_in(PageAlignedAllocator);
        npc_column.resize(trace_len, public_memory_padding_value);

        let mut range_check_column = Vec::new_in(PageAlignedAllocator);
        range_check_column.resize(trace_len, Fp::zero());

        let mut auxiliary_column = Vec::new_in(PageAlignedAllocator);
        auxiliary_column.resize(trace_len, Fp::zero());

        let mut range_check_min = 0;
        let mut range_check_max = 0;

        for (i, &RegisterState { pc, ap, fp }) in register_states.iter().enumerate() {
            let trace_offset = i * CYCLE_HEIGHT;
            let word = mem[pc].unwrap();
            assert!(!word.get_flag(Flag::Zero));

            // range check all offset values
            let off_dst = word.get_off_dst();
            let off_op0 = word.get_off_op0();
            let off_op1 = word.get_off_op1();
            range_check_min = range_check_min.min(off_dst).min(off_op0).min(off_op1);
            range_check_max = range_check_max.max(off_dst).max(off_op0).max(off_op1);

            let off_dst = (off_dst as u64).into();
            let off_op0 = (off_op0 as u64).into();
            let off_op1 = (off_op1 as u64).into();
            let dst_addr = (word.get_dst_addr(ap, fp) as u64).into();
            let op0_addr = (word.get_op0_addr(ap, fp) as u64).into();
            let op1_addr = (word.get_op1_addr(pc, ap, fp, &mem) as u64).into();
            let dst = word.get_dst(ap, fp, &mem);
            let op0 = word.get_op0(ap, fp, &mem);
            let op1 = word.get_op1(pc, ap, fp, &mem);
            let res = word.get_res(pc, ap, fp, &mem);
            let tmp0 = word.get_tmp0(ap, fp, &mem);
            let tmp1 = word.get_tmp1(pc, ap, fp, &mem);

            println!("{:016b} ", pc);
            // println!("{:016b} ", word.get_off_dst() as u64);

            // FLAGS
            let flags_virtual_row = &mut flags_column[trace_offset..trace_offset + CYCLE_HEIGHT];
            for flag in Flag::iter() {
                flags_virtual_row[flag as usize] = word.get_flag_prefix(flag).into();
            }

            // NPC
            let npc_virtual_row = &mut npc_column[trace_offset..trace_offset + CYCLE_HEIGHT];
            npc_virtual_row[Npc::Pc as usize] = (pc as u64).into();
            npc_virtual_row[Npc::Instruction as usize] = word.into();
            npc_virtual_row[Npc::PubMemAddr as usize] = Fp::zero();
            npc_virtual_row[Npc::PubMemVal as usize] = Fp::zero();
            npc_virtual_row[Npc::MemOp0Addr as usize] = op0_addr;
            npc_virtual_row[Npc::MemOp0 as usize] = op0;
            npc_virtual_row[PUBLIC_MEMORY_STEP + Npc::PubMemAddr as usize] = Fp::zero();
            npc_virtual_row[PUBLIC_MEMORY_STEP + Npc::PubMemVal as usize] = Fp::zero();
            npc_virtual_row[Npc::MemDstAddr as usize] = dst_addr;
            npc_virtual_row[Npc::MemDst as usize] = dst;
            npc_virtual_row[Npc::MemOp1Addr as usize] = op1_addr;
            npc_virtual_row[Npc::MemOp1 as usize] = op1;

            // MEMORY

            // RANGE CHECK
            let rc_virtual_row = &mut range_check_column[trace_offset..trace_offset + CYCLE_HEIGHT];
            rc_virtual_row[RangeCheck::OffDst as usize] = off_dst;
            rc_virtual_row[RangeCheck::Fp as usize] = (fp as u64).into();
            rc_virtual_row[RangeCheck::OffOp1 as usize] = off_op1;
            rc_virtual_row[RangeCheck::Op0MulOp1 as usize] = op0 * op1;
            rc_virtual_row[RangeCheck::OffOp0 as usize] = off_op0;
            rc_virtual_row[RangeCheck::Ap as usize] = (ap as u64).into();
            rc_virtual_row[RangeCheck::Res as usize] = res;

            // COL8 - TODO: better name
            let aux_virtual_row = &mut auxiliary_column[trace_offset..trace_offset + CYCLE_HEIGHT];
            aux_virtual_row[Auxiliary::Tmp0 as usize] = tmp0;
            aux_virtual_row[Auxiliary::Tmp1 as usize] = tmp1;
        }

        // pad the execution trace by duplicating
        // trace cells for the last cycle
        for column in [
            &mut flags_column,
            &mut npc_column,
            &mut auxiliary_column,
            &mut range_check_column,
        ] {
            let last_cycle_offset = (num_program_cycles - 1) * CYCLE_HEIGHT;
            let (_, trace_suffix) = column.split_at_mut(last_cycle_offset);
            let (last_cycle, padding_rows) = trace_suffix.split_at_mut(CYCLE_HEIGHT);
            let padding_cycles = padding_rows.chunks_mut(CYCLE_HEIGHT);
            padding_cycles.for_each(|padding_cycle| padding_cycle.copy_from_slice(last_cycle))
        }

        // generate the memory column by ordering memory accesses
        let memory_column = get_ordered_memory_accesses(trace_len, &npc_column, &program);

        let base_trace = Matrix::new(vec![
            flags_column.to_vec_in(PageAlignedAllocator),
            zeros_column.to_vec_in(PageAlignedAllocator),
            zeros_column.to_vec_in(PageAlignedAllocator),
            zeros_column.to_vec_in(PageAlignedAllocator),
            zeros_column.to_vec_in(PageAlignedAllocator),
            npc_column.to_vec_in(PageAlignedAllocator),
            memory_column.to_vec_in(PageAlignedAllocator),
            range_check_column.to_vec_in(PageAlignedAllocator),
            auxiliary_column.to_vec_in(PageAlignedAllocator),
        ]);

        let initial_registers = *register_states.first().unwrap();
        let final_registers = *register_states.last().unwrap();

        ExecutionTrace {
            public_memory_padding_address,
            public_memory_padding_value,
            range_check_min,
            range_check_max,
            public_memory,
            initial_registers,
            final_registers,
            flags_column,
            npc_column,
            memory_column,
            range_check_column,
            auxiliary_column,
            base_trace,
            mem,
            register_states,
            program,
        }
    }

    pub fn from_file(program_path: &PathBuf, trace_path: &PathBuf, memory_path: &PathBuf) -> Self {
        let file = File::open(program_path).expect("program file not found");
        let reader = BufReader::new(file);
        let compiled_program: CompiledProgram = serde_json::from_reader(reader).unwrap();
        #[cfg(debug_assertions)]
        compiled_program.validate();

        let register_states = RegisterStates::from_file(trace_path);
        let memory = Memory::from_file(memory_path);

        Self::new(memory, register_states, compiled_program)
    }
}

impl Trace for ExecutionTrace {
    const NUM_BASE_COLUMNS: usize = 9;
    const NUM_EXTENSION_COLUMNS: usize = 1;
    type Fp = Fp;
    type Fq = Fp;

    fn base_columns(&self) -> &Matrix<Self::Fp> {
        &self.base_trace
    }

    fn build_extension_columns(&self, challenges: &Challenges<Fp>) -> Option<Matrix<Fp>> {
        // see distinction between (a', v') and (a, v) in the Cairo paper.
        let z = challenges[MemoryPermutation::Z];
        let alpha = challenges[MemoryPermutation::A];
        let program_order_accesses = self.npc_column.array_chunks::<MEMORY_STEP>();
        let address_order_accesses = self.memory_column.array_chunks::<MEMORY_STEP>();
        let mut running_mem_permutation = Vec::new();
        let mut accumulator = Fp::one();
        for (&[a, v], &[a_prime, v_prime]) in program_order_accesses.zip(address_order_accesses) {
            accumulator *= (z - (a + alpha * v)) / (z - (a_prime + alpha * v_prime));
            running_mem_permutation.push(accumulator);
        }

        // TODO: range check
        let mut permutation_column = Vec::new_in(PageAlignedAllocator);
        permutation_column.resize(self.base_columns().num_rows(), Fp::zero());
        for (i, permutation) in running_mem_permutation.into_iter().enumerate() {
            permutation_column[i * MEMORY_STEP] = permutation;
        }

        Some(Matrix::new(vec![permutation_column]))
    }
}

/// Cairo flag
/// https://eprint.iacr.org/2021/1063.pdf section 9
#[derive(Clone, Copy, EnumIter, PartialEq, Eq)]
pub enum Flag {
    // Group: [FlagGroup::DstReg]
    DstReg = 0,

    // Group: [FlagGroup::Op0]
    Op0Reg = 1,

    // Group: [FlagGroup::Op1Src]
    Op1Imm = 2,
    Op1Fp = 3,
    Op1Ap = 4,

    // Group: [FlagGroup::ResLogic]
    ResAdd = 5,
    ResMul = 6,

    // Group: [FlagGroup::PcUpdate]
    PcJumpAbs = 7,
    PcJumpRel = 8,
    PcJnz = 9,

    // Group: [FlagGroup::ApUpdate]
    ApAdd = 10,
    ApAdd1 = 11,

    // Group: [FlagGroup::Opcode]
    OpcodeCall = 12,
    OpcodeRet = 13,
    OpcodeAssertEq = 14,

    // 0 - padding to make flag cells a power-of-2
    Zero = 15,
}

impl ExecutionTraceColumn for Flag {
    fn index(&self) -> usize {
        0
    }

    fn offset<Fp: GpuFftField, Fq: StarkExtensionOf<Fp>>(
        &self,
        cycle_offset: isize,
    ) -> AlgebraicExpression<Fp, Fq> {
        use AlgebraicExpression::Trace;
        // Get the individual bit (as opposed to the bit prefix)
        let col = self.index();
        let trace_offset = CYCLE_HEIGHT as isize * cycle_offset;
        let flag_offset = trace_offset + *self as isize;
        Trace(col, flag_offset) - (Trace(col, flag_offset + 1) + Trace(col, flag_offset + 1))
    }
}

// NPC? not sure what it means yet - next program counter?
// Trace column 5
// Perhaps control flow is a better name for this column
#[derive(Clone, Copy)]
pub enum Npc {
    // TODO: first word of each instruction?
    Pc = 0, // Program counter
    Instruction = 1,
    PubMemAddr = 2,
    PubMemVal = 3,
    MemOp0Addr = 4,
    MemOp0 = 5,
    // TODO: What kind of memory address? 8 - memory function?
    MemDstAddr = 8,
    MemDst = 9,
    // NOTE: cycle cells 10 and 11 is occupied by PubMemAddr since the public memory step is 8.
    // This means it applies twice (2, 3) then (8+2, 8+3) within a single 16 row cycle.
    MemOp1Addr = 12,
    MemOp1 = 13,
}

impl ExecutionTraceColumn for Npc {
    fn index(&self) -> usize {
        5
    }

    fn offset<Fp: GpuFftField, Fq: StarkExtensionOf<Fp>>(
        &self,
        offset: isize,
    ) -> AlgebraicExpression<Fp, Fq> {
        let step = match self {
            Npc::PubMemAddr | Npc::PubMemVal => PUBLIC_MEMORY_STEP,
            _ => CYCLE_HEIGHT,
        } as isize;
        let column = self.index();
        let trace_offset = step * offset + *self as isize;
        AlgebraicExpression::Trace(column, trace_offset)
    }
}

// Trace column 6 - memory
#[derive(Clone, Copy)]
pub enum Mem {
    // TODO = 0,
    Address = 0,
    Value = 1,
}

impl ExecutionTraceColumn for Mem {
    fn index(&self) -> usize {
        6
    }

    fn offset<Fp: GpuFftField, Fq: StarkExtensionOf<Fp>>(
        &self,
        mem_offset: isize,
    ) -> AlgebraicExpression<Fp, Fq> {
        let column = self.index();
        let trace_offset = MEMORY_STEP as isize * mem_offset + *self as isize;
        AlgebraicExpression::Trace(column, trace_offset)
    }
}

// Trace column 7
#[derive(Clone, Copy)]
pub enum RangeCheck {
    OffDst = 0,
    Ap = 3, // Allocation pointer (ap)
    // TODO 2
    OffOp1 = 4,
    Op0MulOp1 = 7, // =op0*op1
    OffOp0 = 8,
    Fp = 11, // Frame pointer (fp)
    Res = 15,
}

impl ExecutionTraceColumn for RangeCheck {
    fn index(&self) -> usize {
        7
    }

    fn offset<Fp: GpuFftField, Fq: StarkExtensionOf<Fp>>(
        &self,
        cycle_offset: isize,
    ) -> AlgebraicExpression<Fp, Fq> {
        let column = self.index();
        let trace_offset = CYCLE_HEIGHT as isize * cycle_offset + *self as isize;
        AlgebraicExpression::Trace(column, trace_offset)
    }
}

// Auxiliary column 8
#[derive(Clone, Copy)]
pub enum Auxiliary {
    Tmp0 = 0,
    Tmp1 = 8,
}

impl ExecutionTraceColumn for Auxiliary {
    fn index(&self) -> usize {
        8
    }

    fn offset<Fp: GpuFftField, Fq: StarkExtensionOf<Fp>>(
        &self,
        cycle_offset: isize,
    ) -> AlgebraicExpression<Fp, Fq> {
        let column = self.index();
        let trace_offset = CYCLE_HEIGHT as isize * cycle_offset + *self as isize;
        AlgebraicExpression::Trace(column, trace_offset)
    }
}

// Trace column 6 - permutations
#[derive(Clone, Copy)]
pub enum Permutation {
    // TODO = 0,
    Memory = 0,
    RangeCheck = 1,
}

impl ExecutionTraceColumn for Permutation {
    fn index(&self) -> usize {
        9
    }

    fn offset<Fp: GpuFftField, Fq: StarkExtensionOf<Fp>>(
        &self,
        offset: isize,
    ) -> AlgebraicExpression<Fp, Fq> {
        let column = self.index();
        let trace_offset = match self {
            Permutation::Memory => MEMORY_STEP as isize * offset + *self as isize,
            Permutation::RangeCheck => 4 * offset + *self as isize,
        };
        AlgebraicExpression::Trace(column, trace_offset)
    }
}

/// Obtains an address that can be used for padding public memory accesses
fn public_memory_padding_address(mem: &Memory, register_states: &RegisterStates) -> usize {
    // find the highest memory address accessed during the execution of the program
    let mut highest_access = 1;
    for &RegisterState { ap, fp, pc } in register_states.iter() {
        // TODO: this is pretty wasteful as this info is available in the trace
        // generation loop
        let word = mem[pc].unwrap();
        let dst_addr = word.get_dst_addr(ap, fp);
        let op0_addr = word.get_op0_addr(ap, fp);
        let op1_addr = word.get_op1_addr(pc, ap, fp, mem);
        highest_access = highest_access
            .max(dst_addr)
            .max(op0_addr)
            .max(op1_addr)
            .max(pc);
    }
    highest_access + 1
}

// TODO: support input, output and builtins
fn get_ordered_memory_accesses(
    trace_len: usize,
    npc_column: &[Fp],
    program: &CompiledProgram,
) -> Vec<Fp, PageAlignedAllocator> {
    // the number of cells allocated for the public memory
    let num_pub_mem_cells = trace_len / PUBLIC_MEMORY_STEP;

    let pub_mem = program.get_public_memory();
    // the actual number of public memory cells
    let pub_mem_len = pub_mem.len();
    let pub_mem_accesses = pub_mem.iter().map(|&(a, v)| [(a as u64).into(), v.into()]);

    // order all memory accesses by address
    // memory accesses are of the form [address, value]
    let mut ordered_accesses = npc_column
        .array_chunks()
        .copied()
        .chain(pub_mem_accesses)
        .collect::<Vec<[Fp; MEMORY_STEP]>>();
    ordered_accesses.sort();

    // remove the `pub_mem_len` dummy accesses to address `0`. The justification for
    // this is explained in section 9.8 of the Cairo paper https://eprint.iacr.org/2021/1063.pdf.
    // SHARP requires the first address to start at address 1
    let (zeros, ordered_accesses) = ordered_accesses.split_at(num_pub_mem_cells);
    assert!(zeros.iter().all(|[a, v]| a.is_zero() && v.is_zero()));
    assert!(ordered_accesses[0][0].is_one());

    // check memory is "continuous" and "single valued"
    ordered_accesses
        .array_windows()
        .enumerate()
        .for_each(|(i, &[[a, v], [a_next, v_next]])| {
            assert!(
                (a == a_next && v == v_next) || a == a_next - Fp::one(),
                "mismatch at {i}: a={a}, v={v}, a_next={a_next}, v_next={v_next}"
            );
        });

    // Sandstorm uses the highest access as padding
    let [padding_addr, padding_val] = ordered_accesses.last().unwrap();
    assert_eq!(padding_addr, padding_val);
    let mut res = ordered_accesses.flatten().to_vec_in(PageAlignedAllocator);
    res.resize(trace_len, *padding_val);
    res
}
