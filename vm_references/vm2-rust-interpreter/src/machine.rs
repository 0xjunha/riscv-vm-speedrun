use crate::elf;
use crate::error::GuestTrap;
use crate::memory::{INPUT_START, Image, Memory, PERM_EXEC, PERM_READ, STACK_END};

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

pub struct LoadedProgram {
    image: Image,
}

impl LoadedProgram {
    pub fn new(elf: &[u8]) -> Result<Self, String> {
        Ok(Self {
            image: elf::load(elf)?,
        })
    }

    pub fn machine(&self, input: &[u8], output_limit: u32) -> Machine {
        Machine::new(&self.image, input, output_limit)
    }
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

pub struct Machine {
    pub registers: [u32; 32],
    pub pc: u32,
    pub memory: Memory,
    pub output: Vec<u8>,
    pub retired: u64,
    output_limit: u32,
}

impl Machine {
    fn new(image: &Image, input: &[u8], output_limit: u32) -> Self {
        let mut registers = [0; 32];
        registers[2] = STACK_END;
        registers[10] = INPUT_START;
        registers[11] = input.len() as u32;
        Self {
            registers,
            pc: image.entry,
            memory: Memory::new(image, input),
            output: Vec::new(),
            retired: 0,
            output_limit,
        }
    }

    pub fn run(&mut self, instruction_limit: u64) -> RunResult {
        loop {
            if self.retired >= instruction_limit {
                return self.result(Termination::InstructionLimit);
            }
            match self.step() {
                Ok(Step::Continue) => self.retired += 1,
                Ok(Step::Exit(code)) => {
                    self.retired += 1;
                    return self.result(Termination::Exit(code));
                }
                Err(trap) => return self.result(Termination::Trap(trap)),
            }
        }
    }

    fn result(&self, termination: Termination) -> RunResult {
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

    fn step(&mut self) -> Result<Step, GuestTrap> {
        let pc = self.pc;
        if pc & 3 != 0 {
            return Err(GuestTrap::new("InstructionAddressMisaligned", pc, pc));
        }
        self.memory
            .check(pc, 4, PERM_EXEC, "InstructionAccessFault", pc)?;
        let instruction = self.memory.load_u32(pc);
        let opcode = instruction & 0x7f;
        let rd = ((instruction >> 7) & 0x1f) as usize;
        let funct3 = (instruction >> 12) & 7;
        let rs1 = ((instruction >> 15) & 0x1f) as usize;
        let rs2 = ((instruction >> 20) & 0x1f) as usize;
        let funct7 = instruction >> 25;
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
                let encoded = ((instruction >> 31) << 20)
                    | (((instruction >> 12) & 0xff) << 12)
                    | (((instruction >> 20) & 1) << 11)
                    | (((instruction >> 21) & 0x3ff) << 1);
                let target = pc.wrapping_add(sign_extend(encoded, 21));
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
                    let encoded = (((instruction >> 31) & 1) << 12)
                        | (((instruction >> 7) & 1) << 11)
                        | (((instruction >> 25) & 0x3f) << 5)
                        | (((instruction >> 8) & 0xf) << 1);
                    let target = pc.wrapping_add(sign_extend(encoded, 13));
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

fn sign_extend(value: u32, bits: u32) -> u32 {
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
    use super::{LoadedProgram, Termination};
    use crate::elf::tests::executable;
    use crate::memory::{IMAGE_START, INPUT_START};

    #[test]
    fn instruction_limit_precedes_fetch_and_exit_retires() {
        let program = LoadedProgram::new(&executable(&[0x0000_0073])).unwrap();

        let mut limited = program.machine(&[], 0);
        assert_eq!(limited.run(0).termination, Termination::InstructionLimit);
        assert_eq!(limited.pc, IMAGE_START);
        assert_eq!(limited.retired, 0);

        let mut completed = program.machine(&[], 0);
        assert_eq!(completed.run(1).termination, Termination::Exit(INPUT_START));
        assert_eq!(completed.pc, IMAGE_START);
        assert_eq!(completed.retired, 1);
    }
}
