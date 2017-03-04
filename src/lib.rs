//! A library for encoding/decoding Unix archive files.
//!
//! This library provides utilities necessary to manage [Unix archive
//! files](https://en.wikipedia.org/wiki/Ar_(Unix)) (as generated by the
//! standard `ar` command line utility) abstracted over a reader or writer.
//! This library provides a streaming interface that avoids having to ever load
//! a full archive entry into memory.
//!
//! The API of this crate is meant to be similar to that of the
//! [`tar`](https://crates.io/crates/tar) crate.

#![warn(missing_docs)]

use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io::{self, Error, ErrorKind, Read, Result, Write};
use std::path::Path;
use std::str;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

// ========================================================================= //

const GLOBAL_HEADER_LEN: usize = 8;
const GLOBAL_HEADER: &'static str = "!<arch>\n";

// ========================================================================= //

/// Representation of an archive entry header.
pub struct Header {
    identifier: String,
    mtime: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    size: u64,
}

impl Header {
    /// Creates a header with the given file identifier and size, and all
    /// other fields set to zero.
    pub fn new(identifier: String, size: u64) -> Header {
        Header {
            identifier: identifier,
            mtime: 0,
            uid: 0,
            gid: 0,
            mode: 0,
            size: size,
        }
    }

    /// Creates a header with the given file identifier and all other fields
    /// set from the given filesystem metadata.
    #[cfg(unix)]
    pub fn from_metadata(identifier: String, meta: &Metadata) -> Header {
        Header {
            identifier: identifier,
            mtime: meta.mtime() as u64,
            uid: meta.uid(),
            gid: meta.gid(),
            mode: meta.mode(),
            size: meta.len(),
        }
    }

    #[cfg(not(unix))]
    pub fn from_metadata(identifier: String, meta: &Metadata) -> Header {
        Header::new(identifier, meta.len())
    }

    /// Returns the file identifier.
    pub fn identifier(&self) -> &str { &self.identifier }

    /// Returns the last modification time in Unix time format.
    pub fn mtime(&self) -> u64 { self.mtime }

    /// Returns the value of the owner's user ID field.
    pub fn uid(&self) -> u32 { self.uid }

    /// Returns the value of the groups's user ID field.
    pub fn gid(&self) -> u32 { self.gid }

    /// Returns the mode bits for this file.
    pub fn mode(&self) -> u32 { self.mode }

    /// Returns the length of the file, in bytes.
    pub fn size(&self) -> u64 { self.size }

    /// Parses the next header.  Returns `Ok(None)` if we are at EOF.
    fn read<R: Read>(reader: &mut R) -> Result<Option<Header>> {
        let mut buffer = [0; 60];
        let bytes_read = try!(reader.read(&mut buffer));
        if bytes_read == 0 {
            return Ok(None);
        } else if bytes_read < buffer.len() {
            let msg = "Unexpected EOF in the middle of archive entry header";
            return Err(Error::new(ErrorKind::UnexpectedEof, msg));
        }
        let mut identifier = match str::from_utf8(&buffer[0..16]) {
            Ok(string) => string.trim_right().to_string(),
            Err(_) => {
                let msg = "Non-UTF8 bytes in entry identifier";
                return Err(Error::new(ErrorKind::InvalidData, msg));
            }
        };
        let mtime = try!(parse_number(&buffer[16..28], 10));
        let uid = try!(parse_number(&buffer[28..34], 10)) as u32;
        let gid = try!(parse_number(&buffer[34..40], 10)) as u32;
        let mode = try!(parse_number(&buffer[40..48], 8)) as u32;
        let mut size = try!(parse_number(&buffer[48..58], 10));
        if identifier.starts_with("#1/") {
            let padded_length = try!(parse_number(&buffer[3..16], 10));
            if size < padded_length {
                let msg = format!("Entry size ({}) smaller than extended \
                                   entry identifier length ({})",
                                  size,
                                  padded_length);
                return Err(Error::new(ErrorKind::InvalidData, msg));
            }
            size -= padded_length;
            let mut id_buffer = vec![0; padded_length as usize];
            let bytes_read = try!(reader.read(&mut id_buffer));
            if bytes_read < id_buffer.len() {
                let msg = "Unexpected EOF in the middle of extended entry \
                           identifier";
                return Err(Error::new(ErrorKind::UnexpectedEof, msg));
            }
            while id_buffer.last() == Some(&0) {
                id_buffer.pop();
            }
            identifier = match str::from_utf8(&id_buffer) {
                Ok(string) => string.to_string(),
                Err(_) => {
                    let msg = "Non-UTF8 bytes in extended entry identifier";
                    return Err(Error::new(ErrorKind::InvalidData, msg));
                }
            };
        }
        Ok(Some(Header {
            identifier: identifier,
            mtime: mtime,
            uid: uid,
            gid: gid,
            mode: mode,
            size: size,
        }))
    }

    fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        if self.identifier.len() > 16 || self.identifier.contains(' ') {
            let padding_length = (4 - self.identifier.len() % 4) % 4;
            let padded_length = self.identifier.len() + padding_length;
            try!(write!(writer,
                        "#1/{:<13}{:<12}{:<6}{:<6}{:<8o}{:<10}`\n{}",
                        padded_length,
                        self.mtime,
                        self.uid,
                        self.gid,
                        self.mode,
                        self.size + padded_length as u64,
                        self.identifier));
            writer.write_all(&vec![0; padding_length])
        } else {
            write!(writer,
                   "{:<16}{:<12}{:<6}{:<6}{:<8o}{:<10}`\n",
                   self.identifier,
                   self.mtime,
                   self.uid,
                   self.gid,
                   self.mode,
                   self.size)
        }
    }
}

fn parse_number(bytes: &[u8], radix: u32) -> Result<u64> {
    if let Ok(string) = str::from_utf8(bytes) {
        if let Ok(value) = u64::from_str_radix(string.trim_right(), radix) {
            return Ok(value);
        }
    }
    let msg = "Invalid numeric field in entry header";
    Err(Error::new(ErrorKind::InvalidData, msg))
}

// ========================================================================= //

/// A structure for reading archives.
pub struct Archive<R: Read> {
    reader: R,
    started: bool,
    padding: bool,
    finished: bool,
}

impl<R: Read> Archive<R> {
    /// Create a new archive reader with the underlying reader object as the
    /// source of all data read.
    pub fn new(reader: R) -> Archive<R> {
        Archive {
            reader: reader,
            started: false,
            padding: false,
            finished: false,
        }
    }

    /// Unwrap this archive reader, returning the underlying reader object.
    pub fn into_inner(self) -> Result<R> { Ok(self.reader) }

    /// Reads the next entry from the archive, or returns None if there are no
    /// more.
    pub fn next_entry(&mut self) -> Option<Result<Entry<R>>> {
        if self.finished {
            return None;
        }
        if !self.started {
            let mut buffer = [0; GLOBAL_HEADER_LEN];
            match self.reader.read_exact(&mut buffer) {
                Ok(()) => {}
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            }
            if &buffer != GLOBAL_HEADER.as_bytes() {
                self.finished = true;
                let msg = "Not an archive file (invalid global header)";
                return Some(Err(Error::new(ErrorKind::InvalidData, msg)));
            }
            self.started = true;
        }
        if self.padding {
            let mut buffer = [0; 1];
            match self.reader.read_exact(&mut buffer) {
                Ok(()) => {}
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            }
            if &buffer != "\n".as_bytes() {
                self.finished = true;
                let msg = "Invalid padding byte";
                return Some(Err(Error::new(ErrorKind::InvalidData, msg)));
            }
            self.padding = false;
        }
        match Header::read(&mut self.reader) {
            Ok(Some(header)) => {
                let size = header.size();
                if size % 2 != 0 {
                    self.padding = true;
                }
                Some(Ok(Entry {
                    header: header,
                    reader: self.reader.by_ref().take(size),
                }))
            }
            Ok(None) => {
                self.finished = true;
                None
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

// ========================================================================= //

/// Representation of an archive entry.
///
/// Entry objects implement the `Read` trait, and can be used to extract the
/// data from this archive entry.
pub struct Entry<'a, R: 'a + Read> {
    header: Header,
    reader: io::Take<&'a mut R>,
}

impl<'a, R: 'a + Read> Entry<'a, R> {
    /// Returns the header for this archive entry.
    pub fn header(&self) -> &Header { &self.header }
}

impl<'a, R: 'a + Read> Read for Entry<'a, R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.reader.read(buf)
    }
}

impl<'a, R: 'a + Read> Drop for Entry<'a, R> {
    fn drop(&mut self) {
        if self.reader.limit() > 0 {
            // Consume the rest of the data in this entry.
            let _ = io::copy(&mut self.reader, &mut io::sink());
        }
    }
}

// ========================================================================= //

