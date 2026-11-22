//! Native Rust Game Boy / Game Boy Color (`DMG` / `CGB`) core powered by `termboy-gb`.
//!
//! Features:
//! - Cycle-accurate SM83 CPU + hardware MMU with MBC1 / MBC3 (+ RTC) / MBC5 bank switching
//! - Scanline PPU rendering `160x144` RGBA8888 (DMG 4-shade palette + CGB RGB555)
//! - 4-channel 48 kHz stereo PCM APU synthesis (2 Pulse with Sweep, Wave, LFSR Noise)
//! - Battery-backed Cartridge SRAM persistence, `"TBSS"` save states, and Link Cable (`SB`/`SC`)

use crate::traits::{
    buttons_to_termboy, pcm_rms_level, solid_obsidian_rgba, CpuDebugSnapshot, EmulatorBackend,
    FastFrameMeta, FrameStepResult, DEFAULT_SAMPLE_RATE_HZ,
};
use termboy_core::Core;
use termboy_gb::cpu::registers::{C, H, N, Z};
use termboy_gb::GameBoy;
use zaimule_core::catalog::ConsoleKind;

pub const GB_WIDTH: usize = 160;
pub const GB_HEIGHT: usize = 144;

pub struct GameBoyCore {
    inner: Option<GameBoy>,
    console: ConsoleKind,
    frame_number: u64,
    sample_rate_hz: u32,
    sram_cache: Vec<u8>,
    audio_pairs_scratch: Vec<(f32, f32)>,
    rgba_buf: Vec<u8>,
}

impl Default for GameBoyCore {
    fn default() -> Self {
        Self::new_empty()
    }
}

impl GameBoyCore {
    pub fn new() -> Self {
        Self::new_empty()
    }

    pub fn new_empty() -> Self {
        Self {
            inner: None,
            console: ConsoleKind::Gbc,
            frame_number: 0,
            sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
            sram_cache: Vec::new(),
            audio_pairs_scratch: Vec::with_capacity(1024),
            rgba_buf: solid_obsidian_rgba(GB_WIDTH, GB_HEIGHT),
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.inner.is_some()
    }

    fn refresh_sram_cache(&mut self) {
        if let Some(ref gb) = self.inner {
            self.sram_cache = gb.save_ram().unwrap_or_default();
        } else {
            self.sram_cache.clear();
        }
    }
}

impl EmulatorBackend for GameBoyCore {
    fn console(&self) -> ConsoleKind {
        self.console
    }

    fn load_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        let mut gb = GameBoy::new(rom.to_vec())
            .map_err(|e| format!("chargement ROM Game Boy impossible : {e}"))?;
        gb.set_audio_rate(self.sample_rate_hz);
        self.console = if gb.is_cgb() {
            ConsoleKind::Gbc
        } else {
            ConsoleKind::Gb
        };
        self.frame_number = 0;
        self.inner = Some(gb);
        self.refresh_sram_cache();
        Ok(())
    }

    fn step_frame_borrowed(
        &mut self,
        buttons_mask: u16,
        skip_render: bool,
        pcm_out: &mut Vec<f32>,
    ) -> FastFrameMeta {
        self.frame_number = self.frame_number.saturating_add(1);
        let frame_number = self.frame_number;
        let sample_rate_hz = self.sample_rate_hz;

        let Some(ref mut gb) = self.inner else {
            return FastFrameMeta {
                frame_number,
                width: GB_WIDTH as u16,
                height: GB_HEIGHT as u16,
                audio_level: 0.0,
                audio_freq_hz: 0,
                sample_rate_hz,
                serial_out_byte: None,
            };
        };

        let buttons = buttons_to_termboy(buttons_mask);
        if skip_render {
            gb.run_frame_skip_render(buttons);
        } else {
            let fb = gb.run_frame(buttons);
            for (dst, px) in self
                .rgba_buf
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(fb.pixels.iter())
            {
                *dst = [px.0, px.1, px.2, 255];
            }
        }

        self.audio_pairs_scratch.clear();
        gb.drain_audio(&mut self.audio_pairs_scratch);
        let pcm_start = pcm_out.len();
        pcm_out.reserve(self.audio_pairs_scratch.len() * 2);
        for &(l, r) in &self.audio_pairs_scratch {
            pcm_out.push(l.clamp(-1.0, 1.0));
            pcm_out.push(r.clamp(-1.0, 1.0));
        }

        let (apu_freq, apu_level_pct) = gb.apu_telemetry();
        let rms = pcm_rms_level(&pcm_out[pcm_start..]);
        let audio_level = if rms > 0.001 {
            rms
        } else {
            f32::from(apu_level_pct) / 100.0
        };
        let audio_freq_hz = apu_freq.min(u32::from(u16::MAX)) as u16;
        let serial_out_byte = gb.cpu_mut().bus.serial.take_tx();

        FastFrameMeta {
            frame_number,
            width: GB_WIDTH as u16,
            height: GB_HEIGHT as u16,
            audio_level,
            audio_freq_hz,
            sample_rate_hz,
            serial_out_byte,
        }
    }

