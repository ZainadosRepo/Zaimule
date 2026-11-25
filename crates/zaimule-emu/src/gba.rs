//! Native Rust Game Boy Advance (`GBA`) core powered by `termboy-gba`.
//!
//! Features:
//! - ARM7TDMI CPU (ARM32 + Thumb-16) with waitstated MMU, DMA0-3, Timers, and HLE SWI BIOS
//! - `240x160` scanline PPU (Modes 0-5 + OAM sprites + optional GBA LCD color correction)
//! - 48 kHz stereo PCM APU (4 PSG channels + Direct Sound DMA FIFO A/B)
//! - Cartridge Flash / EEPROM / SRAM persistence and `"TBSS"` savestates

use crate::traits::{
    buttons_to_termboy, pcm_rms_level, solid_obsidian_rgba, CpuDebugSnapshot, EmulatorBackend,
    FastFrameMeta, FrameStepResult, DEFAULT_SAMPLE_RATE_HZ,
};
use termboy_core::Core;
use termboy_gba::cpu::psr::{C, N, V, Z};
use termboy_gba::GbaCore as TermboyGba;
use zaimule_core::catalog::ConsoleKind;

pub const GBA_WIDTH: usize = 240;
pub const GBA_HEIGHT: usize = 160;

pub struct GbaCore {
    inner: Option<TermboyGba>,
    frame_number: u64,
    sample_rate_hz: u32,
    color_correction: bool,
    sram_cache: Vec<u8>,
    audio_pairs_scratch: Vec<(f32, f32)>,
    rgba_buf: Vec<u8>,
}

impl Default for GbaCore {
    fn default() -> Self {
        Self::new_empty()
    }
}

impl GbaCore {
    pub fn new() -> Self {
        Self::new_empty()
    }

    pub fn new_empty() -> Self {
        Self {
            inner: None,
            frame_number: 0,
            sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
            color_correction: true,
            sram_cache: Vec::new(),
            audio_pairs_scratch: Vec::with_capacity(1024),
            rgba_buf: solid_obsidian_rgba(GBA_WIDTH, GBA_HEIGHT),
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.inner.is_some()
    }

    fn refresh_sram_cache(&mut self) {
        if let Some(ref gba) = self.inner {
            self.sram_cache = gba.save_ram().unwrap_or_default();
        } else {
            self.sram_cache.clear();
        }
    }
}

impl EmulatorBackend for GbaCore {
    fn console(&self) -> ConsoleKind {
        ConsoleKind::Gba
    }

    fn load_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        let mut gba = TermboyGba::new(rom.to_vec())
            .map_err(|e| format!("chargement ROM GBA impossible : {e}"))?;
        gba.set_audio_rate(self.sample_rate_hz);
        gba.set_color_correction(self.color_correction);
        self.frame_number = 0;
        self.inner = Some(gba);
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

        let Some(ref mut gba) = self.inner else {
            return FastFrameMeta {
                frame_number,
                width: GBA_WIDTH as u16,
                height: GBA_HEIGHT as u16,
                audio_level: 0.0,
                audio_freq_hz: 0,
                sample_rate_hz,
                serial_out_byte: None,
            };
        };

        let buttons = buttons_to_termboy(buttons_mask);
        if skip_render {
            gba.run_frame_skip_render(buttons);
        } else {
            let fb = gba.run_frame(buttons);
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
        gba.drain_audio(&mut self.audio_pairs_scratch);
        let pcm_start = pcm_out.len();
        pcm_out.reserve(self.audio_pairs_scratch.len() * 2);
        for &(l, r) in &self.audio_pairs_scratch {
            pcm_out.push(l.clamp(-1.0, 1.0));
            pcm_out.push(r.clamp(-1.0, 1.0));
        }

        let (apu_freq, apu_level_pct) = gba.apu_telemetry();
        let rms = pcm_rms_level(&pcm_out[pcm_start..]);
        let audio_level = if rms > 0.001 {
            rms
        } else {
            f32::from(apu_level_pct) / 100.0
        };
        let audio_freq_hz = apu_freq.min(u32::from(u16::MAX)) as u16;

        FastFrameMeta {
            frame_number,
            width: GBA_WIDTH as u16,
            height: GBA_HEIGHT as u16,
            audio_level,
            audio_freq_hz,
            sample_rate_hz,
            serial_out_byte: None,
        }
    }

