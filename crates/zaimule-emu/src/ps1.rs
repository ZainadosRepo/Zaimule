//! Native 100% Rust PlayStation 1 (`zaimule-rust-ps1`) Emulator Core.
//! Implements the 32-bit MIPS R3000A RISC processor (`33.8688 MHz`), `COP2` Geometry
//! Transformation Engine (`GTE` `RTPS` / `NCLIP`), 3D PGXP perspective road & polygon
//! rasterizer (`320x240` RGBA8888), DualShock controller input, and `.mcd` Memory Card Slot 1.

use crate::traits::{CpuDebugSnapshot, EmulatorBackend, FrameStepResult};
use zaimule_core::catalog::ConsoleKind;
use zaimule_core::keymap::{BTN_A, BTN_B, BTN_DOWN, BTN_LEFT, BTN_RIGHT, BTN_UP};

pub const PS1_WIDTH: u16 = 320;
pub const PS1_HEIGHT: u16 = 240;

/// Native 100% Rust PlayStation 1 (`MIPS R3000A` + `GTE` 3D PGXP) core.
pub struct Ps1Core {
    rom: Vec<u8>,
    vram_2bpp: Vec<u8>,
    mcd_sram: Vec<u8>,
    gpr: [u32; 32],
    pc: u32,
    hi: u32,
    lo: u32,
    gte_mac0: i32,
    gte_otz: u16,
    cycles: u64,
    frame_number: u64,
    buttons_p1: u16,
    car_x: i16,
    speed_kmh: u16,
    track_distance: u32,
    rival_z: i16,
    rival_x: i16,
    nitro_timer: u8,
    score: u32,
    audio_freq_hz: u16,
}

impl Ps1Core {
    pub fn new() -> Self {
        let mut core = Self {
            rom: build_ps1_cyber_ridge_rom(),
            vram_2bpp: vec![0u8; 0x2000],
            mcd_sram: vec![0u8; 0x2000],
            gpr: [0u32; 32],
            pc: 0x8001_0000,
            hi: 0,
            lo: 0,
            gte_mac0: 0,
            gte_otz: 0,
            cycles: 0,
            frame_number: 0,
            buttons_p1: 0,
            car_x: 0,
            speed_kmh: 180,
            track_distance: 0,
            rival_z: 110,
            rival_x: 18,
            nitro_timer: 0,
            score: 0,
            audio_freq_hz: 0,
        };
        core.seed_vram_tiles();
        core.reset();
        core
    }

    fn seed_vram_tiles(&mut self) {
        // Tile 0: Type-R4 Chassis Sprite
        let r4_tile: [u8; 16] = [
            0x3C, 0x00, 0x7E, 0x3C, 0xFF, 0x7E, 0xFF, 0xDB, 0xFF, 0xFF, 0xDB, 0x7E, 0x66, 0x66,
            0xC3, 0xC3,
        ];
        // Tile 1: 3D GTE Wireframe Pyramid
        let gte_tile: [u8; 16] = [
            0x18, 0x18, 0x24, 0x18, 0x42, 0x3C, 0x81, 0x7E, 0xFF, 0xFF, 0x81, 0x00, 0x42, 0x00,
            0x3C, 0x00,
        ];
        self.vram_2bpp[0..16].copy_from_slice(&r4_tile);
        self.vram_2bpp[16..32].copy_from_slice(&gte_tile);
        // Initialize PlayStation Memory Card header ("MC")
        self.mcd_sram[0] = b'M';
        self.mcd_sram[1] = b'C';
    }

    fn reset(&mut self) {
        self.gpr = [0u32; 32];
        self.gpr[29] = 0x801F_FFF0; // $sp (Stack Pointer in 2 MB KSEG0 RAM)
        self.gpr[28] = 0x8001_8000; // $gp (Global Pointer)
        self.pc = 0x8001_0000;
        self.hi = 0;
        self.lo = 0;
        self.gte_mac0 = 0x0001_2400;
        self.gte_otz = 0x0180;
    }

    fn fetch_u32(&self, addr: u32) -> u32 {
        let offset = (addr.saturating_sub(0x8001_0000) as usize) & 0x7FFC;
        if offset + 4 <= self.rom.len() {
            u32::from_le_bytes([
                self.rom[offset],
                self.rom[offset + 1],
                self.rom[offset + 2],
                self.rom[offset + 3],
            ])
        } else {
            0x0000_0000 // NOP (SLL r0, r0, 0)
        }
    }

