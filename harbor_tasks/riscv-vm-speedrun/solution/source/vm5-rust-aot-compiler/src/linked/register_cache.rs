//! Persistent guest-register selection and x86 register/operand modeling.

use super::{LinkedBlock, Lowering, MAX_LINKED_BLOCKS};

pub(super) const MAX_CACHED_REGISTERS: usize = 6;
pub(super) const MIN_WEIGHTED_CACHE_ACCESSES: u64 = 5;

impl Lowering {
    pub(super) fn score_register_uses(
        self,
        scores: &mut [u64; 32],
        weighted_accesses: &mut [u64; 32],
        execution_weight: u64,
    ) {
        fn add(array: &mut [u64; 32], register: usize, weight: u64) {
            if register != 0 {
                array[register] = array[register].saturating_add(weight);
            }
        }
        fn read(
            scores: &mut [u64; 32],
            weighted_accesses: &mut [u64; 32],
            register: usize,
            execution_weight: u64,
        ) {
            add(scores, register, execution_weight.saturating_mul(2));
            add(weighted_accesses, register, execution_weight);
        }
        fn write(
            scores: &mut [u64; 32],
            weighted_accesses: &mut [u64; 32],
            register: usize,
            execution_weight: u64,
        ) {
            add(scores, register, execution_weight);
            add(weighted_accesses, register, execution_weight);
        }
        match self {
            Self::WriteImmediate { destination, .. } | Self::Jump { destination, .. } => {
                write(scores, weighted_accesses, destination, execution_weight);
            }
            Self::IndirectJump {
                destination,
                source,
                ..
            } => {
                read(scores, weighted_accesses, source, execution_weight);
                write(scores, weighted_accesses, destination, execution_weight);
            }
            Self::Branch { left, right, .. } => {
                read(scores, weighted_accesses, left, execution_weight);
                read(scores, weighted_accesses, right, execution_weight);
            }
            Self::Immediate {
                destination,
                source,
                ..
            } => {
                // The native lowering deliberately elides an instruction
                // whose destination is x0 and which cannot trap.
                if destination != 0 {
                    read(scores, weighted_accesses, source, execution_weight);
                    write(scores, weighted_accesses, destination, execution_weight);
                }
            }
            Self::Register {
                destination,
                left,
                right,
                ..
            } => {
                if destination != 0 {
                    read(scores, weighted_accesses, left, execution_weight);
                    read(scores, weighted_accesses, right, execution_weight);
                    write(scores, weighted_accesses, destination, execution_weight);
                }
            }
            Self::Load {
                destination, base, ..
            } => {
                read(scores, weighted_accesses, base, execution_weight);
                write(scores, weighted_accesses, destination, execution_weight);
            }
            Self::Store { base, source, .. } => {
                read(scores, weighted_accesses, base, execution_weight);
                read(scores, weighted_accesses, source, execution_weight);
            }
            Self::Fence => {}
        }
    }

