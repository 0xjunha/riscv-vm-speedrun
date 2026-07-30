use rv32vm_rust_common::machine::{Engine, Machine, RunResult, Termination};

#[derive(Default)]
pub(crate) struct Interpreter;

impl Engine for Interpreter {
    fn run(&mut self, machine: &mut Machine, instruction_limit: u64) -> RunResult {
        loop {
            if machine.retired >= instruction_limit {
                return machine.result(Termination::InstructionLimit);
            }
            let instruction = machine.fetch_decode(machine.pc);
            if let Some(termination) = machine.execute_one(instruction) {
                return machine.result(termination);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rv32vm_rust_common::{
        machine::{Engine, Machine, Termination},
        memory::{IMAGE_START, Image, PAGE_COUNT, PAGE_SHIFT, PAGE_SIZE, PERM_EXEC, PERM_READ},
    };

    use super::Interpreter;

    #[test]
    fn instruction_limit_precedes_fetch() {
        let mut permissions = vec![0; PAGE_COUNT];
        let mut pages = std::iter::repeat_with(|| None)
            .take(PAGE_COUNT)
            .collect::<Vec<_>>();
        let page = (IMAGE_START >> PAGE_SHIFT) as usize;
        permissions[page] = PERM_READ | PERM_EXEC;
        pages[page] = Some(Box::new([0; PAGE_SIZE]));
        let image = Image {
            entry: IMAGE_START,
            permissions,
            pages,
        };
        let mut machine = Machine::new(&image, &[], 0);
        let mut engine = Interpreter;

        let result = engine.run(&mut machine, 0);

        assert_eq!(result.termination, Termination::InstructionLimit);
        assert_eq!(machine.pc, IMAGE_START);
        assert_eq!(machine.retired, 0);
    }
}