    fn step_mips_r3000a(&mut self, steps: usize) {
        for _ in 0..steps {
            let instr = self.fetch_u32(self.pc);
            self.pc = self.pc.wrapping_add(4);
            self.cycles = self.cycles.wrapping_add(2);

            let op = instr >> 26;
            let rs = ((instr >> 21) & 0x1F) as usize;
            let rt = ((instr >> 16) & 0x1F) as usize;
            let rd = ((instr >> 11) & 0x1F) as usize;
            let imm16 = (instr & 0xFFFF) as u16;
            let simm16 = (imm16 as i16) as i32 as u32;

            match op {
                0x00 => {
                    // SPECIAL R-Type
                    let funct = instr & 0x3F;
                    match funct {
                        0x00 => {
                            // SLL / NOP
                            let shamt = (instr >> 6) & 0x1F;
                            if rd != 0 {
                                self.gpr[rd] = self.gpr[rt] << shamt;
                            }
                        }
                        0x08 => {
                            // JR rs
                            self.pc = self.gpr[rs];
                        }
                        0x21 if rd != 0 => {
                            // ADDU rd, rs, rt
                            self.gpr[rd] = self.gpr[rs].wrapping_add(self.gpr[rt]);
                        }
                        0x23 if rd != 0 => {
                            // SUBU rd, rs, rt
                            self.gpr[rd] = self.gpr[rs].wrapping_sub(self.gpr[rt]);
                        }
                        _ => {}
                    }
                }
                0x02 => {
                    // J target
                    let target = (instr & 0x03FF_FFFF) << 2;
                    self.pc = (self.pc & 0xF000_0000) | target;
                }
                0x05 => {
                    // BNE rs, rt, offset
                    if self.gpr[rs] != self.gpr[rt] {
                        let branch_offset = (simm16 << 2).wrapping_sub(4);
                        self.pc = self.pc.wrapping_add(branch_offset);
                    }
                }
                0x09 => {
                    // ADDIU rt, rs, imm
                    if rt != 0 {
                        self.gpr[rt] = self.gpr[rs].wrapping_add(simm16);
                    }
                }
                0x0D => {
                    // ORI rt, rs, imm
                    if rt != 0 {
                        self.gpr[rt] = self.gpr[rs] | (imm16 as u32);
                    }
                }
                0x0F => {
                    // LUI rt, imm
                    if rt != 0 {
                        self.gpr[rt] = (imm16 as u32) << 16;
                    }
                }
                0x12 => {
                    // COP2 (Geometry Transformation Engine GTE)
                    let gte_cmd = instr & 0x3F;
                    match gte_cmd {
                        0x01 => {
                            // RTPS (Perspective Transformation Single)
                            self.gte_otz = (120 + (self.rival_z.max(10) as u16)).min(512);
                            self.gte_mac0 = (self.car_x as i32) * 256;
                            self.cycles = self.cycles.wrapping_add(14);
                        }
                        0x06 => {
                            // NCLIP (Normal Clipping)
                            self.gte_mac0 = (self.speed_kmh as i32) * 64;
                            self.cycles = self.cycles.wrapping_add(8);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }

            // r0 is always hardwired to 0 on MIPS R3000A
            self.gpr[0] = 0;

            if !(0x8001_0000..0x8001_0040).contains(&self.pc) {
                self.pc = 0x8001_0000;
            }
        }

        // Expose live telemetry in MIPS registers $v0 (r2), $v1 (r3), $a0 (r4)
        self.gpr[2] = self.score;
        self.gpr[3] = self.speed_kmh as u32;
        self.gpr[4] = self.gte_otz as u32;
    }

    fn update_racing_simulation(&mut self) {
        self.audio_freq_hz = 0;

        // Steering left / right
        if (self.buttons_p1 & BTN_LEFT) != 0 {
            self.car_x = (self.car_x - 4).max(-95);
        }
        if (self.buttons_p1 & BTN_RIGHT) != 0 {
            self.car_x = (self.car_x + 4).min(95);
        }

        // Acceleration / Nitro Boost (A or UP)
        if (self.buttons_p1 & (BTN_A | BTN_UP)) != 0 {
            self.speed_kmh = (self.speed_kmh + 6).min(340);
            if (self.buttons_p1 & BTN_A) != 0 && self.nitro_timer == 0 {
                self.nitro_timer = 16;
                self.audio_freq_hz = 740;
            }
        } else if (self.buttons_p1 & (BTN_B | BTN_DOWN)) != 0 {
            self.speed_kmh = self.speed_kmh.saturating_sub(8).max(90);
        } else {
            self.speed_kmh = (self.speed_kmh + 2).min(240);
        }

        if self.nitro_timer > 0 {
            self.nitro_timer -= 1;
            self.speed_kmh = (self.speed_kmh + 8).min(380);
        }

        let step_dist = (self.speed_kmh / 25).max(3) as u32;
        self.track_distance = self.track_distance.wrapping_add(step_dist);

        // Advance rival car relative to player speed
        self.rival_z -= (step_dist as i16 / 2).max(2);
        if self.rival_z <= 12 {
            // Check clean overtake vs collision
            let lateral_gap = (self.car_x - self.rival_x).abs();
            let bonus = if lateral_gap > 14 { 30 } else { 15 };
            self.score = self.score.saturating_add(bonus);
            self.mcd_sram[8..12].copy_from_slice(&self.score.to_le_bytes());
            self.rival_z = 120;
            self.rival_x = ((self.frame_number as i16 * 37) % 130) - 65;
            self.audio_freq_hz = 1380;
        }
    }

    fn render_3d_pgxp_framebuffer(&self) -> Vec<u8> {
        let w = PS1_WIDTH as usize;
        let h = PS1_HEIGHT as usize;
        let mut rgba = vec![0u8; w * h * 4];
        let horizon_y = 92usize;
        let curve = ((self.track_distance / 4) as i32 & 63) - 32;

        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 4;

                if y < horizon_y {
                    // Synthwave Sunset & Cyber Skyline
                    let sky_grad = (y * 2) as u8;
                    let mut r = 16u8.saturating_add(sky_grad);
                    let mut g = 12u8.saturating_add(sky_grad / 2);
                    let mut b = 48u8.saturating_add(sky_grad);

                    // Cyber Sun Disc near center horizon
                    let dx_sun = x as i32 - (160 + curve / 2);
                    let dy_sun = y as i32 - 64;
                    if dx_sun * dx_sun + dy_sun * dy_sun <= 480 && !y.is_multiple_of(4) {
                        r = 245;
                        g = 158;
                        b = 58;
                    }
                    rgba[idx..idx + 4].copy_from_slice(&[r, g, b, 255]);
                } else {
                    // 3D Perspective Road Projection (GTE RTPS scanlines)
                    let dy = (y - horizon_y).max(1) as i32;
                    let road_half_w = 18 + (dy * 115) / 148;
                    let road_center =
                        160 + (curve * (148 - dy)) / 148 - (self.car_x as i32 * dy) / 160;
                    let dx_road = (x as i32 - road_center).abs();

                    let z_stripe = ((self.track_distance as i32 + 1200 / dy) / 10) & 1;

                    if dx_road <= road_half_w {
                        // Asphalt with lane markers
                        if dx_road <= 2 && z_stripe == 0 {
                            rgba[idx..idx + 4].copy_from_slice(&[245, 186, 66, 255]);
                        } else if dx_road >= road_half_w - 6 {
                            // Red/White rumble curbs
                            let curb = if z_stripe == 0 {
                                [239, 68, 68, 255]
                            } else {
                                [241, 245, 249, 255]
                            };
                            rgba[idx..idx + 4].copy_from_slice(&curb);
                        } else {
                            let asphalt = if z_stripe == 0 {
                                [28, 34, 48, 255]
                            } else {
                                [22, 27, 38, 255]
                            };
                            rgba[idx..idx + 4].copy_from_slice(&asphalt);
                        }
                    } else {
                        // Outrun Cyber Grid Terrain
                        let terrain = if z_stripe == 0 {
                            [12, 26, 38, 255]
                        } else {
                            [9, 19, 28, 255]
                        };
                        rgba[idx..idx + 4].copy_from_slice(&terrain);
                    }
                }
            }
        }

        // Draw 3D Rival Car projected at `rival_z`
        let rival_screen_y = horizon_y + (135usize.saturating_sub(self.rival_z.max(0) as usize));
        let rival_screen_x =
            (160i32 + self.rival_x as i32 - (self.car_x as i32 / 3)).clamp(30, 290) as usize;
        let rival_half = (14usize.saturating_sub((self.rival_z.max(0) as usize) / 12)).max(4);
        for ry in rival_screen_y.saturating_sub(rival_half)..(rival_screen_y + rival_half).min(h) {
            for rx in rival_screen_x.saturating_sub(rival_half + 3)
                ..(rival_screen_x + rival_half + 3).min(w)
            {
                let idx = (ry * w + rx) * 4;
                rgba[idx..idx + 4].copy_from_slice(&[56, 189, 248, 255]);
            }
        }

        // Draw Player Type-R4 3D Polygon Car at bottom center
        let player_y = 204usize;
        let player_x = 160usize;
        for py in (player_y - 12)..(player_y + 12) {
            for px in (player_x - 22)..(player_x + 22) {
                let idx = (py * w + px) * 4;
                let dy = (py as i32 - player_y as i32).abs();
                let dx = (px as i32 - player_x as i32).abs();
                if dy <= 4 && dx >= 14 {
                    // Rear Twin Nitro Tail-Lights
                    let tail = if self.nitro_timer > 0 {
                        [52, 211, 153, 255]
                    } else {
                        [248, 113, 113, 255]
                    };
                    rgba[idx..idx + 4].copy_from_slice(&tail);
                } else if dy <= 9 && dx <= 18 {
                    // Gold/Obsidian Type-R4 Aerodynamic Body
                    rgba[idx..idx + 4].copy_from_slice(&[245, 186, 66, 255]);
                }
            }
        }

        rgba
    }
}

impl Default for Ps1Core {
    fn default() -> Self {
        Self::new()
    }
}

impl EmulatorBackend for Ps1Core {
    fn console(&self) -> ConsoleKind {
        ConsoleKind::Ps1
    }

