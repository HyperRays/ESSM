use std::fmt;
use std::io::{self, Read, Write};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

const MAGIC: &[u8; 4] = b"FIDX";
const ETF_VERSION: u8 = 131;
const MAXIMUM_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAXIMUM_NESTING: usize = 128;

const NEW_FLOAT_EXT: u8 = 70;
const SMALL_INTEGER_EXT: u8 = 97;
const INTEGER_EXT: u8 = 98;
const ATOM_EXT: u8 = 100;
const SMALL_TUPLE_EXT: u8 = 104;
const LARGE_TUPLE_EXT: u8 = 105;
const NIL_EXT: u8 = 106;
const STRING_EXT: u8 = 107;
const LIST_EXT: u8 = 108;
const BINARY_EXT: u8 = 109;
const SMALL_BIG_EXT: u8 = 110;
const LARGE_BIG_EXT: u8 = 111;
const SMALL_ATOM_EXT: u8 = 115;
const MAP_EXT: u8 = 116;
const ATOM_UTF8_EXT: u8 = 118;
const SMALL_ATOM_UTF8_EXT: u8 = 119;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Unsigned(u64),
    Signed(i64),
    Float(f64),
    Binary(Vec<u8>),
    Atom(String),
    Tuple(Vec<Value>),
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
}

impl Value {
    pub(crate) fn binary(value: impl Into<Vec<u8>>) -> Self {
        Self::Binary(value.into())
    }

    #[cfg(test)]
    pub(crate) fn map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Self {
        Self::Map(
            entries
                .into_iter()
                .map(|(key, value)| (Self::binary(key.as_bytes()), value))
                .collect(),
        )
    }

    pub(crate) fn from_json(value: JsonValue) -> Result<Self, Error> {
        match value {
            JsonValue::Null => Ok(Self::Null),
            JsonValue::Bool(value) => Ok(Self::Bool(value)),
            JsonValue::Number(value) => {
                if let Some(value) = value.as_u64() {
                    Ok(Self::Unsigned(value))
                } else if let Some(value) = value.as_i64() {
                    Ok(Self::Signed(value))
                } else if let Some(value) = value.as_f64() {
                    Ok(Self::Float(value))
                } else {
                    Err(Error::new("JSON number cannot be represented on the wire"))
                }
            }
            JsonValue::String(value) => Ok(Self::Binary(value.into_bytes())),
            JsonValue::Array(values) => values
                .into_iter()
                .map(Self::from_json)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::List),
            JsonValue::Object(values) => values
                .into_iter()
                .map(|(key, value)| Ok((Self::binary(key.into_bytes()), Self::from_json(value)?)))
                .collect::<Result<Vec<_>, _>>()
                .map(Self::Map),
        }
    }

    pub(crate) fn into_json(self) -> Result<JsonValue, Error> {
        match self {
            Self::Null => Ok(JsonValue::Null),
            Self::Bool(value) => Ok(JsonValue::Bool(value)),
            Self::Unsigned(value) => Ok(JsonValue::Number(JsonNumber::from(value))),
            Self::Signed(value) => Ok(JsonValue::Number(JsonNumber::from(value))),
            Self::Float(value) => JsonNumber::from_f64(value)
                .map(JsonValue::Number)
                .ok_or_else(|| Error::new("non-finite floating-point wire value")),
            Self::Binary(value) => Ok(binary_json(value)),
            Self::Atom(value) if value == "nil" || value == "null" => Ok(JsonValue::Null),
            Self::Atom(value) if value == "true" => Ok(JsonValue::Bool(true)),
            Self::Atom(value) if value == "false" => Ok(JsonValue::Bool(false)),
            Self::Atom(value) => Ok(JsonValue::String(value)),
            Self::Tuple(values) | Self::List(values) => values
                .into_iter()
                .map(Self::into_json)
                .collect::<Result<Vec<_>, _>>()
                .map(JsonValue::Array),
            Self::Map(entries) => {
                let mut values = JsonMap::with_capacity(entries.len());
                for (key, value) in entries {
                    let key = match key {
                        Self::Binary(bytes) => String::from_utf8(bytes)
                            .map_err(|_| Error::new("wire map key is not UTF-8"))?,
                        Self::Atom(atom) => atom,
                        _other => return Err(Error::new("wire map key is not a string")),
                    };
                    if values.insert(key, value.into_json()?).is_some() {
                        return Err(Error::new("wire map contains a duplicate key"));
                    }
                }
                Ok(JsonValue::Object(values))
            }
        }
    }
}

