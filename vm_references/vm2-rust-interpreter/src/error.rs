#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestTrap {
    pub cause: &'static str,
    pub pc: u32,
    pub value: u32,
}

impl GuestTrap {
    pub const fn new(cause: &'static str, pc: u32, value: u32) -> Self {
        Self { cause, pc, value }
    }
}
