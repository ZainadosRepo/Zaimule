//! Pure-Rust Nintendo DS (`NDS`) core powered by `nds_core` (MIT `x0cero/nds` + Zaimule extensions).
//!
//! Features:
//! - Dual-CPU ARM946E-S + ARM7TDMI with HLE BIOS, `key1.bin` Blowfish decryption, and firmware stub
//! - Dual-screen `256x384` RGBA8888 PPU (Text/Affine/Extended BGs, 1D/2D Bitmap OBJs, `powcnt1` swap, Display Capture & 3D GX engine with 4×4 Format 5 textures and Decal/Toon/Highlight shading)
//! - 16-channel Nintendo DS SPU stereo synthesizer (`PCM8`, `PCM16`, `IMA-ADPCM`, PSG square, LFSR noise) resampled to `48 kHz`
//! - Interactive lower-screen Stylus ADC, 512 KiB Flash backup SRAM, and RLE `"ZMND"` savestates

use crate::nds_core::{NdsMachine, NDS_HEIGHT, NDS_WIDTH};
use crate::traits::{
    pcm_rms_level, solid_obsidian_rgba, CpuDebugSnapshot, EmulatorBackend, FrameStepResult,
    DEFAULT_SAMPLE_RATE_HZ,
};
use zaimule_core::catalog::ConsoleKind;

pub const NDS_SCREEN_WIDTH: usize = NDS_WIDTH;
pub const NDS_SCREEN_HEIGHT: usize = NDS_HEIGHT;

pub struct NdsCore {
    inner: Option<NdsMachine>,
    frame_number: u64,
    stylus_x: u8,
    stylus_y: u8,
    stylus_down: bool,
    sram_cache: Vec<u8>,
    ram_cache: Vec<u8>,
    vram_cache: Vec<u8>,
    standby_rgba: Vec<u8>,
}

impl Default for NdsCore {
    fn default() -> Self {
        Self::new_empty()
    }
}

impl NdsCore {
    pub fn new() -> Self {
        Self::new_empty()
    }

    pub fn new_empty() -> Self {
        Self {
            inner: None,
            frame_number: 0,
            stylus_x: 128,
            stylus_y: 96,
            stylus_down: false,
            sram_cache: Vec::new(),
            ram_cache: Vec::new(),
            vram_cache: Vec::new(),
            standby_rgba: solid_obsidian_rgba(NDS_SCREEN_WIDTH, NDS_SCREEN_HEIGHT),
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.inner.is_some()
    }

    fn sync_debug_and_sram_caches(&mut self, force_sram: bool) {
        let Some(ref mut nds) = self.inner else {
            self.sram_cache.clear();
            self.ram_cache.clear();
            self.vram_cache.clear();
            return;
        };
        if force_sram || nds.is_sram_dirty() {
            self.sram_cache = nds.export_sram();
            nds.clear_sram_dirty();
        }
        self.ram_cache = nds.main_ram_copy(0, 0x8000);
        self.vram_cache = nds.vram_bank_copy(0, 0, 0x4000);
    }
}

impl EmulatorBackend for NdsCore {
    fn console(&self) -> ConsoleKind {
        ConsoleKind::Nds
    }

    fn load_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        let mut nds = NdsMachine::from_rom(rom)?;
        nds.set_audio_rate(DEFAULT_SAMPLE_RATE_HZ);
        self.frame_number = 0;
        self.stylus_down = false;
        self.inner = Some(nds);
        self.sync_debug_and_sram_caches(true);
        Ok(())
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        self.frame_number = self.frame_number.saturating_add(1);
        let frame_number = self.frame_number;

        let (rgba, audio_samples, audio_freq_hz) = if let Some(ref mut nds) = self.inner {
            let touch = if self.stylus_down {
                Some((u32::from(self.stylus_x), u32::from(self.stylus_y), true))
            } else {
                None
            };
            let rgba = nds.step_frame(buttons_mask, touch).to_vec();
            let mut pcm = Vec::with_capacity(1664);
            let freq = nds.drain_audio(&mut pcm);
            (rgba, pcm, freq)
        } else {
            (self.standby_rgba.clone(), Vec::new(), 0)
        };
        let audio_level = pcm_rms_level(&audio_samples);