fn binary_json(value: Vec<u8>) -> JsonValue {
    match String::from_utf8(value) {
        Ok(value) => JsonValue::String(value),
        Err(error) => {
            let mut encoded = JsonMap::new();
            encoded.insert(
                "base64".to_owned(),
                JsonValue::String(BASE64.encode(error.into_bytes())),
            );
            JsonValue::Object(encoded)
        }
    }
}

#[derive(Debug)]
pub(crate) struct Error(String);

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for Error {}

pub(crate) fn write_frame(
    writer: &mut impl Write,
    protocol_version: u8,
    value: &Value,
) -> Result<(), io::Error> {
    let mut payload = Vec::new();
    payload.push(ETF_VERSION);
    encode_value(value, &mut payload);

    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "wire frame exceeds 4 GiB"))?;
    writer.write_all(MAGIC)?;
    writer.write_all(&[protocol_version])?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub(crate) fn read_frame(reader: &mut impl Read, protocol_version: u8) -> Result<Value, Error> {
    let mut header = [0_u8; 9];
    read_exact_frame(reader, &mut header)?;

    if &header[..4] != MAGIC {
        return Err(Error::new("invalid Findex frame magic"));
    }
    if header[4] != protocol_version {
        return Err(Error::new(format!(
            "wire protocol {} is unsupported; expected {protocol_version}",
            header[4]
        )));
    }

    let length = u32::from_be_bytes(header[5..9].try_into().expect("fixed frame header")) as usize;
    if length > MAXIMUM_FRAME_BYTES {
        return Err(Error::new(format!(
            "wire frame is {length} bytes; maximum is {MAXIMUM_FRAME_BYTES}"
        )));
    }

    let mut payload = vec![0_u8; length];
    read_exact_frame(reader, &mut payload)?;
    decode_payload(&payload)
}

fn read_exact_frame(reader: &mut impl Read, buffer: &mut [u8]) -> Result<(), Error> {
    reader.read_exact(buffer).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            Error::new("unexpected end of bridge stream")
        } else {
            Error::new(format!("bridge frame read failed: {error}"))
        }
    })
}

fn encode_value(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Null => encode_atom("nil", output),
        Value::Bool(value) => encode_atom(if *value { "true" } else { "false" }, output),
        Value::Unsigned(value) => encode_unsigned(*value, output),
        Value::Signed(value) if *value >= 0 => encode_unsigned(*value as u64, output),
        Value::Signed(value) if i32::try_from(*value).is_ok() => {
            output.push(INTEGER_EXT);
            output.extend_from_slice(&(*value as i32).to_be_bytes());
        }
        Value::Signed(value) => encode_big(value.unsigned_abs(), true, output),
        Value::Float(value) => {
            output.push(NEW_FLOAT_EXT);
            output.extend_from_slice(&value.to_be_bytes());
        }
        Value::Binary(value) => {
            output.push(BINARY_EXT);
            output.extend_from_slice(&(value.len() as u32).to_be_bytes());
            output.extend_from_slice(value);
        }
        Value::Atom(value) => encode_atom(value, output),
        Value::Tuple(values) => {
            if let Ok(length) = u8::try_from(values.len()) {
                output.push(SMALL_TUPLE_EXT);
                output.push(length);
            } else {
                output.push(LARGE_TUPLE_EXT);
                output.extend_from_slice(&(values.len() as u32).to_be_bytes());
            }
            for value in values {
                encode_value(value, output);
            }
        }
        Value::List(values) => {
            if values.is_empty() {
                output.push(NIL_EXT);
            } else {
                output.push(LIST_EXT);
                output.extend_from_slice(&(values.len() as u32).to_be_bytes());
                for value in values {
                    encode_value(value, output);
                }
                output.push(NIL_EXT);
            }
        }
        Value::Map(entries) => {
            output.push(MAP_EXT);
            output.extend_from_slice(&(entries.len() as u32).to_be_bytes());
            for (key, value) in entries {
                encode_value(key, output);
                encode_value(value, output);
            }
        }
    }
}

