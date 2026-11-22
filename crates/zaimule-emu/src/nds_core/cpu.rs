use super::bus::Bus;

/// Env-gated debug switches. Read ONCE: `std::env::var` scans the whole
/// environment and allocates, which is far too slow for the SWI/decode paths
/// these guard (they were costing real frame time).
static SWILOG: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var("NDS_SWILOG").is_ok());
static STRICT: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var("NDS_STRICT").is_ok());
static BREAKSTACK: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var("NDS_BREAKSTACK").is_ok());

/// CPSR flag bits.
const N: u32 = 1 << 31;
const Z: u32 = 1 << 30;
const C: u32 = 1 << 29;
const V: u32 = 1 << 28;
const Q: u32 = 1 << 27;
const T: u32 = 1 << 5; // Thumb state

const OP_DP: u8 = 0;
const OP_SINGLE: u8 = 1;
const OP_BLOCK: u8 = 2;
const OP_BRANCH: u8 = 3;
const OP_HALFWORD: u8 = 4;
const OP_MUL: u8 = 5;
const OP_MULL: u8 = 6;
const OP_SMLAXY: u8 = 7;
const OP_COPROC: u8 = 8;
const OP_LDC_STC: u8 = 9;
const OP_SWI: u8 = 10;
const OP_MISC: u8 = 11;
const OP_UNDEF: u8 = 12;

/// Compile-time 4096-entry ARM instruction class lookup table indexed by
/// `(((op >> 16) & 0xFF0) | ((op >> 4) & 0x00F))`. Replaces ~16 sequential
/// mask/compare branches per instruction with a single indexed jump.
const ARM_LUT: [u8; 4096] = {
    let mut lut = [OP_UNDEF; 4096];
    let mut idx = 0usize;
    while idx < 4096 {
        let hi = (idx >> 4) as u8; // bits 27..20
        let lo = (idx & 0xF) as u8; // bits 7..4
        lut[idx] = if (hi == 0x12 && (lo == 0x1 || lo == 0x3))
            || (hi == 0x16 && lo == 0x1)
            || ((hi & 0xF9) == 0x10 && lo == 0x5)
        {
            OP_MISC
        } else if (hi & 0xF9) == 0x10 && (lo & 0x9) == 0x8 {
            if hi == 0x10 || hi == 0x14 {
                OP_SMLAXY
            } else {
                OP_MISC
            }
        } else if (hi & 0xE0) == 0xA0 {
            OP_BRANCH
        } else if (hi & 0xF0) == 0xE0 && (lo & 1) == 1 {
            OP_COPROC
        } else if (hi & 0xE0) == 0xC0 {
            OP_LDC_STC
        } else if (hi & 0xFC) == 0x00 && lo == 0x9 {
            OP_MUL
        } else if (hi & 0xF8) == 0x08 && lo == 0x9 {
            OP_MULL
        } else if (hi & 0xFB) == 0x10 && lo == 0x9 {
            OP_MISC
        } else if (hi & 0xE0) == 0x00 && (lo & 0x9) == 0x9 && (lo & 0x6) != 0 {
            OP_HALFWORD
        } else if (hi & 0xFB) == 0x10 && lo == 0x0 {
            OP_MISC
        } else if (hi & 0xDB) == 0x12 {
            OP_MISC
        } else if (hi & 0xC0) == 0x00 {
            OP_DP
        } else if (hi & 0xC0) == 0x40 {
            OP_SINGLE
        } else if (hi & 0xE0) == 0x80 {
            OP_BLOCK
        } else if (hi & 0xF0) == 0xF0 {
            OP_SWI
        } else {
            OP_UNDEF
        };
        idx += 1;
    }
    lut
};

/// One ARM core, generic over its bus view. With `arm9` set it executes the
/// ARMv5TE extensions (BLX, CLZ, saturating ops, halfword multiplies,
/// LDRD/STRD, CP15) on top of the ARMv4T base; the NDS ARM7 uses the base
/// set only. No pipeline emulation: r15 reads as PC+8 (ARM) / PC+4 (Thumb).
/// Everything about a core that belongs in a savestate. Kept in its own
/// struct so the savestate can derive Serialize/Deserialize over it: the
/// remaining `Cpu` fields are the bus view (shared machine handle, not state)
/// and the debug breakpoint (environment config).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CpuState {
    pub regs: [u32; 16],
    pub cpsr: u32,
    spsr: u32,
    bank_usr: [u32; 2], // r13, r14
    bank_fiq: [u32; 7], // r8-r14
    bank_svc: [u32; 2],
    bank_abt: [u32; 2],
    bank_irq: [u32; 2],
    bank_und: [u32; 2],
    bank_fiq_usr: [u32; 5],
    spsr_fiq: u32,
    spsr_svc: u32,
    spsr_abt: u32,
    spsr_irq: u32,
    spsr_und: u32,
    pub halted: bool,
    /// Steps to idle (WaitByLoop HLE): real code relies on this delay for
    /// cross-CPU handshakes, so it must consume scheduler time.
    spin: u32,
    /// Active IntrWait target mask: re-halts until the BIOS flag word (top of
    /// DTCM / ARM7 WRAM) has one of these bits set by the game's IRQ handler.
    intr_wait: Option<u32>,
    pub arm9: bool,
    /// Exception vector base: 0xFFFF_0000 on the ARM9 (CP15 high vectors,
    /// the NDS default), 0 on the ARM7.
    vec_base: u32,
    // CP15 (ARM9 only).
    cp15_control: u32,
    pub cp15_dtcm: u32,
    cp15_itcm: u32,
}

const BLOCK_CACHE_SIZE: usize = 2048;
const MAX_BLOCK_OPS: usize = 16;

#[derive(Clone, Copy)]
enum DecodedOp {
    Nop,
    // ---- ARM pre-decoded ops ----
    ArmMovImm {
        cond: u8,
        rd: u8,
        s: bool,
        carry_out: u8,
        imm: u32,
    },
    ArmAddSubImm {
        cond: u8,
        rd: u8,
        rn: u8,
        flags: u8, // bit0 = S, bit1 = SUB
        imm: u32,
    },
    ArmCmpImm {
        cond: u8,
        rn: u8,
        cmn: bool,
        imm: u32,
    },
    ArmLogicImm {
        cond: u8,
        opc: u8,
        rd: u8,
        rn: u8,
        s: bool,
        carry_out: u8,
        imm: u32,
    },
    ArmDpRegSimple {
        cond: u8,
        opc: u8,
        rd: u8,
        rn: u8,
        rm: u8,
        shift_ty: u8,
        shift_amt: u8,
        s: bool,
    },
    ArmLdr32Pc {
        cond: u8,
        rd: u8,
        addr: u32,
    },
    ArmLdrImm {
        cond: u8,
        rd: u8,
        rn: u8,
        flags: u8, // bit0 = byte, bit1 = pre, bit2 = wb
        off: i32,
    },
    ArmStrImm {
        cond: u8,
        rd: u8,
        rn: u8,
        flags: u8, // bit0 = byte, bit1 = pre, bit2 = wb
        off: i32,
    },
    ArmBranch {
        cond: u8,
        link: bool,
        target: u32,
        ret_pc: u32,
    },
    ArmBx {
        cond: u8,
        rm: u8,
        link: bool,
        ret_pc: u32,
    },
    ArmFallback {
        raw: u32,
        pc: u32,
        ends_block: bool,
    },
    // ---- Thumb pre-decoded ops ----
    ThumbShiftImm {
        opc: u8,
        rd: u8,
        rs: u8,
        imm5: u8,
    },
    ThumbAddSub {
        opc: u8, // bit0 = sub, bit1 = imm3
        rd: u8,
        rs: u8,
        rn_or_imm: u8,
    },
    ThumbImm8 {
        opc: u8,
        rd: u8,
        imm: u8,
    },
    ThumbAlu {
        opc: u8,
        rd: u8,
        rs: u8,
    },
    ThumbHiReg {
        opc: u8, // 0=ADD, 1=CMP, 2=MOV (rd!=15, rs!=15)
        rd: u8,
        rs: u8,
    },
    ThumbLdrPc {
        rd: u8,
        addr: u32,
    },
    ThumbLdrStrImm {
        kind: u8, // 0=STR32, 1=STR8, 2=LDR32, 3=LDR8, 4=STR16, 5=LDR16
        rd: u8,
        rb: u8,
        off: u16,
    },
    ThumbAddPcSp {
        is_sp: bool,
        rd: u8,
        val_or_off: u32,
    },
    ThumbCondBranch {
        cond: u8,
        target: u32,
    },
    ThumbBranch {
        target: u32,
    },
    ThumbBlPair {
        target: u32,
        ret_lr: u32,
        to_arm: bool,
        raw0: u16,
    },
    ThumbFallback {
        raw: u16,
        pc: u32,
        ends_block: bool,
    },
}

#[derive(Clone, Copy)]
struct CachedBlock {
    start_pc: u32,
    page_gen: u32,
    page_idx: u16,
    len: u8,
    flags: u8, // bit0 = thumb, bit1 = is_idle_loop, bit2 = is_poll_loop
    ops: [DecodedOp; MAX_BLOCK_OPS],
}

impl CachedBlock {
    const fn empty() -> Self {
        Self {
            start_pc: 0,
            page_gen: 0,
            page_idx: u16::MAX,
            len: 0,
            flags: 0,
            ops: [DecodedOp::Nop; MAX_BLOCK_OPS],
        }
    }
}

#[inline(always)]
fn block_slot(pc: u32) -> usize {
    (((pc >> 1) ^ (pc >> 12)) as usize) & (BLOCK_CACHE_SIZE - 1)
}

pub struct Cpu<B: Bus> {
    pub st: CpuState,
    brk: Option<u32>,
    block_cache: Box<[CachedBlock; BLOCK_CACHE_SIZE]>,
    pub bus: B,
}