    fn rgba_frame(&self) -> (u16, u16, &[u8]) {
        (GBA_WIDTH as u16, GBA_HEIGHT as u16, &self.rgba_buf)
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        let mut audio_samples = Vec::with_capacity(1664);
        let meta = self.step_frame_borrowed(buttons_mask, false, &mut audio_samples);
        let cpu = self.debug_snapshot();
        FrameStepResult {
            console: ConsoleKind::Gba,
            frame_number: meta.frame_number,
            width: meta.width,
            height: meta.height,
            rgba: self.rgba_buf.clone(),
            audio_level: meta.audio_level,
            audio_freq_hz: meta.audio_freq_hz,
            audio_samples,
            sample_rate_hz: meta.sample_rate_hz,
            serial_out_byte: None,
            cpu,
        }
    }

    fn save_state(&self) -> Result<Vec<u8>, String> {
        let Some(ref gba) = self.inner else {
            return Err("aucune ROM GBA chargée".into());
        };
        Ok(gba.save_state())
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        let Some(ref mut gba) = self.inner else {
            return Err("aucune ROM GBA chargée".into());
        };
        gba.load_state(data)
            .map_err(|e| format!("restauration Save State GBA invalide : {e:?}"))?;
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
        let Some(ref mut gba) = self.inner else {
            return Err("aucune ROM GBA chargée".into());
        };
        gba.load_ram(data);
        self.refresh_sram_cache();
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        incoming
    }

    fn set_gba_multi_connected(&mut self, connected: bool, player_id: u8) {
        if let Some(ref mut gba) = self.inner {
            gba.set_sio_multi_connected(connected, player_id);
        }
    }

    fn take_gba_multi_tx(&mut self) -> Option<(u16, u16)> {
        let gba = self.inner.as_mut()?;
        let word = gba.take_sio_multi_tx()?;
        Some((word, gba.sio_cnt()))
    }

    fn deliver_gba_multi(&mut self, slots: [u16; 4], my_id: u8) {
        if let Some(ref mut gba) = self.inner {
            gba.deliver_sio_multi(slots, my_id);
        }
    }

    fn exchange_gba_multi_slave(&mut self, master_word: u16, my_id: u8) -> (u16, u16) {
        let Some(ref mut gba) = self.inner else {
            return (0xFFFF, 0x0000);
        };
        let slot_idx = usize::from(my_id.clamp(1, 3));
        let slave_word = gba.sio_mlt_send();
        let mut slots = [0xFFFFu16; 4];
        slots[0] = master_word;
        slots[slot_idx] = slave_word;
        gba.deliver_sio_multi(slots, my_id);
        (slave_word, gba.sio_cnt())
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        let Some(ref gba) = self.inner else {
            return CpuDebugSnapshot {
                arch: "ARM7TDMI (En attente)".into(),
                pc_hex: "0x08000000".into(),
                sp_hex: "0x03007F00".into(),
                reg_a_hex: "0x00000000".into(),
                reg_bc_hex: "0x00000000".into(),
                reg_de_hex: "0x00000000".into(),
                reg_hl_hex: "0x00000000".into(),
                flags_str: "STANDBY".into(),
                ly_scanline: 0,
                serial_sb_hex: "0x0000".into(),
                serial_sc_hex: "0x0000".into(),
                cycles: 0,
                score: 0,
            };
        };

        let cpu = gba.cpu();
        let cpsr = cpu.regs.cpsr;
        let flags = format!(
            "{}{}{}{}{}",
            if cpsr.flag(N) { 'N' } else { '-' },
            if cpsr.flag(Z) { 'Z' } else { '-' },
            if cpsr.flag(C) { 'C' } else { '-' },
            if cpsr.flag(V) { 'V' } else { '-' },
            if cpsr.thumb() { 'T' } else { 'A' }
        );
        CpuDebugSnapshot {
            arch: if cpsr.thumb() {
                "ARM7TDMI (GBA · Thumb)".into()
            } else {
                "ARM7TDMI (GBA · ARM32)".into()
            },
            pc_hex: format!("0x{:08X}", cpu.exec_addr()),
            sp_hex: format!("0x{:08X}", cpu.regs.get(13)),
            reg_a_hex: format!("0x{:08X}", cpu.regs.get(0)),
            reg_bc_hex: format!("0x{:08X}", cpu.regs.get(1)),
            reg_de_hex: format!("0x{:08X}", cpu.regs.get(2)),
            reg_hl_hex: format!("0x{:08X}", cpu.regs.get(14)),
            flags_str: flags,
            ly_scanline: ((cpu.bus.cycles / 1232) % 228) as u8,
            serial_sb_hex: format!("0x{:04X}", gba.sio_mlt_send()),
            serial_sc_hex: format!("0x{:04X}", gba.sio_cnt()),
            cycles: cpu.bus.cycles,
            score: 0,
        }
    }