fn encode_unsigned(value: u64, output: &mut Vec<u8>) {
    if let Ok(value) = u8::try_from(value) {
        output.push(SMALL_INTEGER_EXT);
        output.push(value);
    } else if let Ok(value) = i32::try_from(value) {
        output.push(INTEGER_EXT);
        output.extend_from_slice(&value.to_be_bytes());
    } else {
        encode_big(value, false, output);
    }
}

fn encode_big(mut magnitude: u64, negative: bool, output: &mut Vec<u8>) {
    let mut bytes = Vec::with_capacity(8);
    while magnitude != 0 {
        bytes.push(magnitude as u8);
        magnitude >>= 8;
    }
    if bytes.is_empty() {
        bytes.push(0);
    }
    output.push(SMALL_BIG_EXT);
    output.push(bytes.len() as u8);
    output.push(u8::from(negative));
    output.extend_from_slice(&bytes);
}

fn encode_atom(atom: &str, output: &mut Vec<u8>) {
    let bytes = atom.as_bytes();
    if let Ok(length) = u8::try_from(bytes.len()) {
        output.push(SMALL_ATOM_UTF8_EXT);
        output.push(length);
    } else {
        output.push(ATOM_UTF8_EXT);
        output.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    }
    output.extend_from_slice(bytes);
}

fn decode_payload(payload: &[u8]) -> Result<Value, Error> {
    let mut decoder = Decoder {
        input: payload,
        position: 0,
    };
    if decoder.byte()? != ETF_VERSION {
        return Err(Error::new("wire payload is not an external term"));
    }
    let value = decoder.value(0)?;
    if decoder.position != payload.len() {
        return Err(Error::new("wire payload has trailing bytes"));
    }
    Ok(value)
}

struct Decoder<'a> {
    input: &'a [u8],
    position: usize,
}

