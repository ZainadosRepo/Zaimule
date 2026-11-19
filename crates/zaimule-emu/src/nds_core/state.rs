//! Savestate serialization for the vendored NDS core using compact positional
//! binary encoding, PackBits RLE compression, and CRC32 integrity verification.

use super::bus::{Bus, Machine, MachineHandle};
use super::cpu::{Cpu, CpuState};
use super::ppu::Ppu;
use serde::de::{self, DeserializeOwned, DeserializeSeed, SeqAccess, Visitor};
use serde::ser::{
    self, Serialize, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant,
    SerializeTuple, SerializeTupleStruct, SerializeTupleVariant,
};
use std::fmt;

const MAGIC: &[u8; 4] = b"ZMND";
const VERSION: u8 = 1;
const MAX_UNCOMPRESSED_BYTES: usize = 32 * 1024 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SaveState<M = Machine, C = CpuState, P = Ppu> {
    pub machine: M,
    pub cpu9: C,
    pub cpu7: C,
    pub ppu: P,
    pub frame: u32,
}

#[derive(Debug)]
struct BinaryError(String);

impl fmt::Display for BinaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BinaryError {}

impl ser::Error for BinaryError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

impl de::Error for BinaryError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

struct BinarySerializer {
    buf: Vec<u8>,
}

impl BinarySerializer {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(6 * 1024 * 1024),
        }
    }
}

impl ser::Serializer for &mut BinarySerializer {
    type Ok = ();
    type Error = BinaryError;

    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn serialize_bool(self, v: bool) -> Result<Self::Ok, Self::Error> {
        self.buf.push(u8::from(v));
        Ok(())
    }

    fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
        self.buf.push(v as u8);
        Ok(())
    }

    fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
        self.buf.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
        self.buf.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
        self.buf.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
        self.buf.push(v);
        Ok(())
    }

    fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
        self.buf.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
        self.buf.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
        self.buf.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_f32(self, v: f32) -> Result<Self::Ok, Self::Error> {
        self.serialize_u32(v.to_bits())
    }

    fn serialize_f64(self, v: f64) -> Result<Self::Ok, Self::Error> {
        self.serialize_u64(v.to_bits())
    }

    fn serialize_char(self, v: char) -> Result<Self::Ok, Self::Error> {
        self.serialize_u32(u32::from(v))
    }

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        self.serialize_bytes(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        let len = u32::try_from(v.len()).map_err(|_| BinaryError("octet seq too long".into()))?;
        self.buf.extend_from_slice(&len.to_le_bytes());
        self.buf.extend_from_slice(v);
        Ok(())
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        self.buf.push(0);
        Ok(())
    }

    fn serialize_some<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        self.buf.push(1);
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.serialize_u32(variant_index)
    }

    fn serialize_newtype_struct<T>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        self.serialize_u32(variant_index)?;
        value.serialize(self)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        let len = len.ok_or_else(|| BinaryError("sequence length must be known".into()))?;
        let len_u32 = u32::try_from(len).map_err(|_| BinaryError("sequence too long".into()))?;
        self.buf.extend_from_slice(&len_u32.to_le_bytes());
        Ok(self)
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Ok(self)
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Ok(self)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        self.serialize_u32(variant_index)?;
        Ok(self)
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Err(BinaryError(
            "maps are not supported in NDS binary savestates".into(),
        ))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(self)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        self.serialize_u32(variant_index)?;
        Ok(self)
    }
}

impl SerializeSeq for &mut BinarySerializer {
    type Ok = ();
    type Error = BinaryError;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }
}

impl SerializeTuple for &mut BinarySerializer {
    type Ok = ();
    type Error = BinaryError;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }
}

impl SerializeTupleStruct for &mut BinarySerializer {
    type Ok = ();
    type Error = BinaryError;

    fn serialize_field<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }
}

impl SerializeTupleVariant for &mut BinarySerializer {
    type Ok = ();
    type Error = BinaryError;

    fn serialize_field<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }
}

impl SerializeMap for &mut BinarySerializer {
    type Ok = ();
    type Error = BinaryError;

    fn serialize_key<T>(&mut self, _key: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        Err(BinaryError("maps unsupported".into()))
    }

