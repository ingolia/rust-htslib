// Copyright 2014 Christopher Schröder, Johannes Köster.
// Licensed under the MIT license (http://opensource.org/licenses/MIT)
// This file may not be copied, modified, or distributed
// except according to those terms.


pub mod record;
pub mod header;

use std::ffi;
use std::path;
use std::ffi::AsOsStr;
use std::os::unix::prelude::OsStrExt;
use std::str;
use std::ptr;
use libc;

use htslib;


/// A trait for a BAM reader with a read method.
pub trait Read {
    /// Read next BAM record into given record.
    /// Use this method in combination with a single allocated record to avoid the reallocations
    /// occurring with the iterator.
    ///
    /// # Arguments
    ///
    /// * `record` - the record to be filled
    fn read(&self, record: &mut record::Record) -> Result<(), ReadError>;
}


/// A BAM reader.
pub struct Reader {
    f: *mut htslib::Struct_BGZF,
    header: *mut htslib::bam_hdr_t,
}


impl Reader {
    /// Create a new Reader.
    ///
    /// # Arguments
    ///
    /// * `path` - the path. Use "-" for stdin.
    pub fn new<P: path::AsPath>(path: &P) -> Self {
        let f = bgzf_open(path, b"r");
        let header = unsafe { htslib::bam_hdr_read(f) };
        Reader { f : f, header : header }
    }

    /// Iterator over the records of the seeked region.
    /// Note that, while being convenient, this is less efficient than pre-allocating a
    /// `Record` and reading into it with the `read` method, since every iteration involves
    /// the allocation of a new `Record`.
    pub fn records(&self) -> Records<Self> {
        Records { bam: self }
    }
}


impl Read for Reader {
    fn read(&self, record: &mut record::Record) -> Result<(), ReadError> {
        match unsafe { htslib::bam_read1(self.f, &mut record.inner) } {
            -1 => Err(ReadError::NoMoreRecord),
            -2 => Err(ReadError::Truncated),
            -4 => Err(ReadError::Invalid),
            _  => Ok(())
        }
    }
}


impl Drop for Reader {
    fn drop(&mut self) {
        unsafe {
            htslib::bam_hdr_destroy(self.header);
            htslib::bgzf_close(self.f);
        }
    }
}


pub struct IndexedReader {
    bgzf: *mut htslib::Struct_BGZF,
    header: *mut htslib::bam_hdr_t,
    idx: *mut htslib::hts_idx_t,
    itr: Option<*mut htslib:: hts_itr_t>,
}


impl IndexedReader {
    /// Create a new Reader.
    ///
    /// # Arguments
    ///
    /// * `path` - the path. Use "-" for stdin.
    pub fn new<P: path::AsPath>(path: &P) -> Result<Self, IndexError> {
        let bgzf = bgzf_open(path, b"r");
        let header = unsafe { htslib::bam_hdr_read(bgzf) };
        let idx = unsafe {
            htslib::hts_idx_load(
                path.as_path().as_os_str().to_cstring().unwrap().as_ptr(),
                htslib::HTS_FMT_BAI
            )
        };
        if idx.is_null() {
            Err(IndexError::InvalidIndex)
        }
        else {
            Ok(IndexedReader { bgzf: bgzf, header : header, idx: idx, itr: None })
        }
    }

    pub fn seek(&mut self, tid: u32, beg: u32, end: u32) -> Result<(), ()> {
        let itr = unsafe {
            htslib::sam_itr_queryi(self.idx, tid as i32, beg as i32, end as i32)
        };
        if itr.is_null() {
            self.itr = None;
            Err(())
        }
        else {
            self.itr = Some(itr);
            Ok(())
        }
    }

    /// Iterator over the records of the seeked region.
    /// Note that, while being convenient, this is less efficient than pre-allocating a
    /// `Record` and reading into it with the `read` method, since every iteration involves
    /// the allocation of a new `Record`.
    pub fn records(&self) -> Records<Self> {
        Records { bam: self }
    }
}


impl Read for IndexedReader {
    fn read(&self, record: &mut record::Record) -> Result<(), ReadError> {
        match self.itr {
            Some(itr) => match bam_itr_next(self.bgzf, itr, &mut record.inner) {
                -1 => Err(ReadError::NoMoreRecord),
                -2 => Err(ReadError::Truncated),
                -4 => Err(ReadError::Invalid),
                _  => Ok(())
            },
            None      => Err(ReadError::NoMoreRecord)
        }
    }
}


impl Drop for IndexedReader {
    fn drop(&mut self) {
        unsafe {
            if self.itr.is_some() {
                htslib::hts_itr_destroy(self.itr.unwrap());
            }
            htslib::hts_idx_destroy(self.idx);
            htslib::bam_hdr_destroy(self.header);
            htslib::bgzf_close(self.bgzf);
        }
    }
}


/// A BAM writer.
pub struct Writer {
    f: *mut htslib::Struct_BGZF,
    header: *mut htslib::bam_hdr_t,
}


