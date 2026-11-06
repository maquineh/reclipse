//! Minimal JDWP wire protocol (handshake, packets, readers/writers).
//!
//! Command numbers follow the JDWP specification and the constants in the
//! JDK's `com.sun.tools.jdi.JDWP` implementation.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub(crate) const HANDSHAKE: &[u8; 14] = b"JDWP-Handshake";
const MAX_PACKET: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct IdSizes {
    pub field: usize,
    pub method: usize,
    pub object: usize,
    pub reference_type: usize,
    pub frame: usize,
}

impl Default for IdSizes {
    fn default() -> Self {
        Self {
            field: 8,
            method: 8,
            object: 8,
            reference_type: 8,
            frame: 8,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Location {
    pub type_id: u64,
    pub method_id: u64,
    pub index: u64,
}

pub(crate) enum Incoming {
    Reply { id: u32, error: u16, data: Vec<u8> },
    Command { set: u8, command: u8, data: Vec<u8> },
}

pub(crate) struct JdwpClient {
    stream: TcpStream,
    buffer: Vec<u8>,
    next_id: u32,
    pub sizes: IdSizes,
    queued: VecDeque<Incoming>,
}

impl JdwpClient {
    pub(crate) fn connect(mut stream: TcpStream) -> Result<Self, String> {
        stream
            .set_nodelay(true)
            .map_err(|error| format!("falha ao configurar socket JDWP: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|error| format!("falha ao configurar timeout JDWP: {error}"))?;
        stream
            .write_all(HANDSHAKE)
            .map_err(|error| format!("falha no handshake JDWP: {error}"))?;
        stream
            .flush()
            .map_err(|error| format!("falha no handshake JDWP: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut handshake = [0_u8; 14];
        let mut filled = 0;
        while filled < handshake.len() {
            match stream.read(&mut handshake[filled..]) {
                Ok(0) => return Err("JVM encerrou a conexão durante o handshake".to_owned()),
                Ok(read) => filled += read,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if Instant::now() >= deadline {
                        return Err("timeout no handshake JDWP".to_owned());
                    }
                }
                Err(error) => return Err(format!("falha no handshake JDWP: {error}")),
            }
        }
        if &handshake != HANDSHAKE {
            return Err("resposta de handshake JDWP inválida".to_owned());
        }
        let mut client = Self {
            stream,
            buffer: Vec::new(),
            next_id: 1,
            sizes: IdSizes::default(),
            queued: VecDeque::new(),
        };
        let data = client.request(1, 7, &[])?;
        let mut reader = Reader::new(&data, client.sizes);
        let sizes = IdSizes {
            field: reader.u32()? as usize,
            method: reader.u32()? as usize,
            object: reader.u32()? as usize,
            reference_type: reader.u32()? as usize,
            frame: reader.u32()? as usize,
        };
        if sizes.object == 0 || sizes.object > 16 || sizes.frame == 0 || sizes.frame > 16 {
            return Err("tamanhos de ID JDWP inválidos".to_owned());
        }
        client.sizes = sizes;
        Ok(client)
    }

    pub(crate) fn request(&mut self, set: u8, command: u8, data: &[u8]) -> Result<Vec<u8>, String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let length = 11 + data.len();
        let mut packet = Vec::with_capacity(length);
        packet.extend_from_slice(&(length as u32).to_be_bytes());
        packet.extend_from_slice(&id.to_be_bytes());
        packet.push(0);
        packet.push(set);
        packet.push(command);
        packet.extend_from_slice(data);
        self.stream
            .write_all(&packet)
            .map_err(|error| format!("falha ao enviar comando JDWP: {error}"))?;
        self.stream
            .flush()
            .map_err(|error| format!("falha ao enviar comando JDWP: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.next_packet()? {
                Some(Incoming::Reply {
                    id: reply_id,
                    error,
                    data,
                }) if reply_id == id => {
                    if error != 0 {
                        return Err(jdwp_error(error));
                    }
                    return Ok(data);
                }
                Some(Incoming::Reply { .. }) => {}
                Some(event @ Incoming::Command { .. }) => self.queued.push_back(event),
                None => {
                    if Instant::now() >= deadline {
                        return Err("timeout aguardando resposta JDWP".to_owned());
                    }
                }
            }
        }
    }

    pub(crate) fn poll_event(&mut self) -> Result<Option<Incoming>, String> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(Some(event));
        }
        loop {
            match self.next_packet()? {
                Some(event @ Incoming::Command { .. }) => return Ok(Some(event)),
                Some(Incoming::Reply { .. }) => {}
                None => return Ok(None),
            }
        }
    }

    fn next_packet(&mut self) -> Result<Option<Incoming>, String> {
        loop {
            if self.buffer.len() >= 4 {
                let length = u32::from_be_bytes([
                    self.buffer[0],
                    self.buffer[1],
                    self.buffer[2],
                    self.buffer[3],
                ]) as usize;
                if !(11..=MAX_PACKET).contains(&length) {
                    return Err(format!("pacote JDWP inválido (tamanho {length})"));
                }
                if self.buffer.len() >= length {
                    let packet: Vec<u8> = self.buffer.drain(..length).collect();
                    return Ok(Some(parse_packet(&packet)?));
                }
            }
            let mut chunk = [0_u8; 16 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("JVM encerrou a conexão JDWP".to_owned()),
                Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(format!("falha ao ler JDWP: {error}")),
            }
        }
    }
}

fn parse_packet(packet: &[u8]) -> Result<Incoming, String> {
    let id = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
    let flags = packet[8];
    if flags & 0x80 != 0 {
        let error = u16::from_be_bytes([packet[9], packet[10]]);
        Ok(Incoming::Reply {
            id,
            error,
            data: packet[11..].to_vec(),
        })
    } else {
        Ok(Incoming::Command {
            set: packet[9],
            command: packet[10],
            data: packet[11..].to_vec(),
        })
    }
}

fn jdwp_error(code: u16) -> String {
    let name = match code {
        10 => "INVALID_THREAD",
        11 => "INVALID_THREAD_GROUP",
        12 => "INVALID_PRIORITY",
        13 => "THREAD_NOT_SUSPENDED",
        14 => "THREAD_SUSPENDED",
        15 => "THREAD_NOT_ALIVE",
        20 => "INVALID_OBJECT",
        21 => "INVALID_CLASS",
        22 => "CLASS_NOT_PREPARED",
        23 => "INVALID_METHODID",
        24 => "INVALID_LOCATION",
        25 => "INVALID_FIELDID",
        30 => "INVALID_FRAMEID",
        31 => "NO_MORE_FRAMES",
        32 => "OPAQUE_FRAME",
        33 => "NOT_CURRENT_FRAME",
        34 => "INVALID_SLOT",
        35 => "DUPLICATE",
        40 => "NOT_FOUND",
        41 => "INVALID_MONITOR",
        42 => "NOT_MONITOR_OWNER",
        43 => "INTERRUPT",
        44 => "INVALID_CLASS_FORMAT",
        45 => "CIRCULAR_CLASS_DEFINITION",
        46 => "FAILS_VERIFICATION",
        47 => "ADD_METHOD_NOT_IMPLEMENTED",
        48 => "SCHEMA_CHANGE_NOT_IMPLEMENTED",
        49 => "INVALID_TYPESTATE",
        50 => "HIERARCHY_CHANGE_NOT_IMPLEMENTED",
        51 => "DELETE_METHOD_NOT_IMPLEMENTED",
        52 => "UNSUPPORTED_VERSION",
        53 => "NAMES_DONT_MATCH",
        60 => "CLASS_MODIFIERS_CHANGE_NOT_IMPLEMENTED",
        61 => "METHOD_MODIFIERS_CHANGE_NOT_IMPLEMENTED",
        70 => "NOT_IMPLEMENTED",
        71 => "NULL_POINTER",
        72 => "ABSENT_INFORMATION",
        73 => "INVALID_EVENT_TYPE",
        79 => "ILLEGAL_ARGUMENT",
        99 => "OUT_OF_MEMORY",
        100 => "ACCESS_DENIED",
        101 => "VM_DEAD",
        102 => "INTERNAL",
        103 => "UNATTACHED_THREAD",
        104 => "INVALID_TAG",
        105 => "ALREADY_INVOKING",
        106 => "INVALID_INDEX",
        107 => "INVALID_LENGTH",
        108 => "INVALID_STRING",
        109 => "INVALID_CLASS_LOADER",
        110 => "INVALID_ARRAY",
        111 => "TRANSPORT_LOAD",
        112 => "TRANSPORT_INIT",
        113 => "NATIVE_METHOD",
        114 => "INVALID_COUNT",
        500 => "INVALID_EVENT_TYPE",
        _ => "UNKNOWN",
    };
    format!("JDWP error {code} ({name})")
}

pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    sizes: IdSizes,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8], sizes: IdSizes) -> Self {
        Self {
            data,
            pos: 0,
            sizes,
        }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(count)
            .ok_or_else(|| "overflow ao ler JDWP".to_owned())?;
        if end > self.data.len() {
            return Err("pacote JDWP truncado".to_owned());
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, String> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    pub(crate) fn id(&mut self, size: usize) -> Result<u64, String> {
        let bytes = self.take(size)?;
        let mut value = 0_u64;
        for byte in bytes {
            value = (value << 8) | *byte as u64;
        }
        Ok(value)
    }

    pub(crate) fn object(&mut self) -> Result<u64, String> {
        self.id(self.sizes.object)
    }

    pub(crate) fn reference_type(&mut self) -> Result<u64, String> {
        self.id(self.sizes.reference_type)
    }

    pub(crate) fn method(&mut self) -> Result<u64, String> {
        self.id(self.sizes.method)
    }

    pub(crate) fn field(&mut self) -> Result<u64, String> {
        self.id(self.sizes.field)
    }

    pub(crate) fn frame(&mut self) -> Result<u64, String> {
        self.id(self.sizes.frame)
    }

    pub(crate) fn string(&mut self) -> Result<String, String> {
        let length = self.u32()? as usize;
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "string JDWP inválida (UTF-8)".to_owned())
    }

    pub(crate) fn location(&mut self) -> Result<Location, String> {
        let _tag = self.u8()?;
        Ok(Location {
            type_id: self.reference_type()?,
            method_id: self.method()?,
            index: self.u64()?,
        })
    }

    pub(crate) fn value(&mut self) -> Result<Value, String> {
        let tag = self.u8()?;
        Ok(match tag {
            b'B' => Value::Byte(self.u8()? as i8),
            b'Z' => Value::Boolean(self.u8()? != 0),
            b'C' => Value::Char(self.u16()?),
            b'S' => Value::Short(self.u16()? as i16),
            b'I' => Value::Int(self.u32()? as i32),
            b'J' => Value::Long(self.u64()? as i64),
            b'F' => Value::Float(f32::from_bits(self.u32()?)),
            b'D' => Value::Double(f64::from_bits(self.u64()?)),
            b'V' => Value::Void,
            b'L' | b'[' | b's' | b't' | b'g' | b'l' | b'c' => Value::Object {
                tag,
                id: self.object()?,
            },
            other => return Err(format!("tag JDWP desconhecida: {other}")),
        })
    }

    pub(crate) fn tagged_object(&mut self) -> Result<Value, String> {
        self.value()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Value {
    Byte(i8),
    Boolean(bool),
    Char(u16),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Object { tag: u8, id: u64 },
    Void,
}

#[derive(Default)]
pub(crate) struct Writer {
    pub data: Vec<u8>,
}

impl Writer {
    pub(crate) fn u8(&mut self, value: u8) {
        self.data.push(value);
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.data.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.data.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn id(&mut self, value: u64, size: usize) {
        let bytes = value.to_be_bytes();
        self.data.extend_from_slice(&bytes[8 - size..]);
    }

    pub(crate) fn string(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.data.extend_from_slice(value.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_primitive_values() {
        let mut writer = Writer::default();
        writer.string("App");
        writer.u32(42);
        let data = writer.data;
        let mut reader = Reader::new(&data, IdSizes::default());
        assert_eq!(reader.string().unwrap(), "App");
        assert_eq!(reader.u32().unwrap(), 42);
    }

    #[test]
    fn parses_reply_packet() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&15_u32.to_be_bytes());
        packet.extend_from_slice(&7_u32.to_be_bytes());
        packet.push(0x80);
        packet.extend_from_slice(&0_u16.to_be_bytes());
        packet.extend_from_slice(&[1, 2, 3, 4]);
        match parse_packet(&packet).unwrap() {
            Incoming::Reply { id, error, data } => {
                assert_eq!(id, 7);
                assert_eq!(error, 0);
                assert_eq!(data, vec![1, 2, 3, 4]);
            }
            _ => panic!("expected reply"),
        }
    }

    #[test]
    fn parses_location() {
        let mut writer = Writer::default();
        writer.u8(1);
        writer.id(3, 8);
        writer.id(9, 8);
        writer.u64(17);
        let data = writer.data;
        let mut reader = Reader::new(&data, IdSizes::default());
        let location = reader.location().unwrap();
        assert_eq!(location.type_id, 3);
        assert_eq!(location.method_id, 9);
        assert_eq!(location.index, 17);
    }
}