    fn rgba_frame(&self) -> (u16, u16, &[u8]) {
        (GB_WIDTH as u16, GB_HEIGHT as u16, &self.rgba_buf)
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        let mut audio_samples = Vec::with_capacity(1664);
        let meta = self.step_frame_borrowed(buttons_mask, false, &mut audio_samples);
        let cpu = self.debug_snapshot();
        FrameStepResult {
            console: self.console,
            frame_number: meta.frame_number,
            width: meta.width,
            height: meta.height,
            rgba: self.rgba_buf.clone(),
            audio_level: meta.audio_level,
            audio_freq_hz: meta.audio_freq_hz,
            audio_samples,
            sample_rate_hz: meta.sample_rate_hz,
            serial_out_byte: meta.serial_out_byte,
            cpu,
        }
    }

    fn save_state(&self) -> Result<Vec<u8>, String> {
        let Some(ref gb) = self.inner else {
            return Err("aucune ROM Game Boy / GBC chargée".into());
        };
        Ok(gb.save_state())
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        let Some(ref mut gb) = self.inner else {
            return Err("aucune ROM Game Boy / GBC chargée".into());
        };
        gb.load_state(data)
            .map_err(|e| format!("restauration Save State GB/GBC invalide : {e:?}"))?;
        self.refresh_sram_cache();
        Ok(())
    }

    fn sram(&self) -> &[u8] {
        &self.sram_cache
    }

    fn sync_sram_cache(&mut self) {
        self.refresh_sram_cache();
    }

    fn load_sram(&mut self, data: &[u8]) -> Result<(), String> {
        let Some(ref mut gb) = self.inner else {
            return Err("aucune ROM Game Boy / GBC chargée".into());
        };
        gb.load_ram(data);
        self.refresh_sram_cache();
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        let Some(ref mut gb) = self.inner else {
            return 0xFF;
        };
        let prev = gb.cpu().bus.serial.read_sb();
        gb.cpu_mut().bus.deliver_serial_rx(incoming);
        prev
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        let Some(ref gb) = self.inner else {
            return CpuDebugSnapshot {
                arch: "Sharp LR35902 (En attente)".into(),
                pc_hex: "0x0100".into(),
                sp_hex: "0xFFFE".into(),
                reg_a_hex: "0x00".into(),
                reg_bc_hex: "0x0000".into(),
                reg_de_hex: "0x0000".into(),
                reg_hl_hex: "0x0000".into(),
                flags_str: "STANDBY".into(),
                ly_scanline: 0,
                serial_sb_hex: "0x00".into(),
                serial_sc_hex: "0x7E".into(),
                cycles: 0,
                score: 0,
            };
        };

        let cpu = gb.cpu();
        let regs = &cpu.regs;
        let flags = format!(
            "{}{}{}{}",
            if regs.flag(Z) { 'Z' } else { '-' },
            if regs.flag(N) { 'N' } else { '-' },
            if regs.flag(H) { 'H' } else { '-' },
            if regs.flag(C) { 'C' } else { '-' }
        );
        CpuDebugSnapshot {
            arch: if gb.is_cgb() {
                "Sharp LR35902 (CGB)".into()
            } else {
                "Sharp LR35902 (DMG)".into()
            },
            pc_hex: format!("0x{:04X}", regs.pc),
            sp_hex: format!("0x{:04X}", regs.sp),
            reg_a_hex: format!("0x{:02X}", regs.a),
            reg_bc_hex: format!("0x{:04X}", regs.bc()),
            reg_de_hex: format!("0x{:04X}", regs.de()),
            reg_hl_hex: format!("0x{:04X}", regs.hl()),
            flags_str: flags,
            ly_scanline: cpu.bus.ppu.ly,
            serial_sb_hex: format!("0x{:02X}", cpu.bus.serial.read_sb()),
            serial_sc_hex: format!("0x{:02X}", cpu.bus.serial.read_sc()),
            cycles: cpu.bus.cycles,
            score: 0,
        }
    }

    fn apply_ram_cheat(&mut self, addr: u16, val: u8) {
        if addr < 0xA000 {
            return;
        }
        if let Some(ref mut gb) = self.inner {
            gb.cpu_mut().bus.write_debug(addr, val);
            if (0xA000..=0xBFFF).contains(&addr) {
                self.refresh_sram_cache();
            }
        }
    }

