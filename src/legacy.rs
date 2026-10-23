//! Older MNF/DAT/ZOSFT helper routines from EsoExtractData.cpp.
//!
//! None of these are reachable from `_tmain` in the C++ program (the real extraction lives in
//! `CMnfFile::Export`), but they are ported for completeness and because the ZOSFT parser is a good
//! reference for the MNF port.  Quirks of the originals are intentionally kept - for example
//! [`inflate_sub_file`] leaves the file position at the end of the last chunk it read rather than at
//! the end of the zlib stream.
#![allow(dead_code)]

use eso_common::error::Result;
use eso_common::util::{create_filename, ensure_path_exists, find_files, terminate_path};
use eso_common::{eso_error, print_error};
use flate2::{Decompress, DecompressError, FlushDecompress, Status};
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, MAIN_SEPARATOR_STR};

const RENAME_ASSETS: bool = false;

const CHUNK_SIZE: usize = 262_144;
const DATA_HEADER_SIZE: usize = 0x0E;
const ZOSFT_HEADER_SIZE1: usize = 0x2D;
const ZOSFT_MIDHEADER_SIZE: usize = 0x08;

const OFFSET: usize = 0x29; // Game.mnf
const FILE_DELTA: usize = 0x08;
const MNF_OFFSET: u64 = 0x29;
const MNF_FILE_DELTA: i64 = 0x08;

// ---------------------------------------------------------------------------------------------
// Small zlib streaming helper (mirrors the z_stream + InputBuffer/OutputBuffer pairs in the C++)
// ---------------------------------------------------------------------------------------------

struct Step {
    status: Status,
    consumed: usize,
    produced: usize,
}

struct InflateCtx {
    d: Decompress,
    inbuf: Vec<u8>,
    in_pos: usize,
    in_len: usize,
    outbuf: Vec<u8>,
}

impl InflateCtx {
    fn new() -> Self {
        Self::with_input(vec![0; CHUNK_SIZE], 0, 0)
    }

    fn with_input(data: Vec<u8>, start: usize, len: usize) -> Self {
        Self { d: Decompress::new(true), inbuf: data, in_pos: start, in_len: len, outbuf: vec![0; CHUNK_SIZE] }
    }

    fn avail_in(&self) -> usize {
        self.in_len - self.in_pos
    }

    /// Like `fread(InputBuffer, 1, CHUNK_SIZE, f)`: reads until the buffer is full or EOF.
    fn fill<R: Read>(&mut self, r: &mut R) -> io::Result<usize> {
        let mut n = 0;
        while n < CHUNK_SIZE {
            match r.read(&mut self.inbuf[n..CHUNK_SIZE])? {
                0 => break,
                k => n += k,
            }
        }
        self.in_pos = 0;
        self.in_len = n;
        Ok(n)
    }

    /// One `inflate(&Stream, Z_NO_FLUSH)` call with a fresh CHUNK_SIZE output window.
    fn step(&mut self) -> std::result::Result<Step, DecompressError> {
        let (bi, bo) = (self.d.total_in(), self.d.total_out());
        let r = self.d.decompress(&self.inbuf[self.in_pos..self.in_len], &mut self.outbuf, FlushDecompress::None);
        let consumed = (self.d.total_in() - bi) as usize;
        let produced = (self.d.total_out() - bo) as usize;
        self.in_pos += consumed;
        r.map(|status| Step { status, consumed, produced })
    }

    fn out(&self, n: usize) -> &[u8] {
        &self.outbuf[..n]
    }

    fn reset(&mut self) {
        self.d.reset(true);
    }
}

// ---------------------------------------------------------------------------------------------
// esosubfile_t / InflateSubFile
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct EsoSubFile {
    pub file_data: Vec<u8>,
    pub start_offset: u64,
    pub end_offset: u64,
}

impl EsoSubFile {
    pub fn destroy(&mut self) {
        *self = Self::default();
    }

    pub fn append_data(&mut self, data: &[u8]) {
        self.file_data.extend_from_slice(data);
    }

    pub fn input_size(&self) -> i32 {
        (self.end_offset as i64 - self.start_offset as i64) as i32
    }
}

