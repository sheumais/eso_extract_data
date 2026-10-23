//! Command line definition.  Replaces `CCmdParamHandler` / the `g_Cmds` table.
//!
//! Differences from the C++ parser: clap cannot do multi-character short options, so `-i1` / `-i2`
//! are rewritten to `--idfile1` / `--idfile2` before parsing (see [`normalize_args`]).

use clap::Parser;
use eso_common::mnf::MnfExportOptions;
use eso_common::print_error;
use eso_common::util::terminate_path;

pub const APP_TITLE: &str = "EsoExtractData v0.53 (30 April 2024)";
pub const APP_DESCRIPTION: &str = "EsoExtractData v0.53 is a simple command line application to load and export files\n\
from Elder Scrolls Online's MNF and DAT files. Created by Daveh (dave@uesp.net).";

#[derive(Parser, Debug)]
#[command(name = "ExportMnf", about = APP_DESCRIPTION)]
pub struct Args {
    /// Input MNF filename to load.
    pub mnffile: Option<String>,
    /// Path to save extracted data files to.
    pub outputpath: Option<String>,

    /// Dump the MNF filetable to the specified text file.
    #[arg(short = 'm', long)]
    pub mnfft: Option<String>,
    /// Dump the ZOS filetable to the specified text file.
    #[arg(short = 'z', long)]
    pub zosft: Option<String>,
    /// Only extract the given filename.
    #[arg(short = 'n', long)]
    pub filename: Option<String>,
    /// Only extract files with the given extension.
    #[arg(short = 'y', long)]
    pub fileext: Option<String>,
    /// Start exporting sub-files at the given file index.
    #[arg(short = 's', long, default_value_t = -1, allow_negative_numbers = true)]
    pub startindex: i32,
    /// Stop exporting sub-files at the given file index.
    #[arg(short = 'e', long, default_value_t = -1, allow_negative_numbers = true)]
    pub endindex: i32,
    /// Only export MNF file with the given index.
    #[arg(short = 'a', long = "archive", default_value_t = -1, allow_negative_numbers = true)]
    pub archiveindex: i32,
    /// Start with the given MNF file index.
    #[arg(short = 'b', long, default_value_t = -1, allow_negative_numbers = true)]
    pub beginarchive: i32,
    /// Only export MNF the subfile with the given file index.
    #[arg(short = 'f', long)]
    pub fileindex: Option<String>,
    /// Don't export subfiles from the MNF data.
    #[arg(short = 'k', long)]
    pub skipsubfiles: bool,
    /// Convert the given .lang file to a CSV.
    #[arg(short = 'l', long = "lang")]
    pub langfile: Option<String>,
    /// Convert the given language CSV file to a .LANG.
    #[arg(short = 'x', long)]
    pub createlang: Option<String>,
    /// Specify the output file for -l and -x.
    #[arg(short = 'o', long)]
    pub outputfile: Option<String>,
    /// Import/Export the CSV/Text file in a PO compatible format.
    #[arg(short = 'p', long)]
    pub pocsv: bool,
    /// Use the source text when converting a PO-CSV file to a LANG.
    #[arg(long)]
    pub posourcetext: bool,
    /// Output the LANG file in plain text format.
    #[arg(short = 't', long)]
    pub langtext: bool,
    /// The ID file to use when converting a TXT file to LANG.
    #[arg(short = 'i', long)]
    pub idfile: Option<String>,
    /// The ID file to use for the first TXT file when comparing files (-i1).
    #[arg(long)]
    pub idfile1: Option<String>,
    /// The ID file to use for the second TXT file when comparing files (-i2).
    #[arg(long)]
    pub idfile2: Option<String>,
    /// Compare two LANG/CSV/TXT files for differences.
    #[arg(short = 'd', long, num_args = 2, value_names = ["FILE1", "FILE2"])]
    pub difflang: Option<Vec<String>>,
    /// Use this LANG/CSV/TXT file for source texts when comparing files.
    #[arg(short = 'g', long)]
    pub origlang: Option<String>,
    /// Extract compressed subfile data (none/combined/seperate).
    #[arg(long)]
    pub extractsubfile: Option<String>,
    /// Don't parse GR2 files for their original filename.
    #[arg(long)]
    pub noparsegr2: bool,
    /// Output the raw/compressed Oodle files.
    #[arg(long)]
    pub oodleoutput: bool,
    /// Output filenames to a LUA formatted array.
    #[arg(long)]
    pub luafilelist: Option<String>,
    /// Start index for the --luafilelist option.
    #[arg(long, default_value_t = 1, allow_negative_numbers = true)]
    pub luastartindex: i32,
    /// Don't convert RIFF files to WAV/OGG.
    #[arg(long)]
    pub noriffconvert: bool,
    /// Only extract files containing the given substring.
    #[arg(long)]
    pub matchfilename: Option<String>,
    /// Use the specified LANG file for missing entries.
    #[arg(long)]
    pub baselangfile: Option<String>,
    /// Output additional debug messages.
    #[arg(short = 'v', long)]
    pub debugoutput: bool,
}

