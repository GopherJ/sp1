mod instruction;
mod io;
mod opcode;
mod program;
mod record;
mod register;
mod state;
mod syscall;

use crate::cpu::{MemoryReadRecord, MemoryRecord, MemoryWriteRecord};
use crate::utils::env;
use crate::{alu::AluEvent, cpu::CpuEvent};
use hashbrown::hash_map::Entry;
pub use instruction::*;
use nohash_hasher::BuildNoHashHasher;
pub use opcode::*;
pub use program::*;
pub use record::*;
pub use register::*;
pub use state::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;
pub use syscall::*;

use p3_baby_bear::BabyBear;
use p3_field::AbstractField;

use self::state::ExecutionState;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AccessPosition {
    Memory = 0,
    // Note that these AccessPositions mean that when when read/writing registers, they must be
    // read/written in the following order: C, B, A.
    C = 1,
    B = 2,
    A = 3,
}

/// An implementation of a runtime for the SP1 VM.
///
/// The runtime is responsible for executing a user program and tracing important events which occur
/// during execution (i.e., memory reads, alu operations, etc).
///
/// For more information on the RV32IM instruction set, see the following:
/// https://www.cs.sfu.ca/~ashriram/Courses/CS295/assets/notebooks/RISCV/RISCV_CARD.pdf
pub struct Runtime {
    /// The program.
    pub program: Arc<Program>,

    /// Current global state.
    pub state: ExecutionState,

    /// The record containing all events emitted during execution so far.
    pub record: ExecutionRecord,

    /// The record for the current CPU opcode containing relevant events.
    pub cpu_record: CpuRecord,

    /// The maximum size of each shard.
    pub shard_size: u32,

    /// A counter for the number of cycles that have been executed in certain functions.
    pub cycle_tracker: HashMap<String, (u32, u32)>,

    /// A buffer for writing trace events to a file.
    pub trace_buf: Option<BufWriter<File>>,

    /// Whether the runtime is in constrained mode or not.
    /// In unconstrained mode, any events, clock, register, or memory changes are reset after leaving
    /// the unconstrained block. The only thing preserved is writes to the input stream.
    pub unconstrained: bool,

    pub(crate) unconstrained_state: ForkState,

    pub syscall_map: HashMap<SyscallCode, Rc<dyn Syscall>>,
}

impl Runtime {
    // Create a new runtime
    pub fn new(program: Program) -> Self {
        let program_arc = Arc::new(program);
        let record = ExecutionRecord {
            program: program_arc.clone(),
            ..Default::default()
        };
        // Write pc trace to file if TRACE_FILE is set
        let trace_buf = if let Ok(trace_file) = std::env::var("TRACE_FILE") {
            let file = File::create(trace_file).unwrap();
            Some(BufWriter::new(file))
        } else {
            None
        };

        Self {
            record,
            state: ExecutionState::new(program_arc.pc_start),
            program: program_arc,
            cpu_record: CpuRecord::default(),
            shard_size: env::shard_size() as u32 * 4,
            cycle_tracker: HashMap::new(),
            trace_buf,
            unconstrained: false,
            unconstrained_state: ForkState::default(),
            syscall_map: default_syscall_map(),
        }
    }

    /// Get the current values of the registers.
    pub fn registers(&self) -> [u32; 32] {
        let mut registers = [0; 32];
        for i in 0..32 {
            let addr = Register::from_u32(i as u32) as u32;
            registers[i] = match self.state.memory.get(&addr) {
                Some((value, _, _)) => *value,
                None => 0,
            };
        }
        registers
    }

    /// Get the current value of a register.
    pub fn register(&self, register: Register) -> u32 {
        let addr = register as u32;
        match self.state.memory.get(&addr) {
            Some((value, _, _)) => *value,
            None => 0,
        }
    }

    /// Get the current value of a word.
    pub fn word(&self, addr: u32) -> u32 {
        match self.state.memory.get(&addr) {
            Some((value, _, _)) => *value,
            None => 0,
        }
    }

    pub fn byte(&self, addr: u32) -> u8 {
        let word = self.word(addr - addr % 4);
        (word >> ((addr % 4) * 8)) as u8
    }

    #[inline]
    fn clk_from_position(&self, position: &AccessPosition) -> u32 {
        self.state.clk + *position as u32
    }

    pub fn current_shard(&self) -> u32 {
        self.state.current_shard
    }

    fn align(&self, addr: u32) -> u32 {
        addr - addr % 4
    }

    #[inline]
    fn validate_memory_access(&self, addr: u32, position: AccessPosition) {
        if position == AccessPosition::Memory {
            assert_eq!(addr % 4, 0, "addr is not aligned");
            let _ = BabyBear::from_canonical_u32(addr);
            assert!(addr > 40); // Assert that the address is > the max register.
        } else {
            let _ = Register::from_u32(addr);
        }
    }

    pub fn mr(&mut self, addr: u32, shard: u32, clk: u32) -> MemoryReadRecord {
        // Get the memory entry.
        let memory_entry = self.state.memory.entry(addr);
        if self.unconstrained {
            // If we're in unconstrained mode, we don't want to modify state, so we'll save the
            // original state if it's the first time modifying it.
            let prev_value = match memory_entry {
                Entry::Occupied(ref entry) => Some(entry.get()),
                Entry::Vacant(_) => None,
            };
            self.unconstrained_state
                .memory_diff
                .entry(addr)
                .or_insert(prev_value.copied());
        }
        // If it's the first time accessing this address, initialize previous values as zero.
        let entry_value = memory_entry.or_insert((0, 0, 0));
        // Get the last time this memory address was accessed, and then update with current clock.
        let (value, prev_shard, prev_timestamp) = *entry_value;
        (entry_value.1, entry_value.2) = (shard, clk);

        MemoryReadRecord::new(value, shard, clk, prev_shard, prev_timestamp)
    }

    pub fn mw(&mut self, addr: u32, value: u32, shard: u32, clk: u32) -> MemoryWriteRecord {
        // Get the memory entry.
        let memory_entry = self.state.memory.entry(addr);
        if self.unconstrained {
            // If we're in unconstrained mode, we don't want to modify state, so we'll save the
            // original state if it's the first time modifying it.
            let prev_value = match memory_entry {
                Entry::Occupied(ref entry) => Some(entry.get()),
                Entry::Vacant(_) => None,
            };
            self.unconstrained_state
                .memory_diff
                .entry(addr)
                .or_insert(prev_value.copied());
        }
        // If it's the first time accessing this address, initialize previous values as zero.
        let entry_value = memory_entry.or_insert((0, 0, 0));
        // Get previous values and then update with new values.
        let (prev_value, prev_shard, prev_timestamp) = *entry_value;
        *entry_value = (value, shard, clk);
        MemoryWriteRecord::new(value, shard, clk, prev_value, prev_shard, prev_timestamp)
    }

