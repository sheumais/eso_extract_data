//! eso_scan - search every file of an ESO install for text, including inside the MNF/DAT archives
//! (zlib / snappy / Oodle compressed sub-files and the records inside EsoFileData sub-files).
//!
//! Examples:
//!   eso_scan "C:\Program Files (x86)\Steam\steamapps\common\Zenimax Online\The Elder Scrolls Online"
//!   eso_scan <folder> -s "Z_SOMETHING" -s "OTHER" --any --csv hits.csv
//!   eso_scan <folder> --oodle "C:\path\to\oo2core_8_win64.dll" -j 8

use clap::Parser;
use eso_extract_data::scan::{self, ScanOptions};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "eso_scan", about = "Search an Elder Scrolls Online install (including MNF/DAT archives) for text.")]
struct Cli {
    /// Game folder; searched recursively for *.mnf archives and loose files.
    root: PathBuf,

    /// String to search for (repeatable). Defaults to the strings from the screenshot.
    #[arg(short = 's', long = "string")]
    strings: Vec<String>,

    /// Report files containing ANY of the strings (default: all strings in the same sub-file).
    #[arg(long)]
    any: bool,

    /// Match letter case exactly.
    #[arg(long)]
    case_sensitive: bool,

    /// Also search for the UTF-16LE form of each string.
    #[arg(long)]
    utf16: bool,

    /// Skip loose files; only search inside the MNF/DAT archives.
    #[arg(long)]
    no_loose: bool,

    /// Do not inflate the records inside EsoFileData sub-files.
    #[arg(long)]
    no_records: bool,

    /// Only scan MNF files whose path contains this text (e.g. "game.mnf").
    #[arg(long)]
    mnf: Option<String>,

    /// Worker threads (default: one per logical CPU).
    #[arg(short = 'j', long, default_value_t = 0)]
    threads: usize,

    /// Oodle library (oo2core_*.dll / .so) or the folder containing it.
    #[arg(long)]
    oodle: Option<String>,

    /// Write one CSV row per hit to this file.
    #[arg(long)]
    csv: Option<PathBuf>,

    /// Bytes of context shown around each match.
    #[arg(long, default_value_t = 24)]
    context: usize,

    /// Seconds between progress lines (0 = none).
    #[arg(long, default_value_t = 10)]
    progress: u64,
}

fn main() {
    let cli = Cli::parse();
    let mut opts = ScanOptions::new(&cli.root);
    if !cli.strings.is_empty() {
        opts.patterns = cli.strings;
    }
    opts.match_all = !cli.any;
    opts.case_sensitive = cli.case_sensitive;
    opts.utf16 = cli.utf16;
    opts.scan_loose = !cli.no_loose;
    opts.scan_records = !cli.no_records;
    opts.mnf_filter = cli.mnf;
    opts.threads = cli.threads;
    opts.oodle_path = cli.oodle;
    opts.csv_path = cli.csv;
    opts.context_bytes = cli.context;
    opts.progress_secs = cli.progress;

    match scan::run(&opts) {
        Ok(summary) => print!("{}", scan::format_summary(&summary, &opts)),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