pub fn inflate_sub_file(sub: &mut EsoSubFile, input: &mut File, file_size: u64) -> Result<()> {
    sub.destroy();
    sub.start_offset = input.stream_position()?;

    let mut ctx = InflateCtx::new();
    let mut status;

    loop {
        if ctx.avail_in() == 0 {
            let cur = input.stream_position()?;
            sub.end_offset = cur;
            print_error!("\t0x{:08X}: Loading more input data...", cur);

            if cur == file_size {
                print_error!("Reached end of input file.");
                break;
            } else if cur > file_size {
                print_error!("Warning: Exceeded end of input file by 0x{:8X} bytes!", cur - file_size);
                break;
            }

            let n = ctx.fill(input).map_err(|_| eso_error!("Error: Failed to read bytes from input file!"))?;
            sub.end_offset = input.stream_position()?;
            if n == 0 { break; }
        }

        loop {
            let step = ctx.step().map_err(|_| eso_error!("Error: Failed to uncompress data stream!"))?;
            status = step.status;
            sub.append_data(ctx.out(step.produced));
            if step.produced < CHUNK_SIZE { break; }
        }

        if status == Status::StreamEnd { break; }
    }

    sub.end_offset = input.stream_position()?;
    print_error!("Inflated 0x{:X} bytes from input starting at 0x{:X} with a length of 0x{:X}",
        sub.file_data.len(), sub.start_offset, sub.input_size());
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Paths / text files
// ---------------------------------------------------------------------------------------------

/// `<base_output_path>\<filename with extension>\`
pub fn create_output_path(filename: &str, base_output_path: &str) -> String {
    let mut out = terminate_path(base_output_path);
    if let Some(name) = Path::new(filename).file_name() {
        out.push_str(&name.to_string_lossy());
    }
    out.push(std::path::MAIN_SEPARATOR);
    print_error!("OutputPath() = {}", out);
    out
}

// ---------------------------------------------------------------------------------------------
// ZOSFT
// ---------------------------------------------------------------------------------------------

/// Inflates the 8 zlib streams at the start of a ZOSFT blob into `<filename>.1` .. `<filename>.8`, then
/// dumps the trailing string table to `<filename>.ZOSFT`.
pub fn process_zosft(filename: &str, output_path: &str) -> Result<()> {
    let buf = fs::read(filename).map_err(|_| eso_error!("Error: Failed to open the ZOSFT file '{}'!", filename))?;
    if buf.is_empty() {
        return Err(eso_error!("Error: Invalid file size for ZOSFT!"));
    }
    if !buf.starts_with(b"ZOSFT") {
        return Err(eso_error!("Warning: File is not a ZOSFT format!"));
    }

    print_error!("Found ZOSFT data in last file!");

    let file_size = buf.len();
    let mut ctx = InflateCtx::with_input(buf, ZOSFT_HEADER_SIZE1, file_size);
    let mut output_file_count = 0;

    'outer: while ctx.avail_in() > 0 {
        if output_file_count == 8 {
            print_error!("Manually stopping ZOSFT deflation...");
            break;
        }
        if output_file_count > 0 {
            ctx.in_pos = (ctx.in_pos + ZOSFT_MIDHEADER_SIZE).min(ctx.in_len);
        }
        if output_file_count == 3 || output_file_count == 6 {
            ctx.in_pos = (ctx.in_pos + 0x12).min(ctx.in_len);
        }

        print_error!("ZOSFT: Processing input at 0x{:08X}", ctx.in_pos);

        output_file_count += 1;
        let out_name = format!("{}.{}", filename, output_file_count);
        print_error!("{}: Processing ZOSFT output file...", output_file_count);

        let mut dest = io::BufWriter::new(File::create(&out_name)
            .map_err(|_| eso_error!("Error: Failed to open output file '{}' for writing!", out_name))?);

        loop {
            if ctx.avail_in() == 0 { break; }

            let mut status;
            loop {
                // A data error ends the deflation phase (the C++ `goto ENDDEFLATE`).
                let Ok(step) = ctx.step() else { break 'outer };
                status = step.status;

                dest.write_all(ctx.out(step.produced))
                    .map_err(|_| eso_error!("Error: Failed to write data to output file!"))?;

                if step.consumed == 0 && step.produced == 0 { break 'outer; } // no progress: avoid spinning
                if step.produced < CHUNK_SIZE { break; }
            }

            if status == Status::StreamEnd { break; }
        }

        drop(dest);
        ctx.reset();
    }

    let end_pos = ctx.in_pos.saturating_sub(6);
    print_error!("Ended ZOSFT deflation at 0x{:08X}...parsing strings", end_pos);

    let buf = std::mem::take(&mut ctx.inbuf);
    let mut lines: Vec<String> = Vec::new();
    let mut pos = end_pos;
    while pos < buf.len() {
        let rest = &buf[pos..];
        let n = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        lines.push(String::from_utf8_lossy(&rest[..n]).into_owned());
        pos += n + 1;
    }

    print_error!("Found {} lines in ZOSFT...", lines.len());

    let zosft_name = format!("{}.ZOSFT", filename);
    let mut f = File::create(&zosft_name)
        .map_err(|_| eso_error!("Error: Failed to open the ZOSFT text file for output!"))?;
    for line in &lines {
        f.write_all(line.as_bytes())?; // written back to back, as in the original
    }
    drop(f);

    if !RENAME_ASSETS {
        return Ok(());
    }

    for (i, line) in lines.iter().enumerate() {
        let src = format!("{}{}.dat", output_path, i + 1);
        let new_name = format!("{}{}", output_path, line.get(1..).unwrap_or("").replace('/', MAIN_SEPARATOR_STR));
        if let Some(parent) = Path::new(&new_name).parent() {
            ensure_path_exists(&parent.to_string_lossy());
        }
        print_error!("{}: Moving file to '{}'...", i, new_name);
        if fs::rename(&src, &new_name).is_err() {
            print_error!("\tError: File rename failed!");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// ESO data (.dat) files
// ---------------------------------------------------------------------------------------------

/// Inflates every consecutive zlib stream in a data file into `<out>\1.dat`, `2.dat`, ... and then
/// runs [`process_zosft`] on the last one.
pub fn export_eso_data_file(filename: &str, output_path: &str) -> Result<()> {
    let out_path = create_output_path(filename, output_path);
    if fs::create_dir_all(&out_path).is_err() {
        print_error!("Warning: Failed to create the output directory '{}'!", out_path);
    }

    let mut src = File::open(filename).map_err(|_| eso_error!("Error: Failed to open input file '{}'!", filename))?;
    let file_size = src.metadata()?.len();
    if file_size == 0 {
        return Err(eso_error!("Error: Invalid source file size (too small or too large)!"));
    }

    let mut header = [0u8; DATA_HEADER_SIZE];
    src.read_exact(&mut header)
        .map_err(|_| eso_error!("Error: Failed to read {} bytes of header data!", DATA_HEADER_SIZE))?;

    print_error!("Header: ");
    for b in header {
        print_error!("{:02X} ", b);
    }
    print_error!("");

    let mut ctx = InflateCtx::new();
    let mut output_file_count = 0;
    let mut output_filename = String::new();
    let mut eof = false;

    while !eof {
        let cur = src.stream_position()?;
        if cur == file_size {
            print_error!("Reached end of input file.");
            break;
        } else if cur > file_size {
            print_error!("Warning: Exceeded end of input file by 0x{:8X} bytes!", cur - file_size);
            break;
        }

        output_file_count += 1;
        output_filename = format!("{}{}.dat", out_path, output_file_count);
        print_error!("{}: Processing output file...", output_file_count);

        let mut dest = File::create(&output_filename)
            .map_err(|_| eso_error!("Error: Failed to open output file '{}' for writing!", output_filename))?;

        loop {
            if ctx.avail_in() == 0 {
                let cur = src.stream_position()?;
                print_error!("\t0x{:08X}: Loading more input data...", cur);
                if cur == file_size {
                    print_error!("Reached end of input file.");
                    break;
                } else if cur > file_size {
                    print_error!("Warning: Exceeded end of input file by 0x{:8X} bytes!", cur - file_size);
                    break;
                }
                let n = ctx.fill(&mut src).map_err(|_| eso_error!("Error: Failed to read bytes from input file!"))?;
                eof = n < CHUNK_SIZE;
                if n == 0 { break; }
            }

            let mut status;
            loop {
                let step = ctx.step().map_err(|_| eso_error!("Error: Failed to uncompress data stream!"))?;
                status = step.status;
                dest.write_all(ctx.out(step.produced))
                    .map_err(|_| eso_error!("Error: Failed to write data to output file!"))?;
                if step.produced < CHUNK_SIZE { break; }
            }
            if status == Status::StreamEnd { break; }
        }

        drop(dest);
        ctx.reset();
    }

    // Like the original, a failure while processing the ZOSFT is not an error for the export itself.
    let _ = process_zosft(&output_filename, &out_path);
    Ok(())
}

/// Exports every file matching a wildcard spec such as `C:\game\*.dat`.
pub fn export_eso_data_files(file_spec: &str, output_path: &str) -> Result<()> {
    let files = find_files(file_spec).map_err(|_| eso_error!("Error: Failed to find files matching '{}'!", file_spec))?;
    if files.is_empty() {
        return Err(eso_error!("Error: Failed to find files matching '{}'!", file_spec));
    }
    for f in files {
        let _ = export_eso_data_file(&f.to_string_lossy(), output_path);
    }
    Ok(())
}

/// Walks the zlib blocks of a game.mnf-style file (`MNF_OFFSET` header, `MNF_FILE_DELTA` between blocks).
pub fn load_eso_mnf_file(filename: &str, output_path: &str) -> Result<()> {
    let out_path = create_output_path(filename, output_path);
    if fs::create_dir_all(&out_path).is_err() {
        print_error!("Warning: Failed to create the output directory '{}'!", out_path);
    }

    let mut src = File::open(filename).map_err(|_| eso_error!("Error: Failed to open input file '{}'!", filename))?;
    let file_size = src.metadata()?.len();
    if file_size == 0 {
        return Err(eso_error!("Error: Invalid source file size (too small or too large)!"));
    }

    let mut header = [0u8; DATA_HEADER_SIZE];
    src.read_exact(&mut header)
        .map_err(|_| eso_error!("Error: Failed to read {} bytes of header data!", DATA_HEADER_SIZE))?;
    print_error!("Header: ");
    for b in header {
        print_error!("{:02X} ", b);
    }
    print_error!("");

    src.seek(io::SeekFrom::Current(MNF_OFFSET as i64 - DATA_HEADER_SIZE as i64 - MNF_FILE_DELTA))?;

    let mut sub = EsoSubFile::default();
    let mut count = 0;
    loop {
        src.seek(io::SeekFrom::Current(MNF_FILE_DELTA))?;
        let cur = src.stream_position()?;
        if cur == file_size {
            print_error!("Reached end of input file.");
            break;
        } else if cur > file_size {
            print_error!("Warning: Exceeded end of input file by 0x{:8X} bytes!", cur - file_size);
            break;
        }

        count += 1;
        let _output_filename = create_filename(&out_path, &format!("{}.dat", count));
        print_error!("{}: Processing output file...", count);

        inflate_sub_file(&mut sub, &mut src, file_size)?;
    }
    Ok(())
}

/// Decompresses consecutive zlib blocks of a whole file into `output.dat` (debug helper).
pub fn load_eso_data_file1(filename: &str) -> Result<()> {
    let input = fs::read(filename).map_err(|_| eso_error!("Error: Failed to open file!"))?;
    let mut output = vec![0u8; input.len() * 4 + 1024];

    let (mut input_offset, mut output_offset, mut block) = (0usize, 0usize, 0);

    while input_offset + OFFSET < input.len() {
        block += 1;
        print_error!("Block {}: ", block);
        print_error!("Input Offset: 0x{:X} ", input_offset + OFFSET);

        let mut d = Decompress::new(true);
        let r = d.decompress(&input[OFFSET + input_offset..], &mut output[output_offset..], FlushDecompress::None);
        print_error!(" Result={} ", match &r { Ok(Status::Ok) => 0, Ok(Status::StreamEnd) => 1, Ok(Status::BufError) => -5, Err(_) => -3 });

        input_offset += d.total_in() as usize;
        if r.is_err() {
            print_error!("Error(-3): {}", r.err().map(|e| e.to_string()).unwrap_or_default());
            break;
        }
        output_offset += d.total_out() as usize;
        print_error!("In-Out: 0x{:X} 0x{:X},  Total: 0x{:08X} 0x{:08X}", d.total_in(), d.total_out(), input_offset + OFFSET, output_offset);

        input_offset += FILE_DELTA;
    }

    let mut f = File::create("output.dat").map_err(|_| eso_error!("Error: Failed to open file for output!"))?;
    f.write_all(&output[..output_offset])?;

    print_error!("Successfully decompressed file!");
    print_error!("End Input Offset = 0x{:08X}", (input_offset + OFFSET).saturating_sub(FILE_DELTA));
    Ok(())
}