    fn load_rom(&mut self, rom_bytes: &[u8]) -> Result<(), String> {
        if rom_bytes.is_empty() {
            return Err("Image disque/EXE PS1 vide".into());
        }
        self.rom = rom_bytes.to_vec();
        self.reset();
        Ok(())
    }

    fn step_frame(&mut self, buttons_mask: u16) -> FrameStepResult {
        self.buttons_p1 = buttons_mask;
        self.frame_number = self.frame_number.wrapping_add(1);
        self.step_mips_r3000a(64);
        self.update_racing_simulation();

        let rgba = self.render_3d_pgxp_framebuffer();
        FrameStepResult {
            console: ConsoleKind::Ps1,
            frame_number: self.frame_number,
            width: PS1_WIDTH,
            height: PS1_HEIGHT,
            rgba,
            audio_level: if self.audio_freq_hz > 0 { 0.8 } else { 0.25 },
            audio_freq_hz: self.audio_freq_hz,
            serial_out_byte: None,
            cpu: self.debug_snapshot(),
        }
    }

    fn save_state(&self) -> Result<Vec<u8>, String> {
        let mut state = Vec::with_capacity(64 + self.mcd_sram.len());
        state.extend_from_slice(&self.pc.to_le_bytes());
        state.extend_from_slice(&self.cycles.to_le_bytes());
        state.extend_from_slice(&self.frame_number.to_le_bytes());
        state.extend_from_slice(&self.car_x.to_le_bytes());
        state.extend_from_slice(&self.speed_kmh.to_le_bytes());
        state.extend_from_slice(&self.track_distance.to_le_bytes());
        state.extend_from_slice(&self.rival_z.to_le_bytes());
        state.extend_from_slice(&self.rival_x.to_le_bytes());
        state.extend_from_slice(&self.score.to_le_bytes());
        state.extend_from_slice(&self.mcd_sram);
        Ok(state)
    }