    fn read_ram_slice(&self) -> &[u8] {
        self.inner
            .as_ref()
            .map(|gb| gb.cpu().bus.wram())
            .unwrap_or(&[])
    }

    fn read_vram_slice(&self) -> &[u8] {
        self.inner
            .as_ref()
            .map(|gb| &gb.cpu().bus.ppu.vram[..])
            .unwrap_or(&[])
    }

    fn write_vram_byte(&mut self, offset: usize, val: u8) {
        if let Some(ref mut gb) = self.inner {
            let vram = &mut gb.cpu_mut().bus.ppu.vram;
            if offset < vram.len() {
                vram[offset] = val;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaimule_core::keymap::BTN_A;

    /// Builds a minimal real LR35902 ROM (MBC1 + RAM + Battery) that:
    /// - Sets CH2 sound & master volume (`0xFF26`, `0xFF24`, `0xFF25`, `0xFF17`, `0xFF19`)
    /// - Transmits `0x5A` on serial (`SB=0xFF01`, `SC=0xFF02`)
    /// - Writes `0x42` into WRAM `0xC004`
    /// - Loops infinitely at `0x0165`.
    fn minimal_gbc_sound_serial_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        // MBC1 + RAM + BATTERY (0x03), 32 KiB RAM (0x03), CGB supported (0x80)
        rom[0x0143] = 0x80;
        rom[0x0147] = 0x03;
        rom[0x0149] = 0x03;

        // 0x0100: JP 0x0150
        rom[0x0100] = 0xC3;
        rom[0x0101] = 0x50;
        rom[0x0102] = 0x01;

        let code: &[u8] = &[
            0x3E, 0x80, 0xE0, 0x26, // LD A, 0x80; LDH (NR52), A -> APU ON
            0x3E, 0x77, 0xE0, 0x24, // LD A, 0x77; LDH (NR50), A -> full volume
            0x3E, 0x22, 0xE0, 0x25, // LD A, 0x22; LDH (NR51), A -> CH2 to L+R
            0x3E, 0xF0, 0xE0, 0x17, // LD A, 0xF0; LDH (NR22), A -> DAC on max vol
            0x3E, 0x00, 0xE0, 0x18, // LD A, 0x00; LDH (NR23), A -> freq low
            0x3E, 0x87, 0xE0, 0x19, // LD A, 0x87; LDH (NR24), A -> trigger CH2
            0x3E, 0x5A, 0xE0, 0x01, // LD A, 0x5A; LDH (SB), A
            0x3E, 0x81, 0xE0, 0x02, // LD A, 0x81; LDH (SC), A -> send serial byte
            0x3E, 0x42, 0xEA, 0x04, 0xC0, // LD A, 0x42; LD (0xC004), A
            0x18, 0xFE, // JR -2 (spin)
        ];
        rom[0x0150..0x0150 + code.len()].copy_from_slice(code);
        rom
    }

    #[test]
    fn runs_real_gbc_cpu_ppu_apu_pcm_serial_sram_and_savestate() {
        let mut core = GameBoyCore::new_empty();
        assert_eq!(core.step_frame(0).width, 160);
        assert!(core.sram().is_empty());

        let rom = minimal_gbc_sound_serial_rom();
        core.load_rom(&rom).unwrap();
        assert_eq!(core.console(), ConsoleKind::Gbc);
        assert_eq!(core.sram().len(), 0x8000);

        let f1 = core.step_frame(BTN_A);
        assert_eq!(f1.width, 160);
        assert_eq!(f1.height, 144);
        assert_eq!(f1.rgba.len(), 160 * 144 * 4);
        assert_eq!(f1.sample_rate_hz, 48_000);
        assert!(!f1.audio_samples.is_empty());
        assert_eq!(f1.audio_samples.len() % 2, 0);
        assert_eq!(f1.serial_out_byte, Some(0x5A));
        assert_eq!(core.read_ram_slice()[4], 0x42);
        assert!(f1.cpu.cycles >= 65_000);
        let f2 = core.step_frame(BTN_A);
        assert!(f2.cpu.cycles - f1.cpu.cycles >= 70_000);

        let state = core.save_state().unwrap();
        assert!(state.starts_with(b"TBSS"));

        core.apply_ram_cheat(0xC004, 0x99);
        assert_eq!(core.read_ram_slice()[4], 0x99);

        core.load_state(&state).unwrap();
        assert_eq!(core.read_ram_slice()[4], 0x42);
    }
}