    fn serialize_value<T>(&mut self, _value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        Err(BinaryError("maps unsupported".into()))
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Err(BinaryError("maps unsupported".into()))
    }
}

impl SerializeStruct for &mut BinarySerializer {
    type Ok = ();
    type Error = BinaryError;

    fn serialize_field<T>(&mut self, _key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }
}

impl SerializeStructVariant for &mut BinarySerializer {
    type Ok = ();
    type Error = BinaryError;

    fn serialize_field<T>(&mut self, _key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }
}

struct BinaryDeserializer<'de> {
    input: &'de [u8],
    pos: usize,
}

impl<'de> BinaryDeserializer<'de> {
    fn new(input: &'de [u8]) -> Self {
        Self { input, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Result<&'de [u8], BinaryError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&e| e <= self.input.len())
            .ok_or_else(|| BinaryError("unexpected end of savestate payload".into()))?;
        let slice = &self.input[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, BinaryError> {
        Ok(self.take(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, BinaryError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn read_u32(&mut self) -> Result<u32, BinaryError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> Result<u64, BinaryError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

struct FixedSeq<'a, 'de> {
    de: &'a mut BinaryDeserializer<'de>,
    remaining: usize,
}

impl<'a, 'de> SeqAccess<'de> for FixedSeq<'a, 'de> {
    type Error = BinaryError;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(&mut *self.de).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining)
    }
}

impl<'de> de::Deserializer<'de> for &mut BinaryDeserializer<'de> {
    type Error = BinaryError;

    fn deserialize_any<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(BinaryError(
            "self-describing deserialize_any is not supported".into(),
        ))
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.read_u8()? {
            0 => visitor.visit_bool(false),
            1 => visitor.visit_bool(true),
            other => Err(BinaryError(format!("invalid boolean tag {other}"))),
        }
    }

    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i8(self.read_u8()? as i8)
    }

    fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i16(self.read_u16()? as i16)
    }

    fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i32(self.read_u32()? as i32)
    }

    fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i64(self.read_u64()? as i64)
    }

    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u8(self.read_u8()?)
    }

    fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u16(self.read_u16()?)
    }

    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u32(self.read_u32()?)
    }

    fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u64(self.read_u64()?)
    }

    fn deserialize_f32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_f32(f32::from_bits(self.read_u32()?))
    }

    fn deserialize_f64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_f64(f64::from_bits(self.read_u64()?))
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let code = self.read_u32()?;
        let ch = char::from_u32(code).ok_or_else(|| BinaryError("invalid scalar char".into()))?;
        visitor.visit_char(ch)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        let text = std::str::from_utf8(bytes)
            .map_err(|e| BinaryError(format!("invalid utf-8 string: {e}")))?;
        visitor.visit_borrowed_str(text)
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        visitor.visit_borrowed_bytes(bytes)
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        match self.read_u8()? {
            0 => visitor.visit_none(),
            1 => visitor.visit_some(self),
            other => Err(BinaryError(format!("invalid option tag {other}"))),
        }
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let len = self.read_u32()? as usize;
        if len > self.remaining() {
            return Err(BinaryError(format!(
                "corrupt savestate sequence length {len} exceeds remaining bytes {}",
                self.remaining()
            )));
        }
        visitor.visit_seq(FixedSeq {
            de: self,
            remaining: len,
        })
    }

    fn deserialize_tuple<V>(self, len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_seq(FixedSeq {
            de: self,
            remaining: len,
        })
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_seq(FixedSeq {
            de: self,
            remaining: len,
        })
    }

    fn deserialize_map<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(BinaryError("maps unsupported".into()))
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_seq(FixedSeq {
            de: self,
            remaining: fields.len(),
        })
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(BinaryError(
            "enums unsupported in NDS binary savestates".into(),
        ))
    }

    fn deserialize_identifier<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(BinaryError("identifiers unsupported".into()))
    }

    fn deserialize_ignored_any<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(BinaryError("ignored fields unsupported".into()))
    }
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    zaimule_core::catalog::compute_crc32(bytes)
}