impl<B: Bus> Cpu<B> {
    pub fn new(bus: B, arm9: bool, entry: u32, sp: u32, sp_irq: u32, sp_svc: u32) -> Self {
        let mut regs = [0u32; 16];
        regs[13] = sp;
        regs[15] = entry;
        let block_cache = vec![CachedBlock::empty(); BLOCK_CACHE_SIZE]
            .into_boxed_slice()
            .try_into()
            .ok()
            .expect("block cache size");
        Self {
            st: CpuState {
                regs,
                cpsr: 0x1F, // system mode, ARM state
                spsr: 0,
                bank_usr: [sp, 0],
                bank_fiq: [0; 7],
                bank_svc: [sp_svc, 0],
                bank_abt: [0; 2],
                bank_irq: [sp_irq, 0],
                bank_und: [0; 2],
                bank_fiq_usr: [0; 5],
                spsr_fiq: 0,
                spsr_svc: 0,
                spsr_abt: 0,
                spsr_irq: 0,
                spsr_und: 0,
                halted: false,
                spin: 0,
                intr_wait: None,
                arm9,
                vec_base: if arm9 { 0xFFFF_0000 } else { 0 },
                cp15_control: 0x0005_2078,
                cp15_dtcm: 0,
                cp15_itcm: 0,
            },
            brk: std::env::var("NDS_BREAK")
                .ok()
                .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()),
            block_cache,
            bus,
        }
    }

    pub fn flush_block_cache(&mut self) {
        for slot in self.block_cache.iter_mut() {
            slot.page_idx = u16::MAX;
        }
    }

    /// Overwrite the core with a savestate's registers. The bus view and the
    /// debug breakpoint stay as they are.
    pub fn restore(&mut self, st: CpuState) {
        self.st = st;
        self.flush_block_cache();
    }

    #[inline(always)]
    fn thumb(&self) -> bool {
        self.st.cpsr & T != 0
    }

    #[inline(always)]
    fn flag(&self, f: u32) -> bool {
        self.st.cpsr & f != 0
    }

    #[inline(always)]
    fn set_flag(&mut self, f: u32, on: bool) {
        if on {
            self.st.cpsr |= f
        } else {
            self.st.cpsr &= !f
        }
    }

    #[inline(always)]
    fn set_nz(&mut self, v: u32) {
        let mut cpsr = self.st.cpsr & !(N | Z);
        cpsr |= v & N;
        if v == 0 {
            cpsr |= Z;
        }
        self.st.cpsr = cpsr;
    }

    #[inline(always)]
    fn r(&self, i: u32) -> u32 {
        if i == 15 {
            self.st.regs[15].wrapping_add(if self.thumb() { 4 } else { 8 })
        } else {
            self.st.regs[i as usize]
        }
    }

    #[inline(always)]
    fn set_r(&mut self, i: u32, v: u32) {
        if i == 15 {
            self.st.regs[15] = v & if self.thumb() { !1 } else { !3 };
        } else {
            self.st.regs[i as usize] = v;
        }
    }

    #[inline(always)]
    fn mode(&self) -> u32 {
        self.st.cpsr & 0x1F
    }

    fn switch_mode(&mut self, old: u32) {
        let new = self.mode();
        if old == new || (old | new) & !0xF == 0x10 && (old & 0xF) == (new & 0xF) {
            return;
        }
        match old {
            0x11 => {
                self.st.bank_fiq.copy_from_slice(&self.st.regs[8..15]);
                self.st.regs[8..13].copy_from_slice(&self.st.bank_fiq_usr);
            }
            0x13 => self.st.bank_svc.copy_from_slice(&self.st.regs[13..15]),
            0x17 => self.st.bank_abt.copy_from_slice(&self.st.regs[13..15]),
            0x12 => self.st.bank_irq.copy_from_slice(&self.st.regs[13..15]),
            0x1B => self.st.bank_und.copy_from_slice(&self.st.regs[13..15]),
            _ => self.st.bank_usr.copy_from_slice(&self.st.regs[13..15]),
        }
        match new {
            0x11 => {
                self.st.bank_fiq_usr.copy_from_slice(&self.st.regs[8..13]);
                self.st.regs[8..15].copy_from_slice(&self.st.bank_fiq);
            }
            0x13 => self.st.regs[13..15].copy_from_slice(&self.st.bank_svc),
            0x17 => self.st.regs[13..15].copy_from_slice(&self.st.bank_abt),
            0x12 => self.st.regs[13..15].copy_from_slice(&self.st.bank_irq),
            0x1B => self.st.regs[13..15].copy_from_slice(&self.st.bank_und),
            _ => self.st.regs[13..15].copy_from_slice(&self.st.bank_usr),
        }
    }

    fn spsr_for_mode(&mut self) -> &mut u32 {
        match self.mode() {
            0x11 => &mut self.st.spsr_fiq,
            0x13 => &mut self.st.spsr_svc,
            0x17 => &mut self.st.spsr_abt,
            0x12 => &mut self.st.spsr_irq,
            0x1B => &mut self.st.spsr_und,
            _ => &mut self.st.spsr,
        }
    }

    fn user_reg(&self, i: u32) -> u32 {
        let m = self.mode();
        match i {
            8..=12 if m == 0x11 => self.st.bank_fiq_usr[i as usize - 8],
            13 | 14 if m != 0x10 && m != 0x1F => self.st.bank_usr[i as usize - 13],
            _ => self.st.regs[i as usize],
        }
    }

    fn set_user_reg(&mut self, i: u32, v: u32) {
        let m = self.mode();
        match i {
            8..=12 if m == 0x11 => self.st.bank_fiq_usr[i as usize - 8] = v,
            13 | 14 if m != 0x10 && m != 0x1F => self.st.bank_usr[i as usize - 13] = v,
            _ => self.st.regs[i as usize] = v,
        }
    }

    #[inline(always)]
    fn cond(&self, c: u32) -> bool {
        match c {
            0x0 => self.flag(Z),
            0x1 => !self.flag(Z),
            0x2 => self.flag(C),
            0x3 => !self.flag(C),
            0x4 => self.flag(N),
            0x5 => !self.flag(N),
            0x6 => self.flag(V),
            0x7 => !self.flag(V),
            0x8 => self.flag(C) && !self.flag(Z),
            0x9 => !self.flag(C) || self.flag(Z),
            0xA => self.flag(N) == self.flag(V),
            0xB => self.flag(N) != self.flag(V),
            0xC => !self.flag(Z) && self.flag(N) == self.flag(V),
            0xD => self.flag(Z) || self.flag(N) != self.flag(V),
            _ => true,
        }
    }

    #[inline(always)]
    fn shift(&self, ty: u32, val: u32, amount: u32, imm: bool) -> (u32, bool) {
        let c_in = self.flag(C);
        match ty {
            0 => {
                if amount == 0 {
                    (val, c_in)
                } else if amount < 32 {
                    (val << amount, val >> (32 - amount) & 1 != 0)
                } else if amount == 32 {
                    (0, val & 1 != 0)
                } else {
                    (0, false)
                }
            }
            1 => {
                let amount = if imm && amount == 0 { 32 } else { amount };
                if amount == 0 {
                    (val, c_in)
                } else if amount < 32 {
                    (val >> amount, val >> (amount - 1) & 1 != 0)
                } else if amount == 32 {
                    (0, val >> 31 != 0)
                } else {
                    (0, false)
                }
            }
            2 => {
                let amount = if imm && amount == 0 { 32 } else { amount };
                if amount == 0 {
                    (val, c_in)
                } else if amount < 32 {
                    (
                        ((val as i32) >> amount) as u32,
                        val >> (amount - 1) & 1 != 0,
                    )
                } else {
                    let fill = if val >> 31 != 0 { u32::MAX } else { 0 };
                    (fill, val >> 31 != 0)
                }
            }
            _ => {
                if imm && amount == 0 {
                    ((val >> 1) | ((c_in as u32) << 31), val & 1 != 0)
                } else if amount == 0 {
                    (val, c_in)
                } else {
                    let a = amount & 31;
                    if a == 0 {
                        (val, val >> 31 != 0)
                    } else {
                        (val.rotate_right(a), val >> (a - 1) & 1 != 0)
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn add_with_flags(&mut self, a: u32, b: u32, carry: u32, set: bool) -> u32 {
        let r64 = a as u64 + b as u64 + carry as u64;
        let r = r64 as u32;
        if set {
            let mut cpsr = self.st.cpsr & !(N | Z | C | V);
            cpsr |= r & N;
            if r == 0 {
                cpsr |= Z;
            }
            if r64 > 0xFFFF_FFFF {
                cpsr |= C;
            }
            if (!(a ^ b) & (a ^ r)) >> 31 != 0 {
                cpsr |= V;
            }
            self.st.cpsr = cpsr;
        }
        r
    }

    #[inline(always)]
    fn sub_with_flags(&mut self, a: u32, b: u32, carry: u32, set: bool) -> u32 {
        self.add_with_flags(a, !b, carry, set)
    }

    /// Returns true if this core is halted (not spinning) and has no pending IRQ.
    #[inline(always)]
    pub fn is_halted_idle(&mut self) -> bool {
        self.st.spin == 0 && self.st.halted && !self.bus.irq_pending()
    }

    #[cold]
    #[inline(never)]
    fn enter_irq(&mut self) {
        let old_cpsr = self.st.cpsr;
        let old_mode = self.mode();
        let ret = self.st.regs[15].wrapping_add(4);
        self.st.cpsr = (self.st.cpsr & !0x3F) | 0x12 | 0x80; // IRQ mode, ARM, I set
        self.switch_mode(old_mode);
        *self.spsr_for_mode() = old_cpsr;
        self.st.regs[14] = ret;
        self.st.regs[15] = self.st.vec_base + 0x18;
    }

    #[cold]
    #[inline(never)]
    fn check_intr_wait(&mut self, mask: u32) {
        if self.mode() != 0x12 {
            let flag_addr = self.bus.bios_flag_addr();
            let flags = self.bus.read32(flag_addr);
            if flags & mask != 0 {
                self.bus.write32(flag_addr, flags & !mask);
                self.st.intr_wait = None;
            } else {
                self.st.halted = true;
            }
        }
    }

    /// Execute up to `n` instructions, collapsing idle time.
    ///
    /// A halted core with nothing pending, or one spinning out a WaitByLoop,
    /// has no work for the whole slice, yet `step()` still polls the bus
    /// (through RefCell) once per call. Platinum's ARM9 sits halted for most
    /// of every frame, so those polls dominated emulation time: skipping the
    /// slice wholesale is the single biggest speed win available. The slice
    /// length is unchanged, so cross-CPU wake latency is exactly as before.
    pub fn run_slice(&mut self, mut n: u32) {
        if self.st.spin >= n {
            self.st.spin -= n;
            return;
        }
        if self.st.spin > 0 {
            n -= self.st.spin;
            self.st.spin = 0;
        }
        if self.st.halted && !self.bus.irq_pending() {
            // The IntrWait flag is only ever set by this core's own IRQ
            // handler, which cannot run while nothing is pending.
            return;
        }
        if self.brk.is_none() && !self.bus.has_debug_hooks() {
            self.run_slice_fast(n);
            return;
        }
        let mut remaining = n;
        while remaining > 0 {
            self.step();
            remaining -= 1;
            if self.st.spin > 0 {
                if self.st.spin >= remaining {
                    self.st.spin -= remaining;
                    return;
                }
                remaining -= self.st.spin;
                self.st.spin = 0;
            }
            if self.st.halted && !self.bus.irq_pending() {
                return;
            }
        }
    }

    #[inline(always)]
    fn apply_logical_imm_flags(&mut self, r: u32, carry_out: u8) {
        let mut cpsr = self.st.cpsr & !(N | Z);
        cpsr |= r & N;
        if r == 0 {
            cpsr |= Z;
        }
        if carry_out == 1 {
            cpsr &= !C;
        } else if carry_out == 2 {
            cpsr |= C;
        }
        self.st.cpsr = cpsr;
    }

    fn decode_arm_block(
        &mut self,
        start_pc: u32,
        page_idx: u16,
        page_gen: u32,
        page_ptr: *const u8,
        start_off: usize,
    ) -> CachedBlock {
        let mut blk = CachedBlock {
            start_pc,
            page_gen,
            page_idx,
            len: 0,
            flags: 0,
            ops: [DecodedOp::Nop; MAX_BLOCK_OPS],
        };
        let mut cur_pc = start_pc;
        let mut off = start_off & !3;
        let mut written_regs: u16 = 0;
        let mut base_regs: u16 = 0;
        let mut has_load = false;
        let mut poll_pure = true;

        while (blk.len as usize) < MAX_BLOCK_OPS && off + 4 <= 0x1000 {
            // SAFETY: `off + 4 <= 0x1000` within valid 4 KiB page.
            let raw =
                unsafe { u32::from_le(std::ptr::read_unaligned(page_ptr.add(off) as *const u32)) };
            let cond = (raw >> 28) as u8;

            let (op, ends_block) = if cond == 0xF {
                poll_pure = false;
                if B::IS_ARM9 {
                    (
                        DecodedOp::ArmFallback {
                            raw,
                            pc: cur_pc,
                            ends_block: true,
                        },
                        true,
                    )
                } else {
                    (DecodedOp::Nop, false)
                }
            } else {
                let lut_idx = (((raw >> 16) & 0xFF0) | ((raw >> 4) & 0x00F)) as usize;
                match ARM_LUT[lut_idx] {
                    OP_BRANCH => {
                        let delta = ((raw << 8) as i32 >> 6) as u32;
                        let target = cur_pc.wrapping_add(8).wrapping_add(delta);
                        let link = raw & 0x0100_0000 != 0;
                        if !link && target == start_pc {
                            if blk.len == 0 && cond == 0xE {
                                blk.flags |= 2; // unconditional B . idle loop
                            } else if blk.len > 0
                                && cond < 0xE
                                && poll_pure
                                && has_load
                                && (written_regs & base_regs) == 0
                            {
                                blk.flags |= 4; // read-only poll loop
                            }
                        }
                        (
                            DecodedOp::ArmBranch {
                                cond,
                                link,
                                target,
                                ret_pc: cur_pc.wrapping_add(4),
                            },
                            true,
                        )
                    }
                    OP_MISC => {
                        poll_pure = false;
                        if raw & 0x0FFF_FFF0 == 0x012F_FF10 {
                            let rm = (raw & 0xF) as u8;
                            if rm != 15 {
                                (
                                    DecodedOp::ArmBx {
                                        cond,
                                        rm,
                                        link: false,
                                        ret_pc: 0,
                                    },
                                    true,
                                )
                            } else {
                                (
                                    DecodedOp::ArmFallback {
                                        raw,
                                        pc: cur_pc,
                                        ends_block: true,
                                    },
                                    true,
                                )
                            }
                        } else if B::IS_ARM9 && raw & 0x0FFF_FFF0 == 0x012F_FF30 {
                            let rm = (raw & 0xF) as u8;
                            if rm != 15 {
                                (
                                    DecodedOp::ArmBx {
                                        cond,
                                        rm,
                                        link: true,
                                        ret_pc: cur_pc.wrapping_add(4),
                                    },
                                    true,
                                )
                            } else {
                                (
                                    DecodedOp::ArmFallback {
                                        raw,
                                        pc: cur_pc,
                                        ends_block: true,
                                    },
                                    true,
                                )
                            }
                        } else {
                            // MSR/MRS/CLZ/SWP/QADD etc. - end block on MSR (may change CPSR I/mode).
                            let is_msr = raw & 0x0DB0_F000 == 0x0120_F000;
                            let rd = (raw >> 12) & 0xF;
                            let ends = is_msr || rd == 15;
                            (
                                DecodedOp::ArmFallback {
                                    raw,
                                    pc: cur_pc,
                                    ends_block: ends,
                                },
                                ends,
                            )
                        }
                    }
                    OP_DP => {
                        let opcode = ((raw >> 21) & 0xF) as u8;
                        let s = raw & 0x0010_0000 != 0;
                        let rn = ((raw >> 16) & 0xF) as u8;
                        let rd = ((raw >> 12) & 0xF) as u8;
                        let is_test = (0x8..=0xB).contains(&opcode);
                        if rd == 15 || (is_test && !s) {
                            poll_pure = false;
                            (
                                DecodedOp::ArmFallback {
                                    raw,
                                    pc: cur_pc,
                                    ends_block: rd == 15,
                                },
                                rd == 15,
                            )
                        } else if raw & 0x0200_0000 != 0 {
                            // Immediate operand2
                            let imm8 = raw & 0xFF;
                            let rot = ((raw >> 8) & 0xF) * 2;
                            let (imm, carry_out) = if rot == 0 {
                                (imm8, 0u8)
                            } else {
                                let v = imm8.rotate_right(rot);
                                (v, if v >> 31 != 0 { 2u8 } else { 1u8 })
                            };
                            match opcode {
                                0xD => {
                                    // MOV imm
                                    if cond == 0xE {
                                        written_regs |= 1 << rd;
                                    } else {
                                        poll_pure = false;
                                    }
                                    (
                                        DecodedOp::ArmMovImm {
                                            cond,
                                            rd,
                                            s,
                                            carry_out,
                                            imm,
                                        },
                                        false,
                                    )
                                }
                                0x2 | 0x4 if rn != 15 => {
                                    poll_pure = false;
                                    let flags = u8::from(s) | (u8::from(opcode == 0x2) << 1);
                                    (
                                        DecodedOp::ArmAddSubImm {
                                            cond,
                                            rd,
                                            rn,
                                            flags,
                                            imm,
                                        },
                                        false,
                                    )
                                }
                                0xA | 0xB if rn != 15 => {
                                    if cond != 0xE {
                                        poll_pure = false;
                                    }
                                    (
                                        DecodedOp::ArmCmpImm {
                                            cond,
                                            rn,
                                            cmn: opcode == 0xB,
                                            imm,
                                        },
                                        false,
                                    )
                                }
                                0x0 | 0x1 | 0x8 | 0x9 | 0xC | 0xE | 0xF
                                    if rn != 15 || opcode == 0xF =>
                                {
                                    if cond != 0xE {
                                        poll_pure = false;
                                    } else if opcode != 0x8 && opcode != 0x9 {
                                        if opcode != 0xF
                                            && rn == rd
                                            && (written_regs & (1 << rn)) == 0
                                        {
                                            poll_pure = false;
                                        }
                                        written_regs |= 1 << rd;
                                    }
                                    (
                                        DecodedOp::ArmLogicImm {
                                            cond,
                                            opc: opcode,
                                            rd,
                                            rn,
                                            s,
                                            carry_out,
                                            imm,
                                        },
                                        false,
                                    )
                                }
                                _ => {
                                    poll_pure = false;
                                    (
                                        DecodedOp::ArmFallback {
                                            raw,
                                            pc: cur_pc,
                                            ends_block: false,
                                        },
                                        false,
                                    )
                                }
                            }
                        } else if raw & 0x10 == 0 {
                            // Register operand2 with immediate shift
                            let rm = (raw & 0xF) as u8;
                            if rn != 15 && rm != 15 {
                                let shift_ty = ((raw >> 5) & 3) as u8;
                                let shift_amt = ((raw >> 7) & 0x1F) as u8;
                                if cond != 0xE {
                                    poll_pure = false;
                                } else if !is_test {
                                    let pure_logic =
                                        matches!(opcode, 0x0 | 0x1 | 0xC | 0xD | 0xE | 0xF);
                                    if !pure_logic {
                                        poll_pure = false;
                                    } else {
                                        if (opcode != 0xD
                                            && opcode != 0xF
                                            && rn == rd
                                            && (written_regs & (1 << rn)) == 0)
                                            || (rm == rd && (written_regs & (1 << rm)) == 0)
                                        {
                                            poll_pure = false;
                                        }
                                        written_regs |= 1 << rd;
                                    }
                                }
                                (
                                    DecodedOp::ArmDpRegSimple {
                                        cond,
                                        opc: opcode,
                                        rd,
                                        rn,
                                        rm,
                                        shift_ty,
                                        shift_amt,
                                        s,
                                    },
                                    false,
                                )
                            } else {
                                poll_pure = false;
                                (
                                    DecodedOp::ArmFallback {
                                        raw,
                                        pc: cur_pc,
                                        ends_block: false,
                                    },
                                    false,
                                )
                            }
                        } else {
                            poll_pure = false;
                            (
                                DecodedOp::ArmFallback {
                                    raw,
                                    pc: cur_pc,
                                    ends_block: false,
                                },
                                false,
                            )
                        }
                    }
                    OP_SINGLE => {
                        let rn = ((raw >> 16) & 0xF) as u8;
                        let rd = ((raw >> 12) & 0xF) as u8;
                        let reg_off = raw & 0x0200_0000 != 0;
                        let pre = raw & 0x0100_0000 != 0;
                        let up = raw & 0x0080_0000 != 0;
                        let byte = raw & 0x0040_0000 != 0;
                        let wb = raw & 0x0020_0000 != 0;
                        let load = raw & 0x0010_0000 != 0;
                        let imm12 = raw & 0xFFF;
                        if load && !reg_off && !byte && pre && !wb && rn == 15 && rd != 15 {
                            let pc8 = cur_pc.wrapping_add(8);
                            let addr = if up {
                                pc8.wrapping_add(imm12)
                            } else {
                                pc8.wrapping_sub(imm12)
                            };
                            if cond == 0xE {
                                has_load = true;
                                written_regs |= 1 << rd;
                            } else {
                                poll_pure = false;
                            }
                            (DecodedOp::ArmLdr32Pc { cond, rd, addr }, false)
                        } else if !reg_off && rd != 15 && rn != 15 {
                            let off_i32 = if up { imm12 as i32 } else { -(imm12 as i32) };
                            let flags = u8::from(byte) | (u8::from(pre) << 1) | (u8::from(wb) << 2);
                            if load {
                                if cond == 0xE && pre && !wb {
                                    has_load = true;
                                    base_regs |= 1 << rn;
                                    written_regs |= 1 << rd;
                                } else {
                                    poll_pure = false;
                                }
                                (
                                    DecodedOp::ArmLdrImm {
                                        cond,
                                        rd,
                                        rn,
                                        flags,
                                        off: off_i32,
                                    },
                                    false,
                                )
                            } else {
                                poll_pure = false;
                                (
                                    DecodedOp::ArmStrImm {
                                        cond,
                                        rd,
                                        rn,
                                        flags,
                                        off: off_i32,
                                    },
                                    false,
                                )
                            }
                        } else {
                            poll_pure = false;
                            let ends = load && rd == 15;
                            (
                                DecodedOp::ArmFallback {
                                    raw,
                                    pc: cur_pc,
                                    ends_block: ends,
                                },
                                ends,
                            )
                        }
                    }
                    OP_BLOCK => {
                        poll_pure = false;
                        let load = raw & 0x0010_0000 != 0;
                        let list = raw & 0xFFFF;
                        let ends = load && (list == 0 || (list & 0x8000) != 0);
                        (
                            DecodedOp::ArmFallback {
                                raw,
                                pc: cur_pc,
                                ends_block: ends,
                            },
                            ends,
                        )
                    }
                    OP_SWI | OP_COPROC => {
                        poll_pure = false;
                        (
                            DecodedOp::ArmFallback {
                                raw,
                                pc: cur_pc,
                                ends_block: true,
                            },
                            true,
                        )
                    }
                    _ => {
                        poll_pure = false;
                        (
                            DecodedOp::ArmFallback {
                                raw,
                                pc: cur_pc,
                                ends_block: false,
                            },
                            false,
                        )
                    }
                }
            };

            blk.ops[blk.len as usize] = op;
            blk.len += 1;
            cur_pc = cur_pc.wrapping_add(4);
            off += 4;
            if ends_block {
                break;
            }
        }

        self.bus.mark_code_page(page_idx);
        blk
    }

    fn decode_thumb_block(
        &mut self,
        start_pc: u32,
        page_idx: u16,
        page_gen: u32,
        page_ptr: *const u8,
        start_off: usize,
    ) -> CachedBlock {
        let mut blk = CachedBlock {
            start_pc,
            page_gen,
            page_idx,
            len: 0,
            flags: 1, // bit0 = thumb
            ops: [DecodedOp::Nop; MAX_BLOCK_OPS],
        };
        let mut cur_pc = start_pc;
        let mut off = start_off & !1;
        let mut written_regs: u16 = 0;
        let mut base_regs: u16 = 0;
        let mut has_load = false;
        let mut poll_pure = true;

        while (blk.len as usize) < MAX_BLOCK_OPS && off + 2 <= 0x1000 {
            // SAFETY: `off + 2 <= 0x1000` within valid 4 KiB page.
            let raw =
                unsafe { u16::from_le(std::ptr::read_unaligned(page_ptr.add(off) as *const u16)) };
            let op32 = raw as u32;
            let mut advance = 2usize;

            let (op, ends_block) = match op32 >> 13 {
                0b000 => {
                    if (op32 >> 11) & 3 == 3 {
                        poll_pure = false;
                        let rd = (op32 & 7) as u8;
                        let rs = ((op32 >> 3) & 7) as u8;
                        let rn_or_imm = ((op32 >> 6) & 7) as u8;
                        let opc = u8::from(op32 & 0x200 != 0) | (u8::from(op32 & 0x400 != 0) << 1);
                        (
                            DecodedOp::ThumbAddSub {
                                opc,
                                rd,
                                rs,
                                rn_or_imm,
                            },
                            false,
                        )
                    } else {
                        let rd = (op32 & 7) as u8;
                        let rs = ((op32 >> 3) & 7) as u8;
                        let imm5 = ((op32 >> 6) & 0x1F) as u8;
                        let opc = ((op32 >> 11) & 3) as u8;
                        if rs == rd && (written_regs & (1 << rs)) == 0 {
                            poll_pure = false;
                        }
                        written_regs |= 1 << rd;
                        (DecodedOp::ThumbShiftImm { opc, rd, rs, imm5 }, false)
                    }
                }
                0b001 => {
                    let rd = ((op32 >> 8) & 7) as u8;
                    let imm = (op32 & 0xFF) as u8;
                    let opc = ((op32 >> 11) & 3) as u8;
                    if opc == 0 {
                        written_regs |= 1 << rd;
                    } else if opc >= 2 {
                        poll_pure = false;
                    }
                    (DecodedOp::ThumbImm8 { opc, rd, imm }, false)
                }
                0b010 => {
                    if op32 & 0x1C00 == 0x0000 {
                        let rd = (op32 & 7) as u8;
                        let rs = ((op32 >> 3) & 7) as u8;
                        let opc = ((op32 >> 6) & 0xF) as u8;
                        match opc {
                            0x8 | 0xA | 0xB => {}
                            0x0 | 0x1 | 0xC | 0xE | 0xF => {
                                if (opc != 0xF && (written_regs & (1 << rd)) == 0)
                                    || (rs == rd && (written_regs & (1 << rs)) == 0)
                                {
                                    poll_pure = false;
                                }
                                written_regs |= 1 << rd;
                            }
                            _ => poll_pure = false,
                        }
                        (DecodedOp::ThumbAlu { opc, rd, rs }, false)
                    } else if op32 & 0x1C00 == 0x0400 {
                        let opc = ((op32 >> 8) & 3) as u8;
                        let rd = ((op32 & 7) | ((op32 >> 4) & 8)) as u8;
                        let rs = ((op32 >> 3) & 0xF) as u8;
                        if opc < 3 && rd != 15 && rs != 15 {
                            if opc != 1 {
                                poll_pure = false;
                            }
                            (DecodedOp::ThumbHiReg { opc, rd, rs }, false)
                        } else {
                            poll_pure = false;
                            let ends = opc == 3 || rd == 15;
                            (
                                DecodedOp::ThumbFallback {
                                    raw,
                                    pc: cur_pc,
                                    ends_block: ends,
                                },
                                ends,
                            )
                        }
                    } else if op32 & 0x1800 == 0x0800 {
                        let rd = ((op32 >> 8) & 7) as u8;
                        let addr = ((cur_pc.wrapping_add(4)) & !3).wrapping_add((op32 & 0xFF) << 2);
                        has_load = true;
                        written_regs |= 1 << rd;
                        (DecodedOp::ThumbLdrPc { rd, addr }, false)
                    } else {
                        poll_pure = false;
                        (
                            DecodedOp::ThumbFallback {
                                raw,
                                pc: cur_pc,
                                ends_block: false,
                            },
                            false,
                        )
                    }
                }
                0b011 => {
                    let rd = (op32 & 7) as u8;
                    let rb = ((op32 >> 3) & 7) as u8;
                    let imm = (op32 >> 6) & 0x1F;
                    let byte = op32 & 0x1000 != 0;
                    let load = op32 & 0x0800 != 0;
                    let off_u16 = if byte { imm as u16 } else { (imm << 2) as u16 };
                    let kind = match (load, byte) {
                        (false, false) => 0u8,
                        (false, true) => 1u8,
                        (true, false) => 2u8,
                        (true, true) => 3u8,
                    };
                    if load {
                        has_load = true;
                        base_regs |= 1 << rb;
                        written_regs |= 1 << rd;
                    } else {
                        poll_pure = false;
                    }
                    (
                        DecodedOp::ThumbLdrStrImm {
                            kind,
                            rd,
                            rb,
                            off: off_u16,
                        },
                        false,
                    )
                }
                0b100 => {
                    if op32 & 0x1000 == 0 {
                        let rd = (op32 & 7) as u8;
                        let rb = ((op32 >> 3) & 7) as u8;
                        let off_u16 = (((op32 >> 6) & 0x1F) << 1) as u16;
                        let load = op32 & 0x0800 != 0;
                        if load {
                            has_load = true;
                            base_regs |= 1 << rb;
                            written_regs |= 1 << rd;
                        } else {
                            poll_pure = false;
                        }
                        (
                            DecodedOp::ThumbLdrStrImm {
                                kind: if load { 5 } else { 4 },
                                rd,
                                rb,
                                off: off_u16,
                            },
                            false,
                        )
                    } else {
                        let rd = ((op32 >> 8) & 7) as u8;
                        let off_u16 = ((op32 & 0xFF) << 2) as u16;
                        let load = op32 & 0x0800 != 0;
                        if load {
                            has_load = true;
                            base_regs |= 1 << 13;
                            written_regs |= 1 << rd;
                        } else {
                            poll_pure = false;
                        }
                        (
                            DecodedOp::ThumbLdrStrImm {
                                kind: if load { 2 } else { 0 },
                                rd,
                                rb: 13,
                                off: off_u16,
                            },
                            false,
                        )
                    }
                }
                0b101 => {
                    poll_pure = false;
                    if op32 & 0x1000 == 0 {
                        let rd = ((op32 >> 8) & 7) as u8;
                        let imm = (op32 & 0xFF) << 2;
                        let is_sp = op32 & 0x0800 != 0;
                        let val_or_off = if is_sp {
                            imm
                        } else {
                            (cur_pc.wrapping_add(4) & !3).wrapping_add(imm)
                        };
                        (
                            DecodedOp::ThumbAddPcSp {
                                is_sp,
                                rd,
                                val_or_off,
                            },
                            false,
                        )
                    } else if op32 & 0x0F00 == 0 {
                        let imm = (op32 & 0x7F) << 2;
                        let val_or_off = if op32 & 0x80 != 0 {
                            0u32.wrapping_sub(imm)
                        } else {
                            imm
                        };
                        (
                            DecodedOp::ThumbAddPcSp {
                                is_sp: true,
                                rd: 13,
                                val_or_off,
                            },
                            false,
                        )
                    } else {
                        let ends = (op32 & 0x0F00) == 0x0D00; // POP {..., pc}
                        (
                            DecodedOp::ThumbFallback {
                                raw,
                                pc: cur_pc,
                                ends_block: ends,
                            },
                            ends,
                        )
                    }
                }
                0b110 => {
                    if op32 & 0x1000 == 0 {
                        poll_pure = false;
                        let ends = (op32 & 0xFF) == 0 && (op32 & 0x0800 != 0);
                        (
                            DecodedOp::ThumbFallback {
                                raw,
                                pc: cur_pc,
                                ends_block: ends,
                            },
                            ends,
                        )
                    } else {
                        let cond = ((op32 >> 8) & 0xF) as u8;
                        if cond < 0xE {
                            let delta = ((op32 & 0xFF) as i8 as i32) << 1;
                            let target = cur_pc.wrapping_add(4).wrapping_add(delta as u32);
                            if blk.len > 0
                                && target == start_pc
                                && poll_pure
                                && has_load
                                && (written_regs & base_regs) == 0
                            {
                                blk.flags |= 4; // read-only poll loop
                            }
                            (DecodedOp::ThumbCondBranch { cond, target }, true)
                        } else {
                            poll_pure = false;
                            (
                                DecodedOp::ThumbFallback {
                                    raw,
                                    pc: cur_pc,
                                    ends_block: true,
                                },
                                true,
                            )
                        }
                    }
                }
                _ => {
                    if op32 & 0x1800 == 0x0000 {
                        let delta = ((op32 << 21) as i32 >> 20) as u32;
                        let target = cur_pc.wrapping_add(4).wrapping_add(delta);
                        if blk.len == 0 && target == start_pc {
                            blk.flags |= 2; // unconditional B . idle loop
                        }
                        (DecodedOp::ThumbBranch { target }, true)
                    } else if op32 & 0x1800 == 0x1000 && off + 4 <= 0x1000 {
                        // Fuse Thumb BL / BLX prefix+suffix pair within the same 4 KiB page.
                        let next_raw = unsafe {
                            u16::from_le(std::ptr::read_unaligned(
                                page_ptr.add(off + 2) as *const u16
                            ))
                        };
                        let next32 = next_raw as u32;
                        if next32 & 0xF800 == 0xF800 || (B::IS_ARM9 && next32 & 0xF800 == 0xE800) {
                            let hi_off = ((op32 << 21) as i32 >> 9) as u32;
                            let lr_mid = cur_pc.wrapping_add(4).wrapping_add(hi_off);
                            let to_arm = next32 & 0xF800 == 0xE800;
                            let target = lr_mid.wrapping_add((next32 & 0x7FF) << 1)
                                & if to_arm { !3 } else { !1 };
                            let ret_lr = cur_pc.wrapping_add(4) | 1;
                            advance = 4;
                            (
                                DecodedOp::ThumbBlPair {
                                    target,
                                    ret_lr,
                                    to_arm,
                                    raw0: raw,
                                },
                                true,
                            )
                        } else {
                            poll_pure = false;
                            (
                                DecodedOp::ThumbFallback {
                                    raw,
                                    pc: cur_pc,
                                    ends_block: true,
                                },
                                true,
                            )
                        }
                    } else {
                        poll_pure = false;
                        (
                            DecodedOp::ThumbFallback {
                                raw,
                                pc: cur_pc,
                                ends_block: true,
                            },
                            true,
                        )
                    }
                }
            };

            blk.ops[blk.len as usize] = op;
            blk.len += 1;
            cur_pc = cur_pc.wrapping_add(advance as u32);
            off += advance;
            if ends_block {
                break;
            }
        }

        self.bus.mark_code_page(page_idx);
        blk
    }

    #[inline(always)]
    fn exec_cached_block(&mut self, blk_ptr: *const CachedBlock, remaining: &mut u32) {
        // SAFETY: `self.block_cache` is never mutated or reallocated during block execution.
        let blk = unsafe { &*blk_ptr };
        let is_thumb = (blk.flags & 1) != 0;
        let is_idle_loop = (blk.flags & 2) != 0;
        let is_poll_loop = (blk.flags & 4) != 0;
        let page_idx = blk.page_idx;
        let page_gen = blk.page_gen;
        let step_bytes: u32 = if is_thumb { 2 } else { 4 };
        let mut cur_pc = blk.start_pc;
        let mut poll_load_safe = is_poll_loop;
        let len = blk.len as usize;

        for idx in 0..len {
            if *remaining == 0 {
                self.st.regs[15] = cur_pc;
                return;
            }
            let op = blk.ops[idx];
            match op {
                DecodedOp::Nop => {}
                DecodedOp::ArmMovImm {
                    cond,
                    rd,
                    s,
                    carry_out,
                    imm,
                } => {
                    if cond == 0xE || self.cond(cond as u32) {
                        self.st.regs[rd as usize] = imm;
                        if s {
                            self.apply_logical_imm_flags(imm, carry_out);
                        }
                    }
                }
                DecodedOp::ArmAddSubImm {
                    cond,
                    rd,
                    rn,
                    flags,
                    imm,
                } => {
                    if cond == 0xE || self.cond(cond as u32) {
                        let a = self.st.regs[rn as usize];
                        let s = (flags & 1) != 0;
                        let r = if (flags & 2) != 0 {
                            self.sub_with_flags(a, imm, 1, s)
                        } else {
                            self.add_with_flags(a, imm, 0, s)
                        };
                        self.st.regs[rd as usize] = r;
                    }
                }
                DecodedOp::ArmCmpImm { cond, rn, cmn, imm } => {
                    if cond == 0xE || self.cond(cond as u32) {
                        let a = self.st.regs[rn as usize];
                        if cmn {
                            self.add_with_flags(a, imm, 0, true);
                        } else {
                            self.sub_with_flags(a, imm, 1, true);
                        }
                    }
                }
                DecodedOp::ArmLogicImm {
                    cond,
                    opc,
                    rd,
                    rn,
                    s,
                    carry_out,
                    imm,
                } => {
                    if cond == 0xE || self.cond(cond as u32) {
                        let a = self.st.regs[rn as usize];
                        match opc {
                            0x0 => {
                                let r = a & imm;
                                self.st.regs[rd as usize] = r;
                                if s {
                                    self.apply_logical_imm_flags(r, carry_out);
                                }
                            }
                            0x1 => {
                                let r = a ^ imm;
                                self.st.regs[rd as usize] = r;
                                if s {
                                    self.apply_logical_imm_flags(r, carry_out);
                                }
                            }
                            0x8 => self.apply_logical_imm_flags(a & imm, carry_out),
                            0x9 => self.apply_logical_imm_flags(a ^ imm, carry_out),
                            0xC => {
                                let r = a | imm;
                                self.st.regs[rd as usize] = r;
                                if s {
                                    self.apply_logical_imm_flags(r, carry_out);
                                }
                            }
                            0xE => {
                                let r = a & !imm;
                                self.st.regs[rd as usize] = r;
                                if s {
                                    self.apply_logical_imm_flags(r, carry_out);
                                }
                            }
                            _ => {
                                let r = !imm;
                                self.st.regs[rd as usize] = r;
                                if s {
                                    self.apply_logical_imm_flags(r, carry_out);
                                }
                            }
                        }
                    }
                }
                DecodedOp::ArmDpRegSimple {
                    cond,
                    opc,
                    rd,
                    rn,
                    rm,
                    shift_ty,
                    shift_amt,
                    s,
                } => {
                    if cond == 0xE || self.cond(cond as u32) {
                        let rm_val = self.st.regs[rm as usize];
                        let (op2, sh_carry) = if shift_ty == 0 && shift_amt == 0 {
                            (rm_val, self.flag(C))
                        } else {
                            self.shift(shift_ty as u32, rm_val, shift_amt as u32, true)
                        };
                        let a = self.st.regs[rn as usize];
                        let set_log = |cpu: &mut Self, r: u32| {
                            let mut cpsr = cpu.st.cpsr & !(N | Z | C);
                            cpsr |= r & N;
                            if r == 0 {
                                cpsr |= Z;
                            }
                            if sh_carry {
                                cpsr |= C;
                            }
                            cpu.st.cpsr = cpsr;
                        };
                        match opc {
                            0x0 => {
                                let r = a & op2;
                                self.st.regs[rd as usize] = r;
                                if s {
                                    set_log(self, r);
                                }
                            }
                            0x1 => {
                                let r = a ^ op2;
                                self.st.regs[rd as usize] = r;
                                if s {
                                    set_log(self, r);
                                }
                            }
                            0x2 => self.st.regs[rd as usize] = self.sub_with_flags(a, op2, 1, s),
                            0x3 => self.st.regs[rd as usize] = self.sub_with_flags(op2, a, 1, s),
                            0x4 => self.st.regs[rd as usize] = self.add_with_flags(a, op2, 0, s),
                            0x5 => {
                                let c = self.flag(C) as u32;
                                self.st.regs[rd as usize] = self.add_with_flags(a, op2, c, s);
                            }
                            0x6 => {
                                let c = self.flag(C) as u32;
                                self.st.regs[rd as usize] = self.sub_with_flags(a, op2, c, s);
                            }
                            0x7 => {
                                let c = self.flag(C) as u32;
                                self.st.regs[rd as usize] = self.sub_with_flags(op2, a, c, s);
                            }
                            0x8 => set_log(self, a & op2),
                            0x9 => set_log(self, a ^ op2),
                            0xA => {
                                self.sub_with_flags(a, op2, 1, true);
                            }
                            0xB => {
                                self.add_with_flags(a, op2, 0, true);
                            }
                            0xC => {
                                let r = a | op2;
                                self.st.regs[rd as usize] = r;
                                if s {
                                    set_log(self, r);
                                }
                            }
                            0xD => {
                                self.st.regs[rd as usize] = op2;
                                if s {
                                    set_log(self, op2);
                                }
                            }
                            0xE => {
                                let r = a & !op2;
                                self.st.regs[rd as usize] = r;
                                if s {
                                    set_log(self, r);
                                }
                            }
                            _ => {
                                let r = !op2;
                                self.st.regs[rd as usize] = r;
                                if s {
                                    set_log(self, r);
                                }
                            }
                        }
                    }
                }
                DecodedOp::ArmLdr32Pc { cond, rd, addr } => {
                    if cond == 0xE || self.cond(cond as u32) {
                        if poll_load_safe && (addr >> 16) == 0x0410 {
                            poll_load_safe = false;
                        }
                        let raw = self.bus.read32(addr);
                        let v = if addr & 3 == 0 {
                            raw
                        } else {
                            raw.rotate_right((addr & 3) * 8)
                        };
                        self.st.regs[rd as usize] = v;
                    }
                }
                DecodedOp::ArmLdrImm {
                    cond,
                    rd,
                    rn,
                    flags,
                    off,
                } => {
                    if cond == 0xE || self.cond(cond as u32) {
                        let base = self.st.regs[rn as usize];
                        let off_base = base.wrapping_add(off as u32);
                        let pre = (flags & 2) != 0;
                        let addr = if pre { off_base } else { base };
                        if poll_load_safe && (addr >> 16) == 0x0410 {
                            poll_load_safe = false;
                        }
                        let v = if (flags & 1) != 0 {
                            self.bus.read8(addr) as u32
                        } else {
                            let raw = self.bus.read32(addr);
                            if addr & 3 == 0 {
                                raw
                            } else {
                                raw.rotate_right((addr & 3) * 8)
                            }
                        };
                        if !pre || (flags & 4) != 0 {
                            self.st.regs[rn as usize] = off_base;
                        }
                        self.st.regs[rd as usize] = v;
                    }
                }
                DecodedOp::ArmStrImm {
                    cond,
                    rd,
                    rn,
                    flags,
                    off,
                } => {
                    if cond == 0xE || self.cond(cond as u32) {
                        let base = self.st.regs[rn as usize];
                        let off_base = base.wrapping_add(off as u32);
                        let pre = (flags & 2) != 0;
                        let addr = if pre { off_base } else { base };
                        let v = self.st.regs[rd as usize];
                        if (flags & 1) != 0 {
                            self.bus.write8(addr, v as u8);
                        } else {
                            self.bus.write32(addr, v);
                        }
                        if !pre || (flags & 4) != 0 {
                            self.st.regs[rn as usize] = off_base;
                        }
                        if self.bus.code_page_gen(page_idx) != page_gen {
                            *remaining -= 1;
                            self.st.regs[15] = cur_pc.wrapping_add(4);
                            return;
                        }
                    }
                }
                DecodedOp::ArmBranch {
                    cond,
                    link,
                    target,
                    ret_pc,
                } => {
                    *remaining -= 1;
                    if cond == 0xE || self.cond(cond as u32) {
                        if link {
                            self.st.regs[14] = ret_pc;
                        }
                        self.st.regs[15] = target;
                        if is_idle_loop || (is_poll_loop && poll_load_safe) {
                            *remaining = 0;
                        }
                    } else {
                        self.st.regs[15] = cur_pc.wrapping_add(4);
                    }
                    return;
                }
                DecodedOp::ArmBx {
                    cond,
                    rm,
                    link,
                    ret_pc,
                } => {
                    *remaining -= 1;
                    if cond == 0xE || self.cond(cond as u32) {
                        let v = self.st.regs[rm as usize];
                        if link {
                            self.st.regs[14] = ret_pc;
                        }
                        self.set_flag(T, v & 1 != 0);
                        self.st.regs[15] = v & !1 & if v & 1 != 0 { !0 } else { !3 };
                    } else {
                        self.st.regs[15] = cur_pc.wrapping_add(4);
                    }
                    return;
                }
                DecodedOp::ArmFallback {
                    raw,
                    pc,
                    ends_block,
                } => {
                    self.st.regs[15] = pc;
                    let c = raw >> 28;
                    if c == 0xE {
                        self.exec_arm(raw);
                    } else if c == 0xF {
                        if B::IS_ARM9 {
                            self.exec_arm_uncond(raw);
                        }
                    } else if self.cond(c) {
                        self.exec_arm(raw);
                    }
                    *remaining -= 1;
                    let branched = self.st.regs[15] != pc;
                    if !branched {
                        self.st.regs[15] = pc.wrapping_add(4);
                    }
                    if branched
                        || ends_block
                        || self.thumb()
                        || self.st.halted
                        || self.st.spin > 0
                        || self.st.intr_wait.is_some()
                        || self.bus.code_page_gen(page_idx) != page_gen
                    {
                        return;
                    }
                    cur_pc = pc.wrapping_add(4);
                    continue;
                }
                // ---- Thumb pre-decoded ops ----
                DecodedOp::ThumbShiftImm { opc, rd, rs, imm5 } => {
                    let (r, c) =
                        self.shift(opc as u32, self.st.regs[rs as usize], imm5 as u32, true);
                    self.st.regs[rd as usize] = r;
                    self.set_nz(r);
                    self.set_flag(C, c);
                }
                DecodedOp::ThumbAddSub {
                    opc,
                    rd,
                    rs,
                    rn_or_imm,
                } => {
                    let a = self.st.regs[rs as usize];
                    let v = if (opc & 2) != 0 {
                        rn_or_imm as u32
                    } else {
                        self.st.regs[rn_or_imm as usize]
                    };
                    let r = if (opc & 1) != 0 {
                        self.sub_with_flags(a, v, 1, true)
                    } else {
                        self.add_with_flags(a, v, 0, true)
                    };
                    self.st.regs[rd as usize] = r;
                }
                DecodedOp::ThumbImm8 { opc, rd, imm } => {
                    let imm32 = imm as u32;
                    let a = self.st.regs[rd as usize];
                    match opc {
                        0 => {
                            self.st.regs[rd as usize] = imm32;
                            self.set_nz(imm32);
                        }
                        1 => {
                            self.sub_with_flags(a, imm32, 1, true);
                        }
                        2 => {
                            self.st.regs[rd as usize] = self.add_with_flags(a, imm32, 0, true);
                        }
                        _ => {
                            self.st.regs[rd as usize] = self.sub_with_flags(a, imm32, 1, true);
                        }
                    }
                }
                DecodedOp::ThumbAlu { opc, rd, rs } => {
                    let a = self.st.regs[rd as usize];
                    let b = self.st.regs[rs as usize];
                    match opc {
                        0x0 => {
                            let r = a & b;
                            self.st.regs[rd as usize] = r;
                            self.set_nz(r);
                        }
                        0x1 => {
                            let r = a ^ b;
                            self.st.regs[rd as usize] = r;
                            self.set_nz(r);
                        }
                        0x2 => {
                            let (r, cy) = self.shift(0, a, b & 0xFF, false);
                            self.st.regs[rd as usize] = r;
                            self.set_nz(r);
                            self.set_flag(C, cy);
                        }
                        0x3 => {
                            let (r, cy) = self.shift(1, a, b & 0xFF, false);
                            self.st.regs[rd as usize] = r;
                            self.set_nz(r);
                            self.set_flag(C, cy);
                        }
                        0x4 => {
                            let (r, cy) = self.shift(2, a, b & 0xFF, false);
                            self.st.regs[rd as usize] = r;
                            self.set_nz(r);
                            self.set_flag(C, cy);
                        }
                        0x5 => {
                            let c = self.flag(C) as u32;
                            self.st.regs[rd as usize] = self.add_with_flags(a, b, c, true);
                        }
                        0x6 => {
                            let c = self.flag(C) as u32;
                            self.st.regs[rd as usize] = self.sub_with_flags(a, b, c, true);
                        }
                        0x7 => {
                            let (r, cy) = self.shift(3, a, b & 0xFF, false);
                            self.st.regs[rd as usize] = r;
                            self.set_nz(r);
                            self.set_flag(C, cy);
                        }
                        0x8 => self.set_nz(a & b),
                        0x9 => {
                            self.st.regs[rd as usize] = self.sub_with_flags(0, b, 1, true);
                        }
                        0xA => {
                            self.sub_with_flags(a, b, 1, true);
                        }
                        0xB => {
                            self.add_with_flags(a, b, 0, true);
                        }
                        0xC => {
                            let r = a | b;
                            self.st.regs[rd as usize] = r;
                            self.set_nz(r);
                        }
                        0xD => {
                            let r = a.wrapping_mul(b);
                            self.st.regs[rd as usize] = r;
                            self.set_nz(r);
                        }
                        0xE => {
                            let r = a & !b;
                            self.st.regs[rd as usize] = r;
                            self.set_nz(r);
                        }
                        _ => {
                            let r = !b;
                            self.st.regs[rd as usize] = r;
                            self.set_nz(r);
                        }
                    }
                }
                DecodedOp::ThumbHiReg { opc, rd, rs } => match opc {
                    0 => {
                        self.st.regs[rd as usize] =
                            self.st.regs[rd as usize].wrapping_add(self.st.regs[rs as usize]);
                    }
                    1 => {
                        let a = self.st.regs[rd as usize];
                        let b = self.st.regs[rs as usize];
                        self.sub_with_flags(a, b, 1, true);
                    }
                    _ => {
                        self.st.regs[rd as usize] = self.st.regs[rs as usize];
                    }
                },
                DecodedOp::ThumbLdrPc { rd, addr } => {
                    if poll_load_safe && (addr >> 16) == 0x0410 {
                        poll_load_safe = false;
                    }
                    self.st.regs[rd as usize] = self.bus.read32(addr);
                }
                DecodedOp::ThumbLdrStrImm { kind, rd, rb, off } => {
                    let addr = self.st.regs[rb as usize].wrapping_add(off as u32);
                    match kind {
                        0 => {
                            self.bus.write32(addr, self.st.regs[rd as usize]);
                            if self.bus.code_page_gen(page_idx) != page_gen {
                                *remaining -= 1;
                                self.st.regs[15] = cur_pc.wrapping_add(2);
                                return;
                            }
                        }
                        1 => {
                            self.bus.write8(addr, self.st.regs[rd as usize] as u8);
                            if self.bus.code_page_gen(page_idx) != page_gen {
                                *remaining -= 1;
                                self.st.regs[15] = cur_pc.wrapping_add(2);
                                return;
                            }
                        }
                        2 => {
                            if poll_load_safe && (addr >> 16) == 0x0410 {
                                poll_load_safe = false;
                            }
                            let raw = self.bus.read32(addr);
                            self.st.regs[rd as usize] = if addr & 3 == 0 {
                                raw
                            } else {
                                raw.rotate_right((addr & 3) * 8)
                            };
                        }
                        3 => {
                            if poll_load_safe && (addr >> 16) == 0x0410 {
                                poll_load_safe = false;
                            }
                            self.st.regs[rd as usize] = self.bus.read8(addr) as u32;
                        }
                        4 => {
                            self.bus.write16(addr, self.st.regs[rd as usize] as u16);
                            if self.bus.code_page_gen(page_idx) != page_gen {
                                *remaining -= 1;
                                self.st.regs[15] = cur_pc.wrapping_add(2);
                                return;
                            }
                        }
                        _ => {
                            if poll_load_safe && (addr >> 16) == 0x0410 {
                                poll_load_safe = false;
                            }
                            let raw = self.bus.read16(addr) as u32;
                            self.st.regs[rd as usize] = raw.rotate_right((addr & 1) * 8);
                        }
                    }
                }
                DecodedOp::ThumbAddPcSp {
                    is_sp,
                    rd,
                    val_or_off,
                } => {
                    self.st.regs[rd as usize] = if is_sp {
                        self.st.regs[13].wrapping_add(val_or_off)
                    } else {
                        val_or_off
                    };
                }
                DecodedOp::ThumbCondBranch { cond, target } => {
                    *remaining -= 1;
                    if self.cond(cond as u32) {
                        self.st.regs[15] = target;
                        if is_poll_loop && poll_load_safe {
                            *remaining = 0;
                        }
                    } else {
                        self.st.regs[15] = cur_pc.wrapping_add(2);
                    }
                    return;
                }
                DecodedOp::ThumbBranch { target } => {
                    *remaining -= 1;
                    self.st.regs[15] = target;
                    if is_idle_loop {
                        *remaining = 0;
                    }
                    return;
                }
                DecodedOp::ThumbBlPair {
                    target,
                    ret_lr,
                    to_arm,
                    raw0,
                } => {
                    if *remaining < 2 {
                        self.st.regs[15] = cur_pc;
                        self.exec_thumb(raw0);
                        self.st.regs[15] = cur_pc.wrapping_add(2);
                        *remaining = 0;
                        return;
                    }
                    *remaining -= 2;
                    self.st.regs[14] = ret_lr;
                    if to_arm {
                        self.set_flag(T, false);
                    }
                    self.st.regs[15] = target;
                    return;
                }
                DecodedOp::ThumbFallback {
                    raw,
                    pc,
                    ends_block,
                } => {
                    self.st.regs[15] = pc;
                    self.exec_thumb(raw);
                    *remaining -= 1;
                    let branched = self.st.regs[15] != pc;
                    if !branched {
                        self.st.regs[15] = pc.wrapping_add(2);
                    }
                    if branched
                        || ends_block
                        || !self.thumb()
                        || self.st.halted
                        || self.st.spin > 0
                        || self.st.intr_wait.is_some()
                        || self.bus.code_page_gen(page_idx) != page_gen
                    {
                        return;
                    }
                    cur_pc = pc.wrapping_add(2);
                    continue;
                }
            }

            *remaining -= 1;
            cur_pc = cur_pc.wrapping_add(step_bytes);
        }

        self.st.regs[15] = cur_pc;
    }

    /// Tight cached basic-block execution loop used when no breakpoint/watch/I/O-log hooks are active.
    fn run_slice_fast(&mut self, mut remaining: u32) {
        while remaining > 0 {
            if let Some(mask) = self.st.intr_wait {
                self.check_intr_wait(mask);
            }
            if self.st.halted {
                if !self.bus.irq_pending() {
                    return;
                }
                self.st.halted = false;
                if self.st.cpsr & 0x80 == 0 && self.bus.ime() {
                    self.enter_irq();
                    remaining -= 1;
                    continue;
                }
            } else if self.st.cpsr & 0x80 == 0 && self.bus.irq_takeable() {
                self.enter_irq();
                remaining -= 1;
                continue;
            }

            let pc = self.st.regs[15];
            let is_thumb = self.thumb();
            if let Some((page_idx, page_gen, page_ptr, off)) = self.bus.code_page_info(pc) {
                let slot = block_slot(pc);
                let hit = {
                    let b = &self.block_cache[slot];
                    b.page_idx == page_idx
                        && b.start_pc == pc
                        && b.page_gen == page_gen
                        && ((b.flags & 1) != 0) == is_thumb
                        && b.len > 0
                };
                if !hit {
                    let decoded = if is_thumb {
                        self.decode_thumb_block(pc, page_idx, page_gen, page_ptr, off)
                    } else {
                        self.decode_arm_block(pc, page_idx, page_gen, page_ptr, off)
                    };
                    self.block_cache[slot] = decoded;
                }
                let blk_ptr = &self.block_cache[slot] as *const CachedBlock;
                self.exec_cached_block(blk_ptr, &mut remaining);
            } else {
                // Non-RAM execution (BIOS/IRQ stub): single-step fast path.
                if is_thumb {
                    let op = self.bus.fetch16(pc);
                    self.exec_thumb(op);
                    if self.st.regs[15] == pc {
                        self.st.regs[15] = pc.wrapping_add(2);
                    }
                } else {
                    let op = self.bus.fetch32(pc);
                    let c = op >> 28;
                    if c == 0xE {
                        self.exec_arm(op);
                    } else if c == 0xF {
                        if B::IS_ARM9 {
                            self.exec_arm_uncond(op);
                        }
                    } else if self.cond(c) {
                        self.exec_arm(op);
                    }
                    if self.st.regs[15] == pc {
                        self.st.regs[15] = pc.wrapping_add(4);
                    }
                }
                remaining -= 1;
            }

            if self.st.spin > 0 {
                if self.st.spin >= remaining {
                    self.st.spin -= remaining;
                    return;
                }
                remaining -= self.st.spin;
                self.st.spin = 0;
            }
        }
    }

    #[cfg(test)]
    pub fn run_slice_uncached(&mut self, mut remaining: u32) {
        while remaining > 0 {
            self.step();
            remaining -= 1;
            if self.st.spin > 0 {
                if self.st.spin >= remaining {
                    self.st.spin -= remaining;
                    return;
                }
                remaining -= self.st.spin;
                self.st.spin = 0;
            }
            if self.st.halted && !self.bus.irq_pending() {
                return;
            }
        }
    }

    pub fn step(&mut self) {
        if self.st.spin > 0 {
            self.st.spin -= 1;
            return;
        }
        if let Some(mask) = self.st.intr_wait {
            self.check_intr_wait(mask);
        }
        let pending = self.bus.irq_pending();
        if pending {
            self.st.halted = false;
            if self.bus.ime() && self.st.cpsr & 0x80 == 0 {
                self.enter_irq();
                return;
            }
        }
        if self.st.halted {
            return;
        }
        self.bus.note_pc(self.st.regs[15]);
        if self.brk == Some(self.st.regs[15]) {
            // NDS_BREAKSTACK=1 appends the top of the stack, which is how you
            // recover a call chain: the saved link registers sitting there
            // name the callers that the bare lr cannot.
            let stack = if *BREAKSTACK {
                let sp = self.st.regs[13];
                let mut s = String::from(" stack:");
                for i in 0..10 {
                    s += &format!(" {:08X}", self.bus.read32(sp + i * 4));
                }
                s
            } else {
                String::new()
            };
            eprintln!(
                "BREAK [{}] pc={:#010X} r0={:#010X} r1={:#010X} r2={:#010X} r3={:#010X} lr={:#010X} sp={:#010X}{}",
                if self.st.arm9 { "9" } else { "7" },
                self.st.regs[15], self.st.regs[0], self.st.regs[1], self.st.regs[2], self.st.regs[3],
                self.st.regs[14], self.st.regs[13], stack
            );
        }
        if self.thumb() {
            let op = self.bus.read16(self.st.regs[15]);
            let pc_before = self.st.regs[15];
            self.exec_thumb(op);
            if self.st.regs[15] == pc_before {
                self.st.regs[15] = self.st.regs[15].wrapping_add(2);
            }
        } else {
            let op = self.bus.read32(self.st.regs[15]);
            let pc_before = self.st.regs[15];
            let c = op >> 28;
            if c == 0xE {
                self.exec_arm(op);
            } else if c == 0xF {
                if self.st.arm9 {
                    self.exec_arm_uncond(op);
                }
            } else if self.cond(c) {
                self.exec_arm(op);
            }
            if self.st.regs[15] == pc_before {
                self.st.regs[15] = self.st.regs[15].wrapping_add(4);
            }
        }
    }

    // ===================== ARM =====================

    /// Condition field 0xF (ARMv5): BLX immediate and hint space.
    fn exec_arm_uncond(&mut self, op: u32) {
        if op & 0x0E00_0000 == 0x0A00_0000 {
            // BLX <imm>: branch, link, switch to Thumb; H bit is offset bit 1.
            let off = ((op << 8) as i32 >> 6) as u32;
            let h = (op >> 24 & 1) << 1;
            self.st.regs[14] = self.st.regs[15].wrapping_add(4);
            self.st.regs[15] = self.st.regs[15]
                .wrapping_add(8)
                .wrapping_add(off)
                .wrapping_add(h);
            self.set_flag(T, true);
            self.st.regs[15] &= !1;
        }
        // PLD and other hints: no-op.
    }

    #[inline(always)]
    fn exec_arm(&mut self, op: u32) {
        let lut_idx = (((op >> 16) & 0xFF0) | ((op >> 4) & 0x00F)) as usize;
        match ARM_LUT[lut_idx] {
            OP_DP => self.arm_data_processing(op),
            OP_SINGLE => self.arm_single_transfer(op),
            OP_BLOCK => self.arm_block_transfer(op),
            OP_BRANCH => {
                // B/BL
                let off = ((op << 8) as i32 >> 6) as u32;
                if op & 0x0100_0000 != 0 {
                    self.st.regs[14] = self.st.regs[15].wrapping_add(4);
                }
                self.st.regs[15] = self.st.regs[15].wrapping_add(8).wrapping_add(off);
            }
            OP_HALFWORD => self.arm_halfword(op),
            OP_MUL => {
                // MUL/MLA
                let rd = op >> 16 & 0xF;
                let rn = op >> 12 & 0xF;
                let rs = op >> 8 & 0xF;
                let rm = op & 0xF;
                let mut r = self.r(rm).wrapping_mul(self.r(rs));
                if op & 0x0020_0000 != 0 {
                    r = r.wrapping_add(self.r(rn));
                }
                self.set_r(rd, r);
                if op & 0x0010_0000 != 0 {
                    self.set_nz(r);
                }
            }
            OP_MULL => {
                // UMULL/UMLAL/SMULL/SMLAL
                let rdhi = op >> 16 & 0xF;
                let rdlo = op >> 12 & 0xF;
                let rs = op >> 8 & 0xF;
                let rm = op & 0xF;
                let signed = op & 0x0040_0000 != 0;
                let acc = op & 0x0020_0000 != 0;
                let mut r: u64 = if signed {
                    (self.r(rm) as i32 as i64).wrapping_mul(self.r(rs) as i32 as i64) as u64
                } else {
                    (self.r(rm) as u64).wrapping_mul(self.r(rs) as u64)
                };
                if acc {
                    r = r.wrapping_add((self.r(rdhi) as u64) << 32 | self.r(rdlo) as u64);
                }
                self.set_r(rdlo, r as u32);
                self.set_r(rdhi, (r >> 32) as u32);
                if op & 0x0010_0000 != 0 {
                    self.set_flag(N, r >> 63 != 0);
                    self.set_flag(Z, r == 0);
                }
            }
            OP_SMLAXY => {
                if self.st.arm9 {
                    self.exec_arm_smlaxy(op);
                } else {
                    self.arm_data_processing(op);
                }
            }
            OP_COPROC => self.coprocessor(op),
            OP_LDC_STC => {}
            OP_SWI => self.hle_swi(op >> 16 & 0xFF),
            OP_MISC => self.exec_arm_misc(op),
            _ => {
                if *STRICT {
                    panic!(
                        "unimplemented ARM op {op:#010X} at {:#010X}",
                        self.st.regs[15]
                    );
                }
            }
        }
    }

    fn exec_arm_smlaxy(&mut self, op: u32) {
        // SMLAxy / SMLAWy / SMULWy / SMLALxy / SMULxy
        let opc = op >> 21 & 3;
        let x = op & 0x20 != 0;
        let y = op & 0x40 != 0;
        let rm = self.r(op & 0xF);
        let rs = self.r(op >> 8 & 0xF);
        let half = |v: u32, top: bool| {
            if top {
                (v >> 16) as i16 as i32
            } else {
                v as i16 as i32
            }
        };
        match opc {
            0 => {
                // SMLAxy
                let p = half(rm, x).wrapping_mul(half(rs, y));
                let acc = self.r(op >> 12 & 0xF) as i32;
                let (r, ov) = p.overflowing_add(acc);
                if ov {
                    self.st.cpsr |= Q;
                }
                self.set_r(op >> 16 & 0xF, r as u32);
            }
            1 => {
                // SMULWy / SMLAWy
                let p = ((rm as i32 as i64 * half(rs, y) as i64) >> 16) as i32;
                if x {
                    self.set_r(op >> 16 & 0xF, p as u32); // SMULWy
                } else {
                    let acc = self.r(op >> 12 & 0xF) as i32;
                    let (r, ov) = p.overflowing_add(acc);
                    if ov {
                        self.st.cpsr |= Q;
                    }
                    self.set_r(op >> 16 & 0xF, r as u32);
                }
            }
            2 => {
                // SMLALxy
                let p = half(rm, x).wrapping_mul(half(rs, y)) as i64;
                let rdhi = op >> 16 & 0xF;
                let rdlo = op >> 12 & 0xF;
                let acc = ((self.r(rdhi) as u64) << 32 | self.r(rdlo) as u64) as i64;
                let r = acc.wrapping_add(p) as u64;
                self.set_r(rdlo, r as u32);
                self.set_r(rdhi, (r >> 32) as u32);
            }
            _ => {
                // SMULxy
                let p = half(rm, x).wrapping_mul(half(rs, y));
                self.set_r(op >> 16 & 0xF, p as u32);
            }
        }
    }

    fn exec_arm_misc(&mut self, op: u32) {
        if op & 0x0FFF_FFF0 == 0x012F_FF10 {
            // BX
            let v = self.r(op & 0xF);
            self.set_flag(T, v & 1 != 0);
            self.st.regs[15] = v & !1 & if v & 1 != 0 { !0 } else { !3 };
            return;
        }
        if self.st.arm9 && op & 0x0FFF_FFF0 == 0x012F_FF30 {
            // BLX <reg>
            let v = self.r(op & 0xF);
            self.st.regs[14] = self.st.regs[15].wrapping_add(4);
            self.set_flag(T, v & 1 != 0);
            self.st.regs[15] = v & !1 & if v & 1 != 0 { !0 } else { !3 };
            return;
        }
        if self.st.arm9 && op & 0x0FFF_0FF0 == 0x016F_0F10 {
            // CLZ
            let rm = self.r(op & 0xF);
            self.set_r(op >> 12 & 0xF, rm.leading_zeros());
            return;
        }
        if self.st.arm9 && op & 0x0F90_0FF0 == 0x0100_0050 {
            // QADD/QSUB/QDADD/QDSUB
            let rm = self.r(op & 0xF) as i32;
            let rn = self.r(op >> 16 & 0xF) as i32;
            let opc = op >> 21 & 3;
            let sat = |v: i64, cpu: &mut Self| -> i32 {
                if v > i32::MAX as i64 {
                    cpu.st.cpsr |= Q;
                    i32::MAX
                } else if v < i32::MIN as i64 {
                    cpu.st.cpsr |= Q;
                    i32::MIN
                } else {
                    v as i32
                }
            };
            let doubled = if opc >= 2 {
                sat(rn as i64 * 2, self)
            } else {
                rn
            };
            let r = if opc & 1 == 0 {
                sat(rm as i64 + doubled as i64, self)
            } else {
                sat(rm as i64 - doubled as i64, self)
            };
            self.set_r(op >> 12 & 0xF, r as u32);
            return;
        }
        if self.st.arm9 && op & 0x0F90_0090 == 0x0100_0080 {
            self.exec_arm_smlaxy(op);
            return;
        }
        if op & 0x0FB0_0FF0 == 0x0100_0090 {
            // SWP/SWPB
            let addr = self.r(op >> 16 & 0xF);
            let rm = self.r(op & 0xF);
            let rd = op >> 12 & 0xF;
            if op & 0x0040_0000 != 0 {
                let old = self.bus.read8(addr) as u32;
                self.bus.write8(addr, rm as u8);
                self.set_r(rd, old);
            } else {
                let old = self.bus.read32(addr).rotate_right((addr & 3) * 8);
                self.bus.write32(addr, rm);
                self.set_r(rd, old);
            }
            return;
        }
        if op & 0x0FBF_0FFF == 0x010F_0000 {
            // MRS
            let v = if op & 0x0040_0000 != 0 {
                *self.spsr_for_mode()
            } else {
                self.st.cpsr
            };
            self.set_r(op >> 12 & 0xF, v);
            return;
        }
        if op & 0x0DB0_F000 == 0x0120_F000 {
            // MSR
            let val = if op & 0x0200_0000 != 0 {
                let imm = op & 0xFF;
                imm.rotate_right((op >> 8 & 0xF) * 2)
            } else {
                self.r(op & 0xF)
            };
            let mut mask = 0u32;
            if op & 0x0008_0000 != 0 {
                mask |= 0xFF00_0000;
            }
            if op & 0x0001_0000 != 0 {
                mask |= 0x0000_00FF;
            }
            if op & 0x0040_0000 != 0 {
                let s = self.spsr_for_mode();
                *s = (*s & !mask) | (val & mask);
            } else {
                if self.mode() == 0x10 {
                    mask &= 0xFF00_0000;
                }
                let old = self.mode();
                self.st.cpsr = (self.st.cpsr & !mask) | (val & mask);
                self.switch_mode(old);
            }
            return;
        }
        self.arm_data_processing(op);
    }

    /// CP15 (system control) on the ARM9; everything else ignored.
    fn coprocessor(&mut self, op: u32) {
        let cp = op >> 8 & 0xF;
        let load = op & 0x0010_0000 != 0; // MRC
        let rd = op >> 12 & 0xF;
        if cp != 15 || !self.st.arm9 {
            if load {
                self.set_r(rd, 0);
            }
            return;
        }
        let crn = op >> 16 & 0xF;
        let crm = op & 0xF;
        let op2 = op >> 5 & 7;
        if load {
            let v = match (crn, crm, op2) {
                (0, 0, 0) => 0x4105_9461, // main ID: ARM946E-S
                (0, 0, 1) => 0x0F0D_2112, // cache type
                (1, 0, 0) => self.st.cp15_control,
                (9, 1, 0) => self.st.cp15_dtcm,
                (9, 1, 1) => self.st.cp15_itcm,
                _ => 0,
            };
            self.set_r(rd, v);
        } else {
            let v = self.r(rd);
            match (crn, crm, op2) {
                (1, 0, 0) => self.st.cp15_control = v,
                (9, 1, 0) => {
                    self.st.cp15_dtcm = v;
                    self.bus.set_dtcm(v & 0xFFFF_F000);
                }
                (9, 1, 1) => self.st.cp15_itcm = v,
                (7, 0, 4) | (7, 8, 2) => self.st.halted = true, // wait for interrupt
                _ => {}                                         // cache/TLB maintenance: no-op
            }
        }
    }

    #[inline(always)]
    fn dp_operand2(&mut self, op: u32) -> (u32, bool) {
        if op & 0x0200_0000 != 0 {
            let imm = op & 0xFF;
            let rot = (op >> 8 & 0xF) * 2;
            if rot == 0 {
                (imm, self.flag(C))
            } else {
                let v = imm.rotate_right(rot);
                (v, v >> 31 != 0)
            }
        } else {
            let rm = op & 0xF;
            if op & 0xFF0 == 0 {
                // Plain unshifted register (LSL #0): most frequent register operand.
                return (self.r(rm), self.flag(C));
            }
            let ty = op >> 5 & 3;
            if op & 0x10 != 0 {
                let amount = self.r(op >> 8 & 0xF) & 0xFF;
                let val = if rm == 15 {
                    self.r(15).wrapping_add(4)
                } else {
                    self.r(rm)
                };
                self.shift(ty, val, amount, false)
            } else {
                let amount = op >> 7 & 0x1F;
                self.shift(ty, self.r(rm), amount, true)
            }
        }
    }

    #[inline(always)]
    fn arm_data_processing(&mut self, op: u32) {
        let opcode = op >> 21 & 0xF;
        let set = op & 0x0010_0000 != 0;
        let rn = op >> 16 & 0xF;
        let rd = op >> 12 & 0xF;
        if rd == 15 && (0x8..=0xB).contains(&opcode) {
            if set {
                let old = self.mode();
                self.st.cpsr = *self.spsr_for_mode();
                self.switch_mode(old);
            }
            return;
        }
        let (op2, sh_carry) = self.dp_operand2(op);
        let a = if rn == 15 && op & 0x0200_0000 == 0 && op & 0x10 != 0 {
            self.r(15).wrapping_add(4)
        } else {
            self.r(rn)
        };
        let logical_flags = |cpu: &mut Self, r: u32| {
            let mut cpsr = cpu.st.cpsr & !(N | Z | C);
            cpsr |= r & N;
            if r == 0 {
                cpsr |= Z;
            }
            if sh_carry {
                cpsr |= C;
            }
            cpu.st.cpsr = cpsr;
        };
        let c = self.flag(C) as u32;
        let result = match opcode {
            0x0 => {
                let r = a & op2;
                if set {
                    logical_flags(self, r)
                }
                Some(r)
            } // AND
            0x1 => {
                let r = a ^ op2;
                if set {
                    logical_flags(self, r)
                }
                Some(r)
            } // EOR
            0x2 => Some(self.sub_with_flags(a, op2, 1, set)), // SUB
            0x3 => Some(self.sub_with_flags(op2, a, 1, set)), // RSB
            0x4 => Some(self.add_with_flags(a, op2, 0, set)), // ADD
            0x5 => Some(self.add_with_flags(a, op2, c, set)), // ADC
            0x6 => Some(self.sub_with_flags(a, op2, c, set)), // SBC
            0x7 => Some(self.sub_with_flags(op2, a, c, set)), // RSC
            0x8 => {
                let r = a & op2;
                logical_flags(self, r);
                None
            } // TST
            0x9 => {
                let r = a ^ op2;
                logical_flags(self, r);
                None
            } // TEQ
            0xA => {
                self.sub_with_flags(a, op2, 1, true);
                None
            } // CMP
            0xB => {
                self.add_with_flags(a, op2, 0, true);
                None
            } // CMN
            0xC => {
                let r = a | op2;
                if set {
                    logical_flags(self, r)
                }
                Some(r)
            } // ORR
            0xD => {
                let r = op2;
                if set {
                    logical_flags(self, r)
                }
                Some(r)
            } // MOV
            0xE => {
                let r = a & !op2;
                if set {
                    logical_flags(self, r)
                }
                Some(r)
            } // BIC
            _ => {
                let r = !op2;
                if set {
                    logical_flags(self, r)
                }
                Some(r)
            } // MVN
        };
        if let Some(r) = result {
            if rd == 15 {
                if set {
                    let old = self.mode();
                    self.st.cpsr = *self.spsr_for_mode();
                    self.switch_mode(old);
                }
                self.st.regs[15] = r & if self.thumb() { !1 } else { !3 };
            } else {
                self.st.regs[rd as usize] = r;
            }
        }
    }

    #[inline(always)]
    fn arm_single_transfer(&mut self, op: u32) {
        let rn = op >> 16 & 0xF;
        let rd = op >> 12 & 0xF;
        let offset = if op & 0x0200_0000 != 0 {
            let (v, _) = self.shift(op >> 5 & 3, self.r(op & 0xF), op >> 7 & 0x1F, true);
            v
        } else {
            op & 0xFFF
        };
        let base = self.r(rn);
        let up = op & 0x0080_0000 != 0;
        let pre = op & 0x0100_0000 != 0;
        let byte = op & 0x0040_0000 != 0;
        let load = op & 0x0010_0000 != 0;
        let wb = op & 0x0020_0000 != 0;
        let off_base = if up {
            base.wrapping_add(offset)
        } else {
            base.wrapping_sub(offset)
        };
        let addr = if pre { off_base } else { base };
        if load {
            let v = if byte {
                self.bus.read8(addr) as u32
            } else {
                let raw = self.bus.read32(addr);
                if addr & 3 == 0 {
                    raw
                } else {
                    raw.rotate_right((addr & 3) * 8)
                }
            };
            if !pre || wb {
                self.set_r(rn, off_base);
            }
            if rd == 15 && self.st.arm9 {
                // v5: LDR to PC switches to Thumb on bit 0.
                self.set_flag(T, v & 1 != 0);
                self.st.regs[15] = v & !1 & if v & 1 != 0 { !0 } else { !3 };
            } else {
                self.set_r(rd, v);
            }
        } else {
            let v = if rd == 15 {
                self.r(15).wrapping_add(4)
            } else {
                self.st.regs[rd as usize]
            };
            if byte {
                self.bus.write8(addr, v as u8);
            } else {
                self.bus.write32(addr, v);
            }
            if !pre || wb {
                self.set_r(rn, off_base);
            }
        }
    }

    #[inline(always)]
    fn arm_halfword(&mut self, op: u32) {
        let rn = op >> 16 & 0xF;
        let rd = op >> 12 & 0xF;
        let offset = if op & 0x0040_0000 != 0 {
            (op >> 4 & 0xF0) | (op & 0xF)
        } else {
            self.r(op & 0xF)
        };
        let base = self.r(rn);
        let up = op & 0x0080_0000 != 0;
        let pre = op & 0x0100_0000 != 0;
        let load = op & 0x0010_0000 != 0;
        let wb = op & 0x0020_0000 != 0;
        let off_base = if up {
            base.wrapping_add(offset)
        } else {
            base.wrapping_sub(offset)
        };
        let addr = if pre { off_base } else { base };
        let ty = op >> 5 & 3;
        if !load && ty >= 2 && self.st.arm9 {
            // v5TE LDRD/STRD (encoded in the store space).
            let rd2 = rd + 1;
            if ty == 2 {
                // LDRD
                let lo = self.bus.read32(addr & !3);
                let hi = self.bus.read32((addr & !3).wrapping_add(4));
                if !pre || wb {
                    self.set_r(rn, off_base);
                }
                self.set_r(rd, lo);
                self.set_r(rd2, hi);
            } else {
                // STRD
                self.bus.write32(addr & !3, self.r(rd));
                self.bus.write32((addr & !3).wrapping_add(4), self.r(rd2));
                if !pre || wb {
                    self.set_r(rn, off_base);
                }
            }
            return;
        }
        if load {
            let v = match ty {
                1 => {
                    let v = self.bus.read16(addr) as u32;
                    if self.st.arm9 {
                        v
                    } else {
                        v.rotate_right((addr & 1) * 8)
                    }
                }
                2 => self.bus.read8(addr) as i8 as i32 as u32, // LDRSB
                _ => {
                    if addr & 1 != 0 && !self.st.arm9 {
                        self.bus.read8(addr) as i8 as i32 as u32
                    } else {
                        self.bus.read16(addr) as i16 as i32 as u32
                    }
                }
            };
            if !pre || wb {
                self.set_r(rn, off_base);
            }
            self.set_r(rd, v);
        } else {
            let v = self.r(rd);
            self.bus.write16(addr, v as u16);
            if !pre || wb {
                self.set_r(rn, off_base);
            }
        }
    }

    #[inline(always)]
    fn arm_block_transfer(&mut self, op: u32) {
        let rn = op >> 16 & 0xF;
        let list = op & 0xFFFF;
        let load = op & 0x0010_0000 != 0;
        let wb = op & 0x0020_0000 != 0;
        let s_bit = op & 0x0040_0000 != 0;
        let up = op & 0x0080_0000 != 0;
        let pre = op & 0x0100_0000 != 0;
        let n = list.count_ones();
        let base = self.r(rn);

        if list == 0 {
            let addr = if up {
                if pre {
                    base + 4
                } else {
                    base
                }
            } else if pre {
                base - 0x40
            } else {
                base - 0x3C
            };
            if load {
                self.st.regs[15] = self.bus.read32(addr) & !3;
            } else {
                self.bus.write32(addr, self.r(15).wrapping_add(4));
            }
            if wb {
                self.set_r(rn, if up { base + 0x40 } else { base - 0x40 });
            }
            return;
        }

        let start = if up {
            if pre {
                base.wrapping_add(4)
            } else {
                base
            }
        } else {
            let s = base.wrapping_sub(n * 4);
            if pre {
                s
            } else {
                s.wrapping_add(4)
            }
        };
        let new_base = if up {
            base.wrapping_add(n * 4)
        } else {
            base.wrapping_sub(n * 4)
        };
        let user_bank = s_bit && !(load && list & 0x8000 != 0);

        let mut addr = start;
        let first_reg = list.trailing_zeros();
        let in_list = list & (1 << rn) != 0;
        let last_reg = 31 - list.leading_zeros();
        if load {
            if wb {
                self.set_r(rn, new_base);
            }
            // v5 (ARM9): writeback beats the loaded value unless Rn is the
            // last (and not only) register in the list. Applied after loads.
            let v5_wb_wins = self.st.arm9 && wb && in_list && (n == 1 || rn != last_reg);
            let mut rem = list;
            while rem != 0 {
                let i = rem.trailing_zeros();
                rem &= rem - 1;
                let v = self.bus.read32(addr);
                if user_bank && i < 15 {
                    self.set_user_reg(i, v);
                    addr = addr.wrapping_add(4);
                    continue;
                }
                if i == 15 {
                    if s_bit {
                        let old = self.mode();
                        self.st.cpsr = *self.spsr_for_mode();
                        self.switch_mode(old);
                    }
                    if self.st.arm9 && !s_bit {
                        self.set_flag(T, v & 1 != 0);
                    }
                    self.st.regs[15] = v & if self.thumb() { !1 } else { !3 };
                } else {
                    self.st.regs[i as usize] = v;
                }
                addr = addr.wrapping_add(4);
            }
            if v5_wb_wins {
                self.set_r(rn, new_base);
            }
        } else {
            let mut rem = list;
            while rem != 0 {
                let i = rem.trailing_zeros();
                rem &= rem - 1;
                let v = if i == 15 {
                    self.r(15).wrapping_add(4)
                } else if i == rn && i != first_reg && wb {
                    // v4 stores the written-back base; v5 the old one.
                    if self.st.arm9 {
                        base
                    } else {
                        new_base
                    }
                } else if user_bank {
                    self.user_reg(i)
                } else {
                    self.st.regs[i as usize]
                };
                self.bus.write32(addr, v);
                addr = addr.wrapping_add(4);
            }
            if wb {
                self.set_r(rn, new_base);
            }
        }
    }

    /// High-level emulation of NDS BIOS calls (no BIOS images needed).
    /// SWI numbers follow the NDS BIOS table, which differs from GBA.
    fn hle_swi(&mut self, n: u32) {
        if *SWILOG {
            eprintln!(
                "[{}] swi {:#04X} r0={:#010X} r1={:#010X} r2={:#010X} lr={:#010X}",
                if self.st.arm9 { "9" } else { "7" },
                n,
                self.st.regs[0],
                self.st.regs[1],
                self.st.regs[2],
                self.st.regs[14]
            );
        }
        match n {
            0x03 => {
                // WaitByLoop: r0 = loop count (4 cycles each); idle for it
                self.st.spin = self.st.regs[0].min(1_000_000);
                self.st.regs[0] = 0;
            }
            0x04 => {
                // IntrWait(discard_old, mask)
                let mask = self.st.regs[1];
                let flag_addr = self.bus.bios_flag_addr();
                if self.st.regs[0] != 0 {
                    let flags = self.bus.read32(flag_addr);
                    self.bus.write32(flag_addr, flags & !mask);
                }
                self.st.intr_wait = Some(mask);
                self.bus.set_ime(true);
            }
            0x05 => {
                // VBlankIntrWait = IntrWait(1, vblank)
                let flag_addr = self.bus.bios_flag_addr();
                let flags = self.bus.read32(flag_addr);
                self.bus.write32(flag_addr, flags & !1);
                self.st.intr_wait = Some(1);
                self.bus.set_ime(true);
            }
            0x06 | 0x07 => self.st.halted = true, // Halt / Sleep(ARM7)
            0x09 => {
                // Div: r0/r1 -> r0=quot, r1=rem, r3=|quot|
                let num = self.st.regs[0] as i32;
                let den = self.st.regs[1] as i32;
                if den != 0 {
                    let q = num.wrapping_div(den);
                    self.st.regs[0] = q as u32;
                    self.st.regs[1] = num.wrapping_rem(den) as u32;
                    self.st.regs[3] = q.unsigned_abs();
                }
            }
            0x0B => {
                // CpuSet
                let src = self.st.regs[0];
                let dst = self.st.regs[1];
                let cnt = self.st.regs[2];
                let count = cnt & 0x1F_FFFF;
                let fill = cnt & 0x0100_0000 != 0;
                if cnt & 0x0400_0000 != 0 {
                    let v0 = self.bus.read32(src);
                    for i in 0..count {
                        let v = if fill {
                            v0
                        } else {
                            self.bus.read32(src + i * 4)
                        };
                        self.bus.write32(dst + i * 4, v);
                    }
                } else {
                    let v0 = self.bus.read16(src);
                    for i in 0..count {
                        let v = if fill {
                            v0
                        } else {
                            self.bus.read16(src + i * 2)
                        };
                        self.bus.write16(dst + i * 2, v);
                    }
                }
            }
            0x0C => {
                // CpuFastSet
                let src = self.st.regs[0];
                let dst = self.st.regs[1];
                let cnt = self.st.regs[2];
                let count = ((cnt & 0x1F_FFFF) + 7) & !7;
                let fill = cnt & 0x0100_0000 != 0;
                let v0 = self.bus.read32(src);
                for i in 0..count {
                    let v = if fill {
                        v0
                    } else {
                        self.bus.read32(src + i * 4)
                    };
                    self.bus.write32(dst + i * 4, v);
                }
            }
            0x0D => self.st.regs[0] = (self.st.regs[0] as f64).sqrt() as u32, // Sqrt
            0x0E => {
                // GetCRC16(initial, addr, len)
                let mut crc = self.st.regs[0] as u16;
                let addr = self.st.regs[1];
                let len = self.st.regs[2];
                for i in 0..len {
                    crc ^= self.bus.read8(addr + i) as u16;
                    for _ in 0..8 {
                        crc = if crc & 1 != 0 {
                            (crc >> 1) ^ 0xA001
                        } else {
                            crc >> 1
                        };
                    }
                }
                self.st.regs[0] = crc as u32;
            }
            0x11 | 0x12 => {
                // LZ77UnComp (Wram / Vram-callback form treated as flat)
                let mut src = self.st.regs[0];
                let dst = self.st.regs[1];
                let header = self.bus.read32(src);
                let size = header >> 8;
                src += 4;
                let mut written = 0u32;
                while written < size {
                    let flags = self.bus.read8(src);
                    src += 1;
                    for bit in (0..8).rev() {
                        if written >= size {
                            break;
                        }
                        if flags >> bit & 1 == 0 {
                            let b = self.bus.read8(src);
                            src += 1;
                            self.bus.write8(dst + written, b);
                            written += 1;
                        } else {
                            let b0 = self.bus.read8(src) as u32;
                            let b1 = self.bus.read8(src + 1) as u32;
                            src += 2;
                            let len = (b0 >> 4) + 3;
                            let disp = ((b0 & 0xF) << 8 | b1) + 1;
                            for _ in 0..len {
                                if written >= size {
                                    break;
                                }
                                let b = self.bus.read8(dst + written - disp);
                                self.bus.write8(dst + written, b);
                                written += 1;
                            }
                        }
                    }
                }
            }
            0x14 | 0x15 => {
                // RLUnComp
                let mut src = self.st.regs[0];
                let dst = self.st.regs[1];
                let size = self.bus.read32(src) >> 8;
                src += 4;
                let mut written = 0u32;
                while written < size {
                    let flag = self.bus.read8(src);
                    src += 1;
                    if flag & 0x80 != 0 {
                        let len = (flag as u32 & 0x7F) + 3;
                        let b = self.bus.read8(src);
                        src += 1;
                        for _ in 0..len.min(size - written) {
                            self.bus.write8(dst + written, b);
                            written += 1;
                        }
                    } else {
                        let len = (flag as u32 & 0x7F) + 1;
                        for _ in 0..len.min(size - written) {
                            let b = self.bus.read8(src);
                            src += 1;
                            self.bus.write8(dst + written, b);
                            written += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ===================== Thumb =====================

    #[inline(always)]
    fn exec_thumb(&mut self, op: u16) {
        let op = op as u32;
        match op >> 13 {
            0b000 => {
                if op >> 11 & 3 == 3 {
                    let rd = op & 7;
                    let rs = op >> 3 & 7;
                    let v = if op & 0x400 != 0 {
                        op >> 6 & 7
                    } else {
                        self.r(op >> 6 & 7)
                    };
                    let a = self.r(rs);
                    let r = if op & 0x200 != 0 {
                        self.sub_with_flags(a, v, 1, true)
                    } else {
                        self.add_with_flags(a, v, 0, true)
                    };
                    self.set_r(rd, r);
                } else {
                    let rd = op & 7;
                    let rs = op >> 3 & 7;
                    let amount = op >> 6 & 0x1F;
                    let (r, c) = self.shift(op >> 11 & 3, self.r(rs), amount, true);
                    self.set_r(rd, r);
                    self.set_nz(r);
                    self.set_flag(C, c);
                }
            }
            0b001 => {
                let rd = op >> 8 & 7;
                let imm = op & 0xFF;
                let a = self.r(rd);
                match op >> 11 & 3 {
                    0 => {
                        self.set_r(rd, imm);
                        self.set_nz(imm);
                    }
                    1 => {
                        self.sub_with_flags(a, imm, 1, true);
                    }
                    2 => {
                        let r = self.add_with_flags(a, imm, 0, true);
                        self.set_r(rd, r);
                    }
                    _ => {
                        let r = self.sub_with_flags(a, imm, 1, true);
                        self.set_r(rd, r);
                    }
                }
            }
            0b010 => self.thumb_group_010(op),
            0b011 => {
                let rd = op & 7;
                let base = self.r(op >> 3 & 7);
                let imm = op >> 6 & 0x1F;
                let byte = op & 0x1000 != 0;
                let load = op & 0x0800 != 0;
                let addr = base.wrapping_add(if byte { imm } else { imm << 2 });
                match (load, byte) {
                    (false, false) => self.bus.write32(addr, self.r(rd)),
                    (false, true) => self.bus.write8(addr, self.r(rd) as u8),
                    (true, false) => {
                        let v = self.bus.read32(addr).rotate_right((addr & 3) * 8);
                        self.set_r(rd, v);
                    }
                    (true, true) => {
                        let v = self.bus.read8(addr) as u32;
                        self.set_r(rd, v);
                    }
                }
            }
            0b100 => {
                if op & 0x1000 == 0 {
                    let rd = op & 7;
                    let addr = self.r(op >> 3 & 7).wrapping_add((op >> 6 & 0x1F) << 1);
                    if op & 0x0800 != 0 {
                        let v = (self.bus.read16(addr) as u32).rotate_right((addr & 1) * 8);
                        self.set_r(rd, v);
                    } else {
                        self.bus.write16(addr, self.r(rd) as u16);
                    }
                } else {
                    let rd = op >> 8 & 7;
                    let addr = self.r(13).wrapping_add((op & 0xFF) << 2);
                    if op & 0x0800 != 0 {
                        let v = self.bus.read32(addr).rotate_right((addr & 3) * 8);
                        self.set_r(rd, v);
                    } else {
                        self.bus.write32(addr, self.r(rd));
                    }
                }
            }
            0b101 => {
                if op & 0x1000 == 0 {
                    let rd = op >> 8 & 7;
                    let imm = (op & 0xFF) << 2;
                    let base = if op & 0x0800 != 0 {
                        self.r(13)
                    } else {
                        self.r(15) & !3
                    };
                    self.set_r(rd, base.wrapping_add(imm));
                } else if op & 0x0F00 == 0 {
                    let imm = (op & 0x7F) << 2;
                    let sp = self.r(13);
                    self.set_r(
                        13,
                        if op & 0x80 != 0 {
                            sp.wrapping_sub(imm)
                        } else {
                            sp.wrapping_add(imm)
                        },
                    );
                } else if op & 0x0600 == 0x0400 {
                    let load = op & 0x0800 != 0;
                    let r_bit = op & 0x0100 != 0;
                    let list = op & 0xFF;
                    let n = list.count_ones() + r_bit as u32;
                    if load {
                        let mut addr = self.r(13);
                        let mut rem = list;
                        while rem != 0 {
                            let i = rem.trailing_zeros();
                            rem &= rem - 1;
                            self.st.regs[i as usize] = self.bus.read32(addr);
                            addr = addr.wrapping_add(4);
                        }
                        if r_bit {
                            let v = self.bus.read32(addr);
                            if self.st.arm9 {
                                // v5: POP {pc} honors bit 0 for interworking.
                                self.set_flag(T, v & 1 != 0);
                            }
                            self.st.regs[15] = v & !1;
                            addr = addr.wrapping_add(4);
                        }
                        self.set_r(13, addr);
                    } else {
                        let base = self.r(13).wrapping_sub(n * 4);
                        let mut addr = base;
                        let mut rem = list;
                        while rem != 0 {
                            let i = rem.trailing_zeros();
                            rem &= rem - 1;
                            let v = self.st.regs[i as usize];
                            self.bus.write32(addr, v);
                            addr = addr.wrapping_add(4);
                        }
                        if r_bit {
                            self.bus.write32(addr, self.st.regs[14]);
                        }
                        self.set_r(13, base);
                    }
                } else if *STRICT {
                    panic!(
                        "unimplemented Thumb op {op:#06X} at {:#010X}",
                        self.st.regs[15]
                    );
                }
            }
            0b110 => {
                if op & 0x1000 == 0 {
                    let rb = op >> 8 & 7;
                    let list = op & 0xFF;
                    let mut addr = self.r(rb);
                    let load = op & 0x0800 != 0;
                    if list == 0 {
                        if load {
                            self.st.regs[15] = self.bus.read32(addr) & !1;
                        } else {
                            self.bus.write32(addr, self.r(15).wrapping_add(2));
                        }
                        self.set_r(rb, addr.wrapping_add(0x40));
                        return;
                    }
                    let first = list.trailing_zeros();
                    let new_base = addr.wrapping_add(list.count_ones() * 4);
                    let mut rem = list;
                    while rem != 0 {
                        let i = rem.trailing_zeros();
                        rem &= rem - 1;
                        if load {
                            self.st.regs[i as usize] = self.bus.read32(addr);
                        } else {
                            let v = if i == rb && i != first {
                                new_base
                            } else {
                                self.st.regs[i as usize]
                            };
                            self.bus.write32(addr, v);
                        }
                        addr = addr.wrapping_add(4);
                    }
                    if !(load && list & (1 << rb) != 0) {
                        self.set_r(rb, addr);
                    }
                } else {
                    let cond = op >> 8 & 0xF;
                    if cond == 0xF {
                        self.hle_swi(op & 0xFF);
                    } else if self.cond(cond) {
                        let off = ((op & 0xFF) as i8 as i32) << 1;
                        self.st.regs[15] = self.r(15).wrapping_add(off as u32);
                    }
                }
            }
            _ => {
                if op & 0x1800 == 0x0000 {
                    // B unconditional
                    let off = ((op << 21) as i32 >> 20) as u32;
                    self.st.regs[15] = self.r(15).wrapping_add(off);
                } else if op & 0x1800 == 0x1000 {
                    // BL/BLX prefix: LR = PC + offset<<12
                    let off = ((op << 21) as i32 >> 9) as u32;
                    self.st.regs[14] = self.r(15).wrapping_add(off);
                } else if op & 0x1800 == 0x1800 {
                    // BL suffix
                    let lr = self.st.regs[14].wrapping_add((op & 0x7FF) << 1);
                    self.st.regs[14] = self.st.regs[15].wrapping_add(2) | 1;
                    self.st.regs[15] = lr & !1;
                } else {
                    // 0x0800: BLX suffix (v5): like BL but switch to ARM.
                    if self.st.arm9 {
                        let lr = self.st.regs[14].wrapping_add((op & 0x7FF) << 1);
                        self.st.regs[14] = self.st.regs[15].wrapping_add(2) | 1;
                        self.st.regs[15] = lr & !3;
                        self.set_flag(T, false);
                    } else if *STRICT {
                        panic!(
                            "unimplemented Thumb op {op:#06X} at {:#010X}",
                            self.st.regs[15]
                        );
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn thumb_group_010(&mut self, op: u32) {
        if op & 0x1C00 == 0x0000 {
            let rd = op & 7;
            let rs = op >> 3 & 7;
            let a = self.r(rd);
            let b = self.r(rs);
            let c = self.flag(C) as u32;
            match op >> 6 & 0xF {
                0x0 => {
                    let r = a & b;
                    self.set_r(rd, r);
                    self.set_nz(r);
                }
                0x1 => {
                    let r = a ^ b;
                    self.set_r(rd, r);
                    self.set_nz(r);
                }
                0x2 => {
                    let (r, cy) = self.shift(0, a, b & 0xFF, false);
                    self.set_r(rd, r);
                    self.set_nz(r);
                    self.set_flag(C, cy);
                }
                0x3 => {
                    let (r, cy) = self.shift(1, a, b & 0xFF, false);
                    self.set_r(rd, r);
                    self.set_nz(r);
                    self.set_flag(C, cy);
                }
                0x4 => {
                    let (r, cy) = self.shift(2, a, b & 0xFF, false);
                    self.set_r(rd, r);
                    self.set_nz(r);
                    self.set_flag(C, cy);
                }
                0x5 => {
                    let r = self.add_with_flags(a, b, c, true);
                    self.set_r(rd, r);
                }
                0x6 => {
                    let r = self.sub_with_flags(a, b, c, true);
                    self.set_r(rd, r);
                }
                0x7 => {
                    let (r, cy) = self.shift(3, a, b & 0xFF, false);
                    self.set_r(rd, r);
                    self.set_nz(r);
                    self.set_flag(C, cy);
                }
                0x8 => {
                    let r = a & b;
                    self.set_nz(r);
                }
                0x9 => {
                    let r = self.sub_with_flags(0, b, 1, true);
                    self.set_r(rd, r);
                }
                0xA => {
                    self.sub_with_flags(a, b, 1, true);
                }
                0xB => {
                    self.add_with_flags(a, b, 0, true);
                }
                0xC => {
                    let r = a | b;
                    self.set_r(rd, r);
                    self.set_nz(r);
                }
                0xD => {
                    let r = a.wrapping_mul(b);
                    self.set_r(rd, r);
                    self.set_nz(r);
                }
                0xE => {
                    let r = a & !b;
                    self.set_r(rd, r);
                    self.set_nz(r);
                }
                _ => {
                    let r = !b;
                    self.set_r(rd, r);
                    self.set_nz(r);
                }
            }
        } else if op & 0x1C00 == 0x0400 {
            let rd = (op & 7) | (op >> 4 & 8);
            let rs = op >> 3 & 0xF;
            match op >> 8 & 3 {
                0 => {
                    let r = self.r(rd).wrapping_add(self.r(rs));
                    if rd == 15 {
                        self.st.regs[15] = r & !1;
                    } else {
                        self.set_r(rd, r);
                    }
                }
                1 => {
                    let a = self.r(rd);
                    let b = self.r(rs);
                    self.sub_with_flags(a, b, 1, true);
                }
                2 => {
                    let v = self.r(rs);
                    if rd == 15 {
                        self.st.regs[15] = v & !1;
                    } else {
                        self.set_r(rd, v);
                    }
                }
                _ => {
                    // BX / BLX (v5 when bit 7 set)
                    let v = self.r(rs);
                    if self.st.arm9 && op & 0x80 != 0 {
                        self.st.regs[14] = self.st.regs[15].wrapping_add(2) | 1;
                    }
                    self.set_flag(T, v & 1 != 0);
                    self.st.regs[15] = v & !1;
                    if v & 1 == 0 {
                        self.st.regs[15] &= !3;
                    }
                }
            }
        } else if op & 0x1800 == 0x0800 {
            let rd = op >> 8 & 7;
            let addr = (self.r(15) & !3).wrapping_add((op & 0xFF) << 2);
            let v = self.bus.read32(addr);
            self.set_r(rd, v);
        } else {
            let rd = op & 7;
            let addr = self.r(op >> 3 & 7).wrapping_add(self.r(op >> 6 & 7));
            if op & 0x0200 != 0 {
                match op >> 10 & 3 {
                    0 => self.bus.write16(addr, self.r(rd) as u16),
                    1 => {
                        let v = self.bus.read8(addr) as i8 as i32 as u32;
                        self.set_r(rd, v);
                    }
                    2 => {
                        let v = (self.bus.read16(addr) as u32).rotate_right((addr & 1) * 8);
                        self.set_r(rd, v);
                    }
                    _ => {
                        let v = if addr & 1 != 0 && !self.st.arm9 {
                            self.bus.read8(addr) as i8 as i32 as u32
                        } else {
                            self.bus.read16(addr) as i16 as i32 as u32
                        };
                        self.set_r(rd, v);
                    }
                }
            } else {
                match op >> 10 & 3 {
                    0 => self.bus.write32(addr, self.r(rd)),
                    1 => self.bus.write8(addr, self.r(rd) as u8),
                    2 => {
                        let v = self.bus.read32(addr).rotate_right((addr & 3) * 8);
                        self.set_r(rd, v);
                    }
                    _ => {
                        let v = self.bus.read8(addr) as u32;
                        self.set_r(rd, v);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::bus::{Machine, MachineHandle, View};
    use super::*;

    fn write_u32(m: &mut Machine, addr: u32, val: u32) {
        let off = (addr & 0x3F_FFFF) as usize;
        m.main_ram[off..off + 4].copy_from_slice(&val.to_le_bytes());
    }

    fn write_u16(m: &mut Machine, addr: u32, val: u16) {
        let off = (addr & 0x3F_FFFF) as usize;
        m.main_ram[off..off + 2].copy_from_slice(&val.to_le_bytes());
    }

    #[test]
    fn cached_interpreter_matches_uncached_across_arm_and_thumb_programs() {
        let setup = || -> (MachineHandle, Cpu<View<0>>) {
            let mut mh = MachineHandle::new(Machine::new());
            {
                let mut m = mh.borrow_mut();
                // ARM program at 0x0200_0000:
                //   00: movs r0, #0x80, ror #2  -> 0x2000_0000 (sets C=0, N=0, Z=0)
                //   04: mov  r1, #0x10          -> 0x10
                //   08: add  r2, r1, #0x25      -> 0x35
                //   0C: sub  r3, r2, r1, lsl #1 -> 0x15
                //   10: orrs r4, r3, #0x80000000 -> sets N=1, C=1
                //   14: ldr  r5, [pc, #0x24]    -> loads literal at 0x40 (0x0200_1000)
                //   18: str  r4, [r5, #4]!      -> writes r4 to 0x0200_1004, r5 = 0x0200_1004
                //   1C: ldr  r6, [r5, #0]       -> loads 0x8000_0015
                //   20: ldr  r7, [pc, #0x1C]    -> loads Thumb entry 0x0200_0101 from 0x44
                //   24: bx   r7                 -> switch to Thumb at 0x0200_0100
                //   28: add  r8, r0, r6         -> reached after Thumb returns
                //   2C: b    .                  -> idle loop
                let arm_code: [u32; 12] = [
                    0xE3B0_0180, // movs r0, #0x80, ror #2
                    0xE3A0_1010, // mov r1, #0x10
                    0xE281_2025, // add r2, r1, #0x25
                    0xE042_3081, // sub r3, r2, r1, lsl #1
                    0xE393_4102, // orrs r4, r3, #0x8000_0000 (rot=2 of 2 -> 0x8000_0000, C=1)
                    0xE59F_5024, // ldr r5, [pc, #0x24] -> 0x14+8+0x24 = 0x40
                    0xE5A5_4004, // str r4, [r5, #4]!
                    0xE595_6000, // ldr r6, [r5]
                    0xE59F_701C, // ldr r7, [pc, #0x1C] -> 0x20+8+0x1C = 0x44
                    0xE12F_FF17, // bx r7
                    0xE080_8006, // add r8, r0, r6
                    0xEAFF_FFFE, // b .
                ];
                for (i, &w) in arm_code.iter().enumerate() {
                    write_u32(&mut m, 0x0200_0000 + (i as u32) * 4, w);
                }
                write_u32(&mut m, 0x0200_0040, 0x0200_1000);
                write_u32(&mut m, 0x0200_0044, 0x0200_0101); // Thumb bit set

                // Thumb program at 0x0200_0100:
                //   00: mov r0, #7
                //   02: lsl r1, r0, #3          -> 56
                //   04: add r1, r1, #4          -> 60
                //   06: str r1, [r5, #4]        -> write 60 at 0x0200_1008
                //   08: bl  0x0200_0120         -> fused Thumb BL (+0x14 from 0x10C)
                //   0C: ldr r2, [r5, #4]        -> 60
                //   0E: add r0, r0, r2          -> 7 + 13 + 60 = 80
                //   10: ldr r3, [pc, #4]        -> literal at 0x118 (0x0200_0028, ARM)
                //   12: bx  r3                  -> return to ARM at 0x0200_0028
                //   14: nop (mov r8, r8)
                //   16: nop
                //   18: .word 0x0200_0028
                // Subroutine at 0x0200_0120:
                //   20: add r0, #13
                //   22: bx  lr
                let thumb_code: [u16; 10] = [
                    0x2007, // mov r0, #7
                    0x00C1, // lsl r1, r0, #3
                    0x3104, // add r1, #4
                    0x6069, // str r1, [r5, #4]
                    0xF000, // bl prefix (hi=0)
                    0xF80A, // bl suffix: (0x108+4) + 0x14 = 0x120
                    0x686A, // ldr r2, [r5, #4]
                    0x1880, // add r0, r0, r2
                    0x4B01, // ldr r3, [pc, #4] -> ((0x110+4)&!3)+4 = 0x118
                    0x4718, // bx r3
                ];
                for (i, &h) in thumb_code.iter().enumerate() {
                    write_u16(&mut m, 0x0200_0100 + (i as u32) * 2, h);
                }
                write_u32(&mut m, 0x0200_0118, 0x0200_0028);
                write_u16(&mut m, 0x0200_0120, 0x300D); // add r0, #13
                write_u16(&mut m, 0x0200_0122, 0x4770); // bx lr
            }
            let cpu = Cpu::new(
                View::<0>::new(mh.ptr()),
                true,
                0x0200_0000,
                0x0200_2000,
                0x0200_3000,
                0x0200_3800,
            );
            (mh, cpu)
        };

        let (mh_cached, mut cpu_cached) = setup();
        let (mh_uncached, mut cpu_uncached) = setup();

        // Execute exact 23-instruction ARM -> Thumb -> BL -> ARM sequence across odd slice boundaries.
        for slice in [5u32, 7, 10, 1] {
            cpu_cached.run_slice(slice);
            cpu_uncached.run_slice_uncached(slice);
        }

        assert_eq!(cpu_cached.st.regs, cpu_uncached.st.regs);
        assert_eq!(cpu_cached.st.cpsr, cpu_uncached.st.cpsr);
        assert_eq!(cpu_cached.st.regs[0], 80);
        assert_eq!(cpu_cached.st.regs[8], 0x8000_0015u32.wrapping_add(80));
        assert_eq!(cpu_cached.st.regs[15], 0x0200_002C);
        assert_eq!(
            mh_cached.borrow().main_ram[0x1000..0x1010],
            mh_uncached.borrow().main_ram[0x1000..0x1010]
        );

        // Next instruction at 0x0200_002C is `B .`: cached interpreter stays pinned and folds idle slice.
        cpu_cached.run_slice(64);
        assert_eq!(cpu_cached.st.regs[15], 0x0200_002C);
        assert_ne!(cpu_cached.block_cache[block_slot(0x0200_002C)].flags & 2, 0);
    }

    #[test]
    fn self_modifying_code_invalidates_cached_block_immediately() {
        let mut mh = MachineHandle::new(Machine::new());
        {
            let mut m = mh.borrow_mut();
            // Function at 0x0200_0200:
            //   00: mov r0, #11
            //   04: bx  lr
            write_u32(&mut m, 0x0200_0200, 0xE3A0_000B); // mov r0, #11
            write_u32(&mut m, 0x0200_0204, 0xE12F_FF1E); // bx lr
        }
        let mut cpu = Cpu::new(
            View::<0>::new(mh.ptr()),
            true,
            0x0200_0200,
            0x0200_2000,
            0x0200_3000,
            0x0200_3800,
        );
        cpu.st.regs[14] = 0x0200_0200; // loop back to start on bx lr

        cpu.run_slice(2);
        assert_eq!(cpu.st.regs[0], 11);
        assert_eq!(cpu.st.regs[15], 0x0200_0200);

        // Patch instruction at 0x0200_0200 in-place via bus (simulating AR DS 60 FPS patch / dynamic overlay)
        cpu.bus.write32(0x0200_0200, 0xE3A0_002A); // mov r0, #42

        cpu.run_slice(2);
        assert_eq!(
            cpu.st.regs[0], 42,
            "code-page generation invalidation must re-decode patched RAM immediately"
        );
    }

    #[test]
    fn poll_loop_folds_readonly_wait_without_affecting_counter_loops() {
        let mut mh = MachineHandle::new(Machine::new());
        {
            let mut m = mh.borrow_mut();
            // Pure read-only poll loop at 0x0200_0400:
            //   00: ldr r0, [r1]
            //   04: cmp r0, #0
            //   08: beq 0x0200_0400
            write_u32(&mut m, 0x0200_0400, 0xE591_0000);
            write_u32(&mut m, 0x0200_0404, 0xE350_0000);
            write_u32(&mut m, 0x0200_0408, 0x0AFF_FFFC);
            write_u32(&mut m, 0x0200_0500, 0); // flag = 0

            // Counter loop at 0x0200_0600 (MUST NOT fold):
            //   00: subs r2, r2, #1
            //   04: bne  0x0200_0600
            //   08: mov  r3, #99
            write_u32(&mut m, 0x0200_0600, 0xE252_2001);
            write_u32(&mut m, 0x0200_0604, 0x1AFF_FFFD);
            write_u32(&mut m, 0x0200_0608, 0xE3A0_3063);
        }
        let mut cpu = Cpu::new(
            View::<0>::new(mh.ptr()),
            true,
            0x0200_0400,
            0x0200_2000,
            0x0200_3000,
            0x0200_3800,
        );
        cpu.st.regs[1] = 0x0200_0500;
        cpu.run_slice(100);
        assert_eq!(cpu.st.regs[15], 0x0200_0400);
        let slot = block_slot(0x0200_0400);
        assert_ne!(
            cpu.block_cache[slot].flags & 4,
            0,
            "read-only poll loop must be flagged for slice folding"
        );

        // Once flag becomes non-zero, poll loop exits cleanly on next slice
        cpu.bus.write32(0x0200_0500, 1);
        cpu.run_slice(3);
        assert_eq!(cpu.st.regs[0], 1);
        assert_eq!(cpu.st.regs[15], 0x0200_040C);

        // Counter loop executes exact count without folding
        cpu.st.regs[15] = 0x0200_0600;
        cpu.st.regs[2] = 10;
        cpu.run_slice(25);
        assert_eq!(cpu.st.regs[2], 0);
        assert_eq!(cpu.st.regs[3], 99);
        let cslot = block_slot(0x0200_0600);
        assert_eq!(cpu.block_cache[cslot].flags & 6, 0);
    }
}
