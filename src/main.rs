#![allow(non_snake_case)] // binary is named EsoExtractData like the original
//! EsoExtractData - Rust port of Dave Humphrey's (dave@uesp.net) command line tool that extracts
//! data/files from ESO (Elder Scrolls Online) MNF and DAT game data files and converts LANG files.
//!
//! Use `EsoExtractData -h` for help.

use clap::Parser;
use eso_extract_data::cli::{self, Args, APP_TITLE};
use eso_extract_data::lang_tools;
use eso_common::csv::CsvFile;
use eso_common::lang::EsoLangFile;
use eso_common::log::{open_log, set_output_debug_log};
use eso_common::mnf::MnfFile;
use eso_common::oodle::load_oodle_lib;
use eso_common::util::{remove_file_extension, string_ends_with};
use eso_common::{print_error, print_log};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    open_log("exportmnf.log");

    // The C++ program aborted here (return -1) when Oodle could not be loaded.  Only MNF export of
    // Oodle-compressed data needs it, so LANG conversion keeps working without the DLL.
    if !load_oodle_lib() {
        print_error!("Warning: Oodle is not available; Oodle compressed sub-files cannot be extracted.");
    }

    let argv = cli::normalize_args(std::env::args());
    let args = match Args::try_parse_from(argv) {
        Ok(a) => a,
        Err(e) => {
            let _ = e.print();
            if e.use_stderr() {
                print_error!("Invalid command line parameter received...aborting!");
                return -1;
            }
            return 0; // --help
        }
    };

    print_log!("Cmd Params: {:?}", args);

    let mut opts = args.to_export_options();

    print_error!("{}", APP_TITLE);

    if args.debugoutput {
        print_error!("Outputting additional debug messages.");
        opts.debug_output = true;
        set_output_debug_log(true);
    }

    // ---- LANG file comparison ----
    if !opts.diff_lang_filename1.is_empty() && !opts.diff_lang_filename2.is_empty() {
        let output_filename = if opts.output_filename.is_empty() { opts.diff_lang_filename2.clone() } else { opts.output_filename.clone() };

        if opts.import_id_filename.is_empty() { opts.import_id_filename = opts.id_filename1.clone(); }
        if opts.id_filename1.is_empty() { opts.id_filename1 = opts.import_id_filename.clone(); }

        return match lang_tools::diff_lang_files(
            &opts.orig_lang_filename, &opts.import_id_filename,
            &opts.diff_lang_filename1, &opts.diff_lang_filename2,
            &opts.id_filename1, &opts.id_filename2,
            &output_filename, opts.use_lang_text, opts.use_po_csv_format,
        ) {
            Ok(()) => 0,
            Err(_) => -100,
        };
    }

    // ---- .LANG file conversion to CSV / TXT ----
    if !opts.lang_filename.is_empty() {
        let mut id_output_filename = format!("{}{}", opts.lang_filename, if opts.use_lang_text { ".id.txt" } else { ".id.csv" });
        let mut output_filename = format!("{}{}", opts.lang_filename, if opts.use_lang_text { ".txt" } else { ".csv" });

        if !opts.output_filename.is_empty() {
            output_filename = opts.output_filename.clone();
            id_output_filename = format!("{}.id.txt", opts.output_filename);
        }

        print_error!("Converting LANG file '{}' to {} '{}'...", opts.lang_filename, if opts.use_lang_text { "TXT" } else { "CSV" }, output_filename);

        let mut lang = EsoLangFile::new();
        if lang.load(&opts.lang_filename).is_ok() {
            print_error!("Loaded LANG file '{}'...", opts.lang_filename);

            let saved = if opts.use_lang_text {
                lang.dump_text(&output_filename, opts.use_po_csv_format).is_ok()
            } else {
                lang.dump_csv(&output_filename, opts.use_po_csv_format).is_ok()
            };
            if saved {
                print_error!("Saved the LANG file to '{}'!", output_filename);
            } else {
                print_error!("Failed to save the LANG file '{}'!", output_filename);
            }

            if lang.dump_text_id(&id_output_filename).is_ok() {
                print_error!("Saved the ID text file to '{}'!", id_output_filename);
            } else {
                print_error!("Failed to save the ID text file '{}'!", id_output_filename);
            }
        } else {
            print_error!("Failed to load the LANG file '{}'!", opts.lang_filename);
        }
    }

    // ---- .LANG file creation from CSV / TXT ----
    if !opts.create_lang_filename.is_empty() {
        let code = create_lang_file(&opts);
        if code != 0 {
            return code;
        }
    }

    // ---- MNF export ----
    if !opts.mnf_filename.is_empty() {
        let mut mnf = MnfFile::new();
        let _ = mnf.export(&opts); // the C++ ignored the result; the error is already logged
    } else {
        print_error!("Skipping MNF export...no file specified!");
    }

    0
}