        if self.inner.is_some() {
            self.sync_debug_and_sram_caches(false);
        }

        let cpu = self.debug_snapshot();

        FrameStepResult {
            console: ConsoleKind::Nds,
            frame_number,
            width: NDS_SCREEN_WIDTH as u16,
            height: NDS_SCREEN_HEIGHT as u16,
            rgba,
            audio_level,
            audio_freq_hz,
            audio_samples,
            sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
            serial_out_byte: None,
            cpu,
        }
    }

    fn save_state(&self) -> Result<Vec<u8>, String> {
        let Some(ref nds) = self.inner else {
            return Err("aucune ROM Nintendo DS chargée".into());
        };
        nds.save_state_bytes()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        let Some(ref mut nds) = self.inner else {
            return Err("aucune ROM Nintendo DS chargée".into());
        };
        nds.load_state_bytes(data)?;
        nds.set_audio_rate(DEFAULT_SAMPLE_RATE_HZ);
        self.frame_number = u64::from(nds.frame_count());
        self.sync_debug_and_sram_caches(true);
        Ok(())
    }

    fn sram(&self) -> &[u8] {
        &self.sram_cache
    }

    fn load_sram(&mut self, data: &[u8]) -> Result<(), String> {
        let Some(ref mut nds) = self.inner else {
            return Err("aucune ROM Nintendo DS chargée".into());
        };
        nds.load_sram(data);
        self.sync_debug_and_sram_caches(true);
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        incoming
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        let Some(ref nds) = self.inner else {
            return CpuDebugSnapshot {
                arch: "ARM946E-S + ARM7TDMI (En attente)".into(),
                pc_hex: "0x02000000".into(),
                sp_hex: "0x03002F7C".into(),
                reg_a_hex: "0x00000000".into(),
                reg_bc_hex: "0x00000000".into(),
                reg_de_hex: "0x00000000".into(),
                reg_hl_hex: "0x00000000".into(),
                flags_str: "STANDBY".into(),
                ly_scanline: 0,
                serial_sb_hex: "0x00".into(),
                serial_sc_hex: "0x00".into(),
                cycles: 0,
                score: 0,
            };
        };

        let regs = nds.arm9_regs();
        let cpsr = nds.arm9_cpsr();
        let code = nds.game_code();
        let code_tag = if code.is_empty() {
            "NTR".to_string()
        } else {
            code
        };
        let thumb = (cpsr & (1 << 5)) != 0;
        let flags = format!(
            "{}{}{}{}{} · {}",
            if (cpsr >> 31) & 1 != 0 { 'N' } else { '-' },
            if (cpsr >> 30) & 1 != 0 { 'Z' } else { '-' },
            if (cpsr >> 29) & 1 != 0 { 'C' } else { '-' },
            if (cpsr >> 28) & 1 != 0 { 'V' } else { '-' },
            if thumb { 'T' } else { 'A' },
            code_tag
        );
        let arch = if thumb {
            "ARM946E-S (NDS · Thumb · SPU 16-ch)"
        } else {
            "ARM946E-S + ARM7 (NDS · SPU 16-ch)"
        };

        CpuDebugSnapshot {
            arch: arch.into(),
            pc_hex: format!("0x{:08X}", regs[15]),
            sp_hex: format!("0x{:08X}", regs[13]),
            reg_a_hex: format!("0x{:08X}", regs[0]),
            reg_bc_hex: format!("0x{:08X}", regs[1]),
            reg_de_hex: format!("0x{:08X}", regs[2]),
            reg_hl_hex: format!("0x{:08X}", regs[14]),
            flags_str: flags,
            ly_scanline: 0,
            serial_sb_hex: "0x00".into(),
            serial_sc_hex: "0x00".into(),
            cycles: u64::from(nds.frame_count()) * 256,
            score: 0,
        }
    }