    #[cfg(not(feature = "profile"))]
    pub(super) const fn writes_register(self, register: usize) -> bool {
        if register == 0 {
            return false;
        }
        match self {
            Self::WriteImmediate { destination, .. }
            | Self::Jump { destination, .. }
            | Self::IndirectJump { destination, .. }
            | Self::Immediate { destination, .. }
            | Self::Register { destination, .. }
            | Self::Load { destination, .. } => destination == register,
            Self::Branch { .. } | Self::Store { .. } | Self::Fence => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum CachedHost {
    Ebx,
    Ebp,
    R12d,
    R13d,
    R14d,
    R15d,
}

impl CachedHost {
    const ALL: [Self; MAX_CACHED_REGISTERS] = [
        Self::Ebx,
        Self::Ebp,
        Self::R12d,
        Self::R13d,
        Self::R14d,
        Self::R15d,
    ];

    pub(super) const fn register(self) -> Register32 {
        match self {
            Self::Ebx => Register32::Ebx,
            Self::Ebp => Register32::Ebp,
            Self::R12d => Register32::R12d,
            Self::R13d => Register32::R13d,
            Self::R14d => Register32::R14d,
            Self::R15d => Register32::R15d,
        }
    }
}

/// An x86-64 general-purpose register viewed through its wrapping 32-bit
/// subregister. Writing one of these always zero-extends into the full host
/// register, exactly matching RV32 arithmetic modulo 2^32.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum Register32 {
    Eax = 0,
    Ecx = 1,
    Ebx = 3,
    Ebp = 5,
    R12d = 12,
    R13d = 13,
    R14d = 14,
    R15d = 15,
    #[cfg(not(feature = "profile"))]
    R11d = 11,
}

impl Register32 {
    pub(super) const fn encoding(self) -> u8 {
        self as u8
    }
}

/// One encodable x86 r/m32 operand used by the linked backend. Guest memory is
/// always `[RSI + disp8]`; every nonzero RV32 register offset fits exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Operand32 {
    Register(Register32),
    GuestMemory(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BinaryOperation32 {
    Add,
    Subtract,
    Xor,
    Or,
    And,
    Multiply,
}

impl BinaryOperation32 {
    pub(super) const fn opcode(self) -> &'static [u8] {
        match self {
            Self::Add => &[0x03],
            Self::Subtract => &[0x2b],
            Self::Xor => &[0x33],
            Self::Or => &[0x0b],
            Self::And => &[0x23],
            Self::Multiply => &[0x0f, 0xaf],
        }
    }

    pub(super) const fn commutative(self) -> bool {
        !matches!(self, Self::Subtract)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RegisterCache {
    guests: [u8; MAX_CACHED_REGISTERS],
    count: u8,
    host_by_guest: [u8; 32],
}

impl RegisterCache {
    const NONE: u8 = u8::MAX;

    pub(super) const fn empty() -> Self {
        Self {
            guests: [0; MAX_CACHED_REGISTERS],
            count: 0,
            host_by_guest: [Self::NONE; 32],
        }
    }

    #[cfg(test)]
    pub(super) fn from_guests(guests: &[u8]) -> Self {
        assert!(guests.len() <= MAX_CACHED_REGISTERS);
        let mut cache = Self::empty();
        for (host, &guest) in guests.iter().enumerate() {
            assert!((1..32).contains(&guest));
            assert_eq!(cache.host_by_guest[guest as usize], Self::NONE);
            cache.guests[host] = guest;
            cache.host_by_guest[guest as usize] = host as u8;
            cache.count += 1;
        }
        cache
    }

    fn scores_and_weighted_accesses(blocks: &[LinkedBlock]) -> ([u64; 32], [u64; 32]) {
        // Overlapping eager candidates often contain the same guest word, so
        // score each instruction address exactly once. Backward conditional
        // edges and tail jumps identify bounded natural-loop intervals; each
        // enclosing loop adds the same generic 8x hotness proxy. This remains
        // entirely image-derived and deterministic, without runtime profiles.
        let instruction_capacity = blocks
            .iter()
            .fold(0_usize, |total, block| {
                total.saturating_add(block.instructions.len())
            })
            .min(MAX_LINKED_BLOCKS * 64);
        let mut instructions = Vec::<(u32, Lowering)>::with_capacity(instruction_capacity);
        for block in blocks {
            for (index, &instruction) in block.instructions.iter().enumerate() {
                let Some(offset) = u32::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_mul(4))
                else {
                    continue;
                };
                if let Some(pc) = block.pc.checked_add(offset) {
                    instructions.push((pc, instruction));
                }
            }
        }
        instructions.sort_unstable_by_key(|(pc, _)| *pc);
        instructions.dedup_by(|right, left| {
            if right.0 != left.0 {
                return false;
            }
            debug_assert_eq!(right.1, left.1);
            true
        });

        let mut loop_intervals = Vec::with_capacity(blocks.len());
        for &(pc, instruction) in &instructions {
            let target = match instruction {
                Lowering::Branch { target, .. } if target <= pc => Some(target),
                Lowering::Jump {
                    destination: 0,
                    target,
                    ..
                } if target <= pc => Some(target),
                _ => None,
            };
            if let Some(target) = target {
                loop_intervals.push((target, pc));
            }
        }
        loop_intervals.sort_unstable();
        loop_intervals.dedup();
        debug_assert!(loop_intervals.len() <= blocks.len());
        let mut loop_events = Vec::with_capacity(loop_intervals.len().saturating_mul(2));
        for (start, end) in loop_intervals {
            loop_events.push((u64::from(start), 1_i64));
            loop_events.push((u64::from(end) + 4, -1_i64));
        }
        loop_events.sort_unstable_by_key(|(pc, _)| *pc);

        let mut scores = [0_u64; 32];
        let mut weighted_accesses = [0_u64; 32];
        let mut event_index = 0;
        let mut loop_depth = 0_i64;
        for &(pc, instruction) in &instructions {
            while event_index < loop_events.len() && loop_events[event_index].0 <= u64::from(pc) {
                loop_depth += loop_events[event_index].1;
                event_index += 1;
            }
            debug_assert!(loop_depth >= 0);
            let weight = 1_u64.saturating_add((loop_depth as u64).saturating_mul(7));
            instruction.score_register_uses(&mut scores, &mut weighted_accesses, weight);
        }
        (scores, weighted_accesses)
    }

    #[cfg(test)]
    pub(super) fn scores(blocks: &[LinkedBlock]) -> [u64; 32] {
        Self::scores_and_weighted_accesses(blocks).0
    }

    pub(super) fn select(blocks: &[LinkedBlock]) -> Self {
        let (scores, weighted_accesses) = Self::scores_and_weighted_accesses(blocks);

        let mut ranked = (1_u8..32).collect::<Vec<_>>();
        ranked.sort_unstable_by(|left, right| {
            scores[*right as usize]
                .cmp(&scores[*left as usize])
                .then_with(|| left.cmp(right))
        });

        let mut cache = Self::empty();
        for guest in ranked
            .into_iter()
            .filter(|guest| weighted_accesses[*guest as usize] >= MIN_WEIGHTED_CACHE_ACCESSES)
            .take(MAX_CACHED_REGISTERS)
        {
            let host = usize::from(cache.count);
            cache.guests[host] = guest;
            cache.host_by_guest[guest as usize] = host as u8;
            cache.count += 1;
        }
        cache
    }

    pub(super) fn host(self, guest: usize) -> Option<CachedHost> {
        let index = *self.host_by_guest.get(guest)?;
        (index != Self::NONE).then(|| CachedHost::ALL[index as usize])
    }

    pub(super) const fn is_empty(self) -> bool {
        self.count == 0
    }

    pub(super) fn entries(self) -> impl DoubleEndedIterator<Item = (CachedHost, usize)> {
        CachedHost::ALL
            .into_iter()
            .zip(self.guests.map(usize::from))
            .take(usize::from(self.count))
    }

    #[cfg(any(test, feature = "profile"))]
    pub(super) const fn count(self) -> usize {
        self.count as usize
    }

    #[cfg(any(test, feature = "profile"))]
    pub(super) const fn guests(self) -> [u8; MAX_CACHED_REGISTERS] {
        self.guests
    }
}