    /// Read from memory, assuming that all addresses are aligned.
    pub fn mr_cpu(&mut self, addr: u32, position: AccessPosition) -> u32 {
        self.validate_memory_access(addr, position);

        let record = self.mr(
            addr,
            self.current_shard(),
            self.clk_from_position(&position),
        );

        if !self.unconstrained {
            match position {
                AccessPosition::A => self.cpu_record.a = Some(record.into()),
                AccessPosition::B => self.cpu_record.b = Some(record.into()),
                AccessPosition::C => self.cpu_record.c = Some(record.into()),
                AccessPosition::Memory => self.cpu_record.memory = Some(record.into()),
            }
        }
        record.value
    }

    /// Write to memory.
    pub fn mw_cpu(&mut self, addr: u32, value: u32, position: AccessPosition) {
        self.validate_memory_access(addr, position);

        let record = self.mw(
            addr,
            value,
            self.current_shard(),
            self.clk_from_position(&position),
        );

        // Set the records.
        if !self.unconstrained {
            match position {
                AccessPosition::A => {
                    assert!(self.cpu_record.a.is_none());
                    self.cpu_record.a = Some(record.into());
                }
                AccessPosition::B => {
                    assert!(self.cpu_record.b.is_none());
                    self.cpu_record.b = Some(record.into());
                }
                AccessPosition::C => {
                    assert!(self.cpu_record.c.is_none());
                    self.cpu_record.c = Some(record.into());
                }
                AccessPosition::Memory => {
                    assert!(self.cpu_record.memory.is_none());
                    self.cpu_record.memory = Some(record.into());
                }
            }
        }
    }

    /// Read from register.
    pub fn rr(&mut self, register: Register, position: AccessPosition) -> u32 {
        self.mr_cpu(register as u32, position)
    }

    /// Write to register.
    pub fn rw(&mut self, register: Register, value: u32) {
        if register == Register::X0 {
            // We don't write to %x0. See 2.6 Load and Store Instruction on
            // P.18 of the RISC-V spec.
            return;
        }
        // The only time we are writing to a register is when it is register A.
        self.mw_cpu(register as u32, value, AccessPosition::A)
    }

    /// Emit a CPU event.
    #[allow(clippy::too_many_arguments)]
    fn emit_cpu(
        &mut self,
        shard: u32,
        clk: u32,
        pc: u32,
        instruction: Instruction,
        a: u32,
        b: u32,
        c: u32,
        memory_store_value: Option<u32>,
        record: CpuRecord,
    ) {
        let cpu_event = CpuEvent {
            shard,
            clk,
            pc,
            instruction,
            a,
            a_record: record.a,
            b,
            b_record: record.b,
            c,
            c_record: record.c,
            memory: memory_store_value,
            memory_record: record.memory,
        };
        self.record.cpu_events.push(cpu_event);
    }

    /// Emit an ALU event.
    fn emit_alu(&mut self, clk: u32, opcode: Opcode, a: u32, b: u32, c: u32) {
        let event = AluEvent {
            clk,
            opcode,
            a,
            b,
            c,
        };
        match opcode {
            Opcode::ADD => {
                self.record.add_events.push(event);
            }
            Opcode::SUB => {
                self.record.sub_events.push(event);
            }
            Opcode::XOR | Opcode::OR | Opcode::AND => {
                self.record.bitwise_events.push(event);
            }
            Opcode::SLL => {
                self.record.shift_left_events.push(event);
            }
            Opcode::SRL | Opcode::SRA => {
                self.record.shift_right_events.push(event);
            }
            Opcode::SLT | Opcode::SLTU => {
                self.record.lt_events.push(event);
            }
            Opcode::MUL | Opcode::MULHU | Opcode::MULHSU | Opcode::MULH => {
                self.record.mul_events.push(event);
            }
            Opcode::DIVU | Opcode::REMU | Opcode::DIV | Opcode::REM => {
                self.record.divrem_events.push(event);
            }
            _ => {}
        }
    }

    /// Fetch the destination register and input operand values for an ALU instruction.
    #[inline(always)]
    fn alu_rr(&mut self, instruction: Instruction) -> (Register, u32, u32) {
        if !instruction.imm_c {
            let (rd, rs1, rs2) = instruction.r_type();
            let c = self.rr(rs2, AccessPosition::C);
            let b = self.rr(rs1, AccessPosition::B);
            (rd, b, c)
        } else if !instruction.imm_b && instruction.imm_c {
            let (rd, rs1, imm) = instruction.i_type();
            let (rd, b, c) = (rd, self.rr(rs1, AccessPosition::B), imm);
            (rd, b, c)
        } else {
            assert!(instruction.imm_b && instruction.imm_c);
            let (rd, b, c) = (
                Register::from_u32(instruction.op_a),
                instruction.op_b,
                instruction.op_c,
            );
            (rd, b, c)
        }
    }

    /// Set the destination register with the result and emit an ALU event.
    #[inline(always)]
    fn alu_rw(&mut self, instruction: Instruction, rd: Register, a: u32, b: u32, c: u32) {
        self.rw(rd, a);
        self.emit_alu(self.state.clk, instruction.opcode, a, b, c);
    }

    /// Fetch the input operand values for a load instruction.
    #[inline(always)]
    fn load_rr(&mut self, instruction: Instruction) -> (Register, u32, u32, u32, u32) {
        let (rd, rs1, imm) = instruction.i_type();
        let (b, c) = (self.rr(rs1, AccessPosition::B), imm);
        let addr = b.wrapping_add(c);
        let memory_value = self.mr_cpu(self.align(addr), AccessPosition::Memory);
        (rd, b, c, addr, memory_value)
    }

    /// Fetch the input operand values for a store instruction.
    #[inline(always)]
    fn store_rr(&mut self, instruction: Instruction) -> (u32, u32, u32, u32, u32) {
        let (rs1, rs2, imm) = instruction.s_type();
        let c = imm;
        let b = self.rr(rs2, AccessPosition::B);
        let a = self.rr(rs1, AccessPosition::A);
        let addr = b.wrapping_add(c);
        let memory_value = self.word(self.align(addr));
        (a, b, c, addr, memory_value)
    }

    /// Fetch the input operand values for a branch instruction.
    #[inline(always)]
    fn branch_rr(&mut self, instruction: Instruction) -> (u32, u32, u32) {
        let (rs1, rs2, imm) = instruction.b_type();
        let c = imm;
        let b = self.rr(rs2, AccessPosition::B);
        let a = self.rr(rs1, AccessPosition::A);
        (a, b, c)
    }

    /// Fetch the instruction at the current program counter.
    #[inline(always)]
    fn fetch(&self) -> Instruction {
        let idx = ((self.state.pc - self.program.pc_base) / 4) as usize;
        self.program.instructions[idx]
    }

    fn get_syscall(&mut self, code: SyscallCode) -> Option<&Rc<dyn Syscall>> {
        self.syscall_map.get(&code)
    }

