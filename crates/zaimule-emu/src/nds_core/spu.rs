//! Nintendo DS 16-channel SPU synthesizer & stereo resampler.
//!
//! Hardware register layout (ARM7 I/O `0x0400_0400..=0x0400_051F`):
//! - Channels 0..=15 at `0x400 + ch * 0x10`:
//!   - `+0x00`: `SOUNDxCNT` (u32: vol mul/div, hold, pan, duty, repeat, format, start/busy)
//!   - `+0x04`: `SOUNDxSAD` (u32: ARM7 start address, bits 0..26)
//!   - `+0x08`: `SOUNDxTMR` (u16: timer reload at 16.757 MHz = 33.514 MHz / 2)
//!   - `+0x0A`: `SOUNDxPNT` (u16: loop start offset in 32-bit words)
//!   - `+0x0C`: `SOUNDxLEN` (u32: length in 32-bit words, bits 0..21)
//! - Global mixer & capture at `0x500..=0x51F`:
//!   - `0x500`: `SOUNDCNT` (u16: master vol, L/R source mux, ch1/ch3 mix bypass, master enable)
//!   - `0x504`: `SOUNDBIAS` (u16)
//!   - `0x508..=0x51F`: `SNDCAP0`/`SNDCAP1` capture control/dest/len

pub const SPU_CLOCK_HZ: f64 = 16_756_991.0;
const MIXER_PERIOD_CYCLES: i32 = 512;
const MIXER_RATE_HZ: f64 = SPU_CLOCK_HZ / (MIXER_PERIOD_CYCLES as f64); // ~32,728.5 Hz

const ADPCM_INDEX_TABLE: [i8; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

#[rustfmt::skip]
const ADPCM_STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45,
    50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230,
    253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963,
    1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327,
    3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487,
    12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

/// Read-only view of ARM7 audio-accessible memories (Main RAM, Shared WRAM, ARM7 WRAM, ARM7 VRAM).
pub struct Arm7AudioMem<'a> {
    pub main_ram: &'a [u8],
    pub wram7: &'a [u8],
    pub swram: &'a [u8],
    pub wramcnt: u8,
    pub vram: &'a [Vec<u8>; 9],
    pub vramcnt: &'a [u8; 9],
}

impl<'a> Arm7AudioMem<'a> {
    #[inline]
    pub fn read8(&self, addr: u32) -> u8 {
        match addr >> 24 {
            0x02 => self.main_ram[(addr & 0x3F_FFFF) as usize],
            0x03 => {
                if addr >= 0x0380_0000 {
                    self.wram7[(addr & 0xFFFF) as usize]
                } else {
                    let a = addr as usize;
                    let off = match self.wramcnt & 3 {
                        0 => return self.wram7[a & 0xFFFF],
                        1 => a & 0x3FFF,
                        2 => 0x4000 + (a & 0x3FFF),
                        _ => a & 0x7FFF,
                    };
                    self.swram[off]
                }
            }
            0x06 => {
                let a = 0x0600_0000 + (addr & 0x3_FFFF);
                for i in [2usize, 3] {
                    let cnt = self.vramcnt[i];
                    if cnt & 0x87 != 0x82 {
                        continue;
                    }
                    let base = 0x0600_0000 + ((cnt >> 3 & 1) as u32) * 0x2_0000;
                    if a >= base && a < base + 0x2_0000 {
                        return self.vram[i][(a - base) as usize];
                    }
                }
                0
            }
            _ => 0,
        }
    }