/// PackBits-style RLE encoding:
/// - `0..=127`: literal run of `(ctrl + 1)` bytes (`1..=128`)
/// - `128..=255`: repeated run of `(257 - ctrl)` copies (`2..=129`)
fn rle_compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 2);
    let mut i = 0;
    while i < input.len() {
        let mut run = 1usize;
        while i + run < input.len() && run < 129 && input[i + run] == input[i] {
            run += 1;
        }
        if run >= 3 {
            out.push((257 - run) as u8);
            out.push(input[i]);
            i += run;
            continue;
        }
        let lit_start = i;
        while i < input.len() && (i - lit_start) < 128 {
            if i + 2 < input.len() && input[i] == input[i + 1] && input[i] == input[i + 2] {
                break;
            }
            i += 1;
        }
        let lit_len = i - lit_start;
        out.push((lit_len - 1) as u8);
        out.extend_from_slice(&input[lit_start..i]);
    }
    out
}

fn rle_decompress(compressed: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    if expected_len > MAX_UNCOMPRESSED_BYTES {
        return Err(format!("état NDS trop volumineux ({expected_len} octets)"));
    }
    let mut out = Vec::with_capacity(expected_len);
    let mut i = 0;
    while i < compressed.len() {
        let ctrl = compressed[i];
        i += 1;
        if ctrl < 128 {
            let count = usize::from(ctrl) + 1;
            let end = i + count;
            if end > compressed.len() || out.len() + count > expected_len {
                return Err("données RLE littérales NDS invalides".into());
            }
            out.extend_from_slice(&compressed[i..end]);
            i = end;
        } else {
            if i >= compressed.len() {
                return Err("données RLE répétées NDS tronquées".into());
            }
            let count = 257 - usize::from(ctrl);
            if out.len() + count > expected_len {
                return Err("dépassement RLE dans la savestate NDS".into());
            }
            let val = compressed[i];
            i += 1;
            out.resize(out.len() + count, val);
        }
    }
    if out.len() != expected_len {
        return Err(format!(
            "taille décompressée NDS incohérente (attendu {expected_len}, reçu {})",
            out.len()
        ));
    }
    Ok(out)
}

