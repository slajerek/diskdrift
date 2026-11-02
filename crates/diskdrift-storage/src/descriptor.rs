//! Validated descriptors for immutable data streams.

use diskdrift_core::native::PathEncoding;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileKind {
    Ops = 1,
    Records = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamDescriptor {
    pub kind: FileKind,
    pub path_encoding: PathEncoding,
    pub record_schema: u8,
    pub flags: u8,
}

impl StreamDescriptor {
    pub const fn dds1(kind: FileKind) -> Self {
        Self {
            kind,
            path_encoding: PathEncoding::UnixBytes,
            record_schema: 0,
            flags: 0,
        }
    }

    pub fn dds2(kind: FileKind, path_encoding: PathEncoding) -> io::Result<Self> {
        let descriptor = Self {
            kind,
            path_encoding,
            record_schema: 1,
            flags: 0,
        };
        descriptor.validate_dds2()?;
        Ok(descriptor)
    }

    pub fn validate_dds2(self) -> io::Result<()> {
        if self.record_schema != 1 {
            return Err(corrupt(&format!(
                "unsupported record schema {}",
                self.record_schema
            )));
        }
        if self.flags != 0 {
            return Err(corrupt(&format!("unknown stream flags {}", self.flags)));
        }
        Ok(())
    }

    pub fn validate(self) -> io::Result<()> {
        match self.record_schema {
            0 if self.path_encoding == PathEncoding::UnixBytes && self.flags == 0 => Ok(()),
            1 => self.validate_dds2(),
            schema => Err(corrupt(&format!("unsupported record schema {schema}"))),
        }
    }

    /// Verifies that a stream can represent every native path in its metadata
    /// volume table. A DDS2 stream has one encoding for all record paths.
    pub fn validate_metadata(self, metadata: &crate::meta::MetadataV3) -> io::Result<()> {
        metadata
            .validate()
            .map_err(|error| corrupt(&error.to_string()))?;
        self.validate_volume_table(&metadata.volumes)
    }

    /// Verifies that every entry in the volume table uses the stream's single
    /// native path encoding without taking ownership of the table.
    pub fn validate_volume_table(
        self,
        volumes: &[crate::meta::VolumeDescriptor],
    ) -> io::Result<()> {
        self.validate()?;
        if volumes
            .iter()
            .any(|volume| volume.mount_key.encoding() != self.path_encoding)
        {
            return Err(corrupt(
                "stream descriptor path encoding does not match metadata volume table",
            ));
        }
        Ok(())
    }

    pub fn is_dds2(self) -> bool {
        self.record_schema == 1
    }

    /// DDS1 and DDS2 records can share a lineage only when their native path
    /// encoding, schema, and flags describe the same canonical record stream.
    /// File kind is deliberately ignored: a delta is compared with its base.
    pub fn lineage_compatible(self, other: Self) -> bool {
        self.validate().is_ok()
            && other.validate().is_ok()
            && self.path_encoding == other.path_encoding
            && self.flags == other.flags
            && matches!((self.record_schema, other.record_schema), (0..=1, 0..=1))
    }

    pub(crate) fn header(self) -> [u8; 8] {
        let encoding = match self.path_encoding {
            PathEncoding::UnixBytes => 1,
            PathEncoding::WindowsUtf16le => 2,
        };
        [
            b'D',
            b'D',
            b'S',
            b'2',
            self.kind as u8,
            encoding,
            self.record_schema,
            self.flags,
        ]
    }

    pub(crate) fn from_header(header: &[u8; 8]) -> io::Result<Self> {
        let kind = match header[4] {
            1 => FileKind::Ops,
            2 => FileKind::Records,
            value => return Err(corrupt(&format!("unsupported data kind {value}"))),
        };
        let path_encoding = match header[5] {
            1 => PathEncoding::UnixBytes,
            2 => PathEncoding::WindowsUtf16le,
            value => return Err(corrupt(&format!("unknown path encoding {value}"))),
        };
        let descriptor = Self {
            kind,
            path_encoding,
            record_schema: header[6],
            flags: header[7],
        };
        descriptor.validate_dds2()?;
        Ok(descriptor)
    }
}

fn corrupt(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}
