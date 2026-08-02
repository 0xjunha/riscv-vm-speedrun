//! Optional VM5 execution and generated-code profiling.

use std::{fmt::Write, io::Write as IoWrite};

use crate::linked::{BlockFlow as LinkedBlockFlow, LinkedBlock, NativeRunProfile, NativeStop};
use rv32vm_rust_common::{
    GuestTrap,
    machine::{DecodedInstruction, Termination},
};

/// The way the last instruction in a generated block returns to the dispatcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockFlow {
    Fallthrough,
    Branch,
    DirectJump,
    IndirectJump,
}

/// Static properties of one generated block, weighted by dispatches at run time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GeneratedBlockProfile {
    pub(crate) instruction_count: u64,
    pub(crate) flow: BlockFlow,
}

impl GeneratedBlockProfile {
    pub(crate) fn from_compiled(compiled: &LinkedBlock) -> Self {
        let instruction_count = compiled.instruction_count();
        let flow = match compiled.flow() {
            LinkedBlockFlow::Fallthrough { .. } | LinkedBlockFlow::CheckedFallthrough { .. } => {
                BlockFlow::Fallthrough
            }
            LinkedBlockFlow::Branch { .. } => BlockFlow::Branch,
            LinkedBlockFlow::Jump { .. } => BlockFlow::DirectJump,
            LinkedBlockFlow::IndirectJump { .. } => BlockFlow::IndirectJump,
        };

        Self {
            instruction_count: instruction_count as u64,
            flow,
        }
    }
}

/// Reusable code-generation measurements collected once during `LOAD`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LoadProfile {
    pub(crate) compiled_blocks: u64,
    pub(crate) native_guest_instructions: u64,
    pub(crate) code_bytes: u64,
    pub(crate) hot_code_bytes: u64,
    pub(crate) cold_code_bytes: u64,
    pub(crate) mapped_bytes: u64,
    pub(crate) dispatch_table_entries: u64,
    pub(crate) dispatch_table_pages: u64,
    pub(crate) dispatch_table_bytes: u64,
    pub(crate) register_cache_count: u64,
    pub(crate) register_cache_guest_registers: [u8; 6],
    pub(crate) external_thunk_bytes: u64,
    pub(crate) shared_prologue_bytes: u64,
    pub(crate) exit_trampoline_bytes: u64,
    pub(crate) fallthrough_blocks: u64,
    pub(crate) branch_blocks: u64,
    pub(crate) direct_jump_blocks: u64,
    pub(crate) indirect_jump_blocks: u64,
}

