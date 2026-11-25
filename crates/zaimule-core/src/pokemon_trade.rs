//! Moteur d'inspection, d'échange atomique et d'évolution par échange pour sauvegardes Pokémon
//! officielles (`.sav` / `.srm` / `.dsv`) :
//! - **Génération 1 (GB)** : Rouge / Bleu / Jaune (`32 KiB`, sommes de contrôle Banques 1/2/3).
//! - **Génération 2 (GBC)** : Or / Argent / Cristal (`32 KiB` + pied RTC MBC3 optionnel préservé,
//!   14 boîtes PC × 20 emplacements, sommes de contrôle 16 bits primaire & sauvegarde).
//! - **Génération 3 (GBA)** : Rubis / Saphir / Émeraude / Rouge Feu / Vert Feuille (`128 KiB`,
//!   14 sections × 4 KiB par slot A/B, tampon PC sections `5..=13`, `.pk3` chiffrés XOR `PID ^ OT`).
//! - **Génération 4 (NDS)** : Diamant / Perle, Platine et **Or HeartGold / Argent SoulSilver**
//!   (`512 KiB`, double partition `+0x00000` / `+0x40000`, CRC-16-CCITT, `.pk4` chiffrés PRNG LCG
//!   avec permutation 4 blocs ABCD et pas de boîte `0x1000` spécifique HGSS).
//!
//! Garantie anti-corruption : toute lecture et toute écriture vérifient intégralement les sommes
//! de contrôle matérielles avant ET après mutation. L'appelant applique en plus une rotation de
//! sauvegardes `.srm.bak1..3` sur disque avant de remplacer le fichier actif.

use base64::engine::general_purpose::STANDARD as B64_STD;
use base64::Engine as _;
use ring::digest;
use serde::{Deserialize, Serialize};

pub const GEN1_SAVE_LEN: usize = 0x8000;
pub const GEN2_SAVE_LEN: usize = 0x8000;
const GEN2_MAX_RTC_LEN: usize = GEN2_SAVE_LEN + 64;
pub const GEN3_SAVE_LEN: usize = 0x20000;
pub const GEN4_SAVE_LEN: usize = 0x80000;
const GEN4_MAX_DSV_LEN: usize = GEN4_SAVE_LEN + 256;

const GEN1_MAIN_START: usize = 0x2598;
const GEN1_MAIN_END_INCL: usize = 0x3522;
const GEN1_MAIN_CHECKSUM: usize = 0x3523;
const GEN1_CURRENT_BOX_NUM: usize = 0x284C;
const GEN1_CURRENT_BOX_OFFSET: usize = 0x30C0;
const GEN1_BOX_SIZE: usize = 0x462;
const GEN1_BOX_SLOTS: usize = 20;
const GEN1_BANK2_START: usize = 0x4000;
const GEN1_BANK2_CHECKSUM: usize = 0x5A4C;
const GEN1_BANK3_START: usize = 0x6000;
const GEN1_BANK3_CHECKSUM: usize = 0x7A4C;
const GEN1_PK_WIRE_LEN: usize = 55; // 33 struct + 11 OT + 11 nickname

const GEN2_BOX_COUNT: usize = 14;
const GEN2_BOX_SLOTS: usize = 20;
const GEN2_BOX_SIZE: usize = 1102; // 1 count + 21 species + 20*32 struct + 20*11 OT + 20*11 nick
const GEN2_BOX_STRIDE: usize = 0x450;
const GEN2_BANK2_START: usize = 0x4000;
const GEN2_BANK3_START: usize = 0x6000;
const GEN2_PK_WIRE_LEN: usize = 55; // 1 list_species (0xFD = Œuf) + 32 struct + 11 OT + 11 nickname
const GEN2_MAGIC_1_OFFSET: usize = 0x2008;
const GEN2_MAGIC_1_VALUE: u8 = 0x63;
const GEN2_MAGIC_2_VALUE: u8 = 0x7F;
const GEN2_GS_CURRENT_BOX_NUM: usize = 0x2724;
const GEN2_GS_CURRENT_BOX_OFFSET: usize = 0x2D6C;
const GEN2_GS_CHECKSUM_1_OFFSET: usize = 0x2D69;
const GEN2_GS_MAGIC_2_OFFSET: usize = 0x2D6B;
const GEN2_GS_CHECKSUM_2_OFFSET: usize = 0x7E6D;
const GEN2_CRYSTAL_CURRENT_BOX_NUM: usize = 0x2700;
const GEN2_CRYSTAL_CURRENT_BOX_OFFSET: usize = 0x2D10;
const GEN2_CRYSTAL_CHECKSUM_1_OFFSET: usize = 0x2D0D;
const GEN2_CRYSTAL_MAGIC_2_OFFSET: usize = 0x2D0F;
const GEN2_CRYSTAL_CHECKSUM_2_OFFSET: usize = 0x1F0D;

const GEN3_SLOT_STRIDE: usize = 0xE000;
const GEN3_SECTION_SIZE: usize = 0x1000;
const GEN3_SECTION_COUNT: usize = 14;
const GEN3_SIGNATURE: u32 = 0x0801_2025;
const GEN3_BOX_COUNT: usize = 14;
const GEN3_SLOTS_PER_BOX: usize = 30;
const GEN3_PK_LEN: usize = 80;

const GEN4_PARTITION_STRIDE: usize = 0x40000;
const GEN4_BOX_COUNT: usize = 18;
const GEN4_SLOTS_PER_BOX: usize = 30;
const GEN4_PK_LEN: usize = 136;
const GEN4_HGSS_BOX_STRIDE: usize = 0x1000;

/// Format officiel détecté depuis l'empreinte binaire et les sommes de contrôle de la sauvegarde.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PokemonSaveFormat {
    Gen1RedBlueYellow,
    Gen2GoldSilver,
    Gen2Crystal,
    Gen3RubySapphire,
    Gen3Emerald,
    Gen3FireRedLeafGreen,
    Gen4DiamondPearl,
    Gen4Platinum,
    Gen4HeartGoldSoulSilver,
}

impl PokemonSaveFormat {
    pub fn generation(self) -> u8 {
        match self {
            Self::Gen1RedBlueYellow => 1,
            Self::Gen2GoldSilver | Self::Gen2Crystal => 2,
            Self::Gen3RubySapphire | Self::Gen3Emerald | Self::Gen3FireRedLeafGreen => 3,
            Self::Gen4DiamondPearl | Self::Gen4Platinum | Self::Gen4HeartGoldSoulSilver => 4,
        }
    }

    pub fn label_fr(self) -> &'static str {
        match self {
            Self::Gen1RedBlueYellow => "Pokémon Rouge / Bleu / Jaune (Gen 1 · GB)",
            Self::Gen2GoldSilver => "Pokémon Or / Argent (Gen 2 · GBC)",
            Self::Gen2Crystal => "Pokémon Cristal (Gen 2 · GBC)",
            Self::Gen3RubySapphire => "Pokémon Rubis / Saphir (Gen 3 · GBA)",
            Self::Gen3Emerald => "Pokémon Émeraude (Gen 3 · GBA)",
            Self::Gen3FireRedLeafGreen => "Pokémon Rouge Feu / Vert Feuille (Gen 3 · GBA)",
            Self::Gen4DiamondPearl => "Pokémon Diamant / Perle (Gen 4 · NDS)",
            Self::Gen4Platinum => "Pokémon Platine (Gen 4 · NDS)",
            Self::Gen4HeartGoldSoulSilver => {
                "Pokémon Or HeartGold / Argent SoulSilver (Gen 4 · NDS)"
            }
        }
    }

    pub fn box_count(self) -> u8 {
        match self.generation() {
            1 => 12,
            2 => GEN2_BOX_COUNT as u8,
            3 => GEN3_BOX_COUNT as u8,
            _ => GEN4_BOX_COUNT as u8,
        }
    }

    pub fn slots_per_box(self) -> u8 {
        match self.generation() {
            1 => GEN1_BOX_SLOTS as u8,
            2 => GEN2_BOX_SLOTS as u8,
            _ => GEN3_SLOTS_PER_BOX as u8,
        }
    }

    pub fn is_compatible_with(self, other: Self) -> bool {
        self.generation() == other.generation()
    }
}

/// Résumé lisible d'un Pokémon stocké dans une boîte PC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PokemonBoxSlotSummary {
    pub box_index: u8,
    pub slot_index: u8,
    pub species_id: u16,
    pub species_name_fr: String,
    pub nickname: String,
    pub ot_name: String,
    pub level_approx: u8,
    pub held_item_id: u16,
    pub held_item_name_fr: Option<String>,
    pub is_shiny: bool,
    pub is_egg: bool,
    pub trade_evolution_preview: Option<String>,
    pub raw_pk_bytes_len: usize,
}

/// Inventaire validé des boîtes PC d'une sauvegarde Pokémon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PokemonSaveInventory {
    pub format: PokemonSaveFormat,
    pub generation: u8,
    pub format_label: String,
    pub trainer_name: String,
    pub active_slot_label: String,
    pub box_count: u8,
    pub slots_per_box: u8,
    pub current_box: u8,
    pub pokemon: Vec<PokemonBoxSlotSummary>,
}

/// Pokémon extrait prêt à être transmis dans un message E2EE `ZMR1` (`RelayChannel::TradeHub`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractedPokemon {
    pub format: PokemonSaveFormat,
    pub generation: u8,
    pub summary: PokemonBoxSlotSummary,
    pub raw_pk_b64: String,
    pub offer_sha256: String,
}

/// Résultat d'une écriture de Pokémon reçu dans la sauvegarde cible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeWriteOutcome {
    pub updated_save_bytes: Vec<u8>,
    pub received_species_before_fr: String,
    pub received_species_after_fr: String,
    pub evolved: bool,
    pub consumed_item_name_fr: Option<String>,
}

/// Messages JSON chiffrés de bout en bout dans `ZMR1` sur `RelayChannel::TradeHub = 4`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TradeHubWireMessage {
    Offer {
        trade_id: String,
        trainer_name: String,
        pokemon: ExtractedPokemon,
    },
    Accept {
        trade_id: String,
        expected_peer_offer_sha256: String,
        allow_evolution: bool,
    },
    Cancel {
        trade_id: String,
        reason: String,
    },
    Committed {
        trade_id: String,
    },
}