impl Decoder<'_> {
    fn value(&mut self, depth: usize) -> Result<Value, Error> {
        if depth > MAXIMUM_NESTING {
            return Err(Error::new("wire value nesting is too deep"));
        }

        match self.byte()? {
            SMALL_INTEGER_EXT => Ok(Value::Unsigned(u64::from(self.byte()?))),
            INTEGER_EXT => Ok(Value::Signed(i64::from(self.i32()?))),
            NEW_FLOAT_EXT => Ok(Value::Float(f64::from_be_bytes(self.array()?))),
            ATOM_EXT | ATOM_UTF8_EXT => {
                let length = usize::from(self.u16()?);
                self.atom(length)
            }
            SMALL_ATOM_EXT | SMALL_ATOM_UTF8_EXT => {
                let length = usize::from(self.byte()?);
                self.atom(length)
            }
            SMALL_TUPLE_EXT => {
                let length = usize::from(self.byte()?);
                self.sequence(length, depth, Value::Tuple)
            }
            LARGE_TUPLE_EXT => {
                let length = self.length()?;
                self.sequence(length, depth, Value::Tuple)
            }
            NIL_EXT => Ok(Value::List(Vec::new())),
            STRING_EXT => {
                let length = usize::from(self.u16()?);
                let bytes = self.take(length)?;
                Ok(Value::List(
                    bytes
                        .iter()
                        .map(|value| Value::Unsigned(u64::from(*value)))
                        .collect(),
                ))
            }
            LIST_EXT => {
                let length = self.length()?;
                let values = self.values(length, depth)?;
                if self.byte()? != NIL_EXT {
                    return Err(Error::new("improper wire lists are unsupported"));
                }
                Ok(Value::List(values))
            }
            BINARY_EXT => {
                let length = self.length()?;
                Ok(Value::Binary(self.take(length)?.to_vec()))
            }
            SMALL_BIG_EXT => {
                let length = usize::from(self.byte()?);
                self.big_integer(length)
            }
            LARGE_BIG_EXT => {
                let length = self.length()?;
                self.big_integer(length)
            }
            MAP_EXT => {
                let length = self.length()?;
                self.ensure_collection(length, 2)?;
                let mut entries = Vec::with_capacity(length);
                for _ in 0..length {
                    entries.push((self.value(depth + 1)?, self.value(depth + 1)?));
                }
                Ok(Value::Map(entries))
            }
            tag => Err(Error::new(format!("unsupported external-term tag {tag}"))),
        }
    }

    fn sequence(
        &mut self,
        length: usize,
        depth: usize,
        constructor: fn(Vec<Value>) -> Value,
    ) -> Result<Value, Error> {
        self.values(length, depth).map(constructor)
    }

    fn values(&mut self, length: usize, depth: usize) -> Result<Vec<Value>, Error> {
        self.ensure_collection(length, 1)?;
        let mut values = Vec::with_capacity(length);
        for _ in 0..length {
            values.push(self.value(depth + 1)?);
        }
        Ok(values)
    }

    fn ensure_collection(&self, length: usize, minimum_bytes: usize) -> Result<(), Error> {
        if length > self.input.len().saturating_sub(self.position) / minimum_bytes {
            Err(Error::new("wire collection length exceeds its payload"))
        } else {
            Ok(())
        }
    }

    fn atom(&mut self, length: usize) -> Result<Value, Error> {
        let atom = std::str::from_utf8(self.take(length)?)
            .map_err(|_| Error::new("wire atom is not UTF-8"))?
            .to_owned();
        match atom.as_str() {
            "nil" | "null" => Ok(Value::Null),
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _other => Ok(Value::Atom(atom)),
        }
    }

    fn big_integer(&mut self, length: usize) -> Result<Value, Error> {
        if length > 8 {
            return Err(Error::new("wire integer exceeds 64 bits"));
        }
        let negative = match self.byte()? {
            0 => false,
            1 => true,
            _other => return Err(Error::new("wire integer has an invalid sign")),
        };
        let mut magnitude = 0_u64;
        for (shift, byte) in self.take(length)?.iter().enumerate() {
            magnitude |= u64::from(*byte) << (shift * 8);
        }
        if negative {
            if magnitude == (1_u64 << 63) {
                Ok(Value::Signed(i64::MIN))
            } else {
                let magnitude = i64::try_from(magnitude)
                    .map_err(|_| Error::new("negative wire integer exceeds i64"))?;
                Ok(Value::Signed(-magnitude))
            }
        } else {
            Ok(Value::Unsigned(magnitude))
        }
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let byte = *self
            .input
            .get(self.position)
            .ok_or_else(|| Error::new("truncated wire payload"))?;
        self.position += 1;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, Error> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    fn length(&mut self) -> Result<usize, Error> {
        Ok(u32::from_be_bytes(self.array()?) as usize)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.take(N)?
            .try_into()
            .map_err(|_| Error::new("truncated wire payload"))
    }

    fn take(&mut self, length: usize) -> Result<&[u8], Error> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| Error::new("wire length overflow"))?;
        let value = self
            .input
            .get(self.position..end)
            .ok_or_else(|| Error::new("truncated wire payload"))?;
        self.position = end;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etf_round_trip_preserves_arbitrary_binary_bytes() {
        let value = Value::map([
            ("id", Value::Unsigned(u64::MAX)),
            ("payload", Value::Binary(vec![0, 1, 0xff])),
            ("items", Value::List(vec![Value::Signed(-1), Value::Null])),
        ]);
        let mut frame = Vec::new();
        write_frame(&mut frame, 4, &value).expect("encode frame");
        assert_eq!(
            read_frame(&mut frame.as_slice(), 4).expect("decode frame"),
            value
        );
    }

    #[test]
    fn malformed_lengths_are_rejected_before_allocation() {
        let payload = [ETF_VERSION, MAP_EXT, 0xff, 0xff, 0xff, 0xff];
        assert!(decode_payload(&payload).is_err());
    }

    #[test]
    fn non_utf8_binaries_retain_the_public_encoded_binary_shape() {
        let json = Value::Binary(vec![0xff])
            .into_json()
            .expect("convert binary");
        assert_eq!(json["base64"], "/w==");
    }
}