impl LoadProfile {
    pub(crate) fn record_block(&mut self, block: GeneratedBlockProfile) {
        self.compiled_blocks += 1;
        self.native_guest_instructions += block.instruction_count;
        match block.flow {
            BlockFlow::Fallthrough => self.fallthrough_blocks += 1,
            BlockFlow::Branch => self.branch_blocks += 1,
            BlockFlow::DirectJump => self.direct_jump_blocks += 1,
            BlockFlow::IndirectJump => self.indirect_jump_blocks += 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct OpcodeCounts {
    load: u64,
    misc_mem: u64,
    op_imm: u64,
    auipc: u64,
    store: u64,
    op: u64,
    lui: u64,
    branch: u64,
    jalr: u64,
    jal: u64,
    system: u64,
    other: u64,
}

impl OpcodeCounts {
    fn record(&mut self, opcode: u32) {
        let count = match opcode {
            0x03 => &mut self.load,
            0x0f => &mut self.misc_mem,
            0x13 => &mut self.op_imm,
            0x17 => &mut self.auipc,
            0x23 => &mut self.store,
            0x33 => &mut self.op,
            0x37 => &mut self.lui,
            0x63 => &mut self.branch,
            0x67 => &mut self.jalr,
            0x6f => &mut self.jal,
            0x73 => &mut self.system,
            _ => &mut self.other,
        };
        *count += 1;
    }
}

/// Measurements whose counters are reset for every `RUN`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RunProfile {
    pub(crate) native_retired: u64,
    pub(crate) fallback_retired: u64,
    pub(crate) native_invocations: u64,
    pub(crate) native_dispatches: u64,
    pub(crate) native_direct_link_hits: u64,
    pub(crate) native_indirect_link_hits: u64,
    pub(crate) native_indirect_link_misses: u64,
    pub(crate) native_missing_exits: u64,
    pub(crate) native_budget_exits: u64,
    pub(crate) native_interpret_one_exits: u64,
    pub(crate) lookup_fallbacks: u64,
    pub(crate) budget_fallbacks: u64,
    pub(crate) fallback_loads: u64,
    pub(crate) fallback_stores: u64,
    pub(crate) fallback_jalr: u64,
    pub(crate) fallback_m_ops: u64,
    pub(crate) fallback_system: u64,
    pub(crate) fallback_other: u64,
    pub(crate) fallback_fetch_traps: u64,
    pub(crate) generated_guest_register_loads: u64,
    pub(crate) generated_guest_register_stores: u64,
    pub(crate) generated_register_cache_fills: u64,
    pub(crate) generated_register_cache_spills: u64,
    pub(crate) generated_register_cache_read_hits: u64,
    pub(crate) generated_register_cache_write_hits: u64,
    pub(crate) native_memory_loads: u64,
    pub(crate) native_memory_stores: u64,
    pub(crate) direct_immediate_lowerings: u64,
    pub(crate) direct_register_lowerings: u64,
    pub(crate) direct_branch_lowerings: u64,
    pub(crate) direct_memory_load_lowerings: u64,
    pub(crate) direct_memory_store_lowerings: u64,
    pub(crate) native_fallthrough_dispatches: u64,
    pub(crate) native_branch_dispatches: u64,
    pub(crate) native_direct_jump_dispatches: u64,
    pub(crate) native_indirect_jump_dispatches: u64,
    opcodes: OpcodeCounts,
}

impl RunProfile {
    pub(crate) fn record_native_run(
        &mut self,
        retired: u64,
        native: NativeRunProfile,
        stop: NativeStop,
    ) {
        self.native_retired += retired;
        self.native_invocations += 1;
        self.native_dispatches += native.blocks;
        self.native_direct_link_hits += native.direct_links;
        self.native_indirect_link_hits += native.indirect_hits;
        self.native_indirect_link_misses += native.indirect_misses;
        self.generated_guest_register_loads += native.register_loads;
        self.generated_guest_register_stores += native.register_stores;
        self.generated_register_cache_fills += native.cache_fills;
        self.generated_register_cache_spills += native.cache_spills;
        self.generated_register_cache_read_hits += native.cache_read_hits;
        self.generated_register_cache_write_hits += native.cache_write_hits;
        self.native_memory_loads += native.memory_loads;
        self.native_memory_stores += native.memory_stores;
        self.direct_immediate_lowerings += native.direct_immediate;
        self.direct_register_lowerings += native.direct_register;
        self.direct_branch_lowerings += native.direct_branch;
        self.direct_memory_load_lowerings += native.direct_memory_load;
        self.direct_memory_store_lowerings += native.direct_memory_store;
        self.native_fallthrough_dispatches += native.fallthrough_blocks;
        self.native_branch_dispatches += native.branch_blocks;
        self.native_direct_jump_dispatches += native.jump_blocks;
        self.native_indirect_jump_dispatches += native.indirect_hits + native.indirect_misses;
        match stop {
            NativeStop::MissingSuccessor => self.native_missing_exits += 1,
            NativeStop::Budget => self.native_budget_exits += 1,
            NativeStop::InterpretOne => self.native_interpret_one_exits += 1,
        }
    }

    pub(crate) fn record_fallback(&mut self, instruction: &Result<DecodedInstruction, GuestTrap>) {
        let Ok(instruction) = instruction else {
            self.fallback_fetch_traps += 1;
            return;
        };

        self.opcodes.record(instruction.opcode());
        match instruction.opcode() {
            0x03 => self.fallback_loads += 1,
            0x23 => self.fallback_stores += 1,
            0x67 => self.fallback_jalr += 1,
            0x33 if instruction.funct7() == 1 => self.fallback_m_ops += 1,
            0x73 => self.fallback_system += 1,
            _ => self.fallback_other += 1,
        }
    }

    pub(crate) fn json(
        self,
        termination: Termination,
        total_retired: u64,
        load: LoadProfile,
    ) -> String {
        let termination = match termination {
            Termination::Exit(_) => "exit",
            Termination::Trap(_) => "trap",
            Termination::InstructionLimit => "instruction_limit",
        };
        let opcodes = self.opcodes;
        let mut cached_guest_registers = String::from("[");
        for (index, guest) in load
            .register_cache_guest_registers
            .iter()
            .take(load.register_cache_count as usize)
            .enumerate()
        {
            if index != 0 {
                cached_guest_registers.push(',');
            }
            write!(cached_guest_registers, "{guest}")
                .expect("writing cache mapping into a String cannot fail");
        }
        cached_guest_registers.push(']');
        let mut json = String::with_capacity(1_500);
        write!(
            json,
            "{{\"kind\":\"vm5_profile\",\"schema_version\":6,\"termination\":\"{termination}\",\
             \"retired\":{{\"total\":{total_retired},\"native\":{},\"fallback\":{}}},\
             \"dispatch\":{{\"native_invocations\":{},\"native_blocks\":{},\
             \"direct_link_hits\":{},\"indirect_link_hits\":{},\"indirect_link_misses\":{},\
             \"lookup_fallbacks\":{},\"budget_fallbacks\":{},\"native_fallthrough\":{},\
             \"native_branch\":{},\"native_direct_jump\":{},\"native_indirect_jump\":{}}},\
             \"native_exits\":{{\"missing_successor\":{},\"budget\":{},\"interpret_one\":{}}},\
             \"fallback_classes\":{{\"loads\":{},\"stores\":{},\"jalr\":{},\"m_ops\":{},\
             \"system\":{},\"other\":{},\"fetch_traps\":{}}},\
             \"fallback_opcodes\":{{\"load_0x03\":{},\"misc_mem_0x0f\":{},\"op_imm_0x13\":{},\
             \"auipc_0x17\":{},\"store_0x23\":{},\"op_0x33\":{},\"lui_0x37\":{},\
             \"branch_0x63\":{},\"jalr_0x67\":{},\"jal_0x6f\":{},\"system_0x73\":{},\
             \"other\":{},\"fetch_traps\":{}}},\
             \"generated_guest_register_traffic\":{{\"loads\":{},\"stores\":{}}},\
             \"generated_register_cache_traffic\":{{\"fills\":{},\"spills\":{},\
             \"read_hits\":{},\"write_hits\":{},\"avoided_array_accesses\":{}}},\
             \"native_memory_traffic\":{{\"loads\":{},\"stores\":{}}},\
             \"direct_operand_lowering\":{{\"immediate\":{},\"register\":{},\"branch\":{},\
             \"memory_load\":{},\"memory_store\":{}}},\
             \"load\":{{\"compiled_blocks\":{},\"native_guest_instructions\":{},\
             \"code_bytes\":{},\"hot_code_bytes\":{},\"cold_code_bytes\":{},\
             \"mapped_bytes\":{},\"dispatch_table_entries\":{},\
             \"dispatch_table_pages\":{},\"dispatch_table_bytes\":{},\
             \"register_cache_count\":{},\"register_cache_guest_registers\":{},\
             \"external_thunk_bytes\":{},\"shared_prologue_bytes\":{},\
             \"exit_trampoline_bytes\":{},\
             \"fallthrough_blocks\":{},\"branch_blocks\":{},\"direct_jump_blocks\":{},\
             \"indirect_jump_blocks\":{}}}}}",
            self.native_retired,
            self.fallback_retired,
            self.native_invocations,
            self.native_dispatches,
            self.native_direct_link_hits,
            self.native_indirect_link_hits,
            self.native_indirect_link_misses,
            self.lookup_fallbacks,
            self.budget_fallbacks,
            self.native_fallthrough_dispatches,
            self.native_branch_dispatches,
            self.native_direct_jump_dispatches,
            self.native_indirect_jump_dispatches,
            self.native_missing_exits,
            self.native_budget_exits,
            self.native_interpret_one_exits,
            self.fallback_loads,
            self.fallback_stores,
            self.fallback_jalr,
            self.fallback_m_ops,
            self.fallback_system,
            self.fallback_other,
            self.fallback_fetch_traps,
            opcodes.load,
            opcodes.misc_mem,
            opcodes.op_imm,
            opcodes.auipc,
            opcodes.store,
            opcodes.op,
            opcodes.lui,
            opcodes.branch,
            opcodes.jalr,
            opcodes.jal,
            opcodes.system,
            opcodes.other,
            self.fallback_fetch_traps,
            self.generated_guest_register_loads,
            self.generated_guest_register_stores,
            self.generated_register_cache_fills,
            self.generated_register_cache_spills,
            self.generated_register_cache_read_hits,
            self.generated_register_cache_write_hits,
            self.generated_register_cache_read_hits + self.generated_register_cache_write_hits,
            self.native_memory_loads,
            self.native_memory_stores,
            self.direct_immediate_lowerings,
            self.direct_register_lowerings,
            self.direct_branch_lowerings,
            self.direct_memory_load_lowerings,
            self.direct_memory_store_lowerings,
            load.compiled_blocks,
            load.native_guest_instructions,
            load.code_bytes,
            load.hot_code_bytes,
            load.cold_code_bytes,
            load.mapped_bytes,
            load.dispatch_table_entries,
            load.dispatch_table_pages,
            load.dispatch_table_bytes,
            load.register_cache_count,
            cached_guest_registers,
            load.external_thunk_bytes,
            load.shared_prologue_bytes,
            load.exit_trampoline_bytes,
            load.fallthrough_blocks,
            load.branch_blocks,
            load.direct_jump_blocks,
            load.indirect_jump_blocks,
        )
        .expect("writing JSON into a String cannot fail");
        json
    }

    pub(crate) fn emit(self, termination: Termination, total_retired: u64, load: LoadProfile) {
        // Profiling must not change guest-visible completion if stderr becomes
        // unavailable. A successful write is exactly one JSON line per RUN.
        let mut stderr = std::io::stderr().lock();
        let _ = self.write_json_line(&mut stderr, termination, total_retired, load);
    }

    fn write_json_line<W: IoWrite>(
        self,
        writer: &mut W,
        termination: Termination,
        total_retired: u64,
        load: LoadProfile,
    ) -> std::io::Result<()> {
        let record = self.json(termination, total_retired, load);
        writeln!(writer, "{record}")
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::{
        machine::{Machine, Termination},
        memory::IMAGE_START,
    };

    use super::{BlockFlow, GeneratedBlockProfile, LoadProfile, OpcodeCounts, RunProfile};
    use crate::linked::LinkedBlock;
    use crate::test_support::{addi, beq, image_with_code_at, lw};

    fn decoded(code: &[u32]) -> Vec<rv32vm_rust_x86_block_compiler::BlockInstruction> {
        let image = image_with_code_at(code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        (0..code.len())
            .map(|index| machine.fetch_decode(IMAGE_START + index as u32 * 4))
            .collect()
    }

    #[test]
    fn generated_block_profile_records_control_flow() {
        let instructions = decoded(&[addi(5, 0, 1), addi(0, 5, 1), beq(5, 0, -8)]);

        let compiled = LinkedBlock::compile(&instructions).unwrap();
        let profile = GeneratedBlockProfile::from_compiled(&compiled);

        assert_eq!(profile.instruction_count, 3);
        assert_eq!(profile.flow, BlockFlow::Branch);
    }

    #[test]
    fn fallback_classes_distinguish_hot_unsupported_operations_and_fetch_traps() {
        let code = [
            lw(5, 0, 0),
            0x0050_2023, // sw x5, 0(x0)
            0x0000_0067, // jalr x0, 0(x0)
            0x0252_8333, // mul x6, x5, x5
            0x0000_0073, // ecall
            addi(5, 5, 1),
        ];
        let instructions = decoded(&code);
        let mut profile = RunProfile::default();
        for instruction in &instructions {
            profile.record_fallback(instruction);
        }
        let image = image_with_code_at(&code, IMAGE_START);
        let machine = Machine::new(&image, &[], 0);
        profile.record_fallback(&machine.fetch_decode(0));

        assert_eq!(profile.fallback_loads, 1);
        assert_eq!(profile.fallback_stores, 1);
        assert_eq!(profile.fallback_jalr, 1);
        assert_eq!(profile.fallback_m_ops, 1);
        assert_eq!(profile.fallback_system, 1);
        assert_eq!(profile.fallback_other, 1);
        assert_eq!(profile.fallback_fetch_traps, 1);
    }

    fn populated_profile() -> RunProfile {
        RunProfile {
            native_retired: 101,
            fallback_retired: 102,
            native_invocations: 103,
            native_dispatches: 104,
            native_direct_link_hits: 105,
            native_indirect_link_hits: 125,
            native_indirect_link_misses: 126,
            native_missing_exits: 106,
            native_budget_exits: 107,
            native_interpret_one_exits: 122,
            lookup_fallbacks: 108,
            budget_fallbacks: 109,
            fallback_loads: 110,
            fallback_stores: 111,
            fallback_jalr: 112,
            fallback_m_ops: 113,
            fallback_system: 114,
            fallback_other: 115,
            fallback_fetch_traps: 116,
            generated_guest_register_loads: 117,
            generated_guest_register_stores: 118,
            generated_register_cache_fills: 128,
            generated_register_cache_spills: 129,
            generated_register_cache_read_hits: 130,
            generated_register_cache_write_hits: 131,
            native_memory_loads: 123,
            native_memory_stores: 124,
            direct_immediate_lowerings: 132,
            direct_register_lowerings: 133,
            direct_branch_lowerings: 134,
            direct_memory_load_lowerings: 135,
            direct_memory_store_lowerings: 136,
            native_fallthrough_dispatches: 119,
            native_branch_dispatches: 120,
            native_direct_jump_dispatches: 121,
            native_indirect_jump_dispatches: 127,
            opcodes: OpcodeCounts {
                load: 201,
                misc_mem: 202,
                op_imm: 203,
                auipc: 204,
                store: 205,
                op: 206,
                lui: 207,
                branch: 208,
                jalr: 209,
                jal: 210,
                system: 211,
                other: 212,
            },
        }
    }

    fn populated_load_profile() -> LoadProfile {
        LoadProfile {
            compiled_blocks: 301,
            native_guest_instructions: 302,
            code_bytes: 303,
            hot_code_bytes: 315,
            cold_code_bytes: 316,
            mapped_bytes: 304,
            dispatch_table_entries: 308,
            dispatch_table_pages: 309,
            dispatch_table_bytes: 310,
            register_cache_count: 6,
            register_cache_guest_registers: [5, 6, 7, 8, 9, 10],
            external_thunk_bytes: 312,
            shared_prologue_bytes: 313,
            exit_trampoline_bytes: 314,
            fallthrough_blocks: 305,
            branch_blocks: 306,
            direct_jump_blocks: 307,
            indirect_jump_blocks: 311,
        }
    }

    fn expected_populated_record() -> &'static str {
        concat!(
            "{\"kind\":\"vm5_profile\",\"schema_version\":6,",
            "\"termination\":\"instruction_limit\",",
            "\"retired\":{\"total\":999,\"native\":101,\"fallback\":102},",
            "\"dispatch\":{\"native_invocations\":103,\"native_blocks\":104,",
            "\"direct_link_hits\":105,\"indirect_link_hits\":125,",
            "\"indirect_link_misses\":126,\"lookup_fallbacks\":108,",
            "\"budget_fallbacks\":109,\"native_fallthrough\":119,",
            "\"native_branch\":120,\"native_direct_jump\":121,",
            "\"native_indirect_jump\":127},",
            "\"native_exits\":{\"missing_successor\":106,\"budget\":107,",
            "\"interpret_one\":122},",
            "\"fallback_classes\":{\"loads\":110,\"stores\":111,\"jalr\":112,",
            "\"m_ops\":113,\"system\":114,\"other\":115,\"fetch_traps\":116},",
            "\"fallback_opcodes\":{\"load_0x03\":201,\"misc_mem_0x0f\":202,",
            "\"op_imm_0x13\":203,\"auipc_0x17\":204,\"store_0x23\":205,",
            "\"op_0x33\":206,\"lui_0x37\":207,\"branch_0x63\":208,",
            "\"jalr_0x67\":209,\"jal_0x6f\":210,\"system_0x73\":211,",
            "\"other\":212,\"fetch_traps\":116},",
            "\"generated_guest_register_traffic\":{\"loads\":117,\"stores\":118},",
            "\"generated_register_cache_traffic\":{\"fills\":128,\"spills\":129,",
            "\"read_hits\":130,\"write_hits\":131,\"avoided_array_accesses\":261},",
            "\"native_memory_traffic\":{\"loads\":123,\"stores\":124},",
            "\"direct_operand_lowering\":{\"immediate\":132,\"register\":133,",
            "\"branch\":134,\"memory_load\":135,\"memory_store\":136},",
            "\"load\":{\"compiled_blocks\":301,\"native_guest_instructions\":302,",
            "\"code_bytes\":303,\"hot_code_bytes\":315,\"cold_code_bytes\":316,",
            "\"mapped_bytes\":304,\"dispatch_table_entries\":308,",
            "\"dispatch_table_pages\":309,\"dispatch_table_bytes\":310,",
            "\"register_cache_count\":6,\"register_cache_guest_registers\":[5,6,7,8,9,10],",
            "\"external_thunk_bytes\":312,\"shared_prologue_bytes\":313,",
            "\"exit_trampoline_bytes\":314,",
            "\"fallthrough_blocks\":305,\"branch_blocks\":306,",
            "\"direct_jump_blocks\":307,\"indirect_jump_blocks\":311}}"
        )
    }

    #[test]
    fn profile_json_matches_the_complete_schema_v6_record() {
        let json =
            populated_profile().json(Termination::InstructionLimit, 999, populated_load_profile());

        assert_eq!(json, expected_populated_record());
        assert!(!json.contains('\n'));
    }

    #[test]
    fn profile_writer_emits_exactly_one_complete_line_per_run() {
        let mut output = Vec::new();
        let profile = populated_profile();

        profile
            .write_json_line(
                &mut output,
                Termination::InstructionLimit,
                999,
                populated_load_profile(),
            )
            .unwrap();
        profile
            .write_json_line(
                &mut output,
                Termination::InstructionLimit,
                999,
                populated_load_profile(),
            )
            .unwrap();

        let expected = format!(
            "{}\n{}\n",
            expected_populated_record(),
            expected_populated_record()
        );
        assert_eq!(output, expected.as_bytes());
        assert_eq!(output.iter().filter(|&&byte| byte == b'\n').count(), 2);
    }
}