/// A structure for building archives.
///
/// This structure has methods for building up an archive from scratch into any
/// arbitrary writer.
pub struct Builder<W: Write> {
    writer: W,
    started: bool,
}

impl<W: Write> Builder<W> {
    /// Create a new archive builder with the underlying writer object as the
    /// destination of all data written.
    pub fn new(writer: W) -> Builder<W> {
        Builder {
            writer: writer,
            started: false,
        }
    }

    /// Unwrap this archive builder, returning the underlying writer object.
    pub fn into_inner(self) -> Result<W> { Ok(self.writer) }

    /// Adds a new entry to this archive.
    pub fn append<R: Read>(&mut self, header: &Header, mut data: R)
                           -> Result<()> {
        if !self.started {
            try!(self.writer.write_all(GLOBAL_HEADER.as_bytes()));
            self.started = true;
        }
        try!(header.write(&mut self.writer));
        let actual_size = try!(io::copy(&mut data, &mut self.writer));
        if actual_size != header.size() {
            let msg = format!("Wrong file size (header.size() = {}, actual \
                               size was {})",
                              header.size(),
                              actual_size);
            return Err(Error::new(ErrorKind::InvalidData, msg));
        }
        if actual_size % 2 != 0 {
            try!(self.writer.write_all(&['\n' as u8]));
        }
        Ok(())
    }

    /// Adds a file on the local filesystem to this archive, using the file
    /// name as its identifier.
    pub fn append_path<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let name: &OsStr = try!(path.as_ref().file_name().ok_or_else(|| {
            let msg = "Given path doesn't have a file name";
            Error::new(ErrorKind::InvalidInput, msg)
        }));
        let name: &str = try!(name.to_str().ok_or_else(|| {
            let msg = "Given path has a non-UTF8 file name";
            Error::new(ErrorKind::InvalidData, msg)
        }));
        self.append_file(name, &mut try!(File::open(&path)))
    }

    /// Adds a file to this archive, with the given name as its identifier.
    pub fn append_file(&mut self, name: &str, file: &mut File) -> Result<()> {
        let metadata = try!(file.metadata());
        let header = Header::from_metadata(name.to_string(), &metadata);
        self.append(&header, file)
    }
}