    fn apply_ram_cheat(&mut self, addr: u16, val: u8) {
        if let Some(ref mut nds) = self.inner {
            let offset = (addr as usize).wrapping_sub(0xC000) & 0x7FFF;
            nds.write_mem8(0x0200_0000 + offset as u32, val);
            if offset < self.ram_cache.len() {
                self.ram_cache[offset] = val;
            }
        }
    }

    fn read_ram_slice(&self) -> &[u8] {
        &self.ram_cache
    }

    fn read_vram_slice(&self) -> &[u8] {
        &self.vram_cache
    }

    fn write_vram_byte(&mut self, offset: usize, val: u8) {
        if let Some(ref mut nds) = self.inner {
            nds.vram_bank_write(0, offset, val);
            if offset < self.vram_cache.len() {
                self.vram_cache[offset] = val;
            }
        }
    }

    fn set_touch_stylus(&mut self, x: u8, y: u8, pressed: bool) {
        self.stylus_x = x;
        self.stylus_y = y.min(191);
        self.stylus_down = pressed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaimule_core::keymap::BTN_RIGHT;

    /// Builds a valid minimal Nitro NDS ROM whose ARM9 binary loads at `0x0200_0800`,
    /// writes `0x55` at `0x0200_0004`, and loops cleanly without crashing.
    fn minimal_nds_nitro_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x1000];
        rom[0x0000..0x000C].copy_from_slice(b"ZAIMULENDS\0\0");
        rom[0x000C..0x0010].copy_from_slice(b"ZMLN");

        let arm9_off: u32 = 0x200;
        let arm9_entry: u32 = 0x0200_0800;
        let arm9_addr: u32 = 0x0200_0800;
        let arm9_size: u32 = 0x20;
        let arm7_off: u32 = 0x400;
        let arm7_entry: u32 = 0x0380_0000;
        let arm7_addr: u32 = 0x0380_0000;
        let arm7_size: u32 = 0x10;

        rom[0x20..0x24].copy_from_slice(&arm9_off.to_le_bytes());
        rom[0x24..0x28].copy_from_slice(&arm9_entry.to_le_bytes());
        rom[0x28..0x2C].copy_from_slice(&arm9_addr.to_le_bytes());
        rom[0x2C..0x30].copy_from_slice(&arm9_size.to_le_bytes());
        rom[0x30..0x34].copy_from_slice(&arm7_off.to_le_bytes());
        rom[0x34..0x38].copy_from_slice(&arm7_entry.to_le_bytes());
        rom[0x38..0x3C].copy_from_slice(&arm7_addr.to_le_bytes());
        rom[0x3C..0x40].copy_from_slice(&arm7_size.to_le_bytes());

        // ARM9 code at 0x0200_0800:
        // MOV R0, #0x0200_0000 (0xE3A00402)
        // MOV R1, #0x55        (0xE3A01055)
        // STRB R1, [R0, #4]    (0xE5C01004)
        // B .                  (0xEAFFFFFE)
        let arm9_code: &[u32] = &[0xE3A0_0402, 0xE3A0_1055, 0xE5C0_1004, 0xEAFF_FFFE];
        for (i, w) in arm9_code.iter().enumerate() {
            let p = arm9_off as usize + i * 4;
            rom[p..p + 4].copy_from_slice(&w.to_le_bytes());
        }