/// Calcule l'empreinte SHA-256 hexadécimale d'un Pokémon sérialisé (utilisée pour verrouiller
/// cryptographiquement l'acceptation mutuelle et empêcher toute substitution de dernière seconde).
pub fn compute_offer_sha256(format: PokemonSaveFormat, raw_pk_bytes: &[u8]) -> String {
    let mut ctx = digest::Context::new(&digest::SHA256);
    ctx.update(&[format.generation()]);
    ctx.update(raw_pk_bytes);
    let d = ctx.finish();
    let mut hex = String::with_capacity(64);
    for b in d.as_ref() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Détecte le format de sauvegarde Pokémon officiel et vérifie ses sommes de contrôle.
pub fn detect_pokemon_save_format(bytes: &[u8]) -> Result<PokemonSaveFormat, String> {
    if bytes.len() >= GEN2_SAVE_LEN && bytes.len() <= GEN2_MAX_RTC_LEN {
        if let Some(gen2_fmt) = detect_gen2_save_format(&bytes[..GEN2_SAVE_LEN]) {
            return Ok(gen2_fmt);
        }
        if bytes.len() == GEN1_SAVE_LEN && verify_gen1_save(bytes) {
            return Ok(PokemonSaveFormat::Gen1RedBlueYellow);
        }
        return Err(
            "sauvegarde 32 Ko détectée mais somme de contrôle Pokémon Gen 1 (Rouge/Bleu/Jaune) ou Gen 2 (Or/Argent/Cristal) invalide"
                .into(),
        );
    }
    if bytes.len() == GEN3_SAVE_LEN {
        let (_, fmt) = select_gen3_active_slot(bytes)?;
        return Ok(fmt);
    }
    if bytes.len() >= GEN4_SAVE_LEN && bytes.len() <= GEN4_MAX_DSV_LEN {
        let layout = select_gen4_active_layout(&bytes[..GEN4_SAVE_LEN])?;
        return Ok(layout.format);
    }
    Err(format!(
        "taille de sauvegarde non reconnue ({} octets ; attendu : 32 Ko Gen 1/2, 128 Ko Gen 3 ou 512 Ko Gen 4)",
        bytes.len()
    ))
}

/// Inspecte une sauvegarde Pokémon `.sav`/`.srm`, vérifie les sommes de contrôle et liste tous
/// les Pokémon présents dans les boîtes PC.
pub fn inspect_pokemon_save(bytes: &[u8]) -> Result<PokemonSaveInventory, String> {
    let fmt = detect_pokemon_save_format(bytes)?;
    match fmt {
        PokemonSaveFormat::Gen1RedBlueYellow => inspect_gen1_save(bytes),
        PokemonSaveFormat::Gen2GoldSilver | PokemonSaveFormat::Gen2Crystal => {
            inspect_gen2_save(bytes, fmt)
        }
        PokemonSaveFormat::Gen3RubySapphire
        | PokemonSaveFormat::Gen3Emerald
        | PokemonSaveFormat::Gen3FireRedLeafGreen => inspect_gen3_save(bytes),
        PokemonSaveFormat::Gen4DiamondPearl
        | PokemonSaveFormat::Gen4Platinum
        | PokemonSaveFormat::Gen4HeartGoldSoulSilver => inspect_gen4_save(bytes),
    }
}

/// Extrait un Pokémon précis d'une boîte PC pour le proposer à l'échange E2EE.
pub fn extract_boxed_pokemon(
    bytes: &[u8],
    box_index: u8,
    slot_index: u8,
) -> Result<ExtractedPokemon, String> {
    let fmt = detect_pokemon_save_format(bytes)?;
    if box_index >= fmt.box_count() || slot_index >= fmt.slots_per_box() {
        return Err(format!(
            "emplacement PC hors limites (Boîte {}, Emplacement {})",
            box_index + 1,
            slot_index + 1
        ));
    }
    let (summary, raw_bytes) = match fmt {
        PokemonSaveFormat::Gen1RedBlueYellow => extract_gen1_slot(bytes, box_index, slot_index)?,
        PokemonSaveFormat::Gen2GoldSilver | PokemonSaveFormat::Gen2Crystal => {
            extract_gen2_slot(bytes, fmt, box_index, slot_index)?
        }
        PokemonSaveFormat::Gen3RubySapphire
        | PokemonSaveFormat::Gen3Emerald
        | PokemonSaveFormat::Gen3FireRedLeafGreen => {
            extract_gen3_slot(bytes, fmt, box_index, slot_index)?
        }
        PokemonSaveFormat::Gen4DiamondPearl
        | PokemonSaveFormat::Gen4Platinum
        | PokemonSaveFormat::Gen4HeartGoldSoulSilver => {
            extract_gen4_slot(bytes, fmt, box_index, slot_index)?
        }
    };
    let offer_sha256 = compute_offer_sha256(fmt, &raw_bytes);
    Ok(ExtractedPokemon {
        format: fmt,
        generation: fmt.generation(),
        summary,
        raw_pk_b64: B64_STD.encode(&raw_bytes),
        offer_sha256,
    })
}

/// Écrit le Pokémon reçu dans l'emplacement cédé par le joueur local, applique automatiquement
/// son éventuelle évolution par échange (sauf Pierre Stase ou option désactivée), recalcule
/// toutes les sommes de contrôle officielles, puis re-valide l'intégralité de la sauvegarde.
pub fn apply_received_pokemon_trade(
    save_bytes: &[u8],
    target_box_index: u8,
    target_slot_index: u8,
    incoming: &ExtractedPokemon,
    allow_evolution: bool,
) -> Result<TradeWriteOutcome, String> {
    let local_fmt = detect_pokemon_save_format(save_bytes)?;
    if !local_fmt.is_compatible_with(incoming.format) {
        return Err(format!(
            "générations incompatibles : votre sauvegarde est en Gen {} ({}) mais le partenaire propose un Pokémon Gen {}",
            local_fmt.generation(),
            local_fmt.label_fr(),
            incoming.generation
        ));
    }
    if target_box_index >= local_fmt.box_count() || target_slot_index >= local_fmt.slots_per_box() {
        return Err(format!(
            "emplacement PC cible hors limites (Boîte {}, Emplacement {})",
            target_box_index + 1,
            target_slot_index + 1
        ));
    }
    let raw_pk = B64_STD
        .decode(incoming.raw_pk_b64.trim())
        .map_err(|_| "charge utile Pokémon base64 invalide".to_string())?;
    let expected_sha = compute_offer_sha256(incoming.format, &raw_pk);
    if expected_sha != incoming.offer_sha256 {
        return Err("empreinte SHA-256 du Pokémon reçu invalide (intégrité compromise)".into());
    }

    let outcome = match local_fmt {
        PokemonSaveFormat::Gen1RedBlueYellow => write_gen1_received(
            save_bytes,
            target_box_index,
            target_slot_index,
            &raw_pk,
            allow_evolution,
        )?,
        PokemonSaveFormat::Gen2GoldSilver | PokemonSaveFormat::Gen2Crystal => write_gen2_received(
            save_bytes,
            local_fmt,
            target_box_index,
            target_slot_index,
            &raw_pk,
            allow_evolution,
        )?,
        PokemonSaveFormat::Gen3RubySapphire
        | PokemonSaveFormat::Gen3Emerald
        | PokemonSaveFormat::Gen3FireRedLeafGreen => write_gen3_received(
            save_bytes,
            local_fmt,
            target_box_index,
            target_slot_index,
            &raw_pk,
            allow_evolution,
        )?,
        PokemonSaveFormat::Gen4DiamondPearl
        | PokemonSaveFormat::Gen4Platinum
        | PokemonSaveFormat::Gen4HeartGoldSoulSilver => write_gen4_received(
            save_bytes,
            local_fmt,
            target_box_index,
            target_slot_index,
            &raw_pk,
            allow_evolution,
        )?,
    };

    // Re-validation stricte complète après écriture pour garantir 0 corruption
    let verified = inspect_pokemon_save(&outcome.updated_save_bytes)
        .map_err(|e| format!("échec de la re-vérification post-écriture : {e}"))?;
    let found = verified
        .pokemon
        .iter()
        .any(|p| p.box_index == target_box_index && p.slot_index == target_slot_index);
    if !found {
        return Err("le Pokémon écrit est introuvable après relecture de la sauvegarde".into());
    }

    Ok(outcome)
}

// =============================================================================
// RÈGLES D'ÉVOLUTION PAR ÉCHANGE (GEN 1, GEN 2, GEN 3, GEN 4)
// =============================================================================

#[derive(Debug, Clone, Copy)]
struct EvolutionResult {
    new_national_species: u16,
    consume_held_item: bool,
}

const GEN2_ITEM_KINGS_ROCK: u16 = 82; // 0x52
const GEN2_ITEM_EVERSTONE: u16 = 112; // 0x70
const GEN2_ITEM_METAL_COAT: u16 = 143; // 0x8F
const GEN2_ITEM_DRAGON_SCALE: u16 = 151; // 0x97
const GEN2_ITEM_UP_GRADE: u16 = 172; // 0xAC

const GEN3_ITEM_KINGS_ROCK: u16 = 187;
const GEN3_ITEM_DEEP_SEA_TOOTH: u16 = 192;
const GEN3_ITEM_DEEP_SEA_SCALE: u16 = 193;
const GEN3_ITEM_EVERSTONE: u16 = 195;
const GEN3_ITEM_METAL_COAT: u16 = 199;
const GEN3_ITEM_DRAGON_SCALE: u16 = 201;
const GEN3_ITEM_UP_GRADE: u16 = 218;

const GEN4_ITEM_KINGS_ROCK: u16 = 221;
const GEN4_ITEM_DEEP_SEA_TOOTH: u16 = 226;
const GEN4_ITEM_DEEP_SEA_SCALE: u16 = 227;
const GEN4_ITEM_EVERSTONE: u16 = 229;
const GEN4_ITEM_METAL_COAT: u16 = 233;
const GEN4_ITEM_DRAGON_SCALE: u16 = 235;
const GEN4_ITEM_UP_GRADE: u16 = 252;
const GEN4_ITEM_PROTECTOR: u16 = 321;
const GEN4_ITEM_ELECTIRIZER: u16 = 322;
const GEN4_ITEM_MAGMARIZER: u16 = 323;
const GEN4_ITEM_DUBIOUS_DISC: u16 = 324;
const GEN4_ITEM_REAPER_CLOTH: u16 = 325;

fn resolve_trade_evolution(
    generation: u8,
    national_species: u16,
    held_item: u16,
    is_egg: bool,
    allow_evolution: bool,
) -> Option<EvolutionResult> {
    if !allow_evolution || is_egg {
        return None;
    }
    // Pierre Stase bloque toujours l'évolution par échange en Gen 2, Gen 3 et Gen 4
    if (generation == 2 && held_item == GEN2_ITEM_EVERSTONE)
        || (generation == 3 && held_item == GEN3_ITEM_EVERSTONE)
        || (generation == 4 && held_item == GEN4_ITEM_EVERSTONE)
    {
        return None;
    }

    // 4 évolutions par échange sans objet (Gen 1, 2, 3, 4)
    match national_species {
        64 => {
            return Some(EvolutionResult {
                new_national_species: 65,
                consume_held_item: false,
            });
        } // Kadabra -> Alakazam
        67 => {
            return Some(EvolutionResult {
                new_national_species: 68,
                consume_held_item: false,
            });
        } // Machopeur -> Mackogneur
        75 => {
            return Some(EvolutionResult {
                new_national_species: 76,
                consume_held_item: false,
            });
        } // Gravalanch -> Grolem
        93 => {
            return Some(EvolutionResult {
                new_national_species: 94,
                consume_held_item: false,
            });
        } // Spectrum -> Ectoplasma
        _ => {}
    }

    if generation == 2 {
        let target = match (national_species, held_item) {
            (61, GEN2_ITEM_KINGS_ROCK) => Some(186), // Têtarte -> Tarpaud
            (79, GEN2_ITEM_KINGS_ROCK) => Some(199), // Ramoloss -> Roigada
            (95, GEN2_ITEM_METAL_COAT) => Some(208), // Onix -> Steelix
            (117, GEN2_ITEM_DRAGON_SCALE) => Some(230), // Hypocéan -> Hyporoi
            (123, GEN2_ITEM_METAL_COAT) => Some(212), // Insécateur -> Cizayox
            (137, GEN2_ITEM_UP_GRADE) => Some(233),  // Porygon -> Porygon2
            _ => None,
        };
        return target.map(|sp| EvolutionResult {
            new_national_species: sp,
            consume_held_item: true,
        });
    }

    if generation == 3 {
        let target = match (national_species, held_item) {
            (61, GEN3_ITEM_KINGS_ROCK) => Some(186), // Têtarte -> Tarpaud
            (79, GEN3_ITEM_KINGS_ROCK) => Some(199), // Ramoloss -> Roigada
            (95, GEN3_ITEM_METAL_COAT) => Some(208), // Onix -> Steelix
            (117, GEN3_ITEM_DRAGON_SCALE) => Some(230), // Hypocéan -> Hyporoi
            (123, GEN3_ITEM_METAL_COAT) => Some(212), // Insécateur -> Cizayox
            (137, GEN3_ITEM_UP_GRADE) => Some(233),  // Porygon -> Porygon2
            (366, GEN3_ITEM_DEEP_SEA_TOOTH) => Some(367), // Coquiperl -> Serpang
            (366, GEN3_ITEM_DEEP_SEA_SCALE) => Some(368), // Coquiperl -> Rosabyss
            _ => None,
        };
        return target.map(|sp| EvolutionResult {
            new_national_species: sp,
            consume_held_item: true,
        });
    }

    if generation == 4 {
        let target = match (national_species, held_item) {
            (61, GEN4_ITEM_KINGS_ROCK) => Some(186), // Têtarte -> Tarpaud
            (79, GEN4_ITEM_KINGS_ROCK) => Some(199), // Ramoloss -> Roigada
            (95, GEN4_ITEM_METAL_COAT) => Some(208), // Onix -> Steelix
            (112, GEN4_ITEM_PROTECTOR) => Some(464), // Rhinoféros -> Rhinastoc
            (117, GEN4_ITEM_DRAGON_SCALE) => Some(230), // Hypocéan -> Hyporoi
            (123, GEN4_ITEM_METAL_COAT) => Some(212), // Insécateur -> Cizayox
            (125, GEN4_ITEM_ELECTIRIZER) => Some(466), // Élektek -> Élekable
            (126, GEN4_ITEM_MAGMARIZER) => Some(467), // Magmar -> Maganon
            (137, GEN4_ITEM_UP_GRADE) => Some(233),  // Porygon -> Porygon2
            (233, GEN4_ITEM_DUBIOUS_DISC) => Some(474), // Porygon2 -> Porygon-Z
            (356, GEN4_ITEM_REAPER_CLOTH) => Some(477), // Téraclope -> Noctunoir
            (366, GEN4_ITEM_DEEP_SEA_TOOTH) => Some(367), // Coquiperl -> Serpang
            (366, GEN4_ITEM_DEEP_SEA_SCALE) => Some(368), // Coquiperl -> Rosabyss
            _ => None,
        };
        return target.map(|sp| EvolutionResult {
            new_national_species: sp,
            consume_held_item: true,
        });
    }

    None
}

fn trade_evo_preview_label(
    generation: u8,
    national_species: u16,
    held_item: u16,
    is_egg: bool,
) -> Option<String> {
    let evo = resolve_trade_evolution(generation, national_species, held_item, is_egg, true)?;
    Some(format!(
        "{} → {}",
        species_name_fr(national_species),
        species_name_fr(evo.new_national_species)
    ))
}

fn held_item_name_fr(generation: u8, item_id: u16) -> Option<String> {
    if item_id == 0 {
        return None;
    }
    let known = match (generation, item_id) {
        (2, GEN2_ITEM_KINGS_ROCK) | (3, GEN3_ITEM_KINGS_ROCK) | (4, GEN4_ITEM_KINGS_ROCK) => {
            Some("Roche Royale")
        }
        (3, GEN3_ITEM_DEEP_SEA_TOOTH) | (4, GEN4_ITEM_DEEP_SEA_TOOTH) => Some("Dent Océan"),
        (3, GEN3_ITEM_DEEP_SEA_SCALE) | (4, GEN4_ITEM_DEEP_SEA_SCALE) => Some("Écaille Océan"),
        (2, GEN2_ITEM_EVERSTONE) | (3, GEN3_ITEM_EVERSTONE) | (4, GEN4_ITEM_EVERSTONE) => {
            Some("Pierre Stase")
        }
        (2, GEN2_ITEM_METAL_COAT) | (3, GEN3_ITEM_METAL_COAT) | (4, GEN4_ITEM_METAL_COAT) => {
            Some("Peau Métal")
        }
        (2, GEN2_ITEM_DRAGON_SCALE) | (3, GEN3_ITEM_DRAGON_SCALE) | (4, GEN4_ITEM_DRAGON_SCALE) => {
            Some("Écaille Draco")
        }
        (2, GEN2_ITEM_UP_GRADE) | (3, GEN3_ITEM_UP_GRADE) | (4, GEN4_ITEM_UP_GRADE) => {
            Some("Améliorator")
        }
        (4, GEN4_ITEM_PROTECTOR) => Some("Protecteur"),
        (4, GEN4_ITEM_ELECTIRIZER) => Some("Électiriseur"),
        (4, GEN4_ITEM_MAGMARIZER) => Some("Magmariseur"),
        (4, GEN4_ITEM_DUBIOUS_DISC) => Some("CD Douteux"),
        (4, GEN4_ITEM_REAPER_CLOTH) => Some("Tissu Fauche"),
        _ => None,
    };
    Some(
        known
            .map(str::to_string)
            .unwrap_or_else(|| format!("Objet #{item_id}")),
    )
}

fn approx_level_from_exp(exp: u32) -> u8 {
    // Approximation cubique (courbe Moyenne-Rapide n^3) fiable pour l'aperçu UI
    if exp == 0 {
        return 1;
    }
    let lvl = (exp as f64).cbrt().round() as u32;
    lvl.clamp(1, 100) as u8
}

// =============================================================================
// GÉNÉRATION 1 (GAME BOY : ROUGE / BLEU / JAUNE · 32 KiB)
// =============================================================================

pub fn gen1_checksum(slice: &[u8]) -> u8 {
    let sum = slice.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    !sum
}

fn verify_gen1_save(bytes: &[u8]) -> bool {
    if bytes.len() != GEN1_SAVE_LEN {
        return false;
    }
    let expected = gen1_checksum(&bytes[GEN1_MAIN_START..=GEN1_MAIN_END_INCL]);
    bytes[GEN1_MAIN_CHECKSUM] == expected
}

fn gen1_box_offset(box_index: u8, current_box: u8) -> usize {
    if box_index == current_box {
        GEN1_CURRENT_BOX_OFFSET
    } else if box_index < 6 {
        GEN1_BANK2_START + usize::from(box_index) * GEN1_BOX_SIZE
    } else {
        GEN1_BANK3_START + usize::from(box_index - 6) * GEN1_BOX_SIZE
    }
}

fn gen1_bank_box_offset(box_index: u8) -> usize {
    if box_index < 6 {
        GEN1_BANK2_START + usize::from(box_index) * GEN1_BOX_SIZE
    } else {
        GEN1_BANK3_START + usize::from(box_index - 6) * GEN1_BOX_SIZE
    }
}

fn gen1_is_bank_box_valid(bytes: &[u8], box_index: u8) -> bool {
    let off = gen1_bank_box_offset(box_index);
    let chk_off = if box_index < 6 {
        GEN1_BANK2_CHECKSUM + 1 + usize::from(box_index)
    } else {
        GEN1_BANK3_CHECKSUM + 1 + usize::from(box_index - 6)
    };
    bytes[chk_off] == gen1_checksum(&bytes[off..off + GEN1_BOX_SIZE])
}

fn inspect_gen1_save(bytes: &[u8]) -> Result<PokemonSaveInventory, String> {
    if !verify_gen1_save(bytes) {
        return Err("somme de contrôle Pokémon Gen 1 invalide".into());
    }
    let trainer_name = decode_gen1_text(&bytes[0x2598..0x25A3]);
    let current_box = (bytes[GEN1_CURRENT_BOX_NUM] & 0x7F).min(11);
    let mut pokemon = Vec::new();

    for b in 0..12u8 {
        if b != current_box && !gen1_is_bank_box_valid(bytes, b) {
            continue;
        }
        let box_off = gen1_box_offset(b, current_box);
        let count = bytes[box_off].min(GEN1_BOX_SLOTS as u8);
        for s in 0..count {
            if let Ok((summary, _)) = parse_gen1_slot_at(bytes, box_off, b, s) {
                pokemon.push(summary);
            }
        }
    }

    Ok(PokemonSaveInventory {
        format: PokemonSaveFormat::Gen1RedBlueYellow,
        generation: 1,
        format_label: PokemonSaveFormat::Gen1RedBlueYellow.label_fr().to_string(),
        trainer_name: if trainer_name.is_empty() {
            "Dresseur".into()
        } else {
            trainer_name
        },
        active_slot_label: format!("Banque 1 (Boîte active #{})", current_box + 1),
        box_count: 12,
        slots_per_box: GEN1_BOX_SLOTS as u8,
        current_box,
        pokemon,
    })
}

fn parse_gen1_slot_at(
    bytes: &[u8],
    box_off: usize,
    box_index: u8,
    slot_index: u8,
) -> Result<(PokemonBoxSlotSummary, Vec<u8>), String> {
    let count = bytes[box_off];
    if slot_index >= count || usize::from(slot_index) >= GEN1_BOX_SLOTS {
        return Err("emplacement Gen 1 vide".into());
    }
    let s = usize::from(slot_index);
    let list_species = bytes[box_off + 1 + s];
    let struct_off = box_off + 22 + s * 33;
    let ot_off = box_off + 22 + GEN1_BOX_SLOTS * 33 + s * 11;
    let nick_off = ot_off + GEN1_BOX_SLOTS * 11;

    let internal_species = if bytes[struct_off] != 0 {
        bytes[struct_off]
    } else {
        list_species
    };
    let national = gen1_internal_to_national(internal_species)
        .ok_or_else(|| format!("espèce Gen 1 inconnue (0x{internal_species:02X})"))?;
    let level = bytes[struct_off + 3].clamp(1, 100);
    let ot_name = decode_gen1_text(&bytes[ot_off..ot_off + 11]);
    let nickname = decode_gen1_text(&bytes[nick_off..nick_off + 11]);

    let mut raw = Vec::with_capacity(GEN1_PK_WIRE_LEN);
    raw.extend_from_slice(&bytes[struct_off..struct_off + 33]);
    raw.extend_from_slice(&bytes[ot_off..ot_off + 11]);
    raw.extend_from_slice(&bytes[nick_off..nick_off + 11]);

    let species_name = species_name_fr(national).to_string();
    let display_nick = if nickname.is_empty() {
        species_name.clone()
    } else {
        nickname
    };

    Ok((
        PokemonBoxSlotSummary {
            box_index,
            slot_index,
            species_id: national,
            species_name_fr: species_name,
            nickname: display_nick,
            ot_name,
            level_approx: level,
            held_item_id: 0,
            held_item_name_fr: None,
            is_shiny: false,
            is_egg: false,
            trade_evolution_preview: trade_evo_preview_label(1, national, 0, false),
            raw_pk_bytes_len: raw.len(),
        },
        raw,
    ))
}

fn extract_gen1_slot(
    bytes: &[u8],
    box_index: u8,
    slot_index: u8,
) -> Result<(PokemonBoxSlotSummary, Vec<u8>), String> {
    let current_box = (bytes[GEN1_CURRENT_BOX_NUM] & 0x7F).min(11);
    if box_index != current_box && !gen1_is_bank_box_valid(bytes, box_index) {
        return Err(format!("Boîte {} Gen 1 non initialisée", box_index + 1));
    }
    let box_off = gen1_box_offset(box_index, current_box);
    parse_gen1_slot_at(bytes, box_off, box_index, slot_index)
}

fn write_gen1_received(
    save_bytes: &[u8],
    box_index: u8,
    slot_index: u8,
    raw_pk: &[u8],
    allow_evolution: bool,
) -> Result<TradeWriteOutcome, String> {
    if raw_pk.len() != GEN1_PK_WIRE_LEN {
        return Err(format!(
            "taille de Pokémon Gen 1 invalide ({} octets, attendu {GEN1_PK_WIRE_LEN})",
            raw_pk.len()
        ));
    }
    let mut out = save_bytes.to_vec();
    let current_box = (out[GEN1_CURRENT_BOX_NUM] & 0x7F).min(11);

    let mut mutated_pk = raw_pk.to_vec();
    let before_internal = mutated_pk[0];
    let before_nat = gen1_internal_to_national(before_internal)
        .ok_or_else(|| format!("espèce Gen 1 inconnue (0x{before_internal:02X})"))?;
    let mut after_nat = before_nat;
    let mut after_internal = before_internal;
    let mut evolved = false;

    if let Some(evo) = resolve_trade_evolution(1, before_nat, 0, false, allow_evolution) {
        if let Some(target_internal) = national_to_gen1_internal(evo.new_national_species) {
            mutated_pk[0] = target_internal;
            after_internal = target_internal;
            after_nat = evo.new_national_species;
            evolved = true;
        }
    }

    let write_into_box = |buf: &mut [u8], box_off: usize| -> Result<(), String> {
        let count = buf[box_off];
        if slot_index >= count || usize::from(slot_index) >= GEN1_BOX_SLOTS {
            return Err("emplacement cible Gen 1 vide ou hors compteur de boîte".into());
        }
        let s = usize::from(slot_index);
        buf[box_off + 1 + s] = after_internal;
        let struct_off = box_off + 22 + s * 33;
        let ot_off = box_off + 22 + GEN1_BOX_SLOTS * 33 + s * 11;
        let nick_off = ot_off + GEN1_BOX_SLOTS * 11;
        buf[struct_off..struct_off + 33].copy_from_slice(&mutated_pk[0..33]);
        buf[ot_off..ot_off + 11].copy_from_slice(&mutated_pk[33..44]);
        buf[nick_off..nick_off + 11].copy_from_slice(&mutated_pk[44..55]);
        Ok(())
    };

    if box_index == current_box {
        write_into_box(&mut out, GEN1_CURRENT_BOX_OFFSET)?;
        // Miroir sur la banque PC si la boîte correspondante est déjà initialisée
        if gen1_is_bank_box_valid(save_bytes, box_index) {
            let bank_box_off = gen1_bank_box_offset(box_index);
            let cur_copy =
                out[GEN1_CURRENT_BOX_OFFSET..GEN1_CURRENT_BOX_OFFSET + GEN1_BOX_SIZE].to_vec();
            out[bank_box_off..bank_box_off + GEN1_BOX_SIZE].copy_from_slice(&cur_copy);
            recompute_gen1_bank_checksums(&mut out, box_index);
        }
        out[GEN1_MAIN_CHECKSUM] = gen1_checksum(&out[GEN1_MAIN_START..=GEN1_MAIN_END_INCL]);
    } else {
        if !gen1_is_bank_box_valid(save_bytes, box_index) {
            return Err(format!("Boîte {} Gen 1 non initialisée", box_index + 1));
        }
        let bank_box_off = gen1_bank_box_offset(box_index);
        write_into_box(&mut out, bank_box_off)?;
        recompute_gen1_bank_checksums(&mut out, box_index);
    }

    Ok(TradeWriteOutcome {
        updated_save_bytes: out,
        received_species_before_fr: species_name_fr(before_nat).to_string(),
        received_species_after_fr: species_name_fr(after_nat).to_string(),
        evolved,
        consumed_item_name_fr: None,
    })
}

fn recompute_gen1_bank_checksums(buf: &mut [u8], box_index: u8) {
    let (bank_start, bank_chk_off, local_idx) = if box_index < 6 {
        (
            GEN1_BANK2_START,
            GEN1_BANK2_CHECKSUM,
            usize::from(box_index),
        )
    } else {
        (
            GEN1_BANK3_START,
            GEN1_BANK3_CHECKSUM,
            usize::from(box_index - 6),
        )
    };
    let box_off = bank_start + local_idx * GEN1_BOX_SIZE;
    buf[bank_chk_off + 1 + local_idx] = gen1_checksum(&buf[box_off..box_off + GEN1_BOX_SIZE]);
    buf[bank_chk_off] = gen1_checksum(&buf[bank_start..bank_start + 6 * GEN1_BOX_SIZE]);
}

// =============================================================================
// GÉNÉRATION 2 (GAME BOY COLOR : OR / ARGENT / CRISTAL · 32 KiB + RTC)
// =============================================================================

/// Somme de contrôle 16 bits officielle Pokémon Gen 2 (addition modulo 65536).
pub fn gen2_checksum(slice: &[u8]) -> u16 {
    slice
        .iter()
        .fold(0u16, |acc, &b| acc.wrapping_add(u16::from(b)))
}

fn gen2_gs_backup_checksum(bytes: &[u8]) -> u16 {
    gen2_checksum(&bytes[0x0C6B..=0x17EC])
        .wrapping_add(gen2_checksum(&bytes[0x3D96..=0x3F3F]))
        .wrapping_add(gen2_checksum(&bytes[0x7E39..=0x7E6C]))
}

fn detect_gen2_save_format(bytes: &[u8]) -> Option<PokemonSaveFormat> {
    if bytes.len() < GEN2_SAVE_LEN {
        return None;
    }

    // Cristal (occidental 32 KiB) : Checksum 1 (0x2009..=0x2B82 @ 0x2D0D) + Checksum 2 / Magic
    let c_calc1 = gen2_checksum(&bytes[0x2009..=0x2B82]);
    let c_stored1 = u16::from_le_bytes([
        bytes[GEN2_CRYSTAL_CHECKSUM_1_OFFSET],
        bytes[GEN2_CRYSTAL_CHECKSUM_1_OFFSET + 1],
    ]);
    let c_magic_ok = bytes[GEN2_MAGIC_1_OFFSET] == GEN2_MAGIC_1_VALUE
        && bytes[GEN2_CRYSTAL_MAGIC_2_OFFSET] == GEN2_MAGIC_2_VALUE;
    let c_calc2 = gen2_checksum(&bytes[0x1209..=0x1D82]);
    let c_stored2 = u16::from_le_bytes([
        bytes[GEN2_CRYSTAL_CHECKSUM_2_OFFSET],
        bytes[GEN2_CRYSTAL_CHECKSUM_2_OFFSET + 1],
    ]);
    if c_calc1 == c_stored1 && (c_magic_ok || (c_calc1 != 0 && c_calc2 == c_stored2)) {
        return Some(PokemonSaveFormat::Gen2Crystal);
    }

    // Or / Argent (occidental 32 KiB) : Checksum 1 (0x2009..=0x2D68 @ 0x2D69) + Checksum 2 / Magic
    let gs_calc1 = gen2_checksum(&bytes[0x2009..=0x2D68]);
    let gs_stored1 = u16::from_le_bytes([
        bytes[GEN2_GS_CHECKSUM_1_OFFSET],
        bytes[GEN2_GS_CHECKSUM_1_OFFSET + 1],
    ]);
    let gs_magic_ok = bytes[GEN2_MAGIC_1_OFFSET] == GEN2_MAGIC_1_VALUE
        && bytes[GEN2_GS_MAGIC_2_OFFSET] == GEN2_MAGIC_2_VALUE;
    let gs_calc2 = gen2_gs_backup_checksum(bytes);
    let gs_stored2 = u16::from_le_bytes([
        bytes[GEN2_GS_CHECKSUM_2_OFFSET],
        bytes[GEN2_GS_CHECKSUM_2_OFFSET + 1],
    ]);
    if gs_calc1 == gs_stored1 && (gs_magic_ok || (gs_calc1 != 0 && gs_calc2 == gs_stored2)) {
        return Some(PokemonSaveFormat::Gen2GoldSilver);
    }

    None
}

fn gen2_current_box_params(fmt: PokemonSaveFormat) -> (usize, usize) {
    match fmt {
        PokemonSaveFormat::Gen2Crystal => (
            GEN2_CRYSTAL_CURRENT_BOX_NUM,
            GEN2_CRYSTAL_CURRENT_BOX_OFFSET,
        ),
        _ => (GEN2_GS_CURRENT_BOX_NUM, GEN2_GS_CURRENT_BOX_OFFSET),
    }
}

fn gen2_bank_box_offset(box_index: u8) -> usize {
    if box_index < 7 {
        GEN2_BANK2_START + usize::from(box_index) * GEN2_BOX_STRIDE
    } else {
        GEN2_BANK3_START + usize::from(box_index - 7) * GEN2_BOX_STRIDE
    }
}

fn gen2_box_offset(fmt: PokemonSaveFormat, box_index: u8, current_box: u8) -> usize {
    if box_index == current_box {
        gen2_current_box_params(fmt).1
    } else {
        gen2_bank_box_offset(box_index)
    }
}

fn gen2_box_list_valid(bytes: &[u8], box_off: usize) -> bool {
    if box_off + GEN2_BOX_SIZE > bytes.len() {
        return false;
    }
    let count = usize::from(bytes[box_off]);
    count <= GEN2_BOX_SLOTS && bytes[box_off + 1 + count] == 0xFF
}

fn parse_gen2_slot_at(
    bytes: &[u8],
    box_off: usize,
    box_index: u8,
    slot_index: u8,
) -> Result<(PokemonBoxSlotSummary, Vec<u8>), String> {
    if !gen2_box_list_valid(bytes, box_off) {
        return Err(format!("Boîte {} Gen 2 non initialisée", box_index + 1));
    }
    let count = bytes[box_off];
    if slot_index >= count || usize::from(slot_index) >= GEN2_BOX_SLOTS {
        return Err("emplacement Gen 2 vide".into());
    }
    let s = usize::from(slot_index);
    let list_species = bytes[box_off + 1 + s];
    let struct_off = box_off + 22 + s * 32;
    let ot_off = box_off + 22 + GEN2_BOX_SLOTS * 32 + s * 11;
    let nick_off = ot_off + GEN2_BOX_SLOTS * 11;

    let species = u16::from(bytes[struct_off]);
    if species == 0 || species > 251 {
        return Err(format!("espèce Gen 2 invalide (#{species})"));
    }
    let is_egg = list_species == 0xFD;
    let held_item = u16::from(bytes[struct_off + 1]);
    let dv0 = bytes[struct_off + 21];
    let dv1 = bytes[struct_off + 22];
    let atk = (dv0 >> 4) & 0x0F;
    let def = dv0 & 0x0F;
    let spd = (dv1 >> 4) & 0x0F;
    let spc = dv1 & 0x0F;
    let is_shiny = (atk & 0x02) != 0 && def == 10 && spd == 10 && spc == 10;
    let level = bytes[struct_off + 31].clamp(1, 100);
    let ot_name = decode_gen1_text(&bytes[ot_off..ot_off + 11]);
    let nickname = decode_gen1_text(&bytes[nick_off..nick_off + 11]);

    let mut raw = Vec::with_capacity(GEN2_PK_WIRE_LEN);
    raw.push(list_species);
    raw.extend_from_slice(&bytes[struct_off..struct_off + 32]);
    raw.extend_from_slice(&bytes[ot_off..ot_off + 11]);
    raw.extend_from_slice(&bytes[nick_off..nick_off + 11]);

    let species_name = species_name_fr(species).to_string();
    let display_nick = if nickname.is_empty() {
        species_name.clone()
    } else {
        nickname
    };

    Ok((
        PokemonBoxSlotSummary {
            box_index,
            slot_index,
            species_id: species,
            species_name_fr: species_name,
            nickname: display_nick,
            ot_name,
            level_approx: level,
            held_item_id: held_item,
            held_item_name_fr: held_item_name_fr(2, held_item),
            is_shiny,
            is_egg,
            trade_evolution_preview: trade_evo_preview_label(2, species, held_item, is_egg),
            raw_pk_bytes_len: raw.len(),
        },
        raw,
    ))
}

fn inspect_gen2_save(bytes: &[u8], fmt: PokemonSaveFormat) -> Result<PokemonSaveInventory, String> {
    let (cur_box_num_off, _) = gen2_current_box_params(fmt);
    let trainer_name = decode_gen1_text(&bytes[0x200B..0x2016]);
    let current_box = (bytes[cur_box_num_off] & 0x0F).min((GEN2_BOX_COUNT - 1) as u8);
    let mut pokemon = Vec::new();

    for b in 0..GEN2_BOX_COUNT as u8 {
        let box_off = gen2_box_offset(fmt, b, current_box);
        if !gen2_box_list_valid(bytes, box_off) {
            continue;
        }
        let count = bytes[box_off].min(GEN2_BOX_SLOTS as u8);
        for s in 0..count {
            if let Ok((summary, _)) = parse_gen2_slot_at(bytes, box_off, b, s) {
                pokemon.push(summary);
            }
        }
    }

    Ok(PokemonSaveInventory {
        format: fmt,
        generation: 2,
        format_label: fmt.label_fr().to_string(),
        trainer_name: if trainer_name.is_empty() {
            "Dresseur".into()
        } else {
            trainer_name
        },
        active_slot_label: format!("SRAM GBC (Boîte active #{})", current_box + 1),
        box_count: GEN2_BOX_COUNT as u8,
        slots_per_box: GEN2_BOX_SLOTS as u8,
        current_box,
        pokemon,
    })
}

fn extract_gen2_slot(
    bytes: &[u8],
    fmt: PokemonSaveFormat,
    box_index: u8,
    slot_index: u8,
) -> Result<(PokemonBoxSlotSummary, Vec<u8>), String> {
    let (cur_box_num_off, _) = gen2_current_box_params(fmt);
    let current_box = (bytes[cur_box_num_off] & 0x0F).min((GEN2_BOX_COUNT - 1) as u8);
    let box_off = gen2_box_offset(fmt, box_index, current_box);
    parse_gen2_slot_at(bytes, box_off, box_index, slot_index)
}

fn write_gen2_received(
    save_bytes: &[u8],
    fmt: PokemonSaveFormat,
    box_index: u8,
    slot_index: u8,
    raw_pk: &[u8],
    allow_evolution: bool,
) -> Result<TradeWriteOutcome, String> {
    if raw_pk.len() != GEN2_PK_WIRE_LEN {
        return Err(format!(
            "taille de Pokémon Gen 2 invalide ({} octets, attendu {GEN2_PK_WIRE_LEN})",
            raw_pk.len()
        ));
    }
    let (cur_box_num_off, cur_box_off) = gen2_current_box_params(fmt);
    let current_box = (save_bytes[cur_box_num_off] & 0x0F).min((GEN2_BOX_COUNT - 1) as u8);

    let mut mutated_pk = raw_pk.to_vec();
    let is_egg = mutated_pk[0] == 0xFD;
    let before_species = u16::from(mutated_pk[1]);
    if before_species == 0 || before_species > 251 {
        return Err(format!("espèce Gen 2 invalide (#{before_species})"));
    }
    let held_item = u16::from(mutated_pk[2]);

    let mut after_species = before_species;
    let mut evolved = false;
    let mut consumed_item_name_fr = None;

    if let Some(evo) =
        resolve_trade_evolution(2, before_species, held_item, is_egg, allow_evolution)
    {
        if evo.new_national_species <= 251 {
            let new_sp = evo.new_national_species as u8;
            mutated_pk[0] = new_sp;
            mutated_pk[1] = new_sp;
            if evo.consume_held_item {
                consumed_item_name_fr = held_item_name_fr(2, held_item);
                mutated_pk[2] = 0;
            }
            after_species = evo.new_national_species;
            evolved = true;
        }
    }

    // Préserve automatiquement tout éventuel pied RTC MBC3 au-delà de 0x8000
    let mut out = save_bytes.to_vec();

    let write_into_box = |buf: &mut [u8], box_off: usize| -> Result<(), String> {
        if !gen2_box_list_valid(buf, box_off) {
            return Err(format!("Boîte {} Gen 2 non initialisée", box_index + 1));
        }
        let count = buf[box_off];
        if slot_index >= count || usize::from(slot_index) >= GEN2_BOX_SLOTS {
            return Err("emplacement cible Gen 2 vide ou hors compteur de boîte".into());
        }
        let s = usize::from(slot_index);
        buf[box_off + 1 + s] = mutated_pk[0];
        let struct_off = box_off + 22 + s * 32;
        let ot_off = box_off + 22 + GEN2_BOX_SLOTS * 32 + s * 11;
        let nick_off = ot_off + GEN2_BOX_SLOTS * 11;
        buf[struct_off..struct_off + 32].copy_from_slice(&mutated_pk[1..33]);
        buf[ot_off..ot_off + 11].copy_from_slice(&mutated_pk[33..44]);
        buf[nick_off..nick_off + 11].copy_from_slice(&mutated_pk[44..55]);
        Ok(())
    };

    if box_index == current_box {
        write_into_box(&mut out, cur_box_off)?;
        // Miroir systématique sur la copie en Banque 2/3 pour garantir la cohérence lors du changement de boîte
        let bank_off = gen2_bank_box_offset(box_index);
        let cur_copy = out[cur_box_off..cur_box_off + GEN2_BOX_SIZE].to_vec();
        out[bank_off..bank_off + GEN2_BOX_SIZE].copy_from_slice(&cur_copy);
    } else {
        let bank_off = gen2_bank_box_offset(box_index);
        write_into_box(&mut out, bank_off)?;
    }

    Ok(TradeWriteOutcome {
        updated_save_bytes: out,
        received_species_before_fr: species_name_fr(before_species).to_string(),
        received_species_after_fr: species_name_fr(after_species).to_string(),
        evolved,
        consumed_item_name_fr,
    })
}

// =============================================================================
// GÉNÉRATION 3 (GBA : RUBIS / SAPHIR / ÉMERAUDE / ROUGE FEU / VERT FEUILLE · 128 KiB)
// =============================================================================

pub fn gen3_section_data_len(section_id: u16) -> usize {
    match section_id {
        0 => 3884,
        4 => 3848,
        13 => 2000,
        _ => 3968,
    }
}

pub fn gen3_section_checksum(section_data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for &chunk in section_data.as_chunks::<4>().0 {
        sum = sum.wrapping_add(u32::from_le_bytes(chunk));
    }
    ((sum >> 16) + (sum & 0xFFFF)) as u16
}

struct Gen3ActiveSlot {
    slot_index: usize, // 0 = Slot A (0x00000), 1 = Slot B (0x0E000)
    save_index: u32,
    /// Table de correspondance `section_id (0..14)` -> offset physique dans `bytes`.
    section_offsets: [usize; GEN3_SECTION_COUNT],
}

fn validate_gen3_slot(bytes: &[u8], slot_index: usize) -> Option<Gen3ActiveSlot> {
    let base = slot_index * GEN3_SLOT_STRIDE;
    let mut section_offsets = [usize::MAX; GEN3_SECTION_COUNT];
    let mut seen = [false; GEN3_SECTION_COUNT];
    let mut slot_save_index: Option<u32> = None;

    for i in 0..GEN3_SECTION_COUNT {
        let off = base + i * GEN3_SECTION_SIZE;
        let sec = &bytes[off..off + GEN3_SECTION_SIZE];
        let sig = u32::from_le_bytes([sec[0xFF8], sec[0xFF9], sec[0xFFA], sec[0xFFB]]);
        if sig != GEN3_SIGNATURE {
            return None;
        }
        let sec_id = u16::from_le_bytes([sec[0xFF4], sec[0xFF5]]);
        if usize::from(sec_id) >= GEN3_SECTION_COUNT || seen[usize::from(sec_id)] {
            return None;
        }
        let stored_chk = u16::from_le_bytes([sec[0xFF6], sec[0xFF7]]);
        let official_len = gen3_section_data_len(sec_id);
        let official_chk = gen3_section_checksum(&sec[..official_len]);
        let full_chk = gen3_section_checksum(&sec[..3968]);
        let valid_chk = if sec_id == 0 || sec_id == 4 {
            stored_chk == official_chk || stored_chk == full_chk
        } else {
            stored_chk == official_chk
        };
        if !valid_chk {
            return None;
        }
        let save_idx = u32::from_le_bytes([sec[0xFFC], sec[0xFFD], sec[0xFFE], sec[0xFFF]]);
        match slot_save_index {
            None => slot_save_index = Some(save_idx),
            Some(prev) if prev != save_idx => return None,
            _ => {}
        }
        seen[usize::from(sec_id)] = true;
        section_offsets[usize::from(sec_id)] = off;
    }

    Some(Gen3ActiveSlot {
        slot_index,
        save_index: slot_save_index?,
        section_offsets,
    })
}

fn select_gen3_active_slot(bytes: &[u8]) -> Result<(Gen3ActiveSlot, PokemonSaveFormat), String> {
    if bytes.len() != GEN3_SAVE_LEN {
        return Err("sauvegarde Gen 3 attendue à exactement 128 Ko".into());
    }
    let slot_a = validate_gen3_slot(bytes, 0);
    let slot_b = validate_gen3_slot(bytes, 1);
    let active = match (slot_a, slot_b) {
        (Some(a), Some(b)) => {
            if b.save_index > a.save_index {
                b
            } else {
                a
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => {
            return Err(
                "sauvegarde 128 Ko non reconnue ou sommes de contrôle Pokémon Gen 3 invalides"
                    .into(),
            );
        }
    };
    let sec0 = active.section_offsets[0];
    let game_code = u32::from_le_bytes([
        bytes[sec0 + 0xAC],
        bytes[sec0 + 0xAD],
        bytes[sec0 + 0xAE],
        bytes[sec0 + 0xAF],
    ]);
    let fmt = match game_code {
        0 => PokemonSaveFormat::Gen3RubySapphire,
        1 => PokemonSaveFormat::Gen3FireRedLeafGreen,
        _ => PokemonSaveFormat::Gen3Emerald,
    };
    Ok((active, fmt))
}

fn read_gen3_pc_buffer(bytes: &[u8], slot: &Gen3ActiveSlot) -> Vec<u8> {
    let mut pc = Vec::with_capacity(8 * 3968 + 2000);
    for (sec_id, &off) in slot.section_offsets.iter().enumerate().skip(5) {
        let len = gen3_section_data_len(sec_id as u16);
        pc.extend_from_slice(&bytes[off..off + len]);
    }
    pc
}

fn write_gen3_pc_buffer(bytes: &mut [u8], slot: &Gen3ActiveSlot, pc: &[u8]) {
    let mut cursor = 0usize;
    for (sec_id, &off) in slot.section_offsets.iter().enumerate().skip(5) {
        let len = gen3_section_data_len(sec_id as u16);
        bytes[off..off + len].copy_from_slice(&pc[cursor..cursor + len]);
        let chk = gen3_section_checksum(&bytes[off..off + len]);
        bytes[off + 0xFF6..off + 0xFF8].copy_from_slice(&chk.to_le_bytes());
        cursor += len;
    }
}

/// Table des 24 permutations de sous-structures (Gen 3 G/A/E/M et Gen 4 A/B/C/D).
/// `PERM_ORDER[pid_mod_24][slot_pos] = block_id` (`0=G/A, 1=A/B, 2=E/C, 3=M/D`).
const SUBSTRUCT_PERM_ORDER: [[usize; 4]; 24] = [
    [0, 1, 2, 3],
    [0, 1, 3, 2],
    [0, 2, 1, 3],
    [0, 2, 3, 1],
    [0, 3, 1, 2],
    [0, 3, 2, 1],
    [1, 0, 2, 3],
    [1, 0, 3, 2],
    [1, 2, 0, 3],
    [1, 2, 3, 0],
    [1, 3, 0, 2],
    [1, 3, 2, 0],
    [2, 0, 1, 3],
    [2, 0, 3, 1],
    [2, 1, 0, 3],
    [2, 1, 3, 0],
    [2, 3, 0, 1],
    [2, 3, 1, 0],
    [3, 0, 1, 2],
    [3, 0, 2, 1],
    [3, 1, 0, 2],
    [3, 1, 2, 0],
    [3, 2, 0, 1],
    [3, 2, 1, 0],
];

fn pk3_decrypt_unshuffle(pk3: &[u8]) -> Result<([u8; 48], u32, u32), String> {
    if pk3.len() != GEN3_PK_LEN {
        return Err("structure .pk3 invalide (80 octets attendus)".into());
    }
    let pid = u32::from_le_bytes([pk3[0], pk3[1], pk3[2], pk3[3]]);
    let ot_id = u32::from_le_bytes([pk3[4], pk3[5], pk3[6], pk3[7]]);
    let stored_chk = u16::from_le_bytes([pk3[0x1C], pk3[0x1D]]);
    let key = pid ^ ot_id;

    let mut decrypted = [0u8; 48];
    for i in 0..12 {
        let off = 0x20 + i * 4;
        let word = u32::from_le_bytes([pk3[off], pk3[off + 1], pk3[off + 2], pk3[off + 3]]) ^ key;
        decrypted[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }

    let mut calc_chk = 0u16;
    for &chunk in decrypted.as_chunks::<2>().0 {
        calc_chk = calc_chk.wrapping_add(u16::from_le_bytes(chunk));
    }
    if calc_chk != stored_chk {
        return Err("somme de contrôle interne .pk3 invalide".into());
    }

    let perm = SUBSTRUCT_PERM_ORDER[(pid % 24) as usize];
    let mut unshuffled = [0u8; 48];
    for (slot_pos, &block_id) in perm.iter().enumerate() {
        unshuffled[block_id * 12..block_id * 12 + 12]
            .copy_from_slice(&decrypted[slot_pos * 12..slot_pos * 12 + 12]);
    }
    Ok((unshuffled, pid, ot_id))
}

fn pk3_repack_from_unshuffled(
    orig_pk3: &[u8],
    unshuffled: &[u8; 48],
    pid: u32,
    ot_id: u32,
) -> [u8; GEN3_PK_LEN] {
    let mut out = [0u8; GEN3_PK_LEN];
    out.copy_from_slice(orig_pk3);

    let perm = SUBSTRUCT_PERM_ORDER[(pid % 24) as usize];
    let mut shuffled = [0u8; 48];
    for (slot_pos, &block_id) in perm.iter().enumerate() {
        shuffled[slot_pos * 12..slot_pos * 12 + 12]
            .copy_from_slice(&unshuffled[block_id * 12..block_id * 12 + 12]);
    }

    let mut chk = 0u16;
    for &chunk in shuffled.as_chunks::<2>().0 {
        chk = chk.wrapping_add(u16::from_le_bytes(chunk));
    }
    out[0x1C..0x1E].copy_from_slice(&chk.to_le_bytes());

    let key = pid ^ ot_id;
    for i in 0..12 {
        let word = u32::from_le_bytes([
            shuffled[i * 4],
            shuffled[i * 4 + 1],
            shuffled[i * 4 + 2],
            shuffled[i * 4 + 3],
        ]) ^ key;
        let off = 0x20 + i * 4;
        out[off..off + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

fn parse_pk3_summary(pk3: &[u8], box_index: u8, slot_index: u8) -> Option<PokemonBoxSlotSummary> {
    if pk3.len() != GEN3_PK_LEN {
        return None;
    }
    let flags = pk3[0x13];
    // Bit 1 = Has Species (0 = slot vide)
    if (flags & 0x02) == 0 || pk3.iter().all(|&b| b == 0) {
        return None;
    }
    let (unshuffled, pid, ot_id) = pk3_decrypt_unshuffle(pk3).ok()?;
    let internal_species = u16::from_le_bytes([unshuffled[0], unshuffled[1]]);
    if internal_species == 0 {
        return None;
    }
    let national = gen3_internal_to_national(internal_species)?;
    let held_item = u16::from_le_bytes([unshuffled[2], unshuffled[3]]);
    let exp = u32::from_le_bytes([unshuffled[4], unshuffled[5], unshuffled[6], unshuffled[7]]);
    let misc_iv_word = u32::from_le_bytes([
        unshuffled[36 + 4],
        unshuffled[36 + 5],
        unshuffled[36 + 6],
        unshuffled[36 + 7],
    ]);
    let is_egg = (flags & 0x04) != 0 || ((misc_iv_word >> 30) & 1) != 0;

    let tid = (ot_id & 0xFFFF) as u16;
    let sid = ((ot_id >> 16) & 0xFFFF) as u16;
    let p_lo = (pid & 0xFFFF) as u16;
    let p_hi = ((pid >> 16) & 0xFFFF) as u16;
    let is_shiny = ((tid ^ sid) ^ (p_lo ^ p_hi)) < 8;

    let nickname = decode_gen3_text(&pk3[0x08..0x12]);
    let ot_name = decode_gen3_text(&pk3[0x14..0x1B]);
    let species_name = species_name_fr(national).to_string();

    Some(PokemonBoxSlotSummary {
        box_index,
        slot_index,
        species_id: national,
        species_name_fr: species_name.clone(),
        nickname: if nickname.is_empty() {
            species_name
        } else {
            nickname
        },
        ot_name,
        level_approx: approx_level_from_exp(exp),
        held_item_id: held_item,
        held_item_name_fr: held_item_name_fr(3, held_item),
        is_shiny,
        is_egg,
        trade_evolution_preview: trade_evo_preview_label(3, national, held_item, is_egg),
        raw_pk_bytes_len: GEN3_PK_LEN,
    })
}

fn inspect_gen3_save(bytes: &[u8]) -> Result<PokemonSaveInventory, String> {
    let (slot, fmt) = select_gen3_active_slot(bytes)?;
    let sec0 = slot.section_offsets[0];
    let trainer_name = decode_gen3_text(&bytes[sec0..sec0 + 8]);
    let pc = read_gen3_pc_buffer(bytes, &slot);
    let current_box = u32::from_le_bytes([pc[0], pc[1], pc[2], pc[3]]).min(13) as u8;

    let mut pokemon = Vec::new();
    for b in 0..GEN3_BOX_COUNT {
        for s in 0..GEN3_SLOTS_PER_BOX {
            let off = 4 + (b * GEN3_SLOTS_PER_BOX + s) * GEN3_PK_LEN;
            if let Some(summary) = parse_pk3_summary(&pc[off..off + GEN3_PK_LEN], b as u8, s as u8)
            {
                pokemon.push(summary);
            }
        }
    }

    Ok(PokemonSaveInventory {
        format: fmt,
        generation: 3,
        format_label: fmt.label_fr().to_string(),
        trainer_name: if trainer_name.is_empty() {
            "Dresseur".into()
        } else {
            trainer_name
        },
        active_slot_label: format!(
            "Slot {} (index #{})",
            if slot.slot_index == 0 { "A" } else { "B" },
            slot.save_index
        ),
        box_count: GEN3_BOX_COUNT as u8,
        slots_per_box: GEN3_SLOTS_PER_BOX as u8,
        current_box,
        pokemon,
    })
}

fn extract_gen3_slot(
    bytes: &[u8],
    _fmt: PokemonSaveFormat,
    box_index: u8,
    slot_index: u8,
) -> Result<(PokemonBoxSlotSummary, Vec<u8>), String> {
    let (slot, _) = select_gen3_active_slot(bytes)?;
    let pc = read_gen3_pc_buffer(bytes, &slot);
    let off =
        4 + (usize::from(box_index) * GEN3_SLOTS_PER_BOX + usize::from(slot_index)) * GEN3_PK_LEN;
    let pk3 = &pc[off..off + GEN3_PK_LEN];
    let summary = parse_pk3_summary(pk3, box_index, slot_index)
        .ok_or_else(|| "aucun Pokémon valide dans cet emplacement PC Gen 3".to_string())?;
    Ok((summary, pk3.to_vec()))
}

fn write_gen3_received(
    save_bytes: &[u8],
    _fmt: PokemonSaveFormat,
    box_index: u8,
    slot_index: u8,
    raw_pk: &[u8],
    allow_evolution: bool,
) -> Result<TradeWriteOutcome, String> {
    if raw_pk.len() != GEN3_PK_LEN {
        return Err(format!(
            "structure .pk3 invalide ({} octets, attendu {GEN3_PK_LEN})",
            raw_pk.len()
        ));
    }
    let (slot, _) = select_gen3_active_slot(save_bytes)?;
    let (mut unshuffled, pid, ot_id) = pk3_decrypt_unshuffle(raw_pk)?;
    let before_internal = u16::from_le_bytes([unshuffled[0], unshuffled[1]]);
    let before_nat = gen3_internal_to_national(before_internal)
        .ok_or_else(|| format!("espèce Gen 3 inconnue ({before_internal})"))?;
    let held_item = u16::from_le_bytes([unshuffled[2], unshuffled[3]]);
    let is_egg = (raw_pk[0x13] & 0x04) != 0
        || ((u32::from_le_bytes([
            unshuffled[40],
            unshuffled[41],
            unshuffled[42],
            unshuffled[43],
        ]) >> 30)
            & 1)
            != 0;

    let mut after_nat = before_nat;
    let mut evolved = false;
    let mut consumed_item_name_fr = None;

    if let Some(evo) = resolve_trade_evolution(3, before_nat, held_item, is_egg, allow_evolution) {
        if let Some(target_internal) = national_to_gen3_internal(evo.new_national_species) {
            unshuffled[0..2].copy_from_slice(&target_internal.to_le_bytes());
            if evo.consume_held_item {
                consumed_item_name_fr = held_item_name_fr(3, held_item);
                unshuffled[2..4].copy_from_slice(&0u16.to_le_bytes());
            }
            after_nat = evo.new_national_species;
            evolved = true;
        }
    }

    let final_pk3 = pk3_repack_from_unshuffled(raw_pk, &unshuffled, pid, ot_id);
    let mut pc = read_gen3_pc_buffer(save_bytes, &slot);
    let off =
        4 + (usize::from(box_index) * GEN3_SLOTS_PER_BOX + usize::from(slot_index)) * GEN3_PK_LEN;
    pc[off..off + GEN3_PK_LEN].copy_from_slice(&final_pk3);

    let mut out = save_bytes.to_vec();
    write_gen3_pc_buffer(&mut out, &slot, &pc);

    Ok(TradeWriteOutcome {
        updated_save_bytes: out,
        received_species_before_fr: species_name_fr(before_nat).to_string(),
        received_species_after_fr: species_name_fr(after_nat).to_string(),
        evolved,
        consumed_item_name_fr,
    })
}

// =============================================================================
// GÉNÉRATION 4 (NDS : DIAMANT / PERLE, PLATINE, HEARTGOLD / SOULSILVER · 512 KiB)
// =============================================================================

/// CRC-16-CCITT officiel Nintendo DS Pokémon Gen 4 (polynôme `0x1021`, init `0xFFFF`, MSB-first).
pub fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[derive(Debug, Clone, Copy)]
struct Gen4BlockSpec {
    format: PokemonSaveFormat,
    general_offset: usize,
    general_size: usize,
    storage_offset: usize,
    storage_size: usize,
    footer_len: usize, // 16 pour HGSS, 20 pour DP/Pt
    trainer_name_offset: usize,
}

const GEN4_SPECS: [Gen4BlockSpec; 3] = [
    // HeartGold / SoulSilver (vérifié contre pokeheartgold save.c:429-526)
    Gen4BlockSpec {
        format: PokemonSaveFormat::Gen4HeartGoldSoulSilver,
        general_offset: 0x00000,
        general_size: 0x0F628,
        storage_offset: 0x0F700,
        storage_size: 0x12310,
        footer_len: 16,
        trainer_name_offset: 0x64,
    },
    // Platine
    Gen4BlockSpec {
        format: PokemonSaveFormat::Gen4Platinum,
        general_offset: 0x00000,
        general_size: 0x0CF2C,
        storage_offset: 0x0CF2C,
        storage_size: 0x121E4,
        footer_len: 20,
        trainer_name_offset: 0x68,
    },
    // Diamant / Perle
    Gen4BlockSpec {
        format: PokemonSaveFormat::Gen4DiamondPearl,
        general_offset: 0x00000,
        general_size: 0x0C100,
        storage_offset: 0x0C100,
        storage_size: 0x121E0,
        footer_len: 20,
        trainer_name_offset: 0x64,
    },
];

#[derive(Debug, Clone, Copy)]
struct Gen4ValidatedBlock {
    partition: usize,
    start: usize,
    size: usize,
    footer_len: usize,
    link_count: u32,
    save_count: u32,
}

#[derive(Debug, Clone, Copy)]
struct Gen4ActiveLayout {
    format: PokemonSaveFormat,
    spec: Gen4BlockSpec,
    general: Gen4ValidatedBlock,
    storage: Gen4ValidatedBlock,
}

fn validate_gen4_block(
    bytes: &[u8],
    partition: usize,
    rel_offset: usize,
    expected_size: usize,
    footer_len: usize,
) -> Option<Gen4ValidatedBlock> {
    let start = partition * GEN4_PARTITION_STRIDE + rel_offset;
    let end = start.checked_add(expected_size)?;
    if end > bytes.len() || expected_size <= footer_len {
        return None;
    }
    let data_len = expected_size - footer_len;
    let footer = &bytes[start + data_len..end];
    let (link_count, save_count, block_size, stored_crc) = if footer_len == 16 {
        let count = u32::from_le_bytes([footer[0], footer[1], footer[2], footer[3]]);
        let size = u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]) as usize;
        let crc = u16::from_le_bytes([footer[14], footer[15]]);
        (count, count, size, crc)
    } else {
        let link = u32::from_le_bytes([footer[0], footer[1], footer[2], footer[3]]);
        let count = u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]);
        let size = u32::from_le_bytes([footer[8], footer[9], footer[10], footer[11]]) as usize;
        let crc = u16::from_le_bytes([footer[18], footer[19]]);
        (link, count, size, crc)
    };
    if block_size != expected_size {
        return None;
    }
    let computed_crc = crc16_ccitt(&bytes[start..start + data_len]);
    if computed_crc != stored_crc {
        return None;
    }
    Some(Gen4ValidatedBlock {
        partition,
        start,
        size: expected_size,
        footer_len,
        link_count,
        save_count,
    })
}

fn select_gen4_active_layout(bytes: &[u8]) -> Result<Gen4ActiveLayout, String> {
    for spec in GEN4_SPECS {
        let gens = [
            validate_gen4_block(
                bytes,
                0,
                spec.general_offset,
                spec.general_size,
                spec.footer_len,
            ),
            validate_gen4_block(
                bytes,
                1,
                spec.general_offset,
                spec.general_size,
                spec.footer_len,
            ),
        ];
        let stors = [
            validate_gen4_block(
                bytes,
                0,
                spec.storage_offset,
                spec.storage_size,
                spec.footer_len,
            ),
            validate_gen4_block(
                bytes,
                1,
                spec.storage_offset,
                spec.storage_size,
                spec.footer_len,
            ),
        ];

        if spec.format == PokemonSaveFormat::Gen4HeartGoldSoulSilver {
            // HGSS : exige une partition cohérente (General & Storage valides dans la même
            // partition avec le même compteur save_count, conformément à pokeheartgold save.c)
            let mut best: Option<(Gen4ValidatedBlock, Gen4ValidatedBlock)> = None;
            for p in 0..2 {
                if let (Some(g), Some(s)) = (gens[p], stors[p]) {
                    if g.save_count == s.save_count {
                        let replace = match best {
                            None => true,
                            Some((bg, _)) => g.save_count > bg.save_count,
                        };
                        if replace {
                            best = Some((g, s));
                        }
                    }
                }
            }
            if let Some((general, storage)) = best {
                return Ok(Gen4ActiveLayout {
                    format: spec.format,
                    spec,
                    general,
                    storage,
                });
            }
            // Repli sûr : si une seule partition a General + Storage CRC valides
            for p in 0..2 {
                if let (Some(g), Some(s)) = (gens[p], stors[p]) {
                    return Ok(Gen4ActiveLayout {
                        format: spec.format,
                        spec,
                        general: g,
                        storage: s,
                    });
                }
            }
        } else {
            // DP / Platine : bloc Général le plus récent, puis bloc Stockage relié par link_count
            let active_gen = match (gens[0], gens[1]) {
                (Some(a), Some(b)) => Some(if b.save_count > a.save_count { b } else { a }),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            if let Some(general) = active_gen {
                let valid_stors: Vec<Gen4ValidatedBlock> = stors.into_iter().flatten().collect();
                let linked = valid_stors
                    .iter()
                    .copied()
                    .find(|s| s.link_count == general.link_count)
                    .or_else(|| valid_stors.into_iter().max_by_key(|s| s.save_count));
                if let Some(storage) = linked {
                    return Ok(Gen4ActiveLayout {
                        format: spec.format,
                        spec,
                        general,
                        storage,
                    });
                }
            }
        }
    }

    Err(
        "sauvegarde 512 Ko non reconnue ou sommes de contrôle CRC-16-CCITT Pokémon Gen 4 (DP/Pt/HGSS) invalides"
            .into(),
    )
}

fn gen4_slot_offset(layout: &Gen4ActiveLayout, box_index: u8, slot_index: u8) -> usize {
    let b = usize::from(box_index);
    let s = usize::from(slot_index);
    if layout.format == PokemonSaveFormat::Gen4HeartGoldSoulSilver {
        layout.storage.start + b * GEN4_HGSS_BOX_STRIDE + s * GEN4_PK_LEN
    } else {
        layout.storage.start + 4 + (b * GEN4_SLOTS_PER_BOX + s) * GEN4_PK_LEN
    }
}

fn pk4_prng_xor(body: &mut [u8; 128], seed_checksum: u16) {
    let mut seed = u32::from(seed_checksum);
    for chunk in body.as_chunks_mut::<2>().0 {
        seed = seed.wrapping_mul(0x41C6_4E6D).wrapping_add(0x6073);
        let key = (seed >> 16) as u16;
        let word = u16::from_le_bytes(*chunk) ^ key;
        *chunk = word.to_le_bytes();
    }
}

fn pk4_decrypt_unshuffle(pk4: &[u8]) -> Result<([u8; 128], u32), String> {
    if pk4.len() != GEN4_PK_LEN {
        return Err("structure .pk4 invalide (136 octets attendus)".into());
    }
    let pid = u32::from_le_bytes([pk4[0], pk4[1], pk4[2], pk4[3]]);
    let stored_chk = u16::from_le_bytes([pk4[6], pk4[7]]);
    let mut decrypted = [0u8; 128];
    decrypted.copy_from_slice(&pk4[8..136]);
    pk4_prng_xor(&mut decrypted, stored_chk);

    let mut calc_chk = 0u16;
    for &chunk in decrypted.as_chunks::<2>().0 {
        calc_chk = calc_chk.wrapping_add(u16::from_le_bytes(chunk));
    }
    if calc_chk != stored_chk {
        return Err("somme de contrôle interne .pk4 invalide".into());
    }

    let shift = (((pid & 0x0003_E000) >> 13) % 24) as usize;
    let perm = SUBSTRUCT_PERM_ORDER[shift];
    let mut unshuffled = [0u8; 128];
    for (slot_pos, &block_id) in perm.iter().enumerate() {
        unshuffled[block_id * 32..block_id * 32 + 32]
            .copy_from_slice(&decrypted[slot_pos * 32..slot_pos * 32 + 32]);
    }
    Ok((unshuffled, pid))
}

fn pk4_repack_from_unshuffled(
    orig_pk4: &[u8],
    unshuffled: &[u8; 128],
    pid: u32,
) -> [u8; GEN4_PK_LEN] {
    let mut out = [0u8; GEN4_PK_LEN];
    out[0..8].copy_from_slice(&orig_pk4[0..8]);

    let shift = (((pid & 0x0003_E000) >> 13) % 24) as usize;
    let perm = SUBSTRUCT_PERM_ORDER[shift];
    let mut shuffled = [0u8; 128];
    for (slot_pos, &block_id) in perm.iter().enumerate() {
        shuffled[slot_pos * 32..slot_pos * 32 + 32]
            .copy_from_slice(&unshuffled[block_id * 32..block_id * 32 + 32]);
    }

    let mut new_chk = 0u16;
    for &chunk in shuffled.as_chunks::<2>().0 {
        new_chk = new_chk.wrapping_add(u16::from_le_bytes(chunk));
    }
    out[6..8].copy_from_slice(&new_chk.to_le_bytes());

    pk4_prng_xor(&mut shuffled, new_chk);
    out[8..136].copy_from_slice(&shuffled);
    out
}

fn parse_pk4_summary(pk4: &[u8], box_index: u8, slot_index: u8) -> Option<PokemonBoxSlotSummary> {
    if pk4.len() != GEN4_PK_LEN {
        return None;
    }
    let pid = u32::from_le_bytes([pk4[0], pk4[1], pk4[2], pk4[3]]);
    let chk = u16::from_le_bytes([pk4[6], pk4[7]]);
    if pid == 0 && chk == 0 {
        return None;
    }
    let (unshuffled, pid) = pk4_decrypt_unshuffle(pk4).ok()?;
    let species = u16::from_le_bytes([unshuffled[0], unshuffled[1]]);
    if species == 0 || usize::from(species) > POKEMON_NAMES_FR.len() {
        return None;
    }
    let held_item = u16::from_le_bytes([unshuffled[2], unshuffled[3]]);
    let tid = u16::from_le_bytes([unshuffled[4], unshuffled[5]]);
    let sid = u16::from_le_bytes([unshuffled[6], unshuffled[7]]);
    let exp = u32::from_le_bytes([unshuffled[8], unshuffled[9], unshuffled[10], unshuffled[11]]);

    // Block B commence à l'offset 32 dans unshuffled ; IV/Egg/Nick bitfield à +0x10 (= 48)
    let iv_word = u32::from_le_bytes([
        unshuffled[48],
        unshuffled[49],
        unshuffled[50],
        unshuffled[51],
    ]);
    let is_egg = ((iv_word >> 30) & 1) != 0;

    let p_lo = (pid & 0xFFFF) as u16;
    let p_hi = ((pid >> 16) & 0xFFFF) as u16;
    let is_shiny = ((tid ^ sid) ^ (p_lo ^ p_hi)) < 8;

    // Block C commence à 64 : Surnom = 11 × u16 LE (64..86)
    let nickname = decode_gen4_text(&unshuffled[64..86]);
    // Block D commence à 96 : Nom DO = 8 × u16 LE (96..112)
    let ot_name = decode_gen4_text(&unshuffled[96..112]);
    let species_name = species_name_fr(species).to_string();

    Some(PokemonBoxSlotSummary {
        box_index,
        slot_index,
        species_id: species,
        species_name_fr: species_name.clone(),
        nickname: if nickname.is_empty() {
            species_name
        } else {
            nickname
        },
        ot_name,
        level_approx: approx_level_from_exp(exp),
        held_item_id: held_item,
        held_item_name_fr: held_item_name_fr(4, held_item),
        is_shiny,
        is_egg,
        trade_evolution_preview: trade_evo_preview_label(4, species, held_item, is_egg),
        raw_pk_bytes_len: GEN4_PK_LEN,
    })
}

fn inspect_gen4_save(bytes: &[u8]) -> Result<PokemonSaveInventory, String> {
    let layout = select_gen4_active_layout(&bytes[..GEN4_SAVE_LEN])?;
    let name_off = layout.general.start + layout.spec.trainer_name_offset;
    let trainer_name = decode_gen4_text(&bytes[name_off..name_off + 16]);

    let cur_box_off = if layout.format == PokemonSaveFormat::Gen4HeartGoldSoulSilver {
        layout.storage.start + 0x12000
    } else {
        layout.storage.start
    };
    let current_box = u32::from_le_bytes([
        bytes[cur_box_off],
        bytes[cur_box_off + 1],
        bytes[cur_box_off + 2],
        bytes[cur_box_off + 3],
    ])
    .min(17) as u8;

    let mut pokemon = Vec::new();
    for b in 0..GEN4_BOX_COUNT as u8 {
        for s in 0..GEN4_SLOTS_PER_BOX as u8 {
            let off = gen4_slot_offset(&layout, b, s);
            if let Some(summary) = parse_pk4_summary(&bytes[off..off + GEN4_PK_LEN], b, s) {
                pokemon.push(summary);
            }
        }
    }

    Ok(PokemonSaveInventory {
        format: layout.format,
        generation: 4,
        format_label: layout.format.label_fr().to_string(),
        trainer_name: if trainer_name.is_empty() {
            "Dresseur".into()
        } else {
            trainer_name
        },
        active_slot_label: format!(
            "Partition {} (sauvegarde #{})",
            layout.storage.partition + 1,
            layout.storage.save_count
        ),
        box_count: GEN4_BOX_COUNT as u8,
        slots_per_box: GEN4_SLOTS_PER_BOX as u8,
        current_box,
        pokemon,
    })
}

fn extract_gen4_slot(
    bytes: &[u8],
    _fmt: PokemonSaveFormat,
    box_index: u8,
    slot_index: u8,
) -> Result<(PokemonBoxSlotSummary, Vec<u8>), String> {
    let layout = select_gen4_active_layout(&bytes[..GEN4_SAVE_LEN])?;
    let off = gen4_slot_offset(&layout, box_index, slot_index);
    let pk4 = &bytes[off..off + GEN4_PK_LEN];
    let summary = parse_pk4_summary(pk4, box_index, slot_index)
        .ok_or_else(|| "aucun Pokémon valide dans cet emplacement PC Gen 4".to_string())?;
    Ok((summary, pk4.to_vec()))
}

fn write_gen4_received(
    save_bytes: &[u8],
    _fmt: PokemonSaveFormat,
    box_index: u8,
    slot_index: u8,
    raw_pk: &[u8],
    allow_evolution: bool,
) -> Result<TradeWriteOutcome, String> {
    if raw_pk.len() != GEN4_PK_LEN {
        return Err(format!(
            "structure .pk4 invalide ({} octets, attendu {GEN4_PK_LEN})",
            raw_pk.len()
        ));
    }
    let layout = select_gen4_active_layout(&save_bytes[..GEN4_SAVE_LEN])?;
    let (mut unshuffled, pid) = pk4_decrypt_unshuffle(raw_pk)?;
    let before_species = u16::from_le_bytes([unshuffled[0], unshuffled[1]]);
    if before_species == 0 || usize::from(before_species) > POKEMON_NAMES_FR.len() {
        return Err(format!("espèce Gen 4 invalide (#{before_species})"));
    }
    let held_item = u16::from_le_bytes([unshuffled[2], unshuffled[3]]);
    let iv_word = u32::from_le_bytes([
        unshuffled[48],
        unshuffled[49],
        unshuffled[50],
        unshuffled[51],
    ]);
    let is_egg = ((iv_word >> 30) & 1) != 0;

    let mut after_species = before_species;
    let mut evolved = false;
    let mut consumed_item_name_fr = None;

    if let Some(evo) =
        resolve_trade_evolution(4, before_species, held_item, is_egg, allow_evolution)
    {
        unshuffled[0..2].copy_from_slice(&evo.new_national_species.to_le_bytes());
        if evo.consume_held_item {
            consumed_item_name_fr = held_item_name_fr(4, held_item);
            unshuffled[2..4].copy_from_slice(&0u16.to_le_bytes());
        }
        after_species = evo.new_national_species;
        evolved = true;
    }

    let final_pk4 = pk4_repack_from_unshuffled(raw_pk, &unshuffled, pid);

    // Préserve tout éventuel pied DeSmuME `.dsv` au-delà de 0x80000
    let mut out = save_bytes.to_vec();
    let off = gen4_slot_offset(&layout, box_index, slot_index);
    out[off..off + GEN4_PK_LEN].copy_from_slice(&final_pk4);

    // Recalculer le CRC-16-CCITT du bloc Stockage actif uniquement (les compteurs restent synchrones)
    let stor_start = layout.storage.start;
    let data_len = layout.storage.size - layout.storage.footer_len;
    let new_crc = crc16_ccitt(&out[stor_start..stor_start + data_len]);
    let crc_pos = stor_start + layout.storage.size - 2;
    out[crc_pos..crc_pos + 2].copy_from_slice(&new_crc.to_le_bytes());

    Ok(TradeWriteOutcome {
        updated_save_bytes: out,
        received_species_before_fr: species_name_fr(before_species).to_string(),
        received_species_after_fr: species_name_fr(after_species).to_string(),
        evolved,
        consumed_item_name_fr,
    })
}

// =============================================================================
// DÉCODEURS DE TEXTE OFFICIELS (GEN 1, GEN 3, GEN 4)
// =============================================================================

fn decode_gen1_text(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        if b == 0x50 || b == 0x00 {
            break;
        }
        let ch = match b {
            0x7F => ' ',
            0x80..=0x99 => (b'A' + (b - 0x80)) as char,
            0xA0..=0xB9 => (b'a' + (b - 0xA0)) as char,
            0xBA => 'é',
            0xE3 => '-',
            0xE6 => '?',
            0xE7 => '!',
            0xE8 => '.',
            0xEF => '♂',
            0xF5 => '♀',
            0xF6..=0xFF => (b'0' + (b - 0xF6)) as char,
            _ => continue,
        };
        out.push(ch);
    }
    out.trim().to_string()
}

fn decode_gen3_text(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        if b == 0xFF {
            break;
        }
        let ch = match b {
            0x00 => ' ',
            0x16 => 'à',
            0x17 => 'ç',
            0x19 => 'è',
            0x1A => 'ê',
            0x1B => 'é',
            0xA1..=0xAA => (b'0' + (b - 0xA1)) as char,
            0xAB => '!',
            0xAC => '?',
            0xAD => '.',
            0xAE => '-',
            0xB5 => '♂',
            0xB6 => '♀',
            0xBB..=0xD4 => (b'A' + (b - 0xBB)) as char,
            0xD5..=0xEE => (b'a' + (b - 0xD5)) as char,
            _ => continue,
        };
        out.push(ch);
    }
    out.trim().to_string()
}

fn decode_gen4_text(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &chunk in bytes.as_chunks::<2>().0 {
        let code = u16::from_le_bytes(chunk);
        if code == 0xFFFF || code == 0x0000 {
            break;
        }
        let ch = match code {
            0x0121..=0x012A => (b'0' + (code - 0x0121) as u8) as char,
            0x012B..=0x0144 => (b'A' + (code - 0x012B) as u8) as char,
            0x0145..=0x015E => (b'a' + (code - 0x0145) as u8) as char,
            0x01AB => '!',
            0x01AC => '?',
            0x01AE => '-',
            0x01B1 => '♂',
            0x01B2 => '♀',
            0x01C0 => '.',
            0x01DE => ' ',
            0x01DF => 'à',
            0x01E2 => 'ç',
            0x01E3 => 'è',
            0x01E4 => 'é',
            0x01E5 => 'ê',
            _ => continue,
        };
        out.push(ch);
    }
    out.trim().to_string()
}

// =============================================================================
// TABLES D'INDEX INTERNES GEN 1 & GEN 3 + POKÉDEX FRANÇAIS COMPLET (#1..#493)
// =============================================================================

/// Table complète de correspondance Index interne Gen 1 (`1..=190`) → Pokédex National (`1..=151`).
const GEN1_INTERNAL_TO_NATIONAL: [u16; 191] = [
    0, 112, 115, 32, 35, 21, 100, 34, 80, 2, 103, 108, 102, 88, 94, 29, 31, 104, 111, 131, 59, 151,
    130, 90, 72, 92, 123, 120, 9, 127, 114, 0, 0, 58, 95, 22, 16, 79, 64, 75, 113, 67, 122, 106,
    107, 24, 47, 54, 96, 76, 0, 126, 0, 125, 82, 109, 0, 56, 86, 50, 128, 0, 0, 0, 83, 48, 149, 0,
    0, 0, 84, 60, 124, 146, 144, 145, 132, 52, 98, 0, 0, 0, 37, 38, 25, 26, 0, 0, 147, 148, 140,
    141, 116, 117, 0, 0, 27, 28, 138, 139, 39, 40, 133, 136, 135, 134, 66, 41, 23, 46, 61, 62, 13,
    14, 15, 0, 85, 57, 51, 49, 87, 0, 0, 10, 11, 12, 68, 0, 55, 97, 42, 150, 143, 129, 0, 0, 89, 0,
    99, 91, 0, 101, 36, 110, 53, 105, 0, 93, 63, 65, 17, 18, 121, 1, 3, 73, 0, 118, 119, 0, 0, 0,
    0, 77, 78, 19, 20, 33, 30, 74, 137, 142, 0, 81, 0, 0, 4, 7, 5, 8, 6, 0, 0, 0, 0, 43, 44, 45,
    69, 70, 71,
];

fn gen1_internal_to_national(internal: u8) -> Option<u16> {
    let nat = *GEN1_INTERNAL_TO_NATIONAL.get(usize::from(internal))?;
    (nat > 0).then_some(nat)
}

fn national_to_gen1_internal(national: u16) -> Option<u8> {
    GEN1_INTERNAL_TO_NATIONAL
        .iter()
        .position(|&n| n == national)
        .and_then(|idx| u8::try_from(idx).ok())
}

/// Table complète de correspondance Index interne Gen 3 Hoenn (`277..=411`) → Pokédex National (`252..=386`).
const GEN3_HOENN_INTERNAL_TO_NATIONAL: [u16; 135] = [
    252, 253, 254, 255, 256, 257, 258, 259, 260, 261, 262, 263, 264, 265, 266, 267, 268, 269, 270,
    271, 272, 273, 274, 275, 290, 291, 292, 276, 277, 285, 286, 327, 278, 279, 283, 284, 320, 321,
    300, 301, 352, 343, 344, 299, 324, 302, 339, 340, 370, 341, 342, 349, 350, 318, 319, 328, 329,
    330, 296, 297, 309, 310, 322, 323, 363, 364, 365, 331, 332, 361, 362, 337, 338, 298, 325, 326,
    311, 312, 303, 307, 308, 333, 334, 360, 355, 356, 315, 287, 288, 289, 316, 317, 357, 293, 294,
    295, 366, 367, 368, 359, 353, 354, 336, 335, 369, 304, 305, 306, 351, 313, 314, 345, 346, 347,
    348, 280, 281, 282, 371, 372, 373, 374, 375, 376, 377, 378, 379, 382, 383, 384, 380, 381, 385,
    386, 358,
];

fn gen3_internal_to_national(internal: u16) -> Option<u16> {
    if (1..=251).contains(&internal) {
        Some(internal)
    } else if (277..=411).contains(&internal) {
        Some(GEN3_HOENN_INTERNAL_TO_NATIONAL[usize::from(internal - 277)])
    } else {
        None
    }
}

fn national_to_gen3_internal(national: u16) -> Option<u16> {
    if (1..=251).contains(&national) {
        Some(national)
    } else {
        GEN3_HOENN_INTERNAL_TO_NATIONAL
            .iter()
            .position(|&n| n == national)
            .map(|idx| 277 + idx as u16)
    }
}

/// Retourne le nom français officiel pour tout Pokémon de #1 Bulbizarre à #493 Arceus.
pub fn species_name_fr(national_id: u16) -> &'static str {
    if national_id == 0 {
        return "Inconnu";
    }
    POKEMON_NAMES_FR
        .get(usize::from(national_id - 1))
        .copied()
        .unwrap_or("Pokémon")
}

/// Pokédex français officiel Gen 1 à Gen 4 (#001 à #493).
const POKEMON_NAMES_FR: [&str; 493] = [
    // Gen 1 (#001..#151)
    "Bulbizarre",
    "Herbizarre",
    "Florizarre",
    "Salamèche",
    "Reptincel",
    "Dracaufeu",
    "Carapuce",
    "Carabaffe",
    "Tortank",
    "Chenipan",
    "Chrysacier",
    "Papilusion",
    "Aspicot",
    "Coconfort",
    "Dardargnan",
    "Roucool",
    "Roucoups",
    "Roucarnage",
    "Rattata",
    "Rattatac",
    "Piafabec",
    "Rapasdepic",
    "Abo",
    "Arbok",
    "Pikachu",
    "Raichu",
    "Sabelette",
    "Sablaireau",
    "Nidoran♀",
    "Nidorina",
    "Nidoqueen",
    "Nidoran♂",
    "Nidorino",
    "Nidoking",
    "Mélofée",
    "Mélodelfe",
    "Goupix",
    "Feunard",
    "Rondoudou",
    "Grodoudou",
    "Nosferapti",
    "Nosferalto",
    "Mystherbe",
    "Ortide",
    "Rafflesia",
    "Paras",
    "Parasect",
    "Mimitoss",
    "Aéromite",
    "Taupiqueur",
    "Triopikeur",
    "Miaouss",
    "Persian",
    "Psykokwak",
    "Akwakwak",
    "Férosinge",
    "Colossinge",
    "Caninos",
    "Arcanin",
    "Ptitard",
    "Têtarte",
    "Tartard",
    "Abra",
    "Kadabra",
    "Alakazam",
    "Machoc",
    "Machopeur",
    "Mackogneur",
    "Chétiflor",
    "Boustiflor",
    "Empiflor",
    "Tentacool",
    "Tentacruel",
    "Racaillou",
    "Gravalanch",
    "Grolem",
    "Ponyta",
    "Galopa",
    "Ramoloss",
    "Flagadoss",
    "Magnéti",
    "Magnéton",
    "Canarticho",
    "Doduo",
    "Dodrio",
    "Otaria",
    "Lamantine",
    "Tadmorv",
    "Grotadmorv",
    "Kokiyas",
    "Crustabri",
    "Fantominus",
    "Spectrum",
    "Ectoplasma",
    "Onix",
    "Soporifik",
    "Hypnomade",
    "Krabby",
    "Krabboss",
    "Voltorbe",
    "Électrode",
    "Nœunœuf",
    "Noadkoko",
    "Osselait",
    "Ossatueur",
    "Kicklee",
    "Tygnon",
    "Excelangue",
    "Smogo",
    "Smogogo",
    "Rhinocorne",
    "Rhinoféros",
    "Leveinard",
    "Saquedeneu",
    "Kangourex",
    "Hypotrempe",
    "Hypocéan",
    "Poissirène",
    "Poissoroy",
    "Stari",
    "Staross",
    "M. Mime",
    "Insécateur",
    "Lippoutou",
    "Élektek",
    "Magmar",
    "Scarabrute",
    "Tauros",
    "Magicarpe",
    "Léviator",
    "Lokhlass",
    "Métamorph",
    "Évoli",
    "Aquali",
    "Voltali",
    "Pyroli",
    "Porygon",
    "Amonita",
    "Amonistar",
    "Kabuto",
    "Kabutops",
    "Ptéra",
    "Ronflex",
    "Artikodin",
    "Électhor",
    "Sulfura",
    "Minidraco",
    "Draco",
    "Dracolosse",
    "Mewtwo",
    "Mew",
    // Gen 2 (#152..#251)
    "Germignon",
    "Macronium",
    "Méganium",
    "Héricendre",
    "Feurisson",
    "Typhlosion",
    "Kaiminus",
    "Crocrodil",
    "Aligatueur",
    "Fouinette",
    "Fouinar",
    "Hoothoot",
    "Noarfang",
    "Coxy",
    "Coxyclaque",
    "Mimigal",
    "Migalos",
    "Nostenfer",
    "Loupio",
    "Lanturn",
    "Pichu",
    "Mélo",
    "Toudoudou",
    "Togepi",
    "Togetic",
    "Natu",
    "Xatu",
    "Wattouat",
    "Lainergie",
    "Pharamp",
    "Joliflor",
    "Marill",
    "Azumarill",
    "Simularbre",
    "Tarpaud",
    "Granivol",
    "Floravol",
    "Cotovol",
    "Capumain",
    "Tournegrin",
    "Héliatronc",
    "Yanma",
    "Axoloto",
    "Maraiste",
    "Mentali",
    "Noctali",
    "Cornèbre",
    "Roigada",
    "Feuforêve",
    "Zarbi",
    "Qulbutoké",
    "Girafarig",
    "Pomdepik",
    "Foretress",
    "Insolourdo",
    "Scorplane",
    "Steelix",
    "Snubbull",
    "Granbull",
    "Qwilfish",
    "Cizayox",
    "Caratroc",
    "Scarhino",
    "Farfuret",
    "Teddiursa",
    "Ursaring",
    "Limagma",
    "Volcaropod",
    "Marcacrin",
    "Cochignon",
    "Corayon",
    "Rémoraid",
    "Octillery",
    "Cadoizo",
    "Démanta",
    "Airmure",
    "Malosse",
    "Démolosse",
    "Hyporoi",
    "Phanpy",
    "Donphan",
    "Porygon2",
    "Cerfrousse",
    "Queulorior",
    "Debugant",
    "Kapoera",
    "Lippouti",
    "Élekid",
    "Magby",
    "Écrémeuh",
    "Leuphorie",
    "Raikou",
    "Entei",
    "Suicune",
    "Embrylex",
    "Ymphect",
    "Tyranocif",
    "Lugia",
    "Ho-Oh",
    "Celebi",
    // Gen 3 (#252..#386)
    "Arcko",
    "Massko",
    "Jungko",
    "Poussifeu",
    "Galifeu",
    "Braségali",
    "Gobou",
    "Flobio",
    "Laggron",
    "Medhyèna",
    "Grahyèna",
    "Zigzaton",
    "Linéon",
    "Chenipotte",
    "Armulys",
    "Charmillon",
    "Blindalys",
    "Papinox",
    "Nénupiot",
    "Lombre",
    "Ludicolo",
    "Grainipiot",
    "Pifeuil",
    "Tengalice",
    "Nirondelle",
    "Hélédelle",
    "Goélise",
    "Bekipan",
    "Tarsal",
    "Kirlia",
    "Gardevoir",
    "Arakdo",
    "Maskadra",
    "Balignon",
    "Chapignon",
    "Parecool",
    "Vigoroth",
    "Monaflèmit",
    "Ningale",
    "Ninjask",
    "Munja",
    "Chuchmur",
    "Ramboum",
    "Brouhabam",
    "Makuhita",
    "Hariyama",
    "Azurill",
    "Tarinor",
    "Skitty",
    "Delcatty",
    "Ténéfix",
    "Mysdibule",
    "Galekid",
    "Galegon",
    "Galeking",
    "Méditikka",
    "Charmina",
    "Dynavolt",
    "Élecsprint",
    "Posipi",
    "Négapi",
    "Muciole",
    "Lumivole",
    "Rosélia",
    "Gloupti",
    "Avaltout",
    "Carvanha",
    "Sharpedo",
    "Wailmer",
    "Wailord",
    "Chamallot",
    "Camérupt",
    "Chartor",
    "Spoink",
    "Groret",
    "Spinda",
    "Kraknoix",
    "Vibraninf",
    "Libégon",
    "Cacnea",
    "Cacturne",
    "Tylton",
    "Altaria",
    "Mangriff",
    "Séviper",
    "Séléroc",
    "Solaroc",
    "Barloche",
    "Barbicha",
    "Écrapince",
    "Colhomard",
    "Balbuto",
    "Kaorine",
    "Lilia",
    "Vacilys",
    "Anorith",
    "Armaldo",
    "Barpau",
    "Milobellus",
    "Morphéo",
    "Kecleon",
    "Polichombr",
    "Branette",
    "Skelénox",
    "Téraclope",
    "Tropius",
    "Éoko",
    "Absol",
    "Okéoké",
    "Stalgamin",
    "Oniglali",
    "Obalie",
    "Phogleur",
    "Kaimorse",
    "Coquiperl",
    "Serpang",
    "Rosabyss",
    "Relicanth",
    "Lovdisc",
    "Draby",
    "Drackhaus",
    "Drattak",
    "Terhal",
    "Métang",
    "Métalosse",
    "Regirock",
    "Regice",
    "Registeel",
    "Latias",
    "Latios",
    "Kyogre",
    "Groudon",
    "Rayquaza",
    "Jirachi",
    "Deoxys",
    // Gen 4 (#387..#493)
    "Tortipouss",
    "Boskara",
    "Torterra",
    "Ouisticram",
    "Chimpenfeu",
    "Simiabraz",
    "Tiplouf",
    "Prinplouf",
    "Pingoléon",
    "Étourmi",
    "Étourvol",
    "Étouraptor",
    "Keunotor",
    "Castorno",
    "Crikzik",
    "Mélokrik",
    "Lixy",
    "Luxio",
    "Luxray",
    "Rozbouton",
    "Roserade",
    "Kranidos",
    "Charkos",
    "Dinoclier",
    "Bastiodon",
    "Cheniti",
    "Cheniselle",
    "Papilord",
    "Apitrini",
    "Apireine",
    "Pachirisu",
    "Mustébouée",
    "Mustéflott",
    "Ceribou",
    "Ceriflor",
    "Sancoki",
    "Tritosor",
    "Capidextre",
    "Baudrive",
    "Grodrive",
    "Laporeille",
    "Lockpin",
    "Magirêve",
    "Corboss",
    "Chaglam",
    "Chaffreux",
    "Korillon",
    "Moufouette",
    "Moufflair",
    "Archéomire",
    "Archéodong",
    "Manzaï",
    "Mime Jr.",
    "Ptiravi",
    "Pijako",
    "Spiritomb",
    "Griknot",
    "Carmache",
    "Carchacrok",
    "Goinfrex",
    "Riolu",
    "Lucario",
    "Hippopotas",
    "Hippodocus",
    "Rapion",
    "Drascore",
    "Cradopaud",
    "Coatox",
    "Vortente",
    "Écayon",
    "Luminéon",
    "Babimanta",
    "Blizzi",
    "Blizzaroi",
    "Dimoret",
    "Magnézone",
    "Coudlangue",
    "Rhinastoc",
    "Bouldeneu",
    "Élekable",
    "Maganon",
    "Togekiss",
    "Yanmega",
    "Phyllali",
    "Givrali",
    "Scorvol",
    "Mammochon",
    "Porygon-Z",
    "Gallame",
    "Tarinorme",
    "Noctunoir",
    "Momartik",
    "Motisma",
    "Créhelf",
    "Créfollet",
    "Créfadet",
    "Dialga",
    "Palkia",
    "Heatran",
    "Regigigas",
    "Giratina",
    "Cresselia",
    "Phione",
    "Manaphy",
    "Darkrai",
    "Shaymin",
    "Arceus",
];

// =============================================================================
// TESTS UNITAIRES COMPLETS (GEN 1, GEN 3, GEN 4 PLATINE & HEARTGOLD)
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_gen1_ascii(text: &str, len: usize) -> Vec<u8> {
        let mut out = vec![0x50u8; len];
        for (i, ch) in text.chars().take(len.saturating_sub(1)).enumerate() {
            out[i] = match ch {
                'A'..='Z' => 0x80 + (ch as u8 - b'A'),
                'a'..='z' => 0xA0 + (ch as u8 - b'a'),
                '0'..='9' => 0xF6 + (ch as u8 - b'0'),
                _ => 0x7F,
            };
        }
        out
    }

    fn encode_gen3_ascii(text: &str, len: usize) -> Vec<u8> {
        let mut out = vec![0xFFu8; len];
        for (i, ch) in text.chars().take(len).enumerate() {
            out[i] = match ch {
                'A'..='Z' => 0xBB + (ch as u8 - b'A'),
                'a'..='z' => 0xD5 + (ch as u8 - b'a'),
                '0'..='9' => 0xA1 + (ch as u8 - b'0'),
                _ => 0x00,
            };
        }
        out
    }

    fn encode_gen4_ascii(text: &str, u16_count: usize) -> Vec<u8> {
        let mut out = vec![0u8; u16_count * 2];
        let mut written = 0usize;
        for ch in text.chars().take(u16_count.saturating_sub(1)) {
            let code: u16 = match ch {
                '0'..='9' => 0x0121 + u16::from(ch as u8 - b'0'),
                'A'..='Z' => 0x012B + u16::from(ch as u8 - b'A'),
                'a'..='z' => 0x0145 + u16::from(ch as u8 - b'a'),
                _ => 0x01DE,
            };
            out[written * 2..written * 2 + 2].copy_from_slice(&code.to_le_bytes());
            written += 1;
        }
        out[written * 2..written * 2 + 2].copy_from_slice(&0xFFFFu16.to_le_bytes());
        out
    }

    fn build_synthetic_gen1_save(
        trainer: &str,
        species_nat: u16,
        level: u8,
        nick: &str,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; GEN1_SAVE_LEN];
        buf[0x2598..0x25A3].copy_from_slice(&encode_gen1_ascii(trainer, 11));
        buf[GEN1_CURRENT_BOX_NUM] = 0; // Boîte 1
        let internal = national_to_gen1_internal(species_nat).unwrap();

        let box_off = GEN1_CURRENT_BOX_OFFSET;
        buf[box_off] = 1; // 1 Pokémon dans la boîte
        buf[box_off + 1] = internal;
        buf[box_off + 2] = 0xFF;
        let struct_off = box_off + 22;
        buf[struct_off] = internal;
        buf[struct_off + 3] = level;
        let ot_off = box_off + 22 + GEN1_BOX_SLOTS * 33;
        let nick_off = ot_off + GEN1_BOX_SLOTS * 11;
        buf[ot_off..ot_off + 11].copy_from_slice(&encode_gen1_ascii(trainer, 11));
        buf[nick_off..nick_off + 11].copy_from_slice(&encode_gen1_ascii(nick, 11));

        // Miroir dans Banque 2 boîte 0 + checksums
        let cur = buf[box_off..box_off + GEN1_BOX_SIZE].to_vec();
        buf[GEN1_BANK2_START..GEN1_BANK2_START + GEN1_BOX_SIZE].copy_from_slice(&cur);
        recompute_gen1_bank_checksums(&mut buf, 0);
        buf[GEN1_MAIN_CHECKSUM] = gen1_checksum(&buf[GEN1_MAIN_START..=GEN1_MAIN_END_INCL]);
        buf
    }

    fn build_synthetic_pk3(
        species_nat: u16,
        held_item: u16,
        pid: u32,
        ot_id: u32,
        nick: &str,
        ot: &str,
    ) -> [u8; GEN3_PK_LEN] {
        let mut raw = [0u8; GEN3_PK_LEN];
        raw[0..4].copy_from_slice(&pid.to_le_bytes());
        raw[4..8].copy_from_slice(&ot_id.to_le_bytes());
        raw[0x08..0x12].copy_from_slice(&encode_gen3_ascii(nick, 10));
        raw[0x12] = 3; // Français
        raw[0x13] = 0x02; // Has species
        raw[0x14..0x1B].copy_from_slice(&encode_gen3_ascii(ot, 7));

        let mut unshuffled = [0u8; 48];
        let internal = national_to_gen3_internal(species_nat).unwrap();
        unshuffled[0..2].copy_from_slice(&internal.to_le_bytes());
        unshuffled[2..4].copy_from_slice(&held_item.to_le_bytes());
        unshuffled[4..8].copy_from_slice(&64_000u32.to_le_bytes()); // ~Nv. 40
        pk3_repack_from_unshuffled(&raw, &unshuffled, pid, ot_id)
    }

    fn build_synthetic_gen3_save(
        trainer: &str,
        game_code: u32,
        pk3: [u8; GEN3_PK_LEN],
        box_idx: usize,
        slot_idx: usize,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; GEN3_SAVE_LEN];
        // Remplir Slot A (save_index=5) et Slot B (save_index=7 -> actif!)
        for (slot_i, save_index) in [(0usize, 5u32), (1usize, 7u32)] {
            let base = slot_i * GEN3_SLOT_STRIDE;
            for sec_id in 0..GEN3_SECTION_COUNT {
                // Rotation réaliste des sections comme sur cartouche GBA
                let phys = (sec_id + 3) % GEN3_SECTION_COUNT;
                let off = base + phys * GEN3_SECTION_SIZE;
                if sec_id == 0 {
                    buf[off..off + 8].copy_from_slice(&encode_gen3_ascii(trainer, 8));
                    buf[off + 0xAC..off + 0xB0].copy_from_slice(&game_code.to_le_bytes());
                }
                buf[off + 0xFF4..off + 0xFF6].copy_from_slice(&(sec_id as u16).to_le_bytes());
                buf[off + 0xFF8..off + 0xFFC].copy_from_slice(&GEN3_SIGNATURE.to_le_bytes());
                buf[off + 0xFFC..off + 0x1000].copy_from_slice(&save_index.to_le_bytes());
            }
            // Injecter le Pokémon uniquement dans Slot B (le plus récent)
            if slot_i == 1 {
                let mut pc = vec![0u8; 8 * 3968 + 2000];
                pc[0..4].copy_from_slice(&(box_idx as u32).to_le_bytes());
                let pk_off = 4 + (box_idx * GEN3_SLOTS_PER_BOX + slot_idx) * GEN3_PK_LEN;
                pc[pk_off..pk_off + GEN3_PK_LEN].copy_from_slice(&pk3);
                let mut cursor = 0usize;
                for sec_id in 5..=13usize {
                    let phys = (sec_id + 3) % GEN3_SECTION_COUNT;
                    let off = base + phys * GEN3_SECTION_SIZE;
                    let len = gen3_section_data_len(sec_id as u16);
                    buf[off..off + len].copy_from_slice(&pc[cursor..cursor + len]);
                    cursor += len;
                }
            }
            for sec_id in 0..GEN3_SECTION_COUNT {
                let phys = (sec_id + 3) % GEN3_SECTION_COUNT;
                let off = base + phys * GEN3_SECTION_SIZE;
                let len = gen3_section_data_len(sec_id as u16);
                let chk = gen3_section_checksum(&buf[off..off + len]);
                buf[off + 0xFF6..off + 0xFF8].copy_from_slice(&chk.to_le_bytes());
            }
        }
        buf
    }

    fn build_synthetic_pk4(
        species: u16,
        held_item: u16,
        pid: u32,
        tid: u16,
        sid: u16,
        nick: &str,
        ot: &str,
    ) -> [u8; GEN4_PK_LEN] {
        let mut header = [0u8; GEN4_PK_LEN];
        header[0..4].copy_from_slice(&pid.to_le_bytes());

        let mut unshuffled = [0u8; 128];
        unshuffled[0..2].copy_from_slice(&species.to_le_bytes());
        unshuffled[2..4].copy_from_slice(&held_item.to_le_bytes());
        unshuffled[4..6].copy_from_slice(&tid.to_le_bytes());
        unshuffled[6..8].copy_from_slice(&sid.to_le_bytes());
        unshuffled[8..12].copy_from_slice(&125_000u32.to_le_bytes()); // ~Nv. 50
        unshuffled[64..86].copy_from_slice(&encode_gen4_ascii(nick, 11));
        unshuffled[96..112].copy_from_slice(&encode_gen4_ascii(ot, 8));

        pk4_repack_from_unshuffled(&header, &unshuffled, pid)
    }

    fn build_synthetic_hgss_save(
        trainer: &str,
        pk4: [u8; GEN4_PK_LEN],
        box_idx: usize,
        slot_idx: usize,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; GEN4_SAVE_LEN];
        let spec = GEN4_SPECS[0]; // HGSS
        for (part, count) in [(0usize, 11u32), (1usize, 12u32)] {
            let gen_start = part * GEN4_PARTITION_STRIDE + spec.general_offset;
            let stor_start = part * GEN4_PARTITION_STRIDE + spec.storage_offset;

            buf[gen_start + spec.trainer_name_offset..gen_start + spec.trainer_name_offset + 16]
                .copy_from_slice(&encode_gen4_ascii(trainer, 8));

            if part == 1 {
                let pk_off = stor_start + box_idx * GEN4_HGSS_BOX_STRIDE + slot_idx * GEN4_PK_LEN;
                buf[pk_off..pk_off + GEN4_PK_LEN].copy_from_slice(&pk4);
            }

            // Écrire les footers 16 octets {count, size, magic, slot, crc}
            for (start, size, slot_id) in [
                (gen_start, spec.general_size, 0u16),
                (stor_start, spec.storage_size, 1u16),
            ] {
                let data_len = size - 16;
                let crc = crc16_ccitt(&buf[start..start + data_len]);
                let f = &mut buf[start + data_len..start + size];
                f[0..4].copy_from_slice(&count.to_le_bytes());
                f[4..8].copy_from_slice(&(size as u32).to_le_bytes());
                f[8..12].copy_from_slice(&0x2006_0623u32.to_le_bytes());
                f[12..14].copy_from_slice(&slot_id.to_le_bytes());
                f[14..16].copy_from_slice(&crc.to_le_bytes());
            }
        }
        buf
    }

    #[test]
    fn gen1_haunter_evolves_to_gengar_with_valid_checksums() {
        let save_a = build_synthetic_gen1_save("RED", 93, 35, "SPECTRUM");
        let save_b = build_synthetic_gen1_save("BLUE", 64, 32, "KADABRA");

        let inv_a = inspect_pokemon_save(&save_a).expect("inspect gen1 A");
        assert_eq!(inv_a.format, PokemonSaveFormat::Gen1RedBlueYellow);
        assert_eq!(inv_a.trainer_name, "RED");
        assert_eq!(inv_a.pokemon.len(), 1);
        assert_eq!(inv_a.pokemon[0].species_name_fr, "Spectrum");
        assert_eq!(
            inv_a.pokemon[0].trade_evolution_preview.as_deref(),
            Some("Spectrum → Ectoplasma")
        );

        let offer_a = extract_boxed_pokemon(&save_a, 0, 0).expect("extract A");
        let out_b = apply_received_pokemon_trade(&save_b, 0, 0, &offer_a, true).expect("trade B");
        assert!(out_b.evolved);
        assert_eq!(out_b.received_species_before_fr, "Spectrum");
        assert_eq!(out_b.received_species_after_fr, "Ectoplasma");

        let inv_b_after = inspect_pokemon_save(&out_b.updated_save_bytes).expect("re-verify B");
        assert_eq!(inv_b_after.pokemon[0].species_id, 94);
        assert_eq!(inv_b_after.pokemon[0].species_name_fr, "Ectoplasma");
    }

    #[test]
    fn gen3_scyther_metal_coat_evolves_and_consumes_item_while_everstone_blocks() {
        let scyther = build_synthetic_pk3(
            123,
            GEN3_ITEM_METAL_COAT,
            0x1234_5678,
            0x0001_0002,
            "INSEC",
            "BRICE",
        );
        let save_emerald = build_synthetic_gen3_save("BRICE", 0xDEAD_BEEF, scyther, 2, 5);

        let inv = inspect_pokemon_save(&save_emerald).expect("inspect Emerald");
        assert_eq!(inv.format, PokemonSaveFormat::Gen3Emerald);
        assert_eq!(inv.trainer_name, "BRICE");
        assert_eq!(inv.pokemon.len(), 1);
        assert_eq!(inv.pokemon[0].species_name_fr, "Insécateur");
        assert_eq!(
            inv.pokemon[0].trade_evolution_preview.as_deref(),
            Some("Insécateur → Cizayox")
        );

        let haunter_everstone = build_synthetic_pk3(
            93,
            GEN3_ITEM_EVERSTONE,
            0x9988_7766,
            0x0005_0006,
            "GHOST",
            "FLORA",
        );
        let save_frlg = build_synthetic_gen3_save("FLORA", 1, haunter_everstone, 0, 0);

        // Échange Insécateur (Peau Métal) -> Rouge Feu : doit évoluer en Cizayox et consommer l'objet
        let offer_scyther = extract_boxed_pokemon(&save_emerald, 2, 5).expect("extract scyther");
        let out_frlg =
            apply_received_pokemon_trade(&save_frlg, 0, 0, &offer_scyther, true).expect("trade");
        assert!(out_frlg.evolved);
        assert_eq!(out_frlg.received_species_after_fr, "Cizayox");
        assert_eq!(
            out_frlg.consumed_item_name_fr.as_deref(),
            Some("Peau Métal")
        );
        let inv_frlg_after = inspect_pokemon_save(&out_frlg.updated_save_bytes).expect("reverify");
        assert_eq!(inv_frlg_after.pokemon[0].species_id, 212);
        assert_eq!(inv_frlg_after.pokemon[0].held_item_id, 0);

        // Échange Spectrum (Pierre Stase) -> Émeraude : la Pierre Stase bloque l'évolution !
        let offer_haunter = extract_boxed_pokemon(&save_frlg, 0, 0).expect("extract haunter");
        assert_eq!(offer_haunter.summary.trade_evolution_preview, None);
        let out_em =
            apply_received_pokemon_trade(&save_emerald, 2, 5, &offer_haunter, true).expect("trade");
        assert!(!out_em.evolved);
        assert_eq!(out_em.received_species_after_fr, "Spectrum");
    }

    #[test]
    fn gen4_heartgold_trade_handles_16byte_footer_4k_box_stride_and_dsv_trailer() {
        // PID shiny exact pour tid=1234, sid=1234 -> (tid^sid)=0, p_lo^p_hi=3 < 8
        let shiny_pid = 0x0001_0002u32;
        let dusclops = build_synthetic_pk4(
            356,
            GEN4_ITEM_REAPER_CLOTH,
            shiny_pid,
            1234,
            1234,
            "TERACLOP",
            "LUTH",
        );
        let mut save_hg = build_synthetic_hgss_save("LUTH", dusclops, 3, 14);
        // Ajouter un pied DeSmuME .dsv de 122 octets pour vérifier sa préservation intégrale
        let dsv_trailer = vec![0x5Au8; 122];
        save_hg.extend_from_slice(&dsv_trailer);

        let kadabra = build_synthetic_pk4(64, 0, 0x2222_3333, 5555, 6666, "KADABRA", "CELESTA");
        let save_ss = build_synthetic_hgss_save("CELESTA", kadabra, 0, 1);

        let inv_hg = inspect_pokemon_save(&save_hg).expect("inspect HG");
        assert_eq!(inv_hg.format, PokemonSaveFormat::Gen4HeartGoldSoulSilver);
        assert_eq!(inv_hg.trainer_name, "LUTH");
        assert_eq!(inv_hg.pokemon.len(), 1);
        assert!(inv_hg.pokemon[0].is_shiny);
        assert_eq!(
            inv_hg.pokemon[0].trade_evolution_preview.as_deref(),
            Some("Téraclope → Noctunoir")
        );

        let offer_hg = extract_boxed_pokemon(&save_hg, 3, 14).expect("extract HG");
        let out_ss =
            apply_received_pokemon_trade(&save_ss, 0, 1, &offer_hg, true).expect("trade SS");
        assert!(out_ss.evolved);
        assert_eq!(out_ss.received_species_after_fr, "Noctunoir");
        assert_eq!(
            out_ss.consumed_item_name_fr.as_deref(),
            Some("Tissu Fauche")
        );

        let offer_ss = extract_boxed_pokemon(&save_ss, 0, 1).expect("extract SS");
        let out_hg =
            apply_received_pokemon_trade(&save_hg, 3, 14, &offer_ss, true).expect("trade HG");
        assert!(out_hg.evolved);
        assert_eq!(out_hg.received_species_after_fr, "Alakazam");
        assert_eq!(
            &out_hg.updated_save_bytes[GEN4_SAVE_LEN..],
            &dsv_trailer[..]
        );

        // Une corruption d'1 bit dans le bloc Stockage doit être rejetée
        let mut corrupted = out_ss.updated_save_bytes.clone();
        corrupted[GEN4_PARTITION_STRIDE + 0x0F700] ^= 0xFF;
        // La partition 0 reste valide en repli ; corrompre aussi partition 0 doit échouer
        corrupted[0x0F700] ^= 0xFF;
        assert!(inspect_pokemon_save(&corrupted).is_err());
    }

    #[test]
    fn gen2_crystal_gold_trade_evolves_onix_preserves_rtc_and_blocks_everstone() {
        #[allow(clippy::too_many_arguments)]
        fn build_synthetic_gen2_save(
            crystal: bool,
            trainer: &str,
            species: u8,
            held_item: u8,
            level: u8,
            shiny: bool,
            nick: &str,
            rtc_trailer: &[u8],
        ) -> Vec<u8> {
            let mut buf = vec![0u8; GEN2_SAVE_LEN];
            buf[GEN2_MAGIC_1_OFFSET] = GEN2_MAGIC_1_VALUE;
            buf[0x200B..0x2016].copy_from_slice(&encode_gen1_ascii(trainer, 11));
            let (cur_num_off, cur_box_off, chk1_end, chk1_off, magic2_off) = if crystal {
                (
                    GEN2_CRYSTAL_CURRENT_BOX_NUM,
                    GEN2_CRYSTAL_CURRENT_BOX_OFFSET,
                    0x2B82,
                    GEN2_CRYSTAL_CHECKSUM_1_OFFSET,
                    GEN2_CRYSTAL_MAGIC_2_OFFSET,
                )
            } else {
                (
                    GEN2_GS_CURRENT_BOX_NUM,
                    GEN2_GS_CURRENT_BOX_OFFSET,
                    0x2D68,
                    GEN2_GS_CHECKSUM_1_OFFSET,
                    GEN2_GS_MAGIC_2_OFFSET,
                )
            };
            buf[cur_num_off] = 0; // Boîte 1
            buf[magic2_off] = GEN2_MAGIC_2_VALUE;

            // Remplir Boîte courante
            buf[cur_box_off] = 1;
            buf[cur_box_off + 1] = species;
            buf[cur_box_off + 2] = 0xFF;
            let struct_off = cur_box_off + 22;
            buf[struct_off] = species;
            buf[struct_off + 1] = held_item;
            if shiny {
                // Attack=10 (bit 1 set), Defense=10, Speed=10, Special=10 -> Shiny Gen 2!
                buf[struct_off + 21] = 0xAA;
                buf[struct_off + 22] = 0xAA;
            }
            buf[struct_off + 31] = level;
            let ot_off = cur_box_off + 22 + GEN2_BOX_SLOTS * 32;
            let nick_off = ot_off + GEN2_BOX_SLOTS * 11;
            buf[ot_off..ot_off + 11].copy_from_slice(&encode_gen1_ascii(trainer, 11));
            buf[nick_off..nick_off + 11].copy_from_slice(&encode_gen1_ascii(nick, 11));

            // Calculer Checksum 1 officiel
            let chk1 = gen2_checksum(&buf[0x2009..=chk1_end]);
            buf[chk1_off..chk1_off + 2].copy_from_slice(&chk1.to_le_bytes());

            // Ajouter un éventuel pied RTC MBC3 (ex: 48 octets)
            buf.extend_from_slice(rtc_trailer);
            buf
        }

        let rtc_trailer = vec![0xA5u8; 48];
        let save_crystal = build_synthetic_gen2_save(
            true,
            "KRIS",
            95, // Onix
            GEN2_ITEM_METAL_COAT as u8,
            40,
            true, // Shiny !
            "ONIX",
            &rtc_trailer,
        );
        let save_gold = build_synthetic_gen2_save(
            false,
            "GOLD",
            123, // Insécateur + Pierre Stase (ne doit PAS évoluer)
            GEN2_ITEM_EVERSTONE as u8,
            35,
            false,
            "INSEC",
            &[],
        );

        let inv_c = inspect_pokemon_save(&save_crystal).expect("inspect Crystal");
        assert_eq!(inv_c.format, PokemonSaveFormat::Gen2Crystal);
        assert_eq!(inv_c.trainer_name, "KRIS");
        assert_eq!(inv_c.pokemon.len(), 1);
        assert!(inv_c.pokemon[0].is_shiny);
        assert_eq!(inv_c.pokemon[0].species_name_fr, "Onix");
        assert_eq!(
            inv_c.pokemon[0].trade_evolution_preview.as_deref(),
            Some("Onix → Steelix")
        );

        let inv_g = inspect_pokemon_save(&save_gold).expect("inspect Gold");
        assert_eq!(inv_g.format, PokemonSaveFormat::Gen2GoldSilver);
        assert_eq!(inv_g.pokemon[0].trade_evolution_preview, None);

        // Cristal -> Or : Onix (Peau Métal) évolue en Steelix shiny et consomme Peau Métal
        let offer_c = extract_boxed_pokemon(&save_crystal, 0, 0).expect("extract Crystal");
        let out_gold =
            apply_received_pokemon_trade(&save_gold, 0, 0, &offer_c, true).expect("trade Gold");
        assert!(out_gold.evolved);
        assert_eq!(out_gold.received_species_after_fr, "Steelix");
        assert_eq!(
            out_gold.consumed_item_name_fr.as_deref(),
            Some("Peau Métal")
        );
        let inv_gold_after = inspect_pokemon_save(&out_gold.updated_save_bytes).expect("reverify");
        assert_eq!(inv_gold_after.pokemon[0].species_id, 208);
        assert_eq!(inv_gold_after.pokemon[0].held_item_id, 0);
        assert!(inv_gold_after.pokemon[0].is_shiny);

        // Or -> Cristal : Insécateur (Pierre Stase) reste Insécateur + RTC 48o préservé
        let offer_g = extract_boxed_pokemon(&save_gold, 0, 0).expect("extract Gold");
        let out_crystal = apply_received_pokemon_trade(&save_crystal, 0, 0, &offer_g, true)
            .expect("trade Crystal");
        assert!(!out_crystal.evolved);
        assert_eq!(out_crystal.received_species_after_fr, "Insécateur");
        assert_eq!(
            &out_crystal.updated_save_bytes[GEN2_SAVE_LEN..],
            &rtc_trailer[..]
        );

        // Miroir Banque 2 Boîte 0 doit être identique à la boîte courante
        assert_eq!(
            &out_crystal.updated_save_bytes[GEN2_BANK2_START..GEN2_BANK2_START + GEN2_BOX_SIZE],
            &out_crystal.updated_save_bytes
                [GEN2_CRYSTAL_CURRENT_BOX_OFFSET..GEN2_CRYSTAL_CURRENT_BOX_OFFSET + GEN2_BOX_SIZE]
        );

        // Corruption de la somme de contrôle -> rejetée
        let mut corrupted = out_gold.updated_save_bytes.clone();
        corrupted[0x2010] ^= 0xFF;
        assert!(inspect_pokemon_save(&corrupted).is_err());
    }

    #[test]
    fn cross_generation_trade_is_rejected() {
        let save_gen1 = build_synthetic_gen1_save("RED", 93, 25, "SPECTRUM");
        let kadabra_pk4 = build_synthetic_pk4(64, 0, 0x1111_2222, 1, 2, "KADABRA", "LUTH");
        let save_gen4 = build_synthetic_hgss_save("LUTH", kadabra_pk4, 0, 0);

        let offer_gen4 = extract_boxed_pokemon(&save_gen4, 0, 0).unwrap();
        let err = apply_received_pokemon_trade(&save_gen1, 0, 0, &offer_gen4, true).unwrap_err();
        assert!(err.contains("incompatibles"));
    }
}