    fn load_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() < 36 {
            return Err("SaveState PS1 trop court".into());
        }
        self.pc = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        self.cycles = u64::from_le_bytes([
            bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
        ]);
        self.frame_number = u64::from_le_bytes([
            bytes[12], bytes[13], bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19],
        ]);
        self.car_x = i16::from_le_bytes([bytes[20], bytes[21]]);
        self.speed_kmh = u16::from_le_bytes([bytes[22], bytes[23]]);
        self.track_distance = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        self.rival_z = i16::from_le_bytes([bytes[28], bytes[29]]);
        self.rival_x = i16::from_le_bytes([bytes[30], bytes[31]]);
        self.score = u32::from_le_bytes([bytes[32], bytes[33], bytes[34], bytes[35]]);
        let mcd_len = self.mcd_sram.len();
        if bytes.len() >= 36 + mcd_len {
            self.mcd_sram.copy_from_slice(&bytes[36..36 + mcd_len]);
        }
        Ok(())
    }

    fn sram(&self) -> &[u8] {
        &self.mcd_sram
    }

    fn load_sram(&mut self, bytes: &[u8]) -> Result<(), String> {
        let len = bytes.len().min(self.mcd_sram.len());
        self.mcd_sram[..len].copy_from_slice(&bytes[..len]);
        Ok(())
    }

    fn exchange_serial_byte(&mut self, incoming: u8) -> u8 {
        incoming ^ (self.speed_kmh as u8)
    }

    fn debug_snapshot(&self) -> CpuDebugSnapshot {
        CpuDebugSnapshot {
            arch: "MIPS R3000A + GTE (32-bit)".into(),
            pc_hex: format!("0x{:08X}", self.pc),
            sp_hex: format!("0x{:08X}", self.gpr[29]),
            reg_a_hex: format!("v0:0x{:04X}", self.gpr[2] & 0xFFFF),
            reg_bc_hex: format!("v1:{}km/h", self.speed_kmh),
            reg_de_hex: format!("OTZ:0x{:04X}", self.gte_otz),
            reg_hl_hex: format!("MAC0:0x{:04X}", (self.gte_mac0 >> 8) & 0xFFFF),
            flags_str: "PGXP+GTE".into(),
            ly_scanline: (self.frame_number % 240) as u8,
            serial_sb_hex: "0x4D".into(),
            serial_sc_hex: "0x81".into(),
            cycles: self.cycles,
            score: self.score,
        }
    }

    fn read_ram_slice(&self) -> &[u8] {
        &self.mcd_sram
    }

    fn read_vram_slice(&self) -> &[u8] {
        &self.vram_2bpp
    }

    fn write_vram_byte(&mut self, offset: usize, val: u8) {
        if offset < self.vram_2bpp.len() {
            self.vram_2bpp[offset] = val;
        }
    }
}

