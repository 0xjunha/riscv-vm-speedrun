use crate::error::GuestTrap;
use crate::memory::{
    INPUT_START, Image, Memory, PERM_EXEC, PERM_READ, PermissionTemplate, STACK_END,
};

/// Instruction limit used when none is specified.
pub const DEFAULT_INSTRUCTION_LIMIT: u64 = 100_000_000;
/// Largest accepted instruction limit.
pub const MAX_INSTRUCTION_LIMIT: u64 = 100_000_000;
/// Output limit used when none is specified, in bytes (1 MiB).
pub const DEFAULT_OUTPUT_LIMIT: u32 = 1_048_576;
/// Largest accepted output limit, in bytes (1 MiB).
pub const MAX_OUTPUT_LIMIT: u32 = 1_048_576;
/// Largest accepted guest input, in bytes (4 MiB).
pub const MAX_INPUT_LENGTH: usize = 4_194_304;

pub struct LoadedProgram<E> {
    image: Image,
    permissions: PermissionTemplate,
    engine: E,
}

impl<E: Engine + Default> LoadedProgram<E> {
    pub fn new(elf: &[u8]) -> Result<Self, String> {
        let image = crate::elf::load(elf)?;
        let mut engine = E::default();
        engine.prepare(&image)?;
        let permissions = PermissionTemplate::new(&image);
        Ok(Self {
            image,
            permissions,
            engine,
        })
    }

    pub fn run(&mut self, input: &[u8], instruction_limit: u64, output_limit: u32) -> CompletedRun {
        let mut machine = Machine::from_permission_template(
            &self.image,
            input,
            output_limit,
            &self.permissions,
            self.engine.initialize_direct_memory(),
        );
        let result = self.engine.run(&mut machine, instruction_limit);
        CompletedRun { machine, result }
    }
}

pub trait Engine {
    fn prepare(&mut self, _image: &Image) -> Result<(), String> {
        Ok(())
    }

    /// Requests direct guest-memory initialization when each run is created.
    ///
    /// Interpreter engines retain the sparse default. Native engines whose
    /// steady state always needs a flat address space may opt in to avoid
    /// cloning sparse pages immediately before converting them.
    fn initialize_direct_memory(&self) -> bool {
        false
    }

    fn run(&mut self, machine: &mut Machine, instruction_limit: u64) -> RunResult;
}