    fn apply_ram_cheat(&mut self, addr: u16, val: u8) {
        if let Some(ref mut gba) = self.inner {
            let iwram = gba.cpu_mut().bus.iwram_mut();
            let idx = (addr as usize).wrapping_sub(0xC000) % iwram.len();
            iwram[idx] = val;
        }
    }

    fn read_ram_slice(&self) -> &[u8] {
        self.inner
            .as_ref()
            .map(|gba| gba.cpu().bus.iwram())
            .unwrap_or(&[])
    }

    fn read_vram_slice(&self) -> &[u8] {
        self.inner
            .as_ref()
            .map(|gba| &gba.cpu().bus.vram[..])
            .unwrap_or(&[])
    }

    fn write_vram_byte(&mut self, offset: usize, val: u8) {
        if let Some(ref mut gba) = self.inner {
            let vram = &mut gba.cpu_mut().bus.vram;
            if offset < vram.len() {
                vram[offset] = val;
            }
        }
    }

    fn set_color_correction(&mut self, enabled: bool) {
        self.color_correction = enabled;
        if let Some(ref mut gba) = self.inner {
            gba.set_color_correction(enabled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaimule_core::keymap::BTN_RIGHT;

    /// Builds a minimal real GBA ARM32 ROM with SRAM signature (`SRAM_V110`)
    /// that writes a color into Palette RAM (`0x0500_0000`) and stores `0x37` at `0x0300_0004`.
    fn minimal_gba_color_ram_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x200];
        // Branch offset at 0x0000 -> jump directly to 0x00C0 (`B +0x00B8/4`, actually at 0x0000 PC+8 = 0x08 -> +0xB8 = (0xC0-8)/4 = 0x2E)
        let instrs: &[u32] = &[
            0xEA00_002E, // 0x0000: B 0x0800_00C0
        ];
        rom[0..4].copy_from_slice(&instrs[0].to_le_bytes());
        rom[0xB2] = 0x96;
        rom[0x140..0x149].copy_from_slice(b"SRAM_V110");

        // Program at 0x00C0 (0x0800_00C0):
        // MOV R0, #0x0500_0000 (0x05 << 24 = ROR 8 -> #5,8 -> 0xE3A00405)
        // MOV R1, #0x1F (red BGR555)
        // STRH R1, [R0]
        // MOV R2, #0x0300_0000 (0xE3A02403)
        // MOV R3, #0x37
        // STRB R3, [R2, #4]
        // B . (0xEAFFFFFE)
        let prog: &[u32] = &[
            0xE3A0_0405,
            0xE3A0_101F,
            0xE1C0_10B0,
            0xE3A0_2403,
            0xE3A0_3037,
            0xE5C2_3004,
            0xEAFF_FFFE,
        ];
        for (i, w) in prog.iter().enumerate() {
            let off = 0xC0 + i * 4;
            rom[off..off + 4].copy_from_slice(&w.to_le_bytes());
        }
        rom
    }

    #[test]
    fn runs_real_gba_arm7tdmi_ppu_pcm_sram_and_savestate() {
        let mut core = GbaCore::new_empty();
        core.set_color_correction(false);
        core.load_rom(&minimal_gba_color_ram_rom()).unwrap();

        assert_eq!(core.sram().len(), 0x8000);
        let f1 = core.step_frame(BTN_RIGHT);
        assert_eq!(f1.width, 240);
        assert_eq!(f1.height, 160);
        assert_eq!(f1.rgba.len(), 240 * 160 * 4);
        assert_eq!(f1.rgba[0..4], [255, 0, 0, 255]);
        assert!(!f1.audio_samples.is_empty());
        assert_eq!(f1.sample_rate_hz, 48_000);
        assert_eq!(core.read_ram_slice()[4], 0x37);

        let state = core.save_state().unwrap();
        assert!(state.starts_with(b"TBSS"));

        core.apply_ram_cheat(0xC004, 0x88);
        assert_eq!(core.read_ram_slice()[4], 0x88);

        core.load_state(&state).unwrap();
        assert_eq!(core.read_ram_slice()[4], 0x37);
    }
}