impl Writer {
    /// Create a new BAM file.
    ///
    /// # Arguments
    ///
    /// * `path` - the path. Use "-" for stdin.
    /// * `header` - header definition to use
    pub fn new<P: path::AsPath>(path: &P, header: &header::Header) -> Self {
        let f = bgzf_open(path, b"w");

        let header_record = unsafe {
            let header_string = header.to_bytes();
            println!("{}", str::from_utf8(&header_string).unwrap());
            htslib::sam_hdr_parse(
                header_string.len() as i32,
                ffi::CString::new(header_string).unwrap().as_ptr()
            )
        };
        unsafe { htslib::bam_hdr_write(f, header_record); }

        Writer { f: f, header: header_record }
    }

    /// Create a new BAM file from template.
    ///
    /// # Arguments
    ///
    /// * `path` - the path. Use "-" for stdin.
    /// * `template` - the template BAM. Use "-" for stdin.
    pub fn with_template<P: path::AsPath, T: path::AsPath>(path: &P, template: &T) -> Self {
        let t = bgzf_open(template, b"r");
        let header = unsafe { htslib::bam_hdr_read(t) };
        unsafe { htslib::bgzf_close(t); }

        let f = bgzf_open(path, b"w");
        unsafe { htslib::bam_hdr_write(f, header); }

        Writer { f: f, header: header }
    }

    /// Write record to BAM.
    ///
    /// # Arguments
    ///
    /// * `record` - the record to write
    pub fn write(&mut self, record: &record::Record) -> Result<(), ()> {
        if unsafe { htslib::bam_write1(self.f, &record.inner) } == -1 {
            Err(())
        }
        else {
            Ok(())
        }
    }
}


impl Drop for Writer {
    fn drop(&mut self) {
        unsafe {
            htslib::bam_hdr_destroy(self.header);
            htslib::bgzf_close(self.f);
        }
    }
}


/// Iterator over the records of a BAM.
pub struct Records<'a, R: 'a + Read> {
    bam: &'a R
}


impl<'a, R: Read> Iterator for Records<'a, R> {
    type Item = Result<record::Record, ReadError>;

    fn next(&mut self) -> Option<Result<record::Record, ReadError>> {
        let mut record = record::Record::new();
        match self.bam.read(&mut record) {
            Err(ReadError::NoMoreRecord) => None,
            Ok(())   => Some(Ok(record)),
            Err(err) => Some(Err(err))
        }
    }
}


pub enum ReadError {
    Truncated,
    Invalid,
    NoMoreRecord,
}


pub enum IndexError {
    InvalidIndex,
}


/// Wrapper for opening a BAM file.
fn bgzf_open<P: path::AsPath>(path: &P, mode: &[u8]) -> *mut htslib::Struct_BGZF {
    unsafe {
        htslib::bgzf_open(
            path.as_path().as_os_str().to_cstring().unwrap().as_ptr(),
            ffi::CString::new(mode).unwrap().as_ptr()
        )
    }
}


