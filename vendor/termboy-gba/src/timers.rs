//! Four timers at coarse bus-cycle resolution, synced lazily from
//! Bus::catch_up. Counter reads are at most a few cycles stale (G7 tightens
//! this); reload registers live in the io array, live counters here.

use crate::bus::Bus;
use termboy_core::state::{Reader, StateError, Writer};

const PRESCALE_SHIFT: [u32; 4] = [0, 6, 8, 10];

#[derive(Default, Clone, Copy)]
pub struct Timer {
    pub(crate) counter: u16,
    /// Cycles accumulated toward the next prescaler tick.
    pub(crate) rem: u64,
}

impl Timer {
    pub(crate) fn serialize(&self, w: &mut Writer) {
        w.put_u16(self.counter);
        w.put_u64(self.rem);
    }

    pub(crate) fn deserialize(&mut self, r: &mut Reader) -> Result<(), StateError> {
        self.counter = r.get_u16()?;
        self.rem = r.get_u64()?;
        Ok(())
    }
}

impl Bus {
    #[inline]
    pub(crate) fn timers_catch_up(&mut self) {
        if self.timers_active_mask == 0 {
            self.timers_synced = self.cycles;
            return;
        }
        let elapsed = self.cycles - self.timers_synced;
        self.timers_synced = self.cycles;
        if elapsed == 0 {
            return;
        }
        let mut overflows = 0u64;
        for t in 0..4 {
            let ctrl = self.io16(0x102 + t * 4);
            if ctrl & 0x80 == 0 {
                overflows = 0; // a disabled timer breaks the cascade chain
                continue;
            }
            let ticks = if t > 0 && ctrl & 4 != 0 {
                overflows
            } else {
                let sh = PRESCALE_SHIFT[(ctrl & 3) as usize];
                let total = self.timers[t].rem + elapsed;
                self.timers[t].rem = total & ((1u64 << sh) - 1);
                total >> sh
            };
            overflows = self.timer_advance(t, ticks);
            // Timers 0/1 clock the Direct Sound FIFOs.
            if t < 2 && overflows > 0 {
                self.apu_catch_up();
                self.apu.on_timer_overflow(t, overflows as u32);
            }
        }
    }

    /// Earliest cycle at which any enabled non-cascade timer will overflow, or `u64::MAX`.
    pub(crate) fn next_timer_overflow_cycle(&self) -> u64 {
        if self.timers_active_mask == 0 {
            return u64::MAX;
        }
        let mut earliest = u64::MAX;
        for t in 0..4 {
            if (self.timers_active_mask & (1 << t)) == 0 {
                continue;
            }
            let ctrl = self.io16(0x102 + t * 4);
            if t > 0 && ctrl & 4 != 0 {
                continue; // cascade timers only overflow when timer t-1 overflows
            }
            let sh = PRESCALE_SHIFT[(ctrl & 3) as usize];
            let ticks_to_ovf = 0x1_0000u64 - self.timers[t].counter as u64;
            let cycles_to_ovf = (ticks_to_ovf << sh)
                .saturating_sub(self.timers[t].rem)
                .max(1);
            earliest = earliest.min(self.timers_synced.saturating_add(cycles_to_ovf));
        }
        earliest
    }

    /// Add `ticks` to timer `t`; returns how many times it overflowed.
    fn timer_advance(&mut self, t: usize, ticks: u64) -> u64 {
        if ticks == 0 {
            return 0;
        }
        let total = self.timers[t].counter as u64 + ticks;
        if total < 0x1_0000 {
            self.timers[t].counter = total as u16;
            return 0;
        }
        let reload = self.io16(0x100 + t * 4) as u64;
        let period = 0x1_0000 - reload;
        let past = total - 0x1_0000;
        self.timers[t].counter = (reload + past % period) as u16;
        if self.io16(0x102 + t * 4) & 0x40 != 0 {
            self.raise_irq(1 << (3 + t));
        }
        1 + past / period
    }
}

#[cfg(test)]
mod tests {
    use crate::bus::Bus;

    fn bus() -> Bus {
        Bus::new(vec![0u8; 0x1000])
    }

    #[test]
    fn prescaler_divides_the_clock() {
        let mut b = bus();
        b.write16(0x0400_0102, 0x0081); // enable, prescaler /64
        b.cycles += 640;
        b.catch_up();
        // peek the field directly: a bus read would itself cost a cycle and
        // shift the prescaler boundary this test is pinning down
        assert_eq!(b.timers[0].counter, 10);
        b.cycles += 63;
        b.catch_up();
        assert_eq!(b.timers[0].counter, 10); // remainder carried, no tick yet
        b.cycles += 1;
        b.catch_up();
        assert_eq!(b.timers[0].counter, 11);
    }

    #[test]
    fn overflow_reloads_and_raises_irq() {
        let mut b = bus();
        b.write16(0x0400_0100, 0xFFF0); // reload
        b.write16(0x0400_0102, 0x00C0); // enable + IRQ, prescaler /1
        b.cycles += 0x10;
        b.catch_up();
        assert_eq!(b.read16(0x0400_0100), 0xFFF0); // wrapped back to reload
        assert_eq!(b.read16(0x0400_0202) & (1 << 3), 1 << 3);
    }

    #[test]
    fn enable_latches_reload_into_counter() {
        let mut b = bus();
        b.write16(0x0400_0104, 0x1234); // timer 1 reload
        b.write16(0x0400_0106, 0x0080); // enable
        assert_eq!(b.read16(0x0400_0104), 0x1234);
    }

    #[test]
    fn cascade_counts_overflows_of_the_previous_timer() {
        let mut b = bus();
        b.write16(0x0400_0100, 0xFFFF); // timer 0 overflows every tick
        b.write16(0x0400_0102, 0x0080);
        b.write16(0x0400_0106, 0x0084); // timer 1: enable + cascade
        b.cycles += 5;
        b.catch_up();
        assert_eq!(b.read16(0x0400_0104), 5);
    }

    #[test]
    fn disabled_timer_neither_counts_nor_cascades() {
        let mut b = bus();
        b.write16(0x0400_0100, 0xFFFF);
        // timer 0 NOT enabled; timer 1 cascades from it
        b.write16(0x0400_0106, 0x0084);
        b.cycles += 100;
        b.catch_up();
        assert_eq!(b.read16(0x0400_0100), 0);
        assert_eq!(b.read16(0x0400_0104), 0);
    }

    #[test]
    fn many_overflows_in_one_batch() {
        let mut b = bus();
        b.write16(0x0400_0100, 0xFF00); // period 0x100
        b.write16(0x0400_0102, 0x00C0); // enable + IRQ
        b.cycles += 0x100 * 3 + 7;
        b.catch_up();
        assert_eq!(b.read16(0x0400_0100), 0xFF07);
        assert_eq!(b.read16(0x0400_0202) & (1 << 3), 1 << 3);
    }
}