fn create_lang_file(opts: &eso_common::mnf::MnfExportOptions) -> i32 {
    let input_type = if opts.use_lang_text { "TXT" } else { "CSV" };
    let mut output_lang_filename = remove_file_extension(&opts.create_lang_filename);
    if !string_ends_with(&output_lang_filename, ".lang") { output_lang_filename.push_str(".lang"); }
    if !opts.output_filename.is_empty() { output_lang_filename = opts.output_filename.clone(); }

    print_error!("Converting {} file '{}' to LANG '{}'...", input_type, opts.create_lang_filename, output_lang_filename);

    let mut base_lang = EsoLangFile::new();
    if !opts.base_lang_filename.is_empty() {
        if base_lang.load(&opts.base_lang_filename).is_err() {
            print_error!("Error: Failed to load the base LANG file '{}'!", opts.base_lang_filename);
            return -20;
        }
        print_error!("Using LANG file '{}' for the base language entries....", opts.base_lang_filename);
    }

    let mut lang = EsoLangFile::new();

    if opts.use_lang_text {
        if opts.import_id_filename.is_empty() {
            print_error!("Error: An ID file (-i <filename>) is required when converting  a TXT file to LANG!");
            return -1;
        }

        let Ok(id_file) = lang_tools::load_simple_text_file(&opts.import_id_filename) else {
            print_error!("Error: Failed to load the ID file '{}'!", opts.import_id_filename);
            return -2;
        };
        print_error!("Loaded ID file '{}' with {} rows...", opts.import_id_filename, id_file.len());

        let Ok(text_file) = lang_tools::load_simple_text_file(&opts.create_lang_filename) else {
            print_error!("Error: Failed to load the TXT file '{}'!", opts.create_lang_filename);
            return -3;
        };
        print_error!("Loaded TXT file '{}' with {} rows...", opts.create_lang_filename, text_file.len());

        if lang.create_from_text(&text_file, &id_file, opts.use_po_csv_format, opts.use_po_source_text).is_err() {
            print_error!("Failed to create the LANG file from {} data!", input_type);
            return -4;
        }
        print_error!("Created the LANG file from the input TEXT data...");
    } else {
        let mut csv = CsvFile::new(!opts.use_lang_text);
        if csv.load(&opts.create_lang_filename).is_err() {
            print_error!("Failed to load the CSV file '{}'!", opts.create_lang_filename);
            return -6;
        }
        print_error!("Loaded {} file '{}' with {} rows...", input_type, opts.create_lang_filename, csv.num_rows());

        if lang.create_from_csv(&csv, opts.use_po_csv_format, opts.use_po_source_text).is_err() {
            print_error!("Failed to create the LANG file from {} data!", input_type);
            return -5;
        }
        print_error!("Created the LANG file from the input CSV data...");
    }

    if !opts.base_lang_filename.is_empty() {
        let filled = lang.fill_missing_entries(&base_lang);
        print_error!("Filled in {} missing entries from the BASE lang file.", filled);
    }

    if lang.save(&output_lang_filename).is_ok() {
        print_error!("Saved the LANG file to '{}'!", output_lang_filename);
    } else {
        print_error!("Failed to save the LANG file to '{}'!", output_lang_filename);
    }
    0
}