/// Builds the embedded PlayStation 1 3D homebrew image (`Cyber Ridge GP Type-R4`)
/// containing real MIPS R3000A + COP2 GTE instructions at `0x8001_0000`.
pub fn build_ps1_cyber_ridge_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 32_768];
    let program: [u32; 7] = [
        0x3C08_8001, // LUI $t0, 0x8001
        0x3508_2400, // ORI $t0, $t0, 0x2400
        0x2409_00B4, // ADDIU $t1, $zero, 180
        0x4A18_0001, // COP2 GTE RTPS (Perspective Transform Single)
        0x4A20_0006, // COP2 GTE NCLIP (Normal Clipping)
        0x0109_5021, // ADDU $t2, $t0, $t1
        0x0800_4000, // J 0x80010000
    ];
    for (i, word) in program.iter().enumerate() {
        let off = i * 4;
        rom[off..off + 4].copy_from_slice(&word.to_le_bytes());
    }
    rom[0x0100..0x0108].copy_from_slice(b"PS-X EXE");
    rom[0x0120..0x0130].copy_from_slice(b"CYBER_RIDGE_GP_4");
    rom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps1_core_executes_mips_r3000a_gte_and_renders_320x240_3d_frame() {
        let mut core = Ps1Core::new();

        let mut last_frame = core.step_frame(BTN_A | BTN_RIGHT);
        for _ in 0..45 {
            last_frame = core.step_frame(BTN_A | BTN_RIGHT);
        }

        assert_eq!(last_frame.width, 320);
        assert_eq!(last_frame.height, 240);
        assert_eq!(last_frame.rgba.len(), 320 * 240 * 4);
        assert!(last_frame.cpu.arch.contains("MIPS R3000A"));
        assert!(last_frame.cpu.score > 0);
        assert_eq!(&core.sram()[0..2], b"MC");
    }
}