pub struct CompletedRun {
    pub machine: Machine,
    pub result: RunResult,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Termination {
    Exit(u32),
    Trap(GuestTrap),
    InstructionLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunResult {
    pub termination: Termination,
    pub retired: u64,
    pub output_length: usize,
}

impl RunResult {
    pub fn json(self) -> String {
        let (status, exit_code, trap, resource_failure) = match self.termination {
            Termination::Exit(code) => ("exit", code.to_string(), "null".into(), "null"),
            Termination::Trap(trap) => (
                "trap",
                "null".into(),
                format!(
                    "{{\"cause\":\"{}\",\"pc\":{},\"value\":{}}}",
                    trap.cause, trap.pc, trap.value
                ),
                "null",
            ),
            Termination::InstructionLimit => (
                "resource_failure",
                "null".into(),
                "null".into(),
                "{\"cause\":\"InstructionLimit\"}",
            ),
        };
        format!(
            "{{\"schema_version\":1,\"status\":\"{status}\",\"exit_code\":{exit_code},\
             \"trap\":{trap},\"resource_failure\":{resource_failure},\
             \"retired_instructions\":{},\"output_length\":{}}}",
            self.retired, self.output_length
        )
    }
}

enum Step {
    Continue,
    Exit(u32),
}

/// A fetched instruction with the fields shared by Rust execution engines.
#[derive(Clone, Copy, Debug)]
pub struct DecodedInstruction {
    pc: u32,
    raw: u32,
    opcode: u32,
    rd: usize,
    funct3: u32,
    rs1: usize,
    rs2: usize,
    funct7: u32,
}

impl DecodedInstruction {
    pub const fn pc(self) -> u32 {
        self.pc
    }

    pub const fn raw(self) -> u32 {
        self.raw
    }

    pub const fn opcode(self) -> u32 {
        self.opcode
    }

    pub const fn rd(self) -> usize {
        self.rd
    }

    pub const fn funct3(self) -> u32 {
        self.funct3
    }

    pub const fn rs1(self) -> usize {
        self.rs1
    }

    pub const fn rs2(self) -> usize {
        self.rs2
    }

    pub const fn funct7(self) -> u32 {
        self.funct7
    }

    pub const fn ends_block(self) -> bool {
        matches!(self.opcode, 0x63 | 0x67 | 0x6f | 0x73)
            || !matches!(self.opcode, 0x03 | 0x0f | 0x13 | 0x17 | 0x23 | 0x33 | 0x37)
    }

    /// Returns the target encoded by a conditional branch.
    pub const fn branch_target(self) -> u32 {
        let encoded = (((self.raw >> 31) & 1) << 12)
            | (((self.raw >> 7) & 1) << 11)
            | (((self.raw >> 25) & 0x3f) << 5)
            | (((self.raw >> 8) & 0xf) << 1);
        self.pc.wrapping_add(sign_extend(encoded, 13))
    }

    /// Returns the target encoded by a direct jump.
    pub const fn jump_target(self) -> u32 {
        let encoded = ((self.raw >> 31) << 20)
            | (((self.raw >> 12) & 0xff) << 12)
            | (((self.raw >> 20) & 1) << 11)
            | (((self.raw >> 21) & 0x3ff) << 1);
        self.pc.wrapping_add(sign_extend(encoded, 21))
    }

    /// Returns the static target of a conditional branch or direct jump.
    pub const fn direct_target(self) -> Option<u32> {
        match self.opcode {
            0x63 => Some(self.branch_target()),
            0x6f => Some(self.jump_target()),
            _ => None,
        }
    }
}

pub struct Machine {
    pub registers: [u32; 32],
    pub pc: u32,
    pub memory: Memory,
    pub output: Vec<u8>,
    pub retired: u64,
    output_limit: u32,
}

impl Machine {
    pub fn new(image: &Image, input: &[u8], output_limit: u32) -> Self {
        Self::with_memory(image, input, output_limit, Memory::new(image, input))
    }

    fn from_permission_template(
        image: &Image,
        input: &[u8],
        output_limit: u32,
        permissions: &PermissionTemplate,
        initialize_direct_memory: bool,
    ) -> Self {
        let memory = if initialize_direct_memory {
            Memory::from_permission_template_direct(image, input, permissions)
        } else {
            Memory::from_permission_template(image, input, permissions)
        };
        Self::with_memory(image, input, output_limit, memory)
    }

    fn with_memory(image: &Image, input: &[u8], output_limit: u32, memory: Memory) -> Self {
        let mut registers = [0; 32];
        registers[2] = STACK_END;
        registers[10] = INPUT_START;
        registers[11] = input.len() as u32;
        Self {
            registers,
            pc: image.entry,
            memory,
            output: Vec::new(),
            retired: 0,
            output_limit,
        }
    }

    pub fn result(&self, termination: Termination) -> RunResult {
        RunResult {
            termination,
            retired: self.retired,
            output_length: self.output.len(),
        }
    }

    fn write_register(&mut self, register: usize, value: u32) {
        if register != 0 {
            self.registers[register] = value;
        }
    }

    fn illegal(&self, instruction: u32) -> GuestTrap {
        GuestTrap::new("IllegalInstruction", self.pc, instruction)
    }

    #[inline(always)]
    pub fn fetch_decode(&self, pc: u32) -> Result<DecodedInstruction, GuestTrap> {
        if pc & 3 != 0 {
            return Err(GuestTrap::new("InstructionAddressMisaligned", pc, pc));
        }
        self.memory
            .check(pc, 4, PERM_EXEC, "InstructionAccessFault", pc)?;
        let raw = self.memory.load_u32(pc);
        Ok(DecodedInstruction {
            pc,
            raw,
            opcode: raw & 0x7f,
            rd: ((raw >> 7) & 0x1f) as usize,
            funct3: (raw >> 12) & 7,
            rs1: ((raw >> 15) & 0x1f) as usize,
            rs2: ((raw >> 20) & 0x1f) as usize,
            funct7: raw >> 25,
        })
    }

    #[inline(always)]
    pub fn execute_one(
        &mut self,
        instruction: Result<DecodedInstruction, GuestTrap>,
    ) -> Option<Termination> {
        match instruction.and_then(|instruction| self.execute(instruction)) {
            Ok(Step::Continue) => {
                self.retired += 1;
                None
            }
            Ok(Step::Exit(code)) => {
                self.retired += 1;
                Some(Termination::Exit(code))
            }
            Err(trap) => Some(Termination::Trap(trap)),
        }
    }

    #[inline(always)]
    fn execute(&mut self, decoded: DecodedInstruction) -> Result<Step, GuestTrap> {
        let DecodedInstruction {
            pc,
            raw: instruction,
            opcode,
            rd,
            funct3,
            rs1,
            rs2,
            funct7,
        } = decoded;
        let next_pc = pc.wrapping_add(4);

        match opcode {
            0x37 => {
                self.write_register(rd, instruction & 0xffff_f000);
                self.pc = next_pc;
            }
            0x17 => {
                self.write_register(rd, pc.wrapping_add(instruction & 0xffff_f000));
                self.pc = next_pc;
            }
            0x6f => {
                let target = decoded.jump_target();
                if target & 3 != 0 {
                    return Err(GuestTrap::new("InstructionAddressMisaligned", pc, target));
                }
                self.write_register(rd, next_pc);
                self.pc = target;
            }
            0x67 => {
                if funct3 != 0 {
                    return Err(self.illegal(instruction));
                }
                let target =
                    self.registers[rs1].wrapping_add(sign_extend(instruction >> 20, 12)) & !1;
                if target & 3 != 0 {
                    return Err(GuestTrap::new("InstructionAddressMisaligned", pc, target));
                }
                self.write_register(rd, next_pc);
                self.pc = target;
            }
            0x63 => {
                let left = self.registers[rs1];
                let right = self.registers[rs2];
                let taken = match funct3 {
                    0 => left == right,
                    1 => left != right,
                    4 => (left as i32) < (right as i32),
                    5 => (left as i32) >= (right as i32),
                    6 => left < right,
                    7 => left >= right,
                    _ => return Err(self.illegal(instruction)),
                };
                if taken {
                    let target = decoded.branch_target();
                    if target & 3 != 0 {
                        return Err(GuestTrap::new("InstructionAddressMisaligned", pc, target));
                    }
                    self.pc = target;
                } else {
                    self.pc = next_pc;
                }
            }
            0x03 => {
                let (size, signed) = match funct3 {
                    0 => (1, true),
                    1 => (2, true),
                    2 => (4, false),
                    4 => (1, false),
                    5 => (2, false),
                    _ => return Err(self.illegal(instruction)),
                };
                let address = self.registers[rs1].wrapping_add(sign_extend(instruction >> 20, 12));
                if address & (size - 1) != 0 {
                    return Err(GuestTrap::new("LoadAddressMisaligned", pc, address));
                }
                self.memory
                    .check(address, size, PERM_READ, "LoadAccessFault", pc)?;
                let value = match (size, signed) {
                    (1, true) => i32::from(self.memory.load_u8(address) as i8) as u32,
                    (1, false) => u32::from(self.memory.load_u8(address)),
                    (2, true) => i32::from(self.memory.load_u16(address) as i16) as u32,
                    (2, false) => u32::from(self.memory.load_u16(address)),
                    (4, _) => self.memory.load_u32(address),
                    _ => unreachable!(),
                };
                self.write_register(rd, value);
                self.pc = next_pc;
            }
            0x23 => {
                let size = match funct3 {
                    0 => 1,
                    1 => 2,
                    2 => 4,
                    _ => return Err(self.illegal(instruction)),
                };
                let encoded = ((instruction >> 7) & 0x1f) | (((instruction >> 25) & 0x7f) << 5);
                let address = self.registers[rs1].wrapping_add(sign_extend(encoded, 12));
                if address & (size - 1) != 0 {
                    return Err(GuestTrap::new("StoreAddressMisaligned", pc, address));
                }
                self.memory.store(address, size, self.registers[rs2], pc)?;
                self.pc = next_pc;
            }
            0x13 => {
                let source = self.registers[rs1];
                let immediate = sign_extend(instruction >> 20, 12);
                let value = match funct3 {
                    0 => source.wrapping_add(immediate),
                    2 => u32::from((source as i32) < (immediate as i32)),
                    3 => u32::from(source < immediate),
                    4 => source ^ immediate,
                    6 => source | immediate,
                    7 => source & immediate,
                    1 if funct7 == 0 => source.wrapping_shl(rs2 as u32),
                    5 if funct7 == 0 => source.wrapping_shr(rs2 as u32),
                    5 if funct7 == 0x20 => (source as i32).wrapping_shr(rs2 as u32) as u32,
                    _ => return Err(self.illegal(instruction)),
                };
                self.write_register(rd, value);
                self.pc = next_pc;
            }
            0x33 => {
                let left = self.registers[rs1];
                let right = self.registers[rs2];
                let shift = right & 31;
                let value = match (funct7, funct3) {
                    (0, 0) => left.wrapping_add(right),
                    (0, 1) => left.wrapping_shl(shift),
                    (0, 2) => u32::from((left as i32) < (right as i32)),
                    (0, 3) => u32::from(left < right),
                    (0, 4) => left ^ right,
                    (0, 5) => left.wrapping_shr(shift),
                    (0, 6) => left | right,
                    (0, 7) => left & right,
                    (0x20, 0) => left.wrapping_sub(right),
                    (0x20, 5) => (left as i32).wrapping_shr(shift) as u32,
                    (1, 0) => left.wrapping_mul(right),
                    (1, 1) => (((left as i32 as i64) * (right as i32 as i64)) >> 32) as u32,
                    (1, 2) => (((left as i32 as i64) * i64::from(right)) >> 32) as u32,
                    (1, 3) => ((u64::from(left) * u64::from(right)) >> 32) as u32,
                    (1, 4) => signed_divide(left, right),
                    (1, 5) => left.checked_div(right).unwrap_or(u32::MAX),
                    (1, 6) => signed_remainder(left, right),
                    (1, 7) => left.checked_rem(right).unwrap_or(left),
                    _ => return Err(self.illegal(instruction)),
                };
                self.write_register(rd, value);
                self.pc = next_pc;
            }
            0x0f => {
                if funct3 != 0 {
                    return Err(self.illegal(instruction));
                }
                self.pc = next_pc;
            }
            0x73 => {
                if instruction == 0x0010_0073 {
                    return Err(GuestTrap::new("Breakpoint", pc, 0));
                }
                if instruction != 0x0000_0073 {
                    return Err(self.illegal(instruction));
                }
                match self.registers[17] {
                    0 => return Ok(Step::Exit(self.registers[10])),
                    1 => {
                        let address = self.registers[10];
                        let length = self.registers[11];
                        if length != 0 {
                            self.memory
                                .check(address, length, PERM_READ, "LoadAccessFault", pc)?;
                        }
                        let resulting_length = self.output.len() + length as usize;
                        if resulting_length > self.output_limit as usize {
                            return Err(GuestTrap::new(
                                "OutputLimitExceeded",
                                pc,
                                resulting_length as u32,
                            ));
                        }
                        self.output.extend(self.memory.read(address, length));
                        self.write_register(10, length);
                        self.pc = next_pc;
                    }
                    syscall => return Err(GuestTrap::new("InvalidSyscall", pc, syscall)),
                }
            }
            _ => return Err(self.illegal(instruction)),
        }
        Ok(Step::Continue)
    }
}

const fn sign_extend(value: u32, bits: u32) -> u32 {
    ((value << (32 - bits)) as i32 >> (32 - bits)) as u32
}

fn signed_divide(left: u32, right: u32) -> u32 {
    if right == 0 {
        return u32::MAX;
    }
    if left == 0x8000_0000 && right == u32::MAX {
        return left;
    }
    ((left as i32) / (right as i32)) as u32
}

fn signed_remainder(left: u32, right: u32) -> u32 {
    if right == 0 {
        return left;
    }
    if left == 0x8000_0000 && right == u32::MAX {
        return 0;
    }
    ((left as i32) % (right as i32)) as u32
}

#[cfg(test)]
mod tests {
    use super::{Engine, LoadedProgram, Machine, RunResult, Termination};
    use crate::elf::load;
    use crate::elf::tests::executable;
    use crate::memory::{
        IMAGE_START, INPUT_START, PAGE_SHIFT, PAGE_SIZE, PERM_EXEC, PERM_READ, STACK_START,
    };

    #[derive(Default)]
    struct LifecycleEngine {
        prepare_calls: usize,
        run_calls: usize,
    }

    impl Engine for LifecycleEngine {
        fn prepare(&mut self, _image: &crate::memory::Image) -> Result<(), String> {
            self.prepare_calls += 1;
            Ok(())
        }

        fn run(&mut self, machine: &mut Machine, _instruction_limit: u64) -> RunResult {
            // Every run receives fresh architectural state and data even
            // though its immutable permission allocation is shared.
            assert_eq!(machine.registers[5], 0);
            assert_eq!(machine.memory.load_u32(STACK_START), 0);
            assert!(machine.output.is_empty());
            assert_eq!(machine.retired, 0);

            self.run_calls += 1;
            machine.registers[5] = self.run_calls as u32;
            machine
                .memory
                .store(STACK_START, 4, self.run_calls as u32, machine.pc)
                .unwrap();
            machine.output.push(machine.memory.load_u8(INPUT_START));
            machine.retired = 1;
            machine.result(Termination::InstructionLimit)
        }
    }

    #[derive(Default)]
    struct DirectMemoryEngine;

    impl Engine for DirectMemoryEngine {
        fn initialize_direct_memory(&self) -> bool {
            true
        }

        fn run(&mut self, machine: &mut Machine, _instruction_limit: u64) -> RunResult {
            assert!(machine.memory.direct_memory_is_initialized());
            machine.result(Termination::InstructionLimit)
        }
    }

    #[test]
    fn exit_retires() {
        let image = load(&executable(&[0x0000_0073])).unwrap();
        let mut completed = Machine::new(&image, &[], 0);
        let instruction = completed.fetch_decode(completed.pc);
        assert_eq!(
            completed.execute_one(instruction),
            Some(Termination::Exit(INPUT_START))
        );
        assert_eq!(completed.pc, IMAGE_START);
        assert_eq!(completed.retired, 1);
    }

    #[test]
    fn decodes_direct_control_flow_targets() {
        let image = load(&executable(&[0x0000_0463, 0x0080_006f, 0x0000_0013])).unwrap();
        let machine = Machine::new(&image, &[], 0);

        let branch = machine.fetch_decode(IMAGE_START).unwrap();
        let jump = machine.fetch_decode(IMAGE_START + 4).unwrap();
        let addi = machine.fetch_decode(IMAGE_START + 8).unwrap();

        assert_eq!(branch.direct_target(), Some(IMAGE_START + 8));
        assert_eq!(jump.direct_target(), Some(IMAGE_START + 12));
        assert_eq!(addi.direct_target(), None);
    }

    #[test]
    fn loaded_program_reuses_permissions_but_keeps_each_run_state_fresh() {
        let elf = executable(&[0x0000_0073]);
        let mut program = LoadedProgram::<LifecycleEngine>::new(&elf).unwrap();
        assert_eq!(program.engine.prepare_calls, 1);
        let template = program.permissions.as_ptr();
        assert_eq!(program.permissions.strong_count(), 1);

        let first = program.run(&[0x11], 1, 4);
        assert_eq!(first.machine.memory.permission_identity(), template);
        assert_eq!(program.permissions.strong_count(), 2);
        assert_eq!(first.machine.registers[5], 1);
        assert_eq!(first.machine.memory.load_u32(STACK_START), 1);
        assert_eq!(first.machine.output, [0x11]);
        assert_eq!(first.result.retired, 1);
        drop(first);
        assert_eq!(program.permissions.strong_count(), 1);

        let second = program.run(&[0x22], 1, 4);
        assert_eq!(second.machine.memory.permission_identity(), template);
        assert_eq!(program.permissions.strong_count(), 2);
        assert_eq!(second.machine.registers[5], 2);
        assert_eq!(second.machine.memory.load_u32(STACK_START), 2);
        assert_eq!(second.machine.output, [0x22]);
        assert_eq!(second.result.retired, 1);
        assert_eq!(program.engine.prepare_calls, 1);
        assert_eq!(program.engine.run_calls, 2);

        // A completed run retains its own Arc, so its permission pointer and
        // data remain valid even after the loaded program is dropped.
        drop(program);
        assert_eq!(second.machine.memory.permission_identity(), template);
        assert!(second.machine.fetch_decode(IMAGE_START).is_ok());
        assert_eq!(second.machine.memory.load_u32(STACK_START), 2);
    }

    #[test]
    fn loaded_program_honors_direct_memory_initialization_requests() {
        let elf = executable(&[0x0000_0073]);
        let mut program = LoadedProgram::<DirectMemoryEngine>::new(&elf).unwrap();

        let completed = program.run(&[0x11], 1, 4);

        assert!(completed.machine.memory.direct_memory_is_initialized());
        assert_eq!(completed.machine.memory.load_u8(INPUT_START), 0x11);
    }

    #[test]
    fn distinct_loaded_programs_own_distinct_permission_templates() {
        let short = LoadedProgram::<LifecycleEngine>::new(&executable(&[0x0000_0013])).unwrap();
        let long_code = vec![0x0000_0013; PAGE_SIZE / 4 + 1];
        let long = LoadedProgram::<LifecycleEngine>::new(&executable(&long_code)).unwrap();
        let second_code_page = (IMAGE_START as usize + PAGE_SIZE) >> PAGE_SHIFT;

        assert_ne!(short.permissions.as_ptr(), long.permissions.as_ptr());
        assert_eq!(short.permissions.get(second_code_page), 0);
        assert_eq!(
            long.permissions.get(second_code_page),
            PERM_READ | PERM_EXEC
        );
        assert_eq!(short.engine.prepare_calls, 1);
        assert_eq!(long.engine.prepare_calls, 1);
    }

    #[test]
    fn direct_machine_construction_remains_independent_and_fresh() {
        let image = load(&executable(&[0x0000_0073])).unwrap();
        let mut first = Machine::new(&image, &[0x11], 4);
        let second = Machine::new(&image, &[0x22], 4);

        assert_ne!(
            first.memory.permission_identity(),
            second.memory.permission_identity()
        );
        first.registers[5] = 9;
        first.output.push(0xaa);
        first.retired = 7;
        first.memory.store(STACK_START, 4, 0x4433_2211, 0).unwrap();

        assert_eq!(second.registers[5], 0);
        assert!(second.output.is_empty());
        assert_eq!(second.retired, 0);
        assert_eq!(second.memory.load_u8(INPUT_START), 0x22);
        assert_eq!(second.memory.load_u32(STACK_START), 0);
    }
}