        // ARM7 code at 0x0380_0000: B . (0xEAFFFFFE)
        rom[arm7_off as usize..arm7_off as usize + 4]
            .copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        rom
    }

    #[test]
    fn runs_real_nds_dual_screen_stylus_sram_and_zmnd_savestate() {
        let mut core = NdsCore::new_empty();
        assert_eq!(core.step_frame(0).height, 384);
        assert!(core.sram().is_empty());

        let rom = minimal_nds_nitro_rom();
        core.load_rom(&rom).unwrap();
        assert_eq!(core.console(), ConsoleKind::Nds);
        assert_eq!(core.sram().len(), 0x8_0000);

        core.set_touch_stylus(160, 120, true);
        let f1 = core.step_frame(BTN_RIGHT);
        assert_eq!(f1.width, 256);
        assert_eq!(f1.height, 384);
        assert_eq!(f1.rgba.len(), 256 * 384 * 4);
        assert!(!f1.audio_samples.is_empty());
        assert_eq!(f1.audio_samples.len() % 2, 0);
        assert_eq!(f1.sample_rate_hz, DEFAULT_SAMPLE_RATE_HZ);
        assert!(f1.cpu.flags_str.contains("ZMLN"));
        assert!(f1.cpu.arch.contains("SPU 16-ch"));
        assert_eq!(core.read_ram_slice()[4], 0x55);

        let state = core.save_state().unwrap();
        assert!(state.starts_with(b"ZMND"));

        core.apply_ram_cheat(0xC004, 0x77);
        assert_eq!(core.read_ram_slice()[4], 0x77);

        core.load_state(&state).unwrap();
        assert_eq!(core.read_ram_slice()[4], 0x55);
    }

    #[test]
    fn arm7_spu_synth_produces_audible_stereo_pcm_and_survives_savestate() {
        let mut rom = minimal_nds_nitro_rom();
        let arm7_off = 0x400usize;
        rom[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());

        // ARM7 program at 0x0380_0000: enables SOUNDCNT (0x807F) and PSG square ch 8 at 440 Hz.
        let arm7_prog: &[u32] = &[
            0xE3A0_0404, // MOV R0, #0x04000000
            0xE380_0B01, // ORR R0, R0, #0x400   ; 0x04000400
            0xE280_2C01, // ADD R2, R0, #0x100   ; 0x04000500 (SOUNDCNT)
            0xE59F_1014, // LDR R1, [PC, #0x14]  ; 0x0000807F
            0xE1C2_10B0, // STRH R1, [R2]
            0xE59F_1010, // LDR R1, [PC, #0x10]  ; 0x0000ED68 (440 Hz)
            0xE1C0_18B8, // STRH R1, [R0, #0x88] ; SOUND8TMR
            0xE59F_100C, // LDR R1, [PC, #0x0C]  ; 0xE440007F (PSG ch8 active, pan 64, vol 127)
            0xE580_1080, // STR R1, [R0, #0x80]  ; SOUND8CNT
            0xEAFF_FFFE, // B .
            0x0000_807F, // literal SOUNDCNT
            0x0000_ED68, // literal SOUND8TMR (-4760)
            0xE440_007F, // literal SOUND8CNT
        ];
        for (i, w) in arm7_prog.iter().enumerate() {
            let p = arm7_off + i * 4;
            rom[p..p + 4].copy_from_slice(&w.to_le_bytes());
        }

        let mut core = NdsCore::new_empty();
        core.load_rom(&rom).unwrap();
        let f1 = core.step_frame(0);
        assert!(
            f1.audio_level > 0.01,
            "expected audible SPU level, got {}",
            f1.audio_level
        );
        assert!(
            (430..=450).contains(&f1.audio_freq_hz),
            "unexpected freq {} Hz",
            f1.audio_freq_hz
        );
        assert!(f1.audio_samples.len() >= 1500);
        let peak = f1.audio_samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(peak > 0.02);

        let saved = core.save_state().unwrap();
        core.load_rom(&minimal_nds_nitro_rom()).unwrap();
        let silent = core.step_frame(0);
        assert!(silent.audio_level < 0.001);

        core.load_state(&saved).unwrap();
        let resumed = core.step_frame(0);
        assert!(resumed.audio_level > 0.01);
        assert!((430..=450).contains(&resumed.audio_freq_hz));
    }
}