/// Wrapper for iterating an indexed BAM file.
fn bam_itr_next(bgzf: *mut htslib::Struct_BGZF, itr: *mut htslib:: hts_itr_t, record: *mut htslib::bam1_t) -> i32 {
    unsafe {
        htslib::hts_itr_next(
            bgzf,
            itr,
            record as *mut libc::types::common::c95::c_void,
            ptr::null_mut()
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate tempdir;
    use super::*;
    use super::record::*;
    use super::header::*;
    use std::str;

    fn gold() -> ([&'static [u8]; 6], [u16; 6], [&'static [u8]; 6], [&'static [u8]; 6], [[Cigar; 3]; 6]) {
        let names = [b"I", b"II.14978392", b"III", b"IV", b"V", b"VI"];
        let flags = [16u16, 16u16, 16u16, 16u16, 16u16, 2048u16];
        let seqs = [
            b"CCTAGCCCTAACCCTAACCCTAACCCTAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAA",
            b"CCTAGCCCTAACCCTAACCCTAACCCTAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAA",
            b"CCTAGCCCTAACCCTAACCCTAACCCTAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAA",
            b"CCTAGCCCTAACCCTAACCCTAACCCTAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAA",
            b"CCTAGCCCTAACCCTAACCCTAACCCTAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAAGCCTAA",
            b"ACTAAGCCTAAGCCTAAGCCTAAGCCAATTATCGATTTCTGAAAAAATTATCGAATTTTCTAGAAATTTTGCAAATTTTTTCATAAAATTATCGATTTTA",
        ];
        let quals = [
            b"#############################@B?8B?BA@@DDBCDDCBC@CDCDCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            b"#############################@B?8B?BA@@DDBCDDCBC@CDCDCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            b"#############################@B?8B?BA@@DDBCDDCBC@CDCDCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            b"#############################@B?8B?BA@@DDBCDDCBC@CDCDCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            b"#############################@B?8B?BA@@DDBCDDCBC@CDCDCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            b"#############################@B?8B?BA@@DDBCDDCBC@CDCDCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
        ];
        let cigars = [
            [Cigar::Match(27), Cigar::Del(1), Cigar::Match(73)],
            [Cigar::Match(27), Cigar::Del(1), Cigar::Match(73)],
            [Cigar::Match(27), Cigar::Del(1), Cigar::Match(73)],
            [Cigar::Match(27), Cigar::Del(1), Cigar::Match(73)],
            [Cigar::Match(27), Cigar::Del(1), Cigar::Match(73)],
            [Cigar::Match(27), Cigar::Del(100000), Cigar::Match(73)],
        ];
        (names, flags, seqs, quals, cigars)
    }

    #[test]
    fn test_read() {
        let (names, flags, seqs, quals, cigars) = gold();
        let bam = Reader::new(&"test.bam");

        for (i, record) in bam.records().enumerate() {
            let rec = record.ok().expect("Expected valid record");
            println!("{}", str::from_utf8(rec.qname()).ok().unwrap());
            assert_eq!(rec.qname(), names[i]);
            assert_eq!(rec.flags(), flags[i]);
            assert_eq!(rec.seq().as_bytes(), seqs[i]);
            assert_eq!(rec.cigar(), cigars[i]);
            // fix qual offset
            let qual: Vec<u8> = quals[i].iter().map(|&q| q - 33).collect();
            assert_eq!(rec.qual(), qual);
        }
    }

    #[test]
    fn test_read_indexed() {
        let (names, flags, seqs, quals, cigars) = gold();
        let mut bam = IndexedReader::new(&"test.bam").ok().expect("Expected valid index.");

        // seek to position containing reads
        bam.seek(0, 0, 2).ok().expect("Expected successful seek.");
        assert!(bam.records().count() == 6);

        // compare reads
        bam.seek(0, 0, 2).ok().expect("Expected successful seek.");
        for (i, record) in bam.records().enumerate() {
            let rec = record.ok().expect("Expected valid record");
            println!("{}", str::from_utf8(rec.qname()).ok().unwrap());
            assert_eq!(rec.qname(), names[i]);
            assert_eq!(rec.flags(), flags[i]);
            assert_eq!(rec.seq().as_bytes(), seqs[i]);
            assert_eq!(rec.cigar(), cigars[i]);
            // fix qual offset
            let qual: Vec<u8> = quals[i].iter().map(|&q| q - 33).collect();
            assert_eq!(rec.qual(), qual);
        }

        // seek to empty position
        bam.seek(2, 1, 1).ok().expect("Expected successful seek.");
        assert!(bam.records().count() == 0);
    }

    #[test]
    fn test_set_record() {

        let (names, _, seqs, quals, cigars) = gold();

        let mut rec = record::Record::new();
        rec.set_reverse();
        rec.set(names[0], &cigars[0], seqs[0], quals[0]);
        rec.push_aux(b"NM", &Aux::Integer(15));

        assert_eq!(rec.qname(), names[0]);
        assert_eq!(rec.cigar(), cigars[0]);
        assert_eq!(rec.seq().as_bytes(), seqs[0]);
        assert_eq!(rec.qual(), quals[0]);
        assert!(rec.is_reverse());
        assert_eq!(rec.aux(b"NM").unwrap(), Aux::Integer(15));
    }

    #[test]
    fn test_write() {
        let qname = b"I";
        let cigar = [Cigar::Match(27), Cigar::Del(1), Cigar::Match(73)];
        let seq =  b"CCTAGCCCTAACCCTAACCCTAACCCTAGCCTAAGCCTAAGCCTAAGCCTAAGC";
        let qual = b"JJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJ";

        let tmp = tempdir::TempDir::new("rust-htslib").ok().expect("Cannot create temp dir");
        let bampath = tmp.path().join("test.bam");
        {
            let mut bam = Writer::new(
                &bampath,
                Header::new().push_record(
                    HeaderRecord::new(b"SQ").push_tag(b"SN", &"chr1")
                                            .push_tag(b"LN", &15072423)
                )
            );

            let mut rec = record::Record::new();
            rec.set(qname, &cigar, seq, qual);
            rec.push_aux(b"NM", &Aux::Integer(15));

            bam.write(&mut rec).ok().expect("Failed to write record.");
        }

        {
            let bam = Reader::new(&bampath);

            let mut rec = record::Record::new();
            bam.read(&mut rec).ok().expect("Failed to read record.");

            assert_eq!(rec.qname(), qname);
            assert_eq!(rec.cigar(), cigar);
            assert_eq!(rec.seq().as_bytes(), seq);
            assert_eq!(rec.qual(), qual);
            assert_eq!(rec.aux(b"NM").unwrap(), Aux::Integer(15));
        }

        tmp.close().ok().expect("Failed to delete temp dir");
    }
}