    #[inline]
    fn read16(&self, addr: u32) -> i16 {
        let a = addr & !1;
        i16::from_le_bytes([self.read8(a), self.read8(a.wrapping_add(1))])
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SoundChannel {
    pub cnt: u32,
    pub sad: u32,
    pub tmr: u16,
    pub pnt: u16,
    pub len: u32,
    until_tick: i32,
    byte_pos: u32,
    nibble_hi: bool,
    cur_sample: i32,
    adpcm_idx: u8,
    loop_sample: i32,
    loop_idx: u8,
    psg_phase: u8,
    lfsr: u16,
}

impl Default for SoundChannel {
    fn default() -> Self {
        Self {
            cnt: 0,
            sad: 0,
            tmr: 0,
            pnt: 0,
            len: 0,
            until_tick: 0,
            byte_pos: 0,
            nibble_hi: false,
            cur_sample: 0,
            adpcm_idx: 0,
            loop_sample: 0,
            loop_idx: 0,
            psg_phase: 0,
            lfsr: 0x7FFF,
        }
    }
}

impl SoundChannel {
    #[inline]
    fn running(&self) -> bool {
        self.cnt & 0x8000_0000 != 0
    }

    #[inline]
    fn format(&self) -> u32 {
        (self.cnt >> 29) & 3
    }

    #[inline]
    fn repeat_mode(&self) -> u32 {
        (self.cnt >> 27) & 3
    }

    #[inline]
    fn duty(&self) -> u8 {
        ((self.cnt >> 24) & 7) as u8
    }

    #[inline]
    fn hold(&self) -> bool {
        self.cnt & 0x8000 != 0
    }

    #[inline]
    fn period(&self) -> i32 {
        (0x1_0000i32 - self.tmr as i32).max(1)
    }

    #[inline]
    fn pnt_bytes(&self) -> u32 {
        (self.pnt as u32) * 4
    }

    #[inline]
    fn total_bytes(&self) -> u32 {
        let len_words = self.len & 0x003F_FFFF;
        (self.pnt as u32)
            .saturating_add(len_words)
            .saturating_mul(4)
    }

    #[inline]
    fn psg_wave_sample(duty: u8, phase: u8) -> i32 {
        if duty == 7 || phase < (7 - duty) {
            -0x7FFF
        } else {
            0x7FFF
        }
    }

    fn trigger(&mut self, ch_idx: usize, mem: &Arm7AudioMem<'_>) {
        self.until_tick = self.period();
        self.byte_pos = 0;
        self.nibble_hi = false;
        self.psg_phase = 0;
        self.lfsr = 0x7FFF;

        let sad = self.sad & 0x07FF_FFFC;
        match self.format() {
            0 => {
                // PCM8: signed 8-bit
                if self.total_bytes() > 0 {
                    self.cur_sample = (mem.read8(sad) as i8 as i32) << 8;
                } else {
                    self.cur_sample = 0;
                }
            }
            1 => {
                // PCM16: signed 16-bit
                if self.total_bytes() >= 2 {
                    self.cur_sample = mem.read16(sad) as i32;
                } else {
                    self.cur_sample = 0;
                }
            }
            2 => {
                // IMA-ADPCM: 4-byte header (16-bit initial sample + 7-bit step index)
                if self.total_bytes() <= 4 {
                    self.cur_sample = 0;
                    self.byte_pos = 4;
                    return;
                }
                let init_sample = mem.read16(sad) as i32;
                let init_idx = (mem.read8(sad.wrapping_add(2)) & 0x7F).min(88);
                self.cur_sample = init_sample;
                self.adpcm_idx = init_idx;
                self.loop_sample = init_sample;
                self.loop_idx = init_idx;
                self.byte_pos = 4;
            }
            _ => {
                if (8..=13).contains(&ch_idx) {
                    self.cur_sample = Self::psg_wave_sample(self.duty(), 0);
                } else {
                    self.cur_sample = 0;
                }
            }
        }
    }

    fn stop(&mut self) {
        self.cnt &= !0x8000_0000;
        if !self.hold() {
            self.cur_sample = 0;
        }
    }

    #[inline]
    fn step_sample(&mut self, ch_idx: usize, mem: &Arm7AudioMem<'_>) {
        if !self.running() {
            return;
        }
        let fmt = self.format();
        match fmt {
            0 => {
                let total = self.total_bytes();
                if total == 0 {
                    self.stop();
                    return;
                }
                self.byte_pos = self.byte_pos.wrapping_add(1);
                if self.byte_pos >= total {
                    if self.repeat_mode() == 2 {
                        self.stop();
                        return;
                    }
                    self.byte_pos = self.pnt_bytes();
                    if self.byte_pos >= total {
                        self.byte_pos = 0;
                    }
                }
                let addr = (self.sad & 0x07FF_FFFC).wrapping_add(self.byte_pos);
                self.cur_sample = (mem.read8(addr) as i8 as i32) << 8;
            }
            1 => {
                let total = self.total_bytes() & !1;
                if total < 2 {
                    self.stop();
                    return;
                }
                self.byte_pos = self.byte_pos.wrapping_add(2);
                if self.byte_pos >= total {
                    if self.repeat_mode() == 2 {
                        self.stop();
                        return;
                    }
                    self.byte_pos = self.pnt_bytes() & !1;
                    if self.byte_pos >= total {
                        self.byte_pos = 0;
                    }
                }
                let addr = (self.sad & 0x07FF_FFFC).wrapping_add(self.byte_pos);
                self.cur_sample = mem.read16(addr) as i32;
            }
            2 => {
                let total = self.total_bytes();
                if total <= 4 {
                    self.stop();
                    return;
                }
                let pnt_bytes = self.pnt_bytes().max(4);
                if !self.nibble_hi && self.byte_pos == pnt_bytes {
                    self.loop_sample = self.cur_sample;
                    self.loop_idx = self.adpcm_idx;
                }
                let addr = (self.sad & 0x07FF_FFFC).wrapping_add(self.byte_pos);
                let byte = mem.read8(addr);
                let nibble = if self.nibble_hi {
                    (byte >> 4) & 0xF
                } else {
                    byte & 0xF
                };

                let step = ADPCM_STEP_TABLE[self.adpcm_idx as usize];
                let mut diff = step >> 3;
                if nibble & 1 != 0 {
                    diff += step >> 2;
                }
                if nibble & 2 != 0 {
                    diff += step >> 1;
                }
                if nibble & 4 != 0 {
                    diff += step;
                }
                if nibble & 8 != 0 {
                    self.cur_sample = (self.cur_sample - diff).max(-0x7FFF);
                } else {
                    self.cur_sample = (self.cur_sample + diff).min(0x7FFF);
                }
                let next_idx =
                    (self.adpcm_idx as i32) + (ADPCM_INDEX_TABLE[(nibble & 7) as usize] as i32);
                self.adpcm_idx = next_idx.clamp(0, 88) as u8;

                if !self.nibble_hi {
                    self.nibble_hi = true;
                } else {
                    self.nibble_hi = false;
                    self.byte_pos = self.byte_pos.wrapping_add(1);
                    if self.byte_pos >= total {
                        if self.repeat_mode() == 2 {
                            self.stop();
                        } else {
                            self.byte_pos = if pnt_bytes < total { pnt_bytes } else { 4 };
                            self.cur_sample = self.loop_sample;
                            self.adpcm_idx = self.loop_idx;
                        }
                    }
                }
            }
            _ => {
                if (8..=13).contains(&ch_idx) {
                    self.psg_phase = (self.psg_phase + 1) & 7;
                    self.cur_sample = Self::psg_wave_sample(self.duty(), self.psg_phase);
                } else if (14..=15).contains(&ch_idx) {
                    let carry = self.lfsr & 1 != 0;
                    self.lfsr >>= 1;
                    if carry {
                        self.lfsr ^= 0x6000;
                        self.cur_sample = -0x7FFF;
                    } else {
                        self.cur_sample = 0x7FFF;
                    }
                }
            }
        }
    }

    /// Advance this channel over a `MIXER_PERIOD_CYCLES` window and return the time-averaged sample.
    #[inline]
    fn step_mixer_window(&mut self, ch_idx: usize, mem: &Arm7AudioMem<'_>) -> i32 {
        if !self.running() {
            return if self.hold() { self.cur_sample } else { 0 };
        }
        if self.until_tick > MIXER_PERIOD_CYCLES {
            self.until_tick -= MIXER_PERIOD_CYCLES;
            return self.cur_sample;
        }
        let mut remaining = MIXER_PERIOD_CYCLES;
        let mut weighted_sum = 0i64;
        let period = self.period();
        while remaining > 0 {
            if self.until_tick > remaining {
                weighted_sum += (self.cur_sample as i64) * (remaining as i64);
                self.until_tick -= remaining;
                break;
            }
            let seg = self.until_tick.max(0);
            weighted_sum += (self.cur_sample as i64) * (seg as i64);
            remaining -= seg;
            self.step_sample(ch_idx, mem);
            if !self.running() {
                if self.hold() && remaining > 0 {
                    weighted_sum += (self.cur_sample as i64) * (remaining as i64);
                }
                break;
            }
            self.until_tick = period;
        }
        (weighted_sum / (MIXER_PERIOD_CYCLES as i64)) as i32
    }

    /// Apply channel volume multiplier, divider shift, and stereo panning.
    /// Returns `(left, right)` in `[-2^23 .. 2^23]` (shifted up relative to `cur_sample`).
    #[inline]
    fn mix_stereo(&self, raw_sample: i32) -> (i64, i64) {
        if raw_sample == 0 {
            return (0, 0);
        }
        let vol_mul = self.cnt & 0x7F;
        let vol_mul = if vol_mul == 127 { 128 } else { vol_mul } as i64;
        if vol_mul == 0 {
            return (0, 0);
        }
        let div_shift = match (self.cnt >> 8) & 3 {
            0 => 0u32,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let pan = (self.cnt >> 16) & 0x7F;
        let pan_r = if pan == 127 { 128 } else { pan } as i64;
        let pan_l = 128 - pan_r;

        // ((sample * vol_mul) >> 7) >> div_shift -> scaled by pan/128
        let atten = ((raw_sample as i64) * vol_mul) >> (7 + div_shift);
        ((atten * pan_l) >> 7, (atten * pan_r) >> 7)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Spu {
    pub channels: [SoundChannel; 16],
    pub soundcnt: u16,
    pub soundbias: u16,
    pub sndcap_regs: [u8; 0x18],
    cycle_acc: i32,
    sample_rate_hz: u32,
    resample_phase: f64,
    prev_mix: (f32, f32),
    cur_mix: (f32, f32),
    dc_prev_in: (f32, f32),
    dc_prev_out: (f32, f32),
    #[serde(skip)]
    frame_pcm: Vec<f32>,
}

impl Default for Spu {
    fn default() -> Self {
        Self::new()
    }
}

impl Spu {
    pub fn new() -> Self {
        Self {
            channels: std::array::from_fn(|_| SoundChannel::default()),
            soundcnt: 0,
            soundbias: 0x0200,
            sndcap_regs: [0; 0x18],
            cycle_acc: 0,
            sample_rate_hz: 48_000,
            resample_phase: 0.0,
            prev_mix: (0.0, 0.0),
            cur_mix: (0.0, 0.0),
            dc_prev_in: (0.0, 0.0),
            dc_prev_out: (0.0, 0.0),
            frame_pcm: Vec::with_capacity(2048),
        }
    }

    pub fn set_sample_rate(&mut self, hz: u32) {
        self.sample_rate_hz = hz.max(8_000);
    }

    pub fn read16(&mut self, off: u32) -> u16 {
        let reg = off & !1;
        if (0x400..0x500).contains(&reg) {
            let ch = &self.channels[((reg - 0x400) >> 4) as usize];
            return match reg & 0xF {
                0x0 => ch.cnt as u16,
                0x2 => (ch.cnt >> 16) as u16,
                0x4 => ch.sad as u16,
                0x6 => (ch.sad >> 16) as u16,
                0x8 => ch.tmr,
                0xA => ch.pnt,
                0xC => ch.len as u16,
                _ => (ch.len >> 16) as u16,
            };
        }
        match reg {
            0x500 => self.soundcnt,
            0x504 => self.soundbias,
            0x508 => {
                let lo = self.sndcap_regs[0] & 0x7F;
                let hi = self.sndcap_regs[1] & 0x7F;
                self.sndcap_regs[0] = lo;
                self.sndcap_regs[1] = hi;
                u16::from_le_bytes([lo, hi])
            }
            0x50A..=0x51F => {
                let i = (reg - 0x508) as usize;
                u16::from_le_bytes([self.sndcap_regs[i], self.sndcap_regs[i + 1]])
            }
            _ => 0,
        }
    }

    pub fn write16(&mut self, off: u32, val: u16, mem: &Arm7AudioMem<'_>) {
        let reg = off & !1;
        if (0x400..0x500).contains(&reg) {
            let ch_idx = ((reg - 0x400) >> 4) as usize;
            let ch = &mut self.channels[ch_idx];
            match reg & 0xF {
                0x0 => ch.cnt = (ch.cnt & 0xFFFF_0000) | (val as u32),
                0x2 => {
                    let was_running = ch.cnt & 0x8000_0000 != 0;
                    ch.cnt = (ch.cnt & 0x0000_FFFF) | ((val as u32) << 16);
                    let now_running = ch.cnt & 0x8000_0000 != 0;
                    if !was_running && now_running {
                        ch.trigger(ch_idx, mem);
                    } else if was_running && !now_running {
                        ch.stop();
                    }
                }
                0x4 => ch.sad = (ch.sad & 0xFFFF_0000) | (val as u32),
                0x6 => ch.sad = (ch.sad & 0x0000_FFFF) | ((val as u32) << 16),
                0x8 => ch.tmr = val,
                0xA => ch.pnt = val,
                0xC => ch.len = (ch.len & 0xFFFF_0000) | (val as u32),
                _ => ch.len = (ch.len & 0x0000_FFFF) | ((val as u32) << 16),
            }
            return;
        }
        match reg {
            0x500 => self.soundcnt = val,
            0x504 => self.soundbias = val & 0x03FF,
            0x508..=0x51F => {
                let i = (reg - 0x508) as usize;
                self.sndcap_regs[i] = val as u8;
                self.sndcap_regs[i + 1] = (val >> 8) as u8;
            }
            _ => {}
        }
    }

    /// Advance SPU by `cycles` 16.757 MHz SPU clock cycles (half ARM7 clock).
    pub fn tick(&mut self, cycles: u32, mem: &Arm7AudioMem<'_>) {
        self.cycle_acc += cycles as i32;
        if self.cycle_acc < MIXER_PERIOD_CYCLES {
            return;
        }
        let step_ratio = MIXER_RATE_HZ / (self.sample_rate_hz.max(8_000) as f64);
        while self.cycle_acc >= MIXER_PERIOD_CYCLES {
            self.cycle_acc -= MIXER_PERIOD_CYCLES;
            let sample = self.mix_step(mem);
            self.prev_mix = self.cur_mix;
            self.cur_mix = sample;

            while self.resample_phase < 1.0 {
                let t = self.resample_phase as f32;
                let l = self.prev_mix.0 + (self.cur_mix.0 - self.prev_mix.0) * t;
                let r = self.prev_mix.1 + (self.cur_mix.1 - self.prev_mix.1) * t;
                let (dl, dr) = self.apply_dc_blocker(l, r);
                self.frame_pcm.push(soft_limit(dl));
                self.frame_pcm.push(soft_limit(dr));
                self.resample_phase += step_ratio;
            }
            self.resample_phase -= 1.0;
        }
    }

    fn mix_step(&mut self, mem: &Arm7AudioMem<'_>) -> (f32, f32) {
        let mut mixer_l = 0i64;
        let mut mixer_r = 0i64;
        let mut ch1_lr = (0i64, 0i64);
        let mut ch3_lr = (0i64, 0i64);

        let no_ch1_mix = self.soundcnt & 0x1000 != 0;
        let no_ch3_mix = self.soundcnt & 0x2000 != 0;

        for (ch_idx, ch) in self.channels.iter_mut().enumerate() {
            let raw = ch.step_mixer_window(ch_idx, mem);
            if raw == 0 {
                continue;
            }
            let (sl, sr) = ch.mix_stereo(raw);
            if ch_idx == 1 {
                ch1_lr = (sl, sr);
                if no_ch1_mix {
                    continue;
                }
            } else if ch_idx == 3 {
                ch3_lr = (sl, sr);
                if no_ch3_mix {
                    continue;
                }
            }
            mixer_l += sl;
            mixer_r += sr;
        }

        if self.soundcnt & 0x8000 == 0 {
            return (0.0, 0.0);
        }

        let mvol_raw = (self.soundcnt & 0x7F) as i64;
        let mvol = if mvol_raw == 127 { 128 } else { mvol_raw };
        if mvol == 0 {
            return (0.0, 0.0);
        }

        let out_l = match (self.soundcnt >> 8) & 3 {
            0 => mixer_l,
            1 => ch1_lr.0,
            2 => ch3_lr.0,
            _ => ch1_lr.0 + ch3_lr.0,
        };
        let out_r = match (self.soundcnt >> 10) & 3 {
            0 => mixer_r,
            1 => ch1_lr.1,
            2 => ch3_lr.1,
            _ => ch1_lr.1 + ch3_lr.1,
        };

        // Scale to normalized stereo f32 headroom (~4 full-volume channels reach 1.0).
        let scale = (mvol as f32) / (128.0 * 32_768.0 * 2.25);
        ((out_l as f32) * scale, (out_r as f32) * scale)
    }

    #[inline]
    fn apply_dc_blocker(&mut self, l: f32, r: f32) -> (f32, f32) {
        const R: f32 = 0.996;
        let yl = l - self.dc_prev_in.0 + R * self.dc_prev_out.0;
        let yr = r - self.dc_prev_in.1 + R * self.dc_prev_out.1;
        self.dc_prev_in = (l, r);
        self.dc_prev_out = (yl, yr);
        (yl, yr)
    }

    /// Drains interleaved stereo PCM accumulated since the previous call and
    /// returns the estimated dominant pitch frequency in Hz for telemetry.
    pub fn drain_audio(&mut self, out: &mut Vec<f32>) -> u16 {
        out.clear();
        std::mem::swap(out, &mut self.frame_pcm);
        self.dominant_freq_hz(out)
    }

    pub fn dominant_freq_hz(&self, pcm: &[f32]) -> u16 {
        if self.soundcnt & 0x8000 == 0 || (self.soundcnt & 0x7F) == 0 {
            return 0;
        }
        // Prefer active PSG square fundamental if present.
        let mut best_vol = 0u32;
        let mut psg_freq = 0u16;
        for ch in &self.channels[8..14] {
            if ch.running() && ch.format() == 3 && ch.duty() < 7 {
                let vol = ch.cnt & 0x7F;
                if vol > best_vol {
                    best_vol = vol;
                    let hz = SPU_CLOCK_HZ / (8.0 * (ch.period() as f64));
                    psg_freq = hz.round().clamp(20.0, 20_000.0) as u16;
                }
            }
        }
        if psg_freq > 0 {
            return psg_freq;
        }
        // Fallback: estimate fundamental from positive zero-crossings when audible.
        let frames = pcm.len() / 2;
        if frames < 16 {
            return 0;
        }
        let mut peak = 0.0f32;
        let mut crossings = 0u32;
        let mut prev_pos = false;
        for i in 0..frames {
            let mono = 0.5 * (pcm[i * 2] + pcm[i * 2 + 1]);
            let ap = mono.abs();
            if ap > peak {
                peak = ap;
            }
            let pos = mono > 0.004;
            if i > 0 && !prev_pos && pos {
                crossings += 1;
            }
            prev_pos = pos;
        }
        if peak < 0.008 {
            return 0;
        }
        if crossings > 0 {
            let hz = (crossings as f64) * (self.sample_rate_hz as f64) / (frames as f64);
            (hz.round() as u32).clamp(60, 12_000) as u16
        } else {
            220
        }
    }
}

#[inline]
fn soft_limit(x: f32) -> f32 {
    let ax = x.abs();
    if ax <= 0.85 {
        x
    } else {
        // Smooth saturation mapping [0.85, +inf) cleanly into [0.85, 1.0).
        let excess = ax - 0.85;
        x.signum() * (0.85 + 0.15 * (excess / (excess + 0.15)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_mem<'a>(
        main_ram: &'a [u8],
        wram7: &'a [u8],
        swram: &'a [u8],
        vram: &'a [Vec<u8>; 9],
        vramcnt: &'a [u8; 9],
    ) -> Arm7AudioMem<'a> {
        Arm7AudioMem {
            main_ram,
            wram7,
            swram,
            wramcnt: 3,
            vram,
            vramcnt,
        }
    }

    #[test]
    fn emits_expected_frame_sample_count_at_48khz() {
        let mut spu = Spu::new();
        spu.set_sample_rate(48_000);
        let ram = vec![0u8; 0x1000];
        let wram7 = vec![0u8; 0x1000];
        let swram = vec![0u8; 0x1000];
        let vram = std::array::from_fn(|_| vec![0u8; 0x4000]);
        let vramcnt = [0u8; 9];
        let mem = test_mem(&ram, &wram7, &swram, &vram, &vramcnt);

        for _ in 0..263 {
            spu.tick(1062, &mem);
        }
        let mut pcm = Vec::new();
        let freq = spu.drain_audio(&mut pcm);
        assert_eq!(freq, 0);
        let pairs = pcm.len() / 2;
        assert!((795..=805).contains(&pairs), "unexpected pairs: {pairs}");
        assert!(pcm.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn psg_square_synthesizes_440hz_with_stereo_panning() {
        let mut spu = Spu::new();
        spu.set_sample_rate(48_000);
        let ram = vec![0u8; 0x1000];
        let wram7 = vec![0u8; 0x1000];
        let swram = vec![0u8; 0x1000];
        let vram = std::array::from_fn(|_| vec![0u8; 0x4000]);
        let vramcnt = [0u8; 9];
        let mem = test_mem(&ram, &wram7, &swram, &vram, &vramcnt);

        // Master enable + max master volume (127).
        spu.write16(0x500, 0x807F, &mem);

        // 440 Hz square wave on ch 8: period = 16_756_991 / (8 * 440) = 4761 -> tmr = 65536 - 4761
        let tmr = (0x1_0000 - 4761) as u16;
        spu.write16(0x488, tmr, &mem);
        // Pan full left (0), duty 3 (50%), format 3 (PSG), vol=127, start=1 -> 0xE300_007F
        spu.write16(0x480, 0x007F, &mem);
        spu.write16(0x482, 0xE300, &mem);

        for _ in 0..263 {
            spu.tick(1062, &mem);
        }
        let mut pcm = Vec::new();
        let freq = spu.drain_audio(&mut pcm);
        assert!((438..=442).contains(&freq), "got {freq} Hz");

        let (pairs, _) = pcm.as_chunks::<2>();
        let left_energy: f32 = pairs.iter().map(|lr| lr[0].abs()).sum();
        let right_energy: f32 = pairs.iter().map(|lr| lr[1].abs()).sum();
        assert!(left_energy > 50.0, "expected strong left energy");
        assert!(
            right_energy < 1e-5,
            "full left pan must silence right channel"
        );
    }

    #[test]
    fn ima_adpcm_decodes_and_loops_with_saved_predictor() {
        let mut spu = Spu::new();
        let mut ram = vec![0u8; 0x4000];
        let wram7 = vec![0u8; 0x1000];
        let swram = vec![0u8; 0x1000];
        let vram = std::array::from_fn(|_| vec![0u8; 0x4000]);
        let vramcnt = [0u8; 9];

        // ADPCM stream at 0x0200_1000: header init sample = 0x1000, idx = 10; loop at word 2 (byte 8)
        ram[0x1000..0x1004].copy_from_slice(&[0x00, 0x10, 10, 0x00]);
        ram[0x1004..0x100C].copy_from_slice(&[0x31, 0x94, 0x27, 0x5A, 0x16, 0x83, 0x42, 0x71]);
        let mem = test_mem(&ram, &wram7, &swram, &vram, &vramcnt);

        spu.write16(0x500, 0x807F, &mem);
        spu.write16(0x404, 0x1000, &mem);
        spu.write16(0x406, 0x0200, &mem);
        spu.write16(0x408, 0xFE00, &mem); // period = 512 SPU cycles (~32.7 kHz)
        spu.write16(0x40A, 2, &mem); // PNT = 8 bytes (word 2)
        spu.write16(0x40C, 1, &mem); // LEN = 4 bytes (word 1) -> total 12 bytes
        spu.write16(0x40E, 0, &mem);
        // Loop Infinite (repeat=1 -> bits 27..28 = 1), ADPCM (fmt=2 -> bits 29..30 = 2), Start -> 0xC840
        spu.write16(0x400, 0x007F, &mem);
        spu.write16(0x402, 0xC840, &mem);

        for _ in 0..80 {
            spu.tick(512, &mem);
        }
        assert!(spu.read16(0x402) & 0x8000 != 0);
        let mut pcm = Vec::new();
        spu.drain_audio(&mut pcm);
        let peak = pcm.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(
            peak > 0.02,
            "expected audible ADPCM output, got peak {peak}"
        );

        // Snapshot of loop predictor matches re-looped values stably across many iterations.
        let loop_sample = spu.channels[0].loop_sample;
        let loop_idx = spu.channels[0].loop_idx;
        for _ in 0..80 {
            spu.tick(512, &mem);
        }
        assert_eq!(spu.channels[0].loop_sample, loop_sample);
        assert_eq!(spu.channels[0].loop_idx, loop_idx);
    }

    #[test]
    fn oneshot_pcm16_clears_busy_bit_on_completion() {
        let mut spu = Spu::new();
        let mut ram = vec![0u8; 0x2000];
        let wram7 = vec![0u8; 0x1000];
        let swram = vec![0u8; 0x1000];
        let vram = std::array::from_fn(|_| vec![0u8; 0x4000]);
        let vramcnt = [0u8; 9];
        // 4 signed i16 samples at 0x0200_0400 (len = 2 words = 8 bytes = 4 samples)
        for (i, s) in [0x4000i16, -0x4000, 0x2000, -0x2000].iter().enumerate() {
            ram[0x400 + i * 2..0x402 + i * 2].copy_from_slice(&s.to_le_bytes());
        }
        let mem = test_mem(&ram, &wram7, &swram, &vram, &vramcnt);

        spu.write16(0x500, 0x807F, &mem);
        spu.write16(0x404, 0x0400, &mem);
        spu.write16(0x406, 0x0200, &mem);
        spu.write16(0x408, 0xFF00, &mem); // period = 256
        spu.write16(0x40A, 0, &mem);
        spu.write16(0x40C, 2, &mem); // 2 words = 8 bytes = 4 samples
                                     // One-shot (repeat=2 -> 0x1000), PCM16 (fmt=1 -> 0x2000), Start -> 0xB040
        spu.write16(0x400, 0x007F, &mem);
        spu.write16(0x402, 0xB040, &mem);

        assert_ne!(spu.read16(0x402) & 0x8000, 0);
        spu.tick(512, &mem);
        spu.tick(512, &mem);
        // After 4 * 256 = 1024 cycles, one-shot finishes cleanly and clears bit 31.
        assert_eq!(spu.read16(0x402) & 0x8000, 0);
    }

    #[test]
    fn noise_channel_and_soundcnt_routing_work() {
        let mut spu = Spu::new();
        let ram = vec![0u8; 0x1000];
        let wram7 = vec![0u8; 0x1000];
        let swram = vec![0u8; 0x1000];
        let vram = std::array::from_fn(|_| vec![0u8; 0x4000]);
        let vramcnt = [0u8; 9];
        let mem = test_mem(&ram, &wram7, &swram, &vram, &vramcnt);

        spu.write16(0x500, 0x807F, &mem);
        // Channel 14 (0x4E0): LFSR white noise, center pan (64)
        spu.write16(0x4E8, 0xFE00, &mem);
        spu.write16(0x4E0, 0x007F, &mem);
        spu.write16(0x4E2, 0xE040, &mem);

        for _ in 0..40 {
            spu.tick(512, &mem);
        }
        let mut pcm = Vec::new();
        spu.drain_audio(&mut pcm);
        let peak = pcm.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(peak > 0.05, "noise channel should produce signal");
    }
}