    fn max_syscall_cycles(&self) -> u32 {
        self.syscall_map
            .values()
            .map(|syscall| syscall.num_extra_cycles())
            .max()
            .unwrap_or(0)
    }

    /// Execute the given instruction over the current state of the runtime.
    fn execute(&mut self, instruction: Instruction) {
        let pc = self.state.pc;
        let mut next_pc = self.state.pc.wrapping_add(4);

        let rd: Register;
        let (a, b, c): (u32, u32, u32);
        let (addr, memory_read_value): (u32, u32);
        let mut memory_store_value: Option<u32> = None;
        self.cpu_record = CpuRecord::default();

        match instruction.opcode {
            // Arithmetic instructions.
            Opcode::ADD => {
                (rd, b, c) = self.alu_rr(instruction);
                a = b.wrapping_add(c);
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::SUB => {
                (rd, b, c) = self.alu_rr(instruction);
                a = b.wrapping_sub(c);
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::XOR => {
                (rd, b, c) = self.alu_rr(instruction);
                a = b ^ c;
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::OR => {
                (rd, b, c) = self.alu_rr(instruction);
                a = b | c;
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::AND => {
                (rd, b, c) = self.alu_rr(instruction);
                a = b & c;
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::SLL => {
                (rd, b, c) = self.alu_rr(instruction);
                a = b.wrapping_shl(c);
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::SRL => {
                (rd, b, c) = self.alu_rr(instruction);
                a = b.wrapping_shr(c);
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::SRA => {
                (rd, b, c) = self.alu_rr(instruction);
                a = (b as i32).wrapping_shr(c) as u32;
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::SLT => {
                (rd, b, c) = self.alu_rr(instruction);
                a = if (b as i32) < (c as i32) { 1 } else { 0 };
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::SLTU => {
                (rd, b, c) = self.alu_rr(instruction);
                a = if b < c { 1 } else { 0 };
                self.alu_rw(instruction, rd, a, b, c);
            }

            // Load instructions.
            Opcode::LB => {
                (rd, b, c, addr, memory_read_value) = self.load_rr(instruction);
                let value = (memory_read_value).to_le_bytes()[(addr % 4) as usize];
                a = ((value as i8) as i32) as u32;
                memory_store_value = Some(memory_read_value);
                self.rw(rd, a);
            }
            Opcode::LH => {
                (rd, b, c, addr, memory_read_value) = self.load_rr(instruction);
                assert_eq!(addr % 2, 0, "addr is not aligned");
                let value = match (addr >> 1) % 2 {
                    0 => memory_read_value & 0x0000FFFF,
                    1 => (memory_read_value & 0xFFFF0000) >> 16,
                    _ => unreachable!(),
                };
                a = ((value as i16) as i32) as u32;
                memory_store_value = Some(memory_read_value);
                self.rw(rd, a);
            }
            Opcode::LW => {
                (rd, b, c, addr, memory_read_value) = self.load_rr(instruction);
                assert_eq!(addr % 4, 0, "addr is not aligned");
                a = memory_read_value;
                memory_store_value = Some(memory_read_value);
                self.rw(rd, a);
            }
            Opcode::LBU => {
                (rd, b, c, addr, memory_read_value) = self.load_rr(instruction);
                let value = (memory_read_value).to_le_bytes()[(addr % 4) as usize];
                a = value as u32;
                memory_store_value = Some(memory_read_value);
                self.rw(rd, a);
            }
            Opcode::LHU => {
                (rd, b, c, addr, memory_read_value) = self.load_rr(instruction);
                assert_eq!(addr % 2, 0, "addr is not aligned");
                let value = match (addr >> 1) % 2 {
                    0 => memory_read_value & 0x0000FFFF,
                    1 => (memory_read_value & 0xFFFF0000) >> 16,
                    _ => unreachable!(),
                };
                a = (value as u16) as u32;
                memory_store_value = Some(memory_read_value);
                self.rw(rd, a);
            }

            // Store instructions.
            Opcode::SB => {
                (a, b, c, addr, memory_read_value) = self.store_rr(instruction);
                let value = match addr % 4 {
                    0 => (a & 0x000000FF) + (memory_read_value & 0xFFFFFF00),
                    1 => ((a & 0x000000FF) << 8) + (memory_read_value & 0xFFFF00FF),
                    2 => ((a & 0x000000FF) << 16) + (memory_read_value & 0xFF00FFFF),
                    3 => ((a & 0x000000FF) << 24) + (memory_read_value & 0x00FFFFFF),
                    _ => unreachable!(),
                };
                memory_store_value = Some(value);
                self.mw_cpu(self.align(addr), value, AccessPosition::Memory);
            }
            Opcode::SH => {
                (a, b, c, addr, memory_read_value) = self.store_rr(instruction);
                assert_eq!(addr % 2, 0, "addr is not aligned");
                let value = match (addr >> 1) % 2 {
                    0 => (a & 0x0000FFFF) + (memory_read_value & 0xFFFF0000),
                    1 => ((a & 0x0000FFFF) << 16) + (memory_read_value & 0x0000FFFF),
                    _ => unreachable!(),
                };
                memory_store_value = Some(value);
                self.mw_cpu(self.align(addr), value, AccessPosition::Memory);
            }
            Opcode::SW => {
                (a, b, c, addr, _) = self.store_rr(instruction);
                assert_eq!(addr % 4, 0, "addr is not aligned");
                let value = a;
                memory_store_value = Some(value);
                self.mw_cpu(self.align(addr), value, AccessPosition::Memory);
            }

            // B-type instructions.
            Opcode::BEQ => {
                (a, b, c) = self.branch_rr(instruction);
                if a == b {
                    next_pc = self.state.pc.wrapping_add(c);
                }
            }
            Opcode::BNE => {
                (a, b, c) = self.branch_rr(instruction);
                if a != b {
                    next_pc = self.state.pc.wrapping_add(c);
                }
            }
            Opcode::BLT => {
                (a, b, c) = self.branch_rr(instruction);
                if (a as i32) < (b as i32) {
                    next_pc = self.state.pc.wrapping_add(c);
                }
            }
            Opcode::BGE => {
                (a, b, c) = self.branch_rr(instruction);
                if (a as i32) >= (b as i32) {
                    next_pc = self.state.pc.wrapping_add(c);
                }
            }
            Opcode::BLTU => {
                (a, b, c) = self.branch_rr(instruction);
                if a < b {
                    next_pc = self.state.pc.wrapping_add(c);
                }
            }
            Opcode::BGEU => {
                (a, b, c) = self.branch_rr(instruction);
                if a >= b {
                    next_pc = self.state.pc.wrapping_add(c);
                }
            }

            // Jump instructions.
            Opcode::JAL => {
                let (rd, imm) = instruction.j_type();
                (b, c) = (imm, 0);
                a = self.state.pc + 4;
                self.rw(rd, a);
                next_pc = self.state.pc.wrapping_add(imm);
            }
            Opcode::JALR => {
                let (rd, rs1, imm) = instruction.i_type();
                (b, c) = (self.rr(rs1, AccessPosition::B), imm);
                a = self.state.pc + 4;
                self.rw(rd, a);
                next_pc = b.wrapping_add(c);
            }

            // Upper immediate instructions.
            Opcode::AUIPC => {
                let (rd, imm) = instruction.u_type();
                (b, c) = (imm, imm);
                a = self.state.pc.wrapping_add(b);
                self.rw(rd, a);
            }

            // System instructions.
            Opcode::ECALL => {
                let t0 = Register::X5;
                let a0 = Register::X10;
                let syscall_id = self.register(t0);
                let syscall = SyscallCode::from_u32(syscall_id);

                let init_clk = self.state.clk;
                let syscall_impl = self.get_syscall(syscall).cloned();
                let mut precompile_rt = SyscallContext::new(self);

                if let Some(syscall_impl) = syscall_impl {
                    a = syscall_impl.execute(&mut precompile_rt);
                    next_pc = precompile_rt.next_pc;
                    self.state.clk = precompile_rt.clk;
                    assert_eq!(init_clk + syscall_impl.num_extra_cycles(), self.state.clk);
                } else {
                    panic!("Unsupported syscall: {:?}", syscall);
                }

                // We have to do this AFTER the precompile execution because the CPU event
                // gets emitted at the end of this loop with the incremented clock.
                // TODO: fix this.
                self.rw(a0, a);
                (b, c) = (self.rr(t0, AccessPosition::B), 0);
            }

            Opcode::EBREAK => {
                todo!()
            }

            // Multiply instructions.
            Opcode::MUL => {
                (rd, b, c) = self.alu_rr(instruction);
                a = b.wrapping_mul(c);
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::MULH => {
                (rd, b, c) = self.alu_rr(instruction);
                a = (((b as i32) as i64).wrapping_mul((c as i32) as i64) >> 32) as u32;
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::MULHU => {
                (rd, b, c) = self.alu_rr(instruction);
                a = ((b as u64).wrapping_mul(c as u64) >> 32) as u32;
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::MULHSU => {
                (rd, b, c) = self.alu_rr(instruction);
                a = (((b as i32) as i64).wrapping_mul(c as i64) >> 32) as u32;
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::DIV => {
                (rd, b, c) = self.alu_rr(instruction);
                if c == 0 {
                    a = u32::MAX;
                } else {
                    a = (b as i32).wrapping_div(c as i32) as u32;
                }
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::DIVU => {
                (rd, b, c) = self.alu_rr(instruction);
                if c == 0 {
                    a = u32::MAX;
                } else {
                    a = b.wrapping_div(c);
                }
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::REM => {
                (rd, b, c) = self.alu_rr(instruction);
                if c == 0 {
                    a = b;
                } else {
                    a = (b as i32).wrapping_rem(c as i32) as u32;
                }
                self.alu_rw(instruction, rd, a, b, c);
            }
            Opcode::REMU => {
                (rd, b, c) = self.alu_rr(instruction);
                if c == 0 {
                    a = b;
                } else {
                    a = b.wrapping_rem(c);
                }
                self.alu_rw(instruction, rd, a, b, c);
            }

            Opcode::UNIMP => {
                // See https://github.com/riscv-non-isa/riscv-asm-manual/blob/master/riscv-asm.md#instruction-aliases
                panic!("UNIMP encountered, we should never get here.");
            }
        }

        // Update the program counter.
        self.state.pc = next_pc;

        // Emit the CPU event for this cycle.
        self.emit_cpu(
            self.current_shard(),
            self.state.clk,
            pc,
            instruction,
            a,
            b,
            c,
            memory_store_value,
            self.cpu_record,
        );
    }

    /// Execute the program.
    pub fn run(&mut self) {
        tracing::info_span!("load memory").in_scope(|| {
            // First load the memory image into the memory table.
            for (addr, value) in self.program.memory_image.iter() {
                self.state.memory.insert(*addr, (*value, 0, 0));
            }
        });

        let max_syscall_cycles = self.max_syscall_cycles();

        self.state.clk += 1;
        while self.state.pc.wrapping_sub(self.program.pc_base)
            < (self.program.instructions.len() * 4) as u32
        {
            // Fetch the instruction at the current program counter.
            let instruction = self.fetch();

            if let Some(ref mut buf) = self.trace_buf {
                if !self.unconstrained {
                    buf.write_all(&u32::to_be_bytes(self.state.pc)).unwrap();
                }
            }

            let width = 12;
            log::trace!(
                "clk={} [pc=0x{:x?}] {:<width$?} |         x0={:<width$} x1={:<width$} x2={:<width$} x3={:<width$} x4={:<width$} x5={:<width$} x6={:<width$} x7={:<width$} x8={:<width$} x9={:<width$} x10={:<width$} x11={:<width$} x12={:<width$} x13={:<width$} x14={:<width$} x15={:<width$} x16={:<width$} x17={:<width$} x18={:<width$}",
                self.state.global_clk,
                self.state.pc,
                instruction,
                self.register(Register::X0),
                self.register(Register::X1),
                self.register(Register::X2),
                self.register(Register::X3),
                self.register(Register::X4),
                self.register(Register::X5),
                self.register(Register::X6),
                self.register(Register::X7),
                self.register(Register::X8),
                self.register(Register::X9),
                self.register(Register::X10),
                self.register(Register::X11),
                self.register(Register::X12),
                self.register(Register::X13),
                self.register(Register::X14),
                self.register(Register::X15),
                self.register(Register::X16),
                self.register(Register::X17),
                self.register(Register::X18),
            );

            // Execute the instruction.
            self.execute(instruction);

            // Increment the clock.
            self.state.global_clk += 1;
            self.state.clk += 4;

            // If there's not enough cycles left for another instruction, move to the next shard.
            // We multiply by 4 because clk is incremented by 4 for each normal instruction.
            if !self.unconstrained && max_syscall_cycles + self.state.clk >= self.shard_size * 4 {
                self.state.current_shard += 1;
                self.state.clk = 0;
            }
        }
        if let Some(ref mut buf) = self.trace_buf {
            buf.flush().unwrap();
        }

        // Call postprocess to set up all variables needed for global accounts, like memory
        // argument or any other deferred tables.
        tracing::info_span!("postprocess").in_scope(|| self.postprocess());
    }

    fn postprocess(&mut self) {
        let mut program_memory_used = HashMap::with_hasher(BuildNoHashHasher::<u32>::default());
        for (key, value) in &self.program.memory_image {
            // By default we assume that the program_memory is used.
            program_memory_used.insert(*key, (*value, 1));
        }

        let mut first_memory_record = Vec::new();
        let mut last_memory_record = Vec::new();

        let memory_keys = self.state.memory.keys().cloned().collect::<Vec<u32>>();
        for addr in memory_keys {
            let (value, shard, timestamp) = *self.state.memory.get(&addr).unwrap();
            if shard == 0 && timestamp == 0 {
                // This means that we never accessed this memory location throughout our entire program.
                // The only way this can happen is if this was in the program memory image.
                // We mark this (addr, value) as not used in the `program_memory_used` map.
                program_memory_used.insert(addr, (value, 0));
                continue;
            }
            // If the memory addr was accessed, we only add it to "first_memory_record" if it was
            // not in the program_memory_image, otherwise we'll add to the memory argument from
            // the program_memory_image table.
            if !self.program.memory_image.contains_key(&addr) {
                first_memory_record.push((
                    addr,
                    MemoryRecord {
                        value: 0,
                        shard: 0,
                        timestamp: 0,
                    },
                    1,
                ));
            }

            last_memory_record.push((
                addr,
                MemoryRecord {
                    value,
                    shard,
                    timestamp,
                },
                1,
            ));
        }

        let mut program_memory_record = program_memory_used
            .iter()
            .map(|(&addr, &(value, used))| {
                (
                    addr,
                    MemoryRecord {
                        value,
                        shard: 0,
                        timestamp: 0,
                    },
                    used,
                )
            })
            .collect::<Vec<(u32, MemoryRecord, u32)>>();
        program_memory_record.sort_by_key(|&(addr, _, _)| addr);

        self.record.first_memory_record = first_memory_record;
        self.record.last_memory_record = last_memory_record;
        self.record.program_memory_record = program_memory_record;
    }
}

#[cfg(test)]
pub mod tests {

    use crate::{
        runtime::Register,
        utils::tests::{FIBONACCI_ELF, SSZ_WITHDRAWALS_ELF},
    };

    use super::{Instruction, Opcode, Program, Runtime};

    pub fn simple_program() -> Program {
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::ADD, 31, 30, 29, false, false),
        ];
        Program::new(instructions, 0, 0)
    }

    pub fn fibonacci_program() -> Program {
        Program::from(FIBONACCI_ELF)
    }

    pub fn ssz_withdrawals_program() -> Program {
        Program::from(SSZ_WITHDRAWALS_ELF)
    }

    pub fn ecall_lwa_program() -> Program {
        let instructions = vec![
            Instruction::new(Opcode::ADD, 5, 0, 101, false, true),
            Instruction::new(Opcode::ECALL, 10, 5, 0, false, true),
        ];
        Program::new(instructions, 0, 0)
    }

    #[test]
    fn test_simple_program_run() {
        let program = simple_program();
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 42);
    }

    #[test]
    fn test_add() {
        // main:
        //     addi x29, x0, 5
        //     addi x30, x0, 37
        //     add x31, x30, x29
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::ADD, 31, 30, 29, false, false),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 42);
    }

    #[test]
    fn test_sub() {
        //     addi x29, x0, 5
        //     addi x30, x0, 37
        //     sub x31, x30, x29
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::SUB, 31, 30, 29, false, false),
        ];
        let program = Program::new(instructions, 0, 0);

        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 32);
    }

    #[test]
    fn test_xor() {
        //     addi x29, x0, 5
        //     addi x30, x0, 37
        //     xor x31, x30, x29
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::XOR, 31, 30, 29, false, false),
        ];
        let program = Program::new(instructions, 0, 0);

        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 32);
    }

    #[test]
    fn test_or() {
        //     addi x29, x0, 5
        //     addi x30, x0, 37
        //     or x31, x30, x29
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::OR, 31, 30, 29, false, false),
        ];
        let program = Program::new(instructions, 0, 0);

        let mut runtime = Runtime::new(program);

        runtime.run();
        assert_eq!(runtime.register(Register::X31), 37);
    }

    #[test]
    fn test_and() {
        //     addi x29, x0, 5
        //     addi x30, x0, 37
        //     and x31, x30, x29
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::AND, 31, 30, 29, false, false),
        ];
        let program = Program::new(instructions, 0, 0);

        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 5);
    }

    #[test]
    fn test_sll() {
        //     addi x29, x0, 5
        //     addi x30, x0, 37
        //     sll x31, x30, x29
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::SLL, 31, 30, 29, false, false),
        ];
        let program = Program::new(instructions, 0, 0);

        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 1184);
    }

    #[test]
    fn test_srl() {
        //     addi x29, x0, 5
        //     addi x30, x0, 37
        //     srl x31, x30, x29
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::SRL, 31, 30, 29, false, false),
        ];
        let program = Program::new(instructions, 0, 0);

        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 1);
    }

    #[test]
    fn test_sra() {
        //     addi x29, x0, 5
        //     addi x30, x0, 37
        //     sra x31, x30, x29
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::SRA, 31, 30, 29, false, false),
        ];
        let program = Program::new(instructions, 0, 0);

        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 1);
    }

    #[test]
    fn test_slt() {
        //     addi x29, x0, 5
        //     addi x30, x0, 37
        //     slt x31, x30, x29
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::SLT, 31, 30, 29, false, false),
        ];
        let program = Program::new(instructions, 0, 0);

        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 0);
    }

    #[test]
    fn test_sltu() {
        //     addi x29, x0, 5
        //     addi x30, x0, 37
        //     sltu x31, x30, x29
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 0, 37, false, true),
            Instruction::new(Opcode::SLTU, 31, 30, 29, false, false),
        ];
        let program = Program::new(instructions, 0, 0);

        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 0);
    }

    #[test]
    fn test_addi() {
        //     addi x29, x0, 5
        //     addi x30, x29, 37
        //     addi x31, x30, 42
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 29, 37, false, true),
            Instruction::new(Opcode::ADD, 31, 30, 42, false, true),
        ];
        let program = Program::new(instructions, 0, 0);

        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 84);
    }

    #[test]
    fn test_addi_negative() {
        //     addi x29, x0, 5
        //     addi x30, x29, -1
        //     addi x31, x30, 4
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::ADD, 30, 29, 0xffffffff, false, true),
            Instruction::new(Opcode::ADD, 31, 30, 4, false, true),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 5 - 1 + 4);
    }

    #[test]
    fn test_xori() {
        //     addi x29, x0, 5
        //     xori x30, x29, 37
        //     xori x31, x30, 42
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::XOR, 30, 29, 37, false, true),
            Instruction::new(Opcode::XOR, 31, 30, 42, false, true),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 10);
    }

    #[test]
    fn test_ori() {
        //     addi x29, x0, 5
        //     ori x30, x29, 37
        //     ori x31, x30, 42
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::OR, 30, 29, 37, false, true),
            Instruction::new(Opcode::OR, 31, 30, 42, false, true),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 47);
    }

    #[test]
    fn test_andi() {
        //     addi x29, x0, 5
        //     andi x30, x29, 37
        //     andi x31, x30, 42
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::AND, 30, 29, 37, false, true),
            Instruction::new(Opcode::AND, 31, 30, 42, false, true),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 0);
    }

    #[test]
    fn test_slli() {
        //     addi x29, x0, 5
        //     slli x31, x29, 37
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 5, false, true),
            Instruction::new(Opcode::SLL, 31, 29, 4, false, true),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 80);
    }

    #[test]
    fn test_srli() {
        //    addi x29, x0, 5
        //    srli x31, x29, 37
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 42, false, true),
            Instruction::new(Opcode::SRL, 31, 29, 4, false, true),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 2);
    }

    #[test]
    fn test_srai() {
        //   addi x29, x0, 5
        //   srai x31, x29, 37
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 42, false, true),
            Instruction::new(Opcode::SRA, 31, 29, 4, false, true),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 2);
    }

    #[test]
    fn test_slti() {
        //   addi x29, x0, 5
        //   slti x31, x29, 37
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 42, false, true),
            Instruction::new(Opcode::SLT, 31, 29, 37, false, true),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 0);
    }

    #[test]
    fn test_sltiu() {
        //   addi x29, x0, 5
        //   sltiu x31, x29, 37
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 42, false, true),
            Instruction::new(Opcode::SLTU, 31, 29, 37, false, true),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.register(Register::X31), 0);
    }

    #[test]
    fn test_jalr() {
        //   addi x11, x11, 100
        //   jalr x5, x11, 8
        //
        // `JALR rd offset(rs)` reads the value at rs, adds offset to it and uses it as the
        // destination address. It then stores the address of the next instruction in rd in case
        // we'd want to come back here.

        let instructions = vec![
            Instruction::new(Opcode::ADD, 11, 11, 100, false, true),
            Instruction::new(Opcode::JALR, 5, 11, 8, false, true),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.registers()[Register::X5 as usize], 8);
        assert_eq!(runtime.registers()[Register::X11 as usize], 100);
        assert_eq!(runtime.state.pc, 108);
    }

    fn simple_op_code_test(opcode: Opcode, expected: u32, a: u32, b: u32) {
        let instructions = vec![
            Instruction::new(Opcode::ADD, 10, 0, a, false, true),
            Instruction::new(Opcode::ADD, 11, 0, b, false, true),
            Instruction::new(opcode, 12, 10, 11, false, false),
        ];
        let program = Program::new(instructions, 0, 0);
        let mut runtime = Runtime::new(program);
        runtime.run();
        assert_eq!(runtime.registers()[Register::X12 as usize], expected);
    }

    #[test]
    fn multiplication_tests() {
        simple_op_code_test(Opcode::MULHU, 0x00000000, 0x00000000, 0x00000000);
        simple_op_code_test(Opcode::MULHU, 0x00000000, 0x00000001, 0x00000001);
        simple_op_code_test(Opcode::MULHU, 0x00000000, 0x00000003, 0x00000007);
        simple_op_code_test(Opcode::MULHU, 0x00000000, 0x00000000, 0xffff8000);
        simple_op_code_test(Opcode::MULHU, 0x00000000, 0x80000000, 0x00000000);
        simple_op_code_test(Opcode::MULHU, 0x7fffc000, 0x80000000, 0xffff8000);
        simple_op_code_test(Opcode::MULHU, 0x0001fefe, 0xaaaaaaab, 0x0002fe7d);
        simple_op_code_test(Opcode::MULHU, 0x0001fefe, 0x0002fe7d, 0xaaaaaaab);
        simple_op_code_test(Opcode::MULHU, 0xfe010000, 0xff000000, 0xff000000);
        simple_op_code_test(Opcode::MULHU, 0xfffffffe, 0xffffffff, 0xffffffff);
        simple_op_code_test(Opcode::MULHU, 0x00000000, 0xffffffff, 0x00000001);
        simple_op_code_test(Opcode::MULHU, 0x00000000, 0x00000001, 0xffffffff);

        simple_op_code_test(Opcode::MULHSU, 0x00000000, 0x00000000, 0x00000000);
        simple_op_code_test(Opcode::MULHSU, 0x00000000, 0x00000001, 0x00000001);
        simple_op_code_test(Opcode::MULHSU, 0x00000000, 0x00000003, 0x00000007);
        simple_op_code_test(Opcode::MULHSU, 0x00000000, 0x00000000, 0xffff8000);
        simple_op_code_test(Opcode::MULHSU, 0x00000000, 0x80000000, 0x00000000);
        simple_op_code_test(Opcode::MULHSU, 0x80004000, 0x80000000, 0xffff8000);
        simple_op_code_test(Opcode::MULHSU, 0xffff0081, 0xaaaaaaab, 0x0002fe7d);
        simple_op_code_test(Opcode::MULHSU, 0x0001fefe, 0x0002fe7d, 0xaaaaaaab);
        simple_op_code_test(Opcode::MULHSU, 0xff010000, 0xff000000, 0xff000000);
        simple_op_code_test(Opcode::MULHSU, 0xffffffff, 0xffffffff, 0xffffffff);
        simple_op_code_test(Opcode::MULHSU, 0xffffffff, 0xffffffff, 0x00000001);
        simple_op_code_test(Opcode::MULHSU, 0x00000000, 0x00000001, 0xffffffff);

        simple_op_code_test(Opcode::MULH, 0x00000000, 0x00000000, 0x00000000);
        simple_op_code_test(Opcode::MULH, 0x00000000, 0x00000001, 0x00000001);
        simple_op_code_test(Opcode::MULH, 0x00000000, 0x00000003, 0x00000007);
        simple_op_code_test(Opcode::MULH, 0x00000000, 0x00000000, 0xffff8000);
        simple_op_code_test(Opcode::MULH, 0x00000000, 0x80000000, 0x00000000);
        simple_op_code_test(Opcode::MULH, 0x00000000, 0x80000000, 0x00000000);
        simple_op_code_test(Opcode::MULH, 0xffff0081, 0xaaaaaaab, 0x0002fe7d);
        simple_op_code_test(Opcode::MULH, 0xffff0081, 0x0002fe7d, 0xaaaaaaab);
        simple_op_code_test(Opcode::MULH, 0x00010000, 0xff000000, 0xff000000);
        simple_op_code_test(Opcode::MULH, 0x00000000, 0xffffffff, 0xffffffff);
        simple_op_code_test(Opcode::MULH, 0xffffffff, 0xffffffff, 0x00000001);
        simple_op_code_test(Opcode::MULH, 0xffffffff, 0x00000001, 0xffffffff);

        simple_op_code_test(Opcode::MUL, 0x00001200, 0x00007e00, 0xb6db6db7);
        simple_op_code_test(Opcode::MUL, 0x00001240, 0x00007fc0, 0xb6db6db7);
        simple_op_code_test(Opcode::MUL, 0x00000000, 0x00000000, 0x00000000);
        simple_op_code_test(Opcode::MUL, 0x00000001, 0x00000001, 0x00000001);
        simple_op_code_test(Opcode::MUL, 0x00000015, 0x00000003, 0x00000007);
        simple_op_code_test(Opcode::MUL, 0x00000000, 0x00000000, 0xffff8000);
        simple_op_code_test(Opcode::MUL, 0x00000000, 0x80000000, 0x00000000);
        simple_op_code_test(Opcode::MUL, 0x00000000, 0x80000000, 0xffff8000);
        simple_op_code_test(Opcode::MUL, 0x0000ff7f, 0xaaaaaaab, 0x0002fe7d);
        simple_op_code_test(Opcode::MUL, 0x0000ff7f, 0x0002fe7d, 0xaaaaaaab);
        simple_op_code_test(Opcode::MUL, 0x00000000, 0xff000000, 0xff000000);
        simple_op_code_test(Opcode::MUL, 0x00000001, 0xffffffff, 0xffffffff);
        simple_op_code_test(Opcode::MUL, 0xffffffff, 0xffffffff, 0x00000001);
        simple_op_code_test(Opcode::MUL, 0xffffffff, 0x00000001, 0xffffffff);
    }

    fn neg(a: u32) -> u32 {
        u32::MAX - a + 1
    }

    #[test]
    fn division_tests() {
        simple_op_code_test(Opcode::DIVU, 3, 20, 6);
        simple_op_code_test(Opcode::DIVU, 715827879, u32::MAX - 20 + 1, 6);
        simple_op_code_test(Opcode::DIVU, 0, 20, u32::MAX - 6 + 1);
        simple_op_code_test(Opcode::DIVU, 0, u32::MAX - 20 + 1, u32::MAX - 6 + 1);

        simple_op_code_test(Opcode::DIVU, 1 << 31, 1 << 31, 1);
        simple_op_code_test(Opcode::DIVU, 0, 1 << 31, u32::MAX - 1 + 1);

        simple_op_code_test(Opcode::DIVU, u32::MAX, 1 << 31, 0);
        simple_op_code_test(Opcode::DIVU, u32::MAX, 1, 0);
        simple_op_code_test(Opcode::DIVU, u32::MAX, 0, 0);

        simple_op_code_test(Opcode::DIV, 3, 18, 6);
        simple_op_code_test(Opcode::DIV, neg(6), neg(24), 4);
        simple_op_code_test(Opcode::DIV, neg(2), 16, neg(8));
        simple_op_code_test(Opcode::DIV, neg(1), 0, 0);

        // Overflow cases
        simple_op_code_test(Opcode::DIV, 1 << 31, 1 << 31, neg(1));
        simple_op_code_test(Opcode::REM, 0, 1 << 31, neg(1));
    }

    #[test]
    fn remainder_tests() {
        simple_op_code_test(Opcode::REM, 7, 16, 9);
        simple_op_code_test(Opcode::REM, neg(4), neg(22), 6);
        simple_op_code_test(Opcode::REM, 1, 25, neg(3));
        simple_op_code_test(Opcode::REM, neg(2), neg(22), neg(4));
        simple_op_code_test(Opcode::REM, 0, 873, 1);
        simple_op_code_test(Opcode::REM, 0, 873, neg(1));
        simple_op_code_test(Opcode::REM, 5, 5, 0);
        simple_op_code_test(Opcode::REM, neg(5), neg(5), 0);
        simple_op_code_test(Opcode::REM, 0, 0, 0);

        simple_op_code_test(Opcode::REMU, 4, 18, 7);
        simple_op_code_test(Opcode::REMU, 6, neg(20), 11);
        simple_op_code_test(Opcode::REMU, 23, 23, neg(6));
        simple_op_code_test(Opcode::REMU, neg(21), neg(21), neg(11));
        simple_op_code_test(Opcode::REMU, 5, 5, 0);
        simple_op_code_test(Opcode::REMU, neg(1), neg(1), 0);
        simple_op_code_test(Opcode::REMU, 0, 0, 0);
    }

    #[test]
    fn shift_tests() {
        simple_op_code_test(Opcode::SLL, 0x00000001, 0x00000001, 0);
        simple_op_code_test(Opcode::SLL, 0x00000002, 0x00000001, 1);
        simple_op_code_test(Opcode::SLL, 0x00000080, 0x00000001, 7);
        simple_op_code_test(Opcode::SLL, 0x00004000, 0x00000001, 14);
        simple_op_code_test(Opcode::SLL, 0x80000000, 0x00000001, 31);
        simple_op_code_test(Opcode::SLL, 0xffffffff, 0xffffffff, 0);
        simple_op_code_test(Opcode::SLL, 0xfffffffe, 0xffffffff, 1);
        simple_op_code_test(Opcode::SLL, 0xffffff80, 0xffffffff, 7);
        simple_op_code_test(Opcode::SLL, 0xffffc000, 0xffffffff, 14);
        simple_op_code_test(Opcode::SLL, 0x80000000, 0xffffffff, 31);
        simple_op_code_test(Opcode::SLL, 0x21212121, 0x21212121, 0);
        simple_op_code_test(Opcode::SLL, 0x42424242, 0x21212121, 1);
        simple_op_code_test(Opcode::SLL, 0x90909080, 0x21212121, 7);
        simple_op_code_test(Opcode::SLL, 0x48484000, 0x21212121, 14);
        simple_op_code_test(Opcode::SLL, 0x80000000, 0x21212121, 31);
        simple_op_code_test(Opcode::SLL, 0x21212121, 0x21212121, 0xffffffe0);
        simple_op_code_test(Opcode::SLL, 0x42424242, 0x21212121, 0xffffffe1);
        simple_op_code_test(Opcode::SLL, 0x90909080, 0x21212121, 0xffffffe7);
        simple_op_code_test(Opcode::SLL, 0x48484000, 0x21212121, 0xffffffee);
        simple_op_code_test(Opcode::SLL, 0x00000000, 0x21212120, 0xffffffff);

        simple_op_code_test(Opcode::SRL, 0xffff8000, 0xffff8000, 0);
        simple_op_code_test(Opcode::SRL, 0x7fffc000, 0xffff8000, 1);
        simple_op_code_test(Opcode::SRL, 0x01ffff00, 0xffff8000, 7);
        simple_op_code_test(Opcode::SRL, 0x0003fffe, 0xffff8000, 14);
        simple_op_code_test(Opcode::SRL, 0x0001ffff, 0xffff8001, 15);
        simple_op_code_test(Opcode::SRL, 0xffffffff, 0xffffffff, 0);
        simple_op_code_test(Opcode::SRL, 0x7fffffff, 0xffffffff, 1);
        simple_op_code_test(Opcode::SRL, 0x01ffffff, 0xffffffff, 7);
        simple_op_code_test(Opcode::SRL, 0x0003ffff, 0xffffffff, 14);
        simple_op_code_test(Opcode::SRL, 0x00000001, 0xffffffff, 31);
        simple_op_code_test(Opcode::SRL, 0x21212121, 0x21212121, 0);
        simple_op_code_test(Opcode::SRL, 0x10909090, 0x21212121, 1);
        simple_op_code_test(Opcode::SRL, 0x00424242, 0x21212121, 7);
        simple_op_code_test(Opcode::SRL, 0x00008484, 0x21212121, 14);
        simple_op_code_test(Opcode::SRL, 0x00000000, 0x21212121, 31);
        simple_op_code_test(Opcode::SRL, 0x21212121, 0x21212121, 0xffffffe0);
        simple_op_code_test(Opcode::SRL, 0x10909090, 0x21212121, 0xffffffe1);
        simple_op_code_test(Opcode::SRL, 0x00424242, 0x21212121, 0xffffffe7);
        simple_op_code_test(Opcode::SRL, 0x00008484, 0x21212121, 0xffffffee);
        simple_op_code_test(Opcode::SRL, 0x00000000, 0x21212121, 0xffffffff);

        simple_op_code_test(Opcode::SRA, 0x00000000, 0x00000000, 0);
        simple_op_code_test(Opcode::SRA, 0xc0000000, 0x80000000, 1);
        simple_op_code_test(Opcode::SRA, 0xff000000, 0x80000000, 7);
        simple_op_code_test(Opcode::SRA, 0xfffe0000, 0x80000000, 14);
        simple_op_code_test(Opcode::SRA, 0xffffffff, 0x80000001, 31);
        simple_op_code_test(Opcode::SRA, 0x7fffffff, 0x7fffffff, 0);
        simple_op_code_test(Opcode::SRA, 0x3fffffff, 0x7fffffff, 1);
        simple_op_code_test(Opcode::SRA, 0x00ffffff, 0x7fffffff, 7);
        simple_op_code_test(Opcode::SRA, 0x0001ffff, 0x7fffffff, 14);
        simple_op_code_test(Opcode::SRA, 0x00000000, 0x7fffffff, 31);
        simple_op_code_test(Opcode::SRA, 0x81818181, 0x81818181, 0);
        simple_op_code_test(Opcode::SRA, 0xc0c0c0c0, 0x81818181, 1);
        simple_op_code_test(Opcode::SRA, 0xff030303, 0x81818181, 7);
        simple_op_code_test(Opcode::SRA, 0xfffe0606, 0x81818181, 14);
        simple_op_code_test(Opcode::SRA, 0xffffffff, 0x81818181, 31);
    }

    pub fn simple_memory_program() -> Program {
        let instructions = vec![
            Instruction::new(Opcode::ADD, 29, 0, 0x12348765, false, true),
            // SW and LW
            Instruction::new(Opcode::SW, 29, 0, 0x27654320, false, true),
            Instruction::new(Opcode::LW, 28, 0, 0x27654320, false, true),
            // LBU
            Instruction::new(Opcode::LBU, 27, 0, 0x27654320, false, true),
            Instruction::new(Opcode::LBU, 26, 0, 0x27654321, false, true),
            Instruction::new(Opcode::LBU, 25, 0, 0x27654322, false, true),
            Instruction::new(Opcode::LBU, 24, 0, 0x27654323, false, true),
            // LB
            Instruction::new(Opcode::LB, 23, 0, 0x27654320, false, true),
            Instruction::new(Opcode::LB, 22, 0, 0x27654321, false, true),
            // LHU
            Instruction::new(Opcode::LHU, 21, 0, 0x27654320, false, true),
            Instruction::new(Opcode::LHU, 20, 0, 0x27654322, false, true),
            // LU
            Instruction::new(Opcode::LH, 19, 0, 0x27654320, false, true),
            Instruction::new(Opcode::LH, 18, 0, 0x27654322, false, true),
            // SB
            Instruction::new(Opcode::ADD, 17, 0, 0x38276525, false, true),
            // Save the value 0x12348765 into address 0x43627530
            Instruction::new(Opcode::SW, 29, 0, 0x43627530, false, true),
            Instruction::new(Opcode::SB, 17, 0, 0x43627530, false, true),
            Instruction::new(Opcode::LW, 16, 0, 0x43627530, false, true),
            Instruction::new(Opcode::SB, 17, 0, 0x43627531, false, true),
            Instruction::new(Opcode::LW, 15, 0, 0x43627530, false, true),
            Instruction::new(Opcode::SB, 17, 0, 0x43627532, false, true),
            Instruction::new(Opcode::LW, 14, 0, 0x43627530, false, true),
            Instruction::new(Opcode::SB, 17, 0, 0x43627533, false, true),
            Instruction::new(Opcode::LW, 13, 0, 0x43627530, false, true),
            // SH
            // Save the value 0x12348765 into address 0x43627530
            Instruction::new(Opcode::SW, 29, 0, 0x43627530, false, true),
            Instruction::new(Opcode::SH, 17, 0, 0x43627530, false, true),
            Instruction::new(Opcode::LW, 12, 0, 0x43627530, false, true),
            Instruction::new(Opcode::SH, 17, 0, 0x43627532, false, true),
            Instruction::new(Opcode::LW, 11, 0, 0x43627530, false, true),
        ];
        Program::new(instructions, 0, 0)
    }

    #[test]
    fn test_simple_memory_program_run() {
        let program = simple_memory_program();
        let mut runtime = Runtime::new(program);
        runtime.run();

        // Assert SW & LW case
        assert_eq!(runtime.register(Register::X28), 0x12348765);

        // Assert LBU cases
        assert_eq!(runtime.register(Register::X27), 0x65);
        assert_eq!(runtime.register(Register::X26), 0x87);
        assert_eq!(runtime.register(Register::X25), 0x34);
        assert_eq!(runtime.register(Register::X24), 0x12);

        // Assert LB cases
        assert_eq!(runtime.register(Register::X23), 0x65);
        assert_eq!(runtime.register(Register::X22), 0xffffff87);

        // Assert LHU cases
        assert_eq!(runtime.register(Register::X21), 0x8765);
        assert_eq!(runtime.register(Register::X20), 0x1234);

        // Assert LH cases
        assert_eq!(runtime.register(Register::X19), 0xffff8765);
        assert_eq!(runtime.register(Register::X18), 0x1234);

        // Assert SB cases
        assert_eq!(runtime.register(Register::X16), 0x12348725);
        assert_eq!(runtime.register(Register::X15), 0x12342525);
        assert_eq!(runtime.register(Register::X14), 0x12252525);
        assert_eq!(runtime.register(Register::X13), 0x25252525);

        // Assert SH cases
        assert_eq!(runtime.register(Register::X12), 0x12346525);
        assert_eq!(runtime.register(Register::X11), 0x65256525);
    }
}