// ========================================================================= //

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::str;
    use super::{Archive, Builder, Header};

    #[test]
    fn build_archive_with_two_files() {
        let mut builder = Builder::new(Vec::new());
        let header1 = Header {
            identifier: "foo.txt".to_string(),
            mtime: 1487552916,
            uid: 501,
            gid: 20,
            mode: 0o100644,
            size: 7,
        };
        builder.append(&header1, "foobar\n".as_bytes()).unwrap();
        let header2 = Header::new("baz.txt".to_string(), 4);
        builder.append(&header2, "baz\n".as_bytes()).unwrap();
        let actual = builder.into_inner().unwrap();
        let expected = "\
        !<arch>\n\
        foo.txt         1487552916  501   20    100644  7         `\n\
        foobar\n\n\
        baz.txt         0           0     0     0       4         `\n\
        baz\n";
        assert_eq!(str::from_utf8(&actual).unwrap(), expected);
    }

    #[test]
    fn build_archive_with_long_filenames() {
        let mut builder = Builder::new(Vec::new());
        let header1 = Header {
            identifier: "this_is_a_very_long_filename.txt".to_string(),
            mtime: 1487552916,
            uid: 501,
            gid: 20,
            mode: 0o100644,
            size: 7,
        };
        builder.append(&header1, "foobar\n".as_bytes()).unwrap();
        let header2 = Header::new("and_this_is_another_very_long_filename.txt"
                                      .to_string(),
                                  4);
        builder.append(&header2, "baz\n".as_bytes()).unwrap();
        let actual = builder.into_inner().unwrap();
        let expected = "\
        !<arch>\n\
        #1/32           1487552916  501   20    100644  39        `\n\
        this_is_a_very_long_filename.txtfoobar\n\n\
        #1/44           0           0     0     0       48        `\n\
        and_this_is_another_very_long_filename.txt\x00\x00baz\n";
        assert_eq!(str::from_utf8(&actual).unwrap(), expected);
    }

    #[test]
    fn build_archive_with_space_in_filename() {
        let mut builder = Builder::new(Vec::new());
        let header = Header::new("foo bar".to_string(), 4);
        builder.append(&header, "baz\n".as_bytes()).unwrap();
        let actual = builder.into_inner().unwrap();
        let expected = "\
        !<arch>\n\
        #1/8            0           0     0     0       12        `\n\
        foo bar\x00baz\n";
        assert_eq!(str::from_utf8(&actual).unwrap(), expected);
    }

    #[test]
    fn read_archive_with_three_files() {
        let input = "\
        !<arch>\n\
        foo.txt         1487552916  501   20    100644  7         `\n\
        foobar\n\n\
        bar.awesome.txt 1487552919  501   20    100644  22        `\n\
        This file is awesome!\n\
        baz.txt         1487552349  42    12345 100664  4         `\n\
        baz\n";
        let mut archive = Archive::new(input.as_bytes());
        {
            // Parse the first entry and check the header values.
            let mut entry = archive.next_entry().unwrap().unwrap();
            assert_eq!(entry.header().identifier(), "foo.txt");
            assert_eq!(entry.header().mtime(), 1487552916);
            assert_eq!(entry.header().uid(), 501);
            assert_eq!(entry.header().gid(), 20);
            assert_eq!(entry.header().mode(), 0o100644);
            assert_eq!(entry.header().size(), 7);
            // Read the first few bytes of the entry data and make sure they're
            // correct.
            let mut buffer = [0; 4];
            entry.read_exact(&mut buffer).unwrap();
            assert_eq!(&buffer, "foob".as_bytes());
            // Dropping the Entry object should automatically consume the rest
            // of the entry data so that the archive reader is ready to parse
            // the next entry.
        }
        {
            // Parse the second entry and check a couple header values.
            let mut entry = archive.next_entry().unwrap().unwrap();
            assert_eq!(entry.header().identifier(), "bar.awesome.txt");
            assert_eq!(entry.header().size(), 22);
            // Read in all the entry data.
            let mut buffer = Vec::new();
            entry.read_to_end(&mut buffer).unwrap();
            assert_eq!(&buffer as &[u8], "This file is awesome!\n".as_bytes());
        }
        {
            // Parse the third entry and check a couple header values.
            let entry = archive.next_entry().unwrap().unwrap();
            assert_eq!(entry.header().identifier(), "baz.txt");
            assert_eq!(entry.header().size(), 4);
        }
    }

    #[test]
    fn read_archive_with_long_filenames() {
        let input = "\
        !<arch>\n\
        #1/32           1487552916  501   20    100644  39        `\n\
        this_is_a_very_long_filename.txtfoobar\n\n\
        #1/44           0           0     0     0       48        `\n\
        and_this_is_another_very_long_filename.txt\x00\x00baz\n";
        let mut archive = Archive::new(input.as_bytes());
        {
            // Parse the first entry and check the header values.
            let mut entry = archive.next_entry().unwrap().unwrap();
            assert_eq!(entry.header().identifier(),
                       "this_is_a_very_long_filename.txt");
            assert_eq!(entry.header().mtime(), 1487552916);
            assert_eq!(entry.header().uid(), 501);
            assert_eq!(entry.header().gid(), 20);
            assert_eq!(entry.header().mode(), 0o100644);
            // We should get the size of the actual file, not including the
            // filename, even though this is not the value that's in the size
            // field in the input.
            assert_eq!(entry.header().size(), 7);
            // Read in the entry data; we should get only the payload and not
            // the filename.
            let mut buffer = Vec::new();
            entry.read_to_end(&mut buffer).unwrap();
            assert_eq!(&buffer as &[u8], "foobar\n".as_bytes());
        }
        {
            // Parse the second entry and check a couple header values.
            let mut entry = archive.next_entry().unwrap().unwrap();
            assert_eq!(entry.header().identifier(),
                       "and_this_is_another_very_long_filename.txt");
            assert_eq!(entry.header().size(), 4);
            // Read in the entry data; we should get only the payload and not
            // the filename or the padding bytes.
            let mut buffer = Vec::new();
            entry.read_to_end(&mut buffer).unwrap();
            assert_eq!(&buffer as &[u8], "baz\n".as_bytes());
        }
    }

    #[test]
    fn read_archive_with_space_in_filename() {
        let input = "\
        !<arch>\n\
        #1/8            0           0     0     0       12        `\n\
        foo bar\x00baz\n";
        let mut archive = Archive::new(input.as_bytes());
        let mut entry = archive.next_entry().unwrap().unwrap();
        assert_eq!(entry.header().identifier(), "foo bar");
        assert_eq!(entry.header().size(), 4);
        let mut buffer = Vec::new();
        entry.read_to_end(&mut buffer).unwrap();
        assert_eq!(&buffer as &[u8], "baz\n".as_bytes());
    }
}

// ========================================================================= //