/// Rewrites the two multi-character short options (`-i1`, `-i2`) to their long forms.
pub fn normalize_args<I: IntoIterator<Item = String>>(args: I) -> Vec<String> {
    args.into_iter()
        .map(|a| match a.as_str() {
            "-i1" => "--idfile1".to_string(),
            "-i2" => "--idfile2".to_string(),
            _ => a,
        })
        .collect()
}

impl Args {
    pub fn to_export_options(&self) -> MnfExportOptions {
        let s = |o: &Option<String>| o.clone().unwrap_or_default();
        let diff = |i: usize| self.difflang.as_ref().and_then(|d| d.get(i)).cloned().unwrap_or_default();

        MnfExportOptions {
            mnf_filename: s(&self.mnffile),
            output_path: terminate_path(&s(&self.outputpath)),
            mnf_output_file_table: s(&self.mnfft),
            zos_output_file_table: s(&self.zosft),
            mnf_start_index: self.startindex,
            mnf_end_index: self.endindex,
            archive_index: self.archiveindex,
            begin_archive_index: self.beginarchive,
            mnf_file_indices: parse_index_list(&s(&self.fileindex)),
            convert_dds: false,
            skip_sub_files: self.skipsubfiles,
            use_lang_text: self.langtext,
            use_po_csv_format: self.pocsv,
            use_po_source_text: self.posourcetext,
            no_parse_gr2: self.noparsegr2,
            lang_filename: s(&self.langfile),
            create_lang_filename: s(&self.createlang),
            output_filename: s(&self.outputfile),
            import_id_filename: s(&self.idfile),
            diff_lang_filename1: diff(0),
            diff_lang_filename2: diff(1),
            id_filename1: s(&self.idfile1),
            id_filename2: s(&self.idfile2),
            orig_lang_filename: s(&self.origlang),
            extract_filename: s(&self.filename),
            oodle_raw_output: self.oodleoutput,
            extract_file_extension: s(&self.fileext),
            lua_file_list: s(&self.luafilelist),
            lua_start_index: self.luastartindex,
            no_riff_convert: self.noriffconvert,
            match_filename: s(&self.matchfilename),
            base_lang_filename: s(&self.baselangfile),
            debug_output: self.debugoutput,
            extract_sub_file_data_type: s(&self.extractsubfile).to_lowercase(),
        }
    }
}

/// Parses "123, 0x336C45,456" into file indices; invalid entries are reported and skipped.
pub fn parse_index_list(s: &str) -> Vec<u32> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .filter_map(|t| {
            let v = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                Some(h) => u32::from_str_radix(h, 16),
                None => t.parse::<u32>(),
            };
            if v.is_err() { print_error!("Warning: Ignoring invalid file index '{}'!", t); }
            v.ok()
        })
        .collect()
}