pub fn encode_binary<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut ser = BinarySerializer::new();
    value.serialize(&mut ser).map_err(|e| e.to_string())?;
    let raw = ser.buf;
    let raw_len = u32::try_from(raw.len()).map_err(|_| "état NDS trop grand".to_string())?;
    let crc = crc32_ieee(&raw);
    let compressed = rle_compress(&raw);
    let mut out = Vec::with_capacity(13 + compressed.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&raw_len.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

pub fn decode_binary<T: DeserializeOwned>(packet: &[u8]) -> Result<T, String> {
    if packet.len() < 13 || &packet[..4] != MAGIC {
        return Err("en-tête savestate NDS invalide (attendu ZMND)".into());
    }
    if packet[4] != VERSION {
        return Err(format!(
            "version de savestate NDS non prise en charge ({})",
            packet[4]
        ));
    }
    let expected_len = u32::from_le_bytes(packet[5..9].try_into().unwrap()) as usize;
    let expected_crc = u32::from_le_bytes(packet[9..13].try_into().unwrap());
    let raw = rle_decompress(&packet[13..], expected_len)?;
    let actual_crc = crc32_ieee(&raw);
    if actual_crc != expected_crc {
        return Err("somme de contrôle CRC32 savestate NDS invalide".into());
    }
    let mut de = BinaryDeserializer::new(&raw);
    let value = T::deserialize(&mut de).map_err(|e| format!("décodage état NDS : {e}"))?;
    if de.remaining() != 0 {
        return Err("octets résiduels après décodage de la savestate NDS".into());
    }
    Ok(value)
}

pub fn save_snapshot<B: Bus>(
    machine: &MachineHandle,
    cpu9: &Cpu<B>,
    cpu7: &Cpu<B>,
    ppu: &Ppu,
    frame: u32,
) -> Result<Vec<u8>, String> {
    let mm = machine.borrow();
    let snap = SaveState {
        machine: &*mm,
        cpu9: &cpu9.st,
        cpu7: &cpu7.st,
        ppu,
        frame,
    };
    encode_binary(&snap)
}

pub fn restore_snapshot<B: Bus>(
    bytes: &[u8],
    machine: &mut MachineHandle,
    cpu9: &mut Cpu<B>,
    cpu7: &mut Cpu<B>,
    ppu: &mut Ppu,
    rom: &[u8],
) -> Result<u32, String> {
    let SaveState {
        machine: restored_machine,
        cpu9: c9,
        cpu7: c7,
        ppu: p,
        frame,
    } = decode_binary::<SaveState>(bytes)?;
    {
        let mut mm = machine.borrow_mut();
        let io_log = mm.io_log.take();
        let bios7 = mm.bios7.take();
        let bios9 = mm.bios9.take();
        let custom_firmware = mm.custom_firmware;
        let touch_cal = mm.touch_cal;
        *mm = restored_machine;
        mm.rom = rom.to_vec();
        mm.io_log = io_log;
        mm.bios7 = bios7;
        mm.bios9 = bios9;
        mm.custom_firmware = custom_firmware;
        mm.touch_cal = super::bus::extract_firmware_user_settings(&mm.firmware)
            .and_then(|us| super::bus::TouchCalibration::from_user_settings(&us))
            .or(touch_cal);
        mm.save_dirty = false;
    }
    cpu9.restore(c9);
    cpu7.restore(c7);
    *ppu = p;
    let cp15 = cpu9.st.cp15_dtcm;
    if cp15 != 0 {
        cpu9.bus.set_dtcm(cp15 & 0xFFFF_F000);
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nds_core::bus::View;

    #[test]
    fn roundtrip_save_state_snapshot() {
        let mut machine = MachineHandle::new(Machine::new());
        {
            let mut mm = machine.borrow_mut();
            mm.main_ram[0x1234] = 0xAB;
            mm.vram[0][0x40] = 0x55;
            mm.save[0x10] = 0x7E;
            mm.fifo_to7.push_back(0x1234_5678);
        }
        let mut cpu9 = Cpu::new(
            View {
                m: machine.ptr(),
                cpu: 0,
                in_dma: false,
            },
            true,
            0x0200_0800,
            0x0300_2F7C,
            0x0300_3F80,
            0x0300_3FC0,
        );
        let mut cpu7 = Cpu::new(
            View {
                m: machine.ptr(),
                cpu: 1,
                in_dma: false,
            },
            false,
            0x0238_0000,
            0x0380_FD80,
            0x0380_FF80,
            0x0380_FFC0,
        );
        cpu9.st.regs[3] = 0xDEAD_BEEF;
        cpu7.st.regs[2] = 0xCAFE_BABE;
        let mut ppu = Ppu::new();
        ppu.fb_a[42] = 0x0011_2233;

        let encoded = save_snapshot(&machine, &cpu9, &cpu7, &ppu, 144).expect("encode snapshot");
        assert!(
            encoded.len() < 128 * 1024,
            "RLE should compact sparse state cleanly"
        );

        {
            let mut mm = machine.borrow_mut();
            mm.main_ram[0x1234] = 0;
            mm.vram[0][0x40] = 0;
            mm.fifo_to7.clear();
        }
        cpu9.st.regs[3] = 0;
        cpu7.st.regs[2] = 0;
        ppu.fb_a[42] = 0;

        let frame = restore_snapshot(
            &encoded,
            &mut machine,
            &mut cpu9,
            &mut cpu7,
            &mut ppu,
            &[1, 2, 3, 4],
        )
        .expect("restore snapshot");
        assert_eq!(frame, 144);
        assert_eq!(cpu9.st.regs[3], 0xDEAD_BEEF);
        assert_eq!(cpu7.st.regs[2], 0xCAFE_BABE);
        assert_eq!(ppu.fb_a[42], 0x0011_2233);
        let mm = machine.borrow();
        assert_eq!(mm.main_ram[0x1234], 0xAB);
        assert_eq!(mm.vram[0][0x40], 0x55);
        assert_eq!(mm.save[0x10], 0x7E);
        assert_eq!(mm.fifo_to7.front().copied(), Some(0x1234_5678));
        assert_eq!(mm.rom, vec![1, 2, 3, 4]);
    }
